//! Live values for open device charts (rebuild checklist I3, Laysi/trustiot-v2#121): the leases
//! the cloud opens on `down/live-control`, and every sample of a leased device published on
//! `up/live` at once (protocol README, 「即時值」).
//!
//! - [`Leases`]: the dispatcher hands it every `down/live-control` message
//!   ([`Leases::deliver`]), which starts, renews or stops one lease at once, without waiting.
//!   A lease ends by itself `ttl_s` seconds after its last start or renewal: every reader
//!   compares the end with the time it reads, so an ended lease never counts, whether or not
//!   anything has removed it yet. At most [`MAX_LIVE_LEASES`] leases that have not ended are
//!   kept; a new lease beyond that is ignored and logged.
//! - [`Watcher`]: what the leases cover now ([`Coverage`]). Each device's polling loop holds a
//!   [`DeviceLive`] ([`crate::polling::Live`]): while a lease asks for samples faster than the
//!   device's poll interval, the loop polls at the lease's interval as well
//!   ([`crate::polling`], 「Live values」).
//! - [`LiveSink`]: the fleet's sink in the service. A poll of a device that a lease covers when
//!   the poll ends goes to the [`Publisher`] at once, and every poll goes on to the inner sink
//!   (the uploader's [`crate::upload::Readings`], which takes only the regular polls into the
//!   telemetry window). The hand-over never waits: when the publisher's queue
//!   ([`LIVE_QUEUE`] polls) is full, the poll's live values are dropped.
//! - [`Publisher`]: publishes each poll's readings as [`LiveSamples`] on `up/live` (QoS 0),
//!   one poll after the other. Never into the offline buffer, never waiting for an
//!   acknowledgement (QoS 0 has none), never to disk: a message the link does not take at once
//!   (no connection, its queue full) is dropped. One message at a time, so live values hold at
//!   most one place in the link's QoS 0 queue and the heartbeat always finds room.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use protocol::live::{LiveAction, LiveControl, LiveSamples, MAX_LIVE_LEASES};
use protocol::telemetry::{Reading, Series};
use protocol::topic::Channel;
use protocol::{DeviceId, LeaseId};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::desired::fleet::Sink;
use crate::link::{Link, PublishError};
use crate::polling::{self, Polled};

/// Polls waiting for the [`Publisher`]; when it is full, a covered poll's live values are
/// dropped. Live values are only worth sending at once, so a short queue is enough.
pub const LIVE_QUEUE: usize = 16;

/// How often each kind of trouble is logged at most.
const LOG_EVERY: Duration = Duration::from_secs(60);

/// One lease, as the gateway keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Lease {
    devices: Vec<DeviceId>,
    interval: Duration,
    ends: Instant,
}

/// What [`Leases::apply`] did with a control message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// `start` or `renew`: the lease runs until `ends`.
    Leased {
        /// When the lease ends unless renewed.
        ends: Instant,
    },
    /// `stop`: the lease ended (or did not exist).
    Stopped,
    /// An operation this build does not know: ignored.
    Unknown,
    /// A new lease while [`MAX_LIVE_LEASES`] leases run: ignored.
    TooMany,
}

/// The leases the cloud opened; the dispatcher's handler for `down/live-control`.
#[derive(Debug)]
pub struct Leases {
    leases: HashMap<LeaseId, Lease>,
    coverage: watch::Sender<Arc<Coverage>>,
    undecodable: Throttled,
    too_many: Throttled,
}

/// Which devices the leases cover, how fast and until when: a snapshot, rebuilt on every change.
#[derive(Debug, Default)]
pub struct Coverage {
    devices: HashMap<DeviceId, Vec<Cover>>,
}

#[derive(Clone, Copy, Debug)]
struct Cover {
    interval: Duration,
    ends: Instant,
}

impl Coverage {
    /// The shortest interval among the leases that cover `device` at `now`; `None` when none
    /// does (an ended lease never counts).
    pub fn interval(&self, device: &DeviceId, now: Instant) -> Option<Duration> {
        self.devices
            .get(device)?
            .iter()
            .filter(|cover| cover.ends > now)
            .map(|cover| cover.interval)
            .min()
    }

    /// Whether a lease covers `device` at `now`.
    pub fn covers(&self, device: &DeviceId, now: Instant) -> bool {
        self.interval(device, now).is_some()
    }
}

/// No leases, and the [`Watcher`] that sees them.
pub fn leases() -> (Leases, Watcher) {
    let (coverage, receiver) = watch::channel(Arc::new(Coverage::default()));
    (
        Leases {
            leases: HashMap::new(),
            coverage,
            undecodable: Throttled::default(),
            too_many: Throttled::default(),
        },
        Watcher(receiver),
    )
}

impl Leases {
    /// Decodes a `down/live-control` payload and applies it at once; a payload that does not
    /// decode (or fails the protocol's checks) is logged, at most once a minute, and ignored.
    pub fn deliver(&mut self, payload: &[u8]) {
        match protocol::decode::<LiveControl>(payload) {
            Ok(control) => {
                self.apply(&control, Instant::now());
            }
            Err(error) => {
                if let Some(count) = self.undecodable.tick() {
                    tracing::warn!(%error, undecodable = count, "ignored a live-control message that does not decode");
                }
            }
        }
    }

    /// Applies `control` as of `now`: `start` and `renew` open or replace the lease, which ends
    /// `ttl_s` from `now`; `stop` ends it. Leases that ended by `now` are dropped first, so they
    /// never count towards [`MAX_LIVE_LEASES`].
    pub fn apply(&mut self, control: &LiveControl, now: Instant) -> Applied {
        self.leases.retain(|_, lease| lease.ends > now);
        let applied = match &control.action {
            LiveAction::Start(spec) | LiveAction::Renew(spec) => {
                if !self.leases.contains_key(&control.lease) && self.leases.len() >= MAX_LIVE_LEASES
                {
                    if let Some(count) = self.too_many.tick() {
                        tracing::warn!(
                            lease = %control.lease,
                            ignored = count,
                            "ignored a new live lease: {MAX_LIVE_LEASES} leases are running"
                        );
                    }
                    return Applied::TooMany;
                }
                let ends = after(now, Duration::from_secs(u64::from(spec.ttl_s)));
                self.leases.insert(
                    control.lease.clone(),
                    Lease {
                        devices: spec.devices.clone(),
                        interval: Duration::from_millis(u64::from(spec.interval_ms)),
                        ends,
                    },
                );
                tracing::debug!(lease = %control.lease, devices = spec.devices.len(), interval_ms = spec.interval_ms, ttl_s = spec.ttl_s, "live lease");
                Applied::Leased { ends }
            }
            LiveAction::Stop => {
                self.leases.remove(&control.lease);
                tracing::debug!(lease = %control.lease, "live lease stopped");
                Applied::Stopped
            }
            LiveAction::Unknown => {
                tracing::debug!(lease = %control.lease, "ignored a live-control operation this agent does not know");
                return Applied::Unknown;
            }
        };
        self.publish();
        applied
    }

    /// Hands the new coverage to every watcher and wakes the polling loops.
    fn publish(&self) {
        let mut devices: HashMap<DeviceId, Vec<Cover>> = HashMap::new();
        for lease in self.leases.values() {
            for device in &lease.devices {
                devices.entry(device.clone()).or_default().push(Cover {
                    interval: lease.interval,
                    ends: lease.ends,
                });
            }
        }
        self.coverage.send_replace(Arc::new(Coverage { devices }));
    }

    /// The leases kept (including any that ended since the last change).
    pub fn len(&self) -> usize {
        self.leases.len()
    }

    /// Whether no lease is kept.
    pub fn is_empty(&self) -> bool {
        self.leases.is_empty()
    }
}

/// What the leases cover; cheap to clone.
#[derive(Clone, Debug)]
pub struct Watcher(watch::Receiver<Arc<Coverage>>);

impl Watcher {
    /// A watcher of leases that never exist, for a fleet without live values.
    pub fn none() -> Self {
        Self(watch::channel(Arc::new(Coverage::default())).1)
    }

    /// The coverage now.
    pub fn coverage(&self) -> Arc<Coverage> {
        Arc::clone(&self.0.borrow())
    }

    /// The view of one device's polling loop.
    pub fn device(&self, device: DeviceId) -> DeviceLive {
        let mut receiver = self.0.clone();
        receiver.mark_unchanged();
        DeviceLive { device, receiver }
    }
}

/// One device's view of the leases, for its polling loop.
#[derive(Debug)]
pub struct DeviceLive {
    device: DeviceId,
    receiver: watch::Receiver<Arc<Coverage>>,
}

impl polling::Live for DeviceLive {
    fn interval(&self, now: Instant) -> Option<Duration> {
        self.receiver.borrow().interval(&self.device, now)
    }

    async fn changed(&mut self) {
        // `Err` once the leases are gone (the dispatcher ended): they cannot change any more.
        if self.receiver.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// The fleet's sink with live values: covered polls to the [`Publisher`], every poll to the
/// inner sink.
pub struct LiveSink {
    inner: Arc<dyn Sink>,
    watcher: Watcher,
    queue: mpsc::Sender<Vec<Reading>>,
    full: Mutex<Throttled>,
}

impl std::fmt::Debug for LiveSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveSink").finish_non_exhaustive()
    }
}

/// Publishes the live values the [`LiveSink`] hands over, on `up/live`.
#[derive(Debug)]
pub struct Publisher {
    link: Link,
    queue: mpsc::Receiver<Vec<Reading>>,
}

/// The sink that sends the polls of covered devices to the publisher and every poll to
/// `inner`, and the publisher that sends them through `link`. Run [`Publisher::run`] in a task;
/// it ends once the sink is dropped.
pub fn publisher(link: Link, watcher: Watcher, inner: Arc<dyn Sink>) -> (LiveSink, Publisher) {
    let (sender, receiver) = mpsc::channel(LIVE_QUEUE);
    (
        LiveSink {
            inner,
            watcher,
            queue: sender,
            full: Mutex::new(Throttled::default()),
        },
        Publisher {
            link,
            queue: receiver,
        },
    )
}

impl Sink for LiveSink {
    fn started(&self, device: &DeviceId) {
        self.inner.started(device);
    }

    fn polled(&self, device: &DeviceId, polled: Polled) {
        // Covered when the poll ends: nothing is sent for a poll that ends after the lease.
        if !polled.poll.readings.is_empty()
            && self.watcher.0.borrow().covers(device, Instant::now())
        {
            match self.queue.try_send(polled.poll.readings.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    let mut full = self.full.lock().unwrap_or_else(PoisonError::into_inner);
                    if let Some(count) = full.tick() {
                        tracing::warn!(%device, dropped = count, "dropped live values: the live publisher is behind");
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    tracing::debug!(%device, "dropped live values: the live publisher has ended");
                }
            }
        }
        self.inner.polled(device, polled);
    }

    fn removed(&self, device: &DeviceId) {
        self.inner.removed(device);
    }
}

impl Publisher {
    /// Publishes every poll handed over, until the [`LiveSink`] is dropped.
    pub async fn run(mut self) {
        let mut failed = Throttled::default();
        let mut unsendable = Throttled::default();
        while let Some(readings) = self.queue.recv().await {
            for payload in encode(readings, &mut unsendable) {
                match self.link.publish(Channel::Live, payload).await {
                    Ok(_) => {}
                    // No connection, or the link is stopping: live values are not kept for
                    // later.
                    Err(PublishError::NotConnected | PublishError::Stopped) => {
                        tracing::debug!("dropped live values: no connection");
                        break;
                    }
                    Err(error) => {
                        if let Some(count) = failed.tick() {
                            tracing::warn!(%error, dropped = count, "dropped live values the link did not send");
                        }
                    }
                }
            }
        }
    }
}

/// The `up/live` messages of one poll's readings: one series per data point, split within the
/// channel's limit. Readings whose value cannot be sent (a NaN, for one) are left out, as the
/// telemetry batches leave them out.
fn encode(readings: Vec<Reading>, unsendable: &mut Throttled) -> Vec<Vec<u8>> {
    let mut left_out = 0_u64;
    let series: Vec<Series> = readings
        .into_iter()
        .filter(|reading| {
            let valid = reading.sample.value.validate().is_ok();
            if !valid {
                left_out += 1;
            }
            valid
        })
        .map(|reading| Series {
            device: reading.device,
            point: reading.point,
            samples: vec![reading.sample],
        })
        .collect();
    if left_out > 0
        && let Some(count) = unsendable.tick()
    {
        tracing::warn!(
            readings = left_out,
            times = count,
            "left out live readings whose value cannot be sent"
        );
    }
    let messages = match LiveSamples::split(series) {
        Ok(messages) => messages,
        Err(error) => {
            if let Some(count) = unsendable.tick() {
                tracing::warn!(%error, times = count, "dropped live values that do not fit a message");
            }
            return Vec::new();
        }
    };
    messages
        .iter()
        .filter_map(|message| match message.encode() {
            Ok(payload) => Some(payload),
            Err(error) => {
                if let Some(count) = unsendable.tick() {
                    tracing::warn!(%error, times = count, "dropped live values that do not encode");
                }
                None
            }
        })
        .collect()
}

/// Counts occurrences and says when one should be logged: the first, then at most once per
/// [`LOG_EVERY`], with the count since the last one logged.
#[derive(Debug, Default)]
struct Throttled {
    logged: Option<Instant>,
    count: u64,
}

impl Throttled {
    fn tick(&mut self) -> Option<u64> {
        self.count = self.count.saturating_add(1);
        if self
            .logged
            .is_none_or(|logged| logged.elapsed() >= LOG_EVERY)
        {
            self.logged = Some(Instant::now());
            Some(std::mem::take(&mut self.count))
        } else {
            None
        }
    }
}

/// The instant `duration` after `at`; a year after it when that does not fit the clock.
fn after(at: Instant, duration: Duration) -> Instant {
    at.checked_add(duration)
        .unwrap_or_else(|| at + Duration::from_secs(365 * 24 * 60 * 60))
}

#[cfg(test)]
mod tests {
    use protocol::V1;
    use protocol::live::LiveSpec;

    use super::*;
    use crate::polling::Live;

    fn id(name: &str) -> DeviceId {
        DeviceId::new(name).unwrap()
    }

    fn control(lease: &str, action: LiveAction) -> LiveControl {
        LiveControl {
            v: V1,
            lease: LeaseId::new(lease).unwrap(),
            action,
        }
    }

    fn start(lease: &str, devices: &[&str], interval_ms: u32, ttl_s: u32) -> LiveControl {
        control(
            lease,
            LiveAction::Start(LiveSpec {
                devices: devices.iter().map(|name| id(name)).collect(),
                interval_ms,
                ttl_s,
            }),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn a_lease_covers_its_devices_until_it_ends() {
        let (mut leases, watcher) = leases();
        let now = Instant::now();
        let ends = now + Duration::from_secs(30);
        assert_eq!(
            leases.apply(&start("chart-1", &["a", "b"], 500, 30), now),
            Applied::Leased { ends }
        );
        let coverage = watcher.coverage();
        assert_eq!(
            coverage.interval(&id("a"), now),
            Some(Duration::from_millis(500))
        );
        assert!(coverage.covers(&id("b"), ends - Duration::from_millis(1)));
        assert!(!coverage.covers(&id("c"), now), "not in the lease");
        // It ends at `ends`, whether or not anything removed it.
        assert!(!coverage.covers(&id("a"), ends));
        assert!(!coverage.covers(&id("b"), ends + Duration::from_secs(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn renew_extends_stop_ends_and_the_shortest_interval_counts() {
        let (mut leases, watcher) = leases();
        let now = Instant::now();
        leases.apply(&start("slow", &["a"], 1_000, 10), now);
        leases.apply(&start("fast", &["a"], 200, 20), now);
        assert_eq!(
            watcher.coverage().interval(&id("a"), now),
            Some(Duration::from_millis(200)),
            "the shortest of the leases covering the device"
        );
        // Renewing replaces the lease: a new end, a new device list.
        let later = now + Duration::from_secs(5);
        leases.apply(
            &control(
                "slow",
                LiveAction::Renew(LiveSpec {
                    devices: vec![id("a"), id("b")],
                    interval_ms: 1_000,
                    ttl_s: 60,
                }),
            ),
            later,
        );
        let coverage = watcher.coverage();
        assert!(coverage.covers(&id("b"), later + Duration::from_secs(59)));
        // Once the fast lease ends, the slow one's interval counts.
        assert_eq!(
            coverage.interval(&id("a"), now + Duration::from_secs(20)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            leases.apply(&control("fast", LiveAction::Stop), later),
            Applied::Stopped
        );
        assert_eq!(
            watcher.coverage().interval(&id("a"), later),
            Some(Duration::from_secs(1))
        );
        // Stopping a lease that does not exist does nothing.
        assert_eq!(
            leases.apply(&control("nope", LiveAction::Stop), later),
            Applied::Stopped
        );
        assert_eq!(leases.len(), 1);
        assert_eq!(
            leases.apply(&control("slow", LiveAction::Stop), later),
            Applied::Stopped
        );
        assert!(!watcher.coverage().covers(&id("a"), later));
        assert!(leases.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn an_unknown_operation_changes_nothing() {
        let (mut leases, watcher) = leases();
        let now = Instant::now();
        leases.apply(&start("chart", &["a"], 500, 30), now);
        assert_eq!(
            leases.apply(&control("chart", LiveAction::Unknown), now),
            Applied::Unknown
        );
        assert!(watcher.coverage().covers(&id("a"), now));
    }

    #[tokio::test(start_paused = true)]
    async fn at_most_max_live_leases_are_kept() {
        let (mut leases, watcher) = leases();
        let now = Instant::now();
        for index in 0..MAX_LIVE_LEASES {
            let applied = leases.apply(&start(&format!("lease-{index}"), &["a"], 500, 10), now);
            assert!(matches!(applied, Applied::Leased { .. }), "{index}");
        }
        assert_eq!(
            leases.apply(&start("one-more", &["b"], 500, 10), now),
            Applied::TooMany
        );
        assert!(!watcher.coverage().covers(&id("b"), now));
        // A lease that is kept can still be renewed.
        assert!(matches!(
            leases.apply(&start("lease-0", &["b"], 500, 30), now),
            Applied::Leased { .. }
        ));
        assert!(watcher.coverage().covers(&id("b"), now));
        // Once the others have ended, they no longer count.
        let later = now + Duration::from_secs(10);
        assert!(matches!(
            leases.apply(&start("one-more", &["c"], 500, 10), later),
            Applied::Leased { .. }
        ));
        assert_eq!(leases.len(), 2, "lease-0 (renewed) and one-more");
    }

    #[tokio::test(start_paused = true)]
    async fn a_payload_that_does_not_decode_is_ignored() {
        let (mut leases, watcher) = leases();
        leases.deliver(b"\xa0");
        leases.deliver(&[]);
        assert!(leases.is_empty());
        // One that decodes is applied as of its delivery.
        leases.deliver(&start("chart", &["a"], 500, 30).encode().unwrap());
        assert!(watcher.coverage().covers(&id("a"), Instant::now()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_device_loop_wakes_on_a_change_and_never_once_the_leases_are_gone() {
        let (mut leases, watcher) = leases();
        let mut device = watcher.device(id("a"));
        assert_eq!(device.interval(Instant::now()), None);
        let now = Instant::now();
        leases.apply(&start("chart", &["a"], 250, 30), now);
        tokio::time::timeout(Duration::from_secs(1), device.changed())
            .await
            .expect("a change wakes the loop");
        assert_eq!(device.interval(now), Some(Duration::from_millis(250)));
        drop(leases);
        assert!(
            tokio::time::timeout(Duration::from_secs(3600), device.changed())
                .await
                .is_err(),
            "no more changes once the leases are gone"
        );
        // A fleet without live values never sees a lease.
        let mut none = Watcher::none().device(id("a"));
        assert_eq!(none.interval(now), None);
        assert!(
            tokio::time::timeout(Duration::from_secs(3600), none.changed())
                .await
                .is_err()
        );
    }

    #[test]
    fn readings_become_one_series_per_point_and_unsendable_values_are_left_out() {
        let reading = |point: &str, value: protocol::Value| Reading {
            device: id("a"),
            point: protocol::PointKey::new(point).unwrap(),
            sample: protocol::telemetry::Sample {
                at: protocol::UnixMillis::new(1_760_054_400_000),
                value,
            },
            clock_synced: true,
        };
        let mut throttled = Throttled::default();
        let payloads = encode(
            vec![
                reading("power", protocol::Value::Int(7)),
                reading("bad", protocol::Value::Float(f64::NAN)),
                reading("energy", protocol::Value::Float(1.5)),
            ],
            &mut throttled,
        );
        assert_eq!(payloads.len(), 1);
        let samples = LiveSamples::decode(&payloads[0]).unwrap();
        let points: Vec<(&str, usize)> = samples
            .series
            .iter()
            .map(|series| (series.point.as_str(), series.samples.len()))
            .collect();
        assert_eq!(points, [("power", 1), ("energy", 1)]);
        assert!(encode(Vec::new(), &mut throttled).is_empty());
    }
}
