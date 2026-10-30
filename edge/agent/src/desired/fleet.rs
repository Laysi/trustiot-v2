//! The devices the agent polls: one polling loop ([`crate::polling::run`]) per device of the
//! applied configuration, each a task of its own, reading through the shared connection of its
//! endpoint ([`Endpoints`]).
//!
//! A new configuration is applied as a difference ([`Fleet::apply`]):
//!
//! - A device whose settings and model are unchanged keeps its task: its schedule, its
//!   connection, its backoff and watchdog state and whatever the [`Sink`] keeps for it go on as
//!   if nothing happened, so a change to other devices never resets its values or its health
//!   (rebuild checklist E1: the legacy system zeroed every device and showed it offline on any
//!   change).
//! - A device whose settings or model changed gets a new task with the new reader; the sink
//!   keeps its state (its last values stay until new ones arrive).
//! - A device no longer in the configuration has its task stopped before `apply` returns, so it
//!   is never polled again, and the sink is told to drop its state ([`Sink::removed`]). An
//!   endpoint no device reads through any more has its connection closed.
//!
//! A task is stopped between two requests, never during one: its reads go through a
//! [`Stoppable`] handle, and stopping waits for the request in flight, if any, to get its
//! answer, its failure or its timeout before the task is aborted. The endpoint's connection,
//! which other devices may go on reading through, is thus never left with a request whose
//! answer nobody waits for. The wait is bounded by the turn and the request timeout of that one
//! read (and by the watchdog), as every read at a shared endpoint is.
//!
//! Everything that can fail (planning every device's reads) happens in [`Plan::new`], before
//! anything changes; [`Fleet::apply`] itself cannot fail, so a configuration is applied whole or
//! not at all.

use std::collections::HashMap;
use std::fmt;
use std::ops::ControlFlow;
use std::sync::Arc;

use protocol::config::{DesiredConfig, Device, DeviceModel};
use std::time::Duration;

use futures_util::future::join_all;
use protocol::{DeviceId, Issue, IssueCode};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::live::{DeviceLive, Watcher};
use crate::modbus::endpoint::{Endpoints, SharedConnection};
use crate::modbus::pdu::Read;
use crate::modbus::plan::PlanError;
use crate::modbus::{Clock, Connection, DeviceReader, Exchange, Timestamp};
use crate::polling::{self, Disconnect, Event, Polled, Settings};

/// Takes what the polling loops produce. Called from the device's polling task, so it must
/// return quickly and never block: hand the poll over (to the offline buffer, the health
/// statistics) and return.
pub trait Sink: Send + Sync + 'static {
    /// A poll of `device` ended: its readings, failures and requests.
    fn polled(&self, device: &DeviceId, polled: Polled);

    /// `device` is no longer in the configuration and is no longer polled: drop everything kept
    /// for it. Called after its task stopped, so no [`Sink::polled`] for it follows (unless a
    /// later configuration adds it again).
    fn removed(&self, device: &DeviceId);
}

/// The clock that stamps readings, shared by every device's task.
pub type SharedClock = Arc<dyn Clock + Send + Sync>;

/// Every device of a configuration with its planned reads: the part of applying that can fail,
/// done before anything changes.
#[derive(Debug)]
pub struct Plan {
    devices: Vec<Planned>,
}

#[derive(Debug)]
struct Planned {
    device: Device,
    model: DeviceModel,
    reader: DeviceReader,
}

impl Plan {
    /// Plans the reads of every device of `config`, which passed [`DesiredConfig`]'s
    /// validation. The issues describe what cannot be planned; a validated configuration has
    /// none, so they are a last line of defence, with paths as in the validation's issues.
    ///
    /// # Errors
    ///
    /// The issues of every device that cannot be planned.
    pub fn new(config: &DesiredConfig) -> Result<Self, Vec<Issue>> {
        let mut devices = Vec::with_capacity(config.devices.len());
        let mut issues = Vec::new();
        for (index, device) in config.devices.iter().enumerate() {
            let Some((model_index, model)) = config
                .models
                .iter()
                .enumerate()
                .find(|(_, model)| model.name == device.model)
            else {
                issues.push(Issue::new(
                    IssueCode::UnknownModel,
                    format!("devices[{index}].model"),
                    format!("model {} is not in the configuration", device.model),
                ));
                continue;
            };
            match DeviceReader::new(device, model) {
                Ok(reader) => devices.push(Planned {
                    device: device.clone(),
                    model: model.clone(),
                    reader,
                }),
                Err(error) => {
                    let point = match error {
                        PlanError::Width { index } | PlanError::PastEnd { index } => index,
                    };
                    issues.push(Issue::new(
                        IssueCode::InvalidValue,
                        format!("models[{model_index}].points[{point}]"),
                        error.to_string(),
                    ));
                }
            }
        }
        if issues.is_empty() {
            Ok(Self { devices })
        } else {
            Err(issues)
        }
    }

    /// How many devices the configuration has.
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// Whether the configuration has no devices.
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

/// What [`Fleet::apply`] changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// Devices that were not polled before.
    pub added: usize,
    /// Devices that are no longer polled.
    pub removed: usize,
    /// Devices whose settings or model changed, now polled with the new ones.
    pub restarted: usize,
    /// Devices whose polling went on untouched.
    pub unchanged: usize,
}

impl fmt::Display for Changes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} added, {} removed, {} changed, {} unchanged",
            self.added, self.removed, self.restarted, self.unchanged
        )
    }
}

/// The polling task of every device of the applied configuration.
pub struct Fleet {
    endpoints: Endpoints,
    settings: Settings,
    clock: SharedClock,
    sink: Arc<dyn Sink>,
    live: Watcher,
    running: HashMap<DeviceId, Running>,
}

struct Running {
    device: Device,
    model: DeviceModel,
    stop: Stop,
    task: JoinHandle<()>,
}

impl Running {
    /// Stops the task between two requests (see the module documentation) and waits for it to
    /// end; `false` when it had panicked.
    async fn stop(self) -> bool {
        self.stop.stop().await;
        self.task.abort();
        match self.task.await {
            Err(error) => !error.is_panic(),
            Ok(()) => true,
        }
    }
}

impl fmt::Debug for Fleet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fleet")
            .field("devices", &self.running.len())
            .field("endpoints", &self.endpoints.len())
            .finish_non_exhaustive()
    }
}

impl Fleet {
    /// No devices yet. Connections come from `endpoints` (in production
    /// [`Endpoints::new`] with `settings.connect_timeout`, which applies the address guard),
    /// readings are stamped with `clock` and every poll goes to `sink`. `settings` should have
    /// passed [`Settings::validate`].
    pub fn new(
        endpoints: Endpoints,
        settings: Settings,
        clock: SharedClock,
        sink: Arc<dyn Sink>,
    ) -> Self {
        Self {
            endpoints,
            settings,
            clock,
            sink,
            live: Watcher::none(),
            running: HashMap::new(),
        }
    }

    /// Polls a device faster while a live lease of `live` asks for it (rebuild checklist I3,
    /// [`crate::live`]); without this no lease ever covers a device. Applies to the tasks
    /// started from now on, so set it before the first [`Fleet::apply`].
    #[must_use]
    pub fn with_live(mut self, live: Watcher) -> Self {
        self.live = live;
        self
    }

    /// Polls exactly the devices of `plan` from now on (see the module documentation). Must be
    /// called from within a tokio runtime, which runs the polling tasks. When it returns, no
    /// removed device is polled any more.
    pub async fn apply(&mut self, plan: Plan) -> Changes {
        let mut changes = Changes::default();
        let mut next: HashMap<DeviceId, Planned> = plan
            .devices
            .into_iter()
            .map(|planned| (planned.device.id.clone(), planned))
            .collect();
        let mut stopping = Vec::new();
        let current: Vec<DeviceId> = self.running.keys().cloned().collect();
        for id in current {
            let keep = match (self.running.get(&id), next.get(&id)) {
                // A task that ended (it never does by itself) is started again.
                (Some(running), Some(planned)) => {
                    running.device == planned.device
                        && running.model == planned.model
                        && !running.task.is_finished()
                }
                _ => false,
            };
            if keep {
                next.remove(&id);
                changes.unchanged += 1;
            } else if let Some(running) = self.running.remove(&id) {
                stopping.push((id, running));
            }
        }
        // All at once, so that the wait is that of the slowest request in flight, not their sum.
        let stopped = join_all(
            stopping
                .into_iter()
                .map(|(id, running)| async move { (running.stop().await, id) }),
        )
        .await;
        for (ended, id) in stopped {
            if !ended {
                tracing::error!(device = %id, "the device's polling task panicked");
            }
            if next.contains_key(&id) {
                changes.restarted += 1;
            } else {
                changes.removed += 1;
                self.sink.removed(&id);
            }
        }
        changes.added = next.len() - changes.restarted;
        for (id, planned) in next {
            let (connection, stop) = Stoppable::new(self.endpoints.connection(&planned.device));
            let task = tokio::spawn(poll_device(
                id.clone(),
                planned.reader,
                polling::poll_period(&planned.device),
                self.live.device(id.clone()),
                connection,
                SharedClockRef(Arc::clone(&self.clock)),
                self.settings,
                Arc::clone(&self.sink),
            ));
            self.running.insert(
                id,
                Running {
                    device: planned.device,
                    model: planned.model,
                    stop,
                    task,
                },
            );
        }
        // After the new tasks took their connections, so that an endpoint whose only device
        // was restarted keeps its connection.
        self.endpoints.prune();
        changes
    }

    /// Stops every device's task, for shutdown. The sink is not told: the devices are still
    /// configured.
    pub async fn stop(&mut self) {
        join_all(self.running.drain().map(|(_, running)| running.stop())).await;
        self.endpoints.prune();
    }

    /// The devices being polled.
    pub fn devices(&self) -> impl Iterator<Item = &DeviceId> {
        self.running.keys()
    }

    /// How many devices are being polled.
    pub fn len(&self) -> usize {
        self.running.len()
    }

    /// Whether no device is being polled.
    pub fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    /// How many endpoints have a connection.
    pub fn endpoints(&self) -> usize {
        self.endpoints.len()
    }
}

/// [`SharedClock`] as a sized [`Clock`] for [`polling::run`].
struct SharedClockRef(SharedClock);

impl Clock for SharedClockRef {
    fn now(&self) -> Timestamp {
        self.0.now()
    }
}

/// One device's handle to its endpoint's connection, through which its task is stopped between
/// two requests: a read or disconnect holds the device's [`Stop`] lock until it ends, and once
/// [`Stop::stop`] has run, no read or disconnect starts any more (it waits for the abort
/// instead).
struct Stoppable {
    connection: SharedConnection,
    stopped: Arc<Mutex<bool>>,
}

/// Stops the task that reads through a [`Stoppable`].
struct Stop(Arc<Mutex<bool>>);

impl Stoppable {
    fn new(connection: SharedConnection) -> (Self, Stop) {
        let stopped = Arc::new(Mutex::new(false));
        (
            Self {
                connection,
                stopped: Arc::clone(&stopped),
            },
            Stop(stopped),
        )
    }
}

impl Stop {
    /// Waits for the read or disconnect in flight, if any, to end, and keeps the task from
    /// starting another. The task is then safe to abort: whatever it waits for, it is not a
    /// request on the shared connection.
    async fn stop(&self) {
        *self.0.lock().await = true;
    }
}

impl Connection for Stoppable {
    async fn read(&mut self, unit: u8, read: Read, timeout: Duration) -> Exchange {
        let stopped = self.stopped.lock().await;
        if *stopped {
            drop(stopped);
            return std::future::pending().await;
        }
        let exchange = self.connection.read(unit, read, timeout).await;
        drop(stopped);
        exchange
    }
}

impl Disconnect for Stoppable {
    async fn disconnect(&mut self) {
        let stopped = self.stopped.lock().await;
        if *stopped {
            drop(stopped);
            return std::future::pending().await;
        }
        self.connection.disconnect().await;
        drop(stopped);
    }
}

/// Polls one device until its task is aborted.
#[allow(
    clippy::too_many_arguments,
    reason = "everything one device's task owns; a struct would only rename it"
)]
async fn poll_device(
    id: DeviceId,
    reader: DeviceReader,
    period: Duration,
    mut live: DeviceLive,
    mut connection: Stoppable,
    clock: SharedClockRef,
    settings: Settings,
    sink: Arc<dyn Sink>,
) {
    polling::run_live(
        &reader,
        period,
        &mut live,
        &mut connection,
        &clock,
        &settings,
        |event| {
            if let Event::Polled(polled) = event {
                sink.polled(&id, polled);
            }
            ControlFlow::Continue(())
        },
    )
    .await;
}
