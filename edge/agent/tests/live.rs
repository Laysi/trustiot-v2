//! Live values on the gateway (Laysi/trustiot-v2#121, rebuild checklist I3) against testkit's
//! fake Modbus endpoint, wired as `main.rs` wires the service: the dispatcher hands
//! `down/live-control` to the leases, the fleet polls leased devices at the lease's interval,
//! its sink publishes their samples on `up/live` and passes every poll on to the pipeline's
//! `Readings`. Only the connection (in memory), the address guard's exemption for the fake
//! endpoint and a sink that also tells the test about each poll differ.
//!
//! - a leased device's sample goes up on `up/live` right after the poll, at QoS 0 (no
//!   acknowledgement to wait for), without the clock moving, and never into the offline buffer:
//!   the extra polls' readings are in no telemetry batch, the regular ones are;
//! - once the lease ends (its `ttl_s` runs out, or `stop`), nothing more goes to `up/live` and
//!   the device is polled at its regular interval again; a renewal moves the end.
//!
//! The clock moves only when a test advances it (`testkit::clock::manual`), and only once the
//! awaited polls have been reported, so no timeout fires because the machine is slow.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod support;

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use agent::clock::{GatewayClock, SystemWall};
use agent::desired::fleet::{Fleet, Sink};
use agent::desired::store::Store;
use agent::desired::{self, Applier};
use agent::dispatch::{self, Handlers};
use agent::link::{self, LinkSettings};
use agent::live;
use agent::modbus::endpoint::Endpoints;
use agent::polling::{Polled, Settings};
use agent::upload;
use protocol::config::{
    ByteOrder, DataPoint, DataType, DesiredConfig, Device, DeviceModel, FunctionCode, Host,
    Transport,
};
use protocol::live::{LiveAction, LiveControl, LiveSamples, LiveSpec};
use protocol::telemetry::TelemetryBatch;
use protocol::topic::Channel;
use protocol::{AgentVersion, ConfigVersion, DeviceId, LeaseId, ModelName, PointKey, V1, Value};
use support::{FakeBroker, Packet, gateway, puback, publish};
use testkit::clock::{self, bounded};
use testkit::modbus::{self as fake, FakeModbus};
use testkit::mqtt::ScratchDir;
use tokio::io::DuplexStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::advance;
use tokio_util::sync::CancellationToken;

/// Every device's poll interval: regular polls at 0, 6 and 12 s, so the window that closes at
/// 10 s holds two of them and no poll falls on its close.
const PERIOD: Duration = Duration::from_secs(6);
/// The leases' sampling interval: the plan's shortest one, shorter than the poll interval.
const LIVE_INTERVAL_MS: u32 = 1_000;
/// The upload interval: longer than a lease, so a test sees one window's batch.
const UPLOAD_INTERVAL: Duration = Duration::from_secs(10);
/// What unit `u` holds in holding register 0 at first: `BASE + u`.
const BASE: u16 = 2300;

/// A poll, as the test sees it: the device, whether it was a regular poll, and its value.
#[derive(Debug, PartialEq)]
struct Seen {
    device: String,
    regular: bool,
    value: Value,
}

/// The fleet's sink in the service (`live::LiveSink`), watched: every poll also tells the test
/// which device it was, whether it was regular and what it read.
struct Watched {
    inner: live::LiveSink,
    polls: mpsc::UnboundedSender<Seen>,
}

impl Sink for Watched {
    fn polled(&self, device: &DeviceId, polled: Polled) {
        assert_eq!(polled.poll.readings.len(), 1, "{device}: {:?}", polled.poll);
        let _ = self.polls.send(Seen {
            device: device.as_str().to_owned(),
            regular: polled.regular,
            value: polled.poll.readings[0].sample.value.clone(),
        });
        self.inner.polled(device, polled);
    }

    fn removed(&self, device: &DeviceId) {
        self.inner.removed(device);
    }
}

fn model() -> DeviceModel {
    DeviceModel {
        name: ModelName::new("meter").unwrap(),
        points: vec![DataPoint {
            key: PointKey::new("power").unwrap(),
            function_code: FunctionCode::ReadHoldingRegisters,
            address: 0,
            data_type: DataType::U16,
            byte_order: ByteOrder::Abcd,
            length: None,
            scale: None,
            offset: None,
        }],
    }
}

fn device(id: &str, endpoint: &FakeModbus, unit: u8) -> Device {
    Device {
        id: DeviceId::new(id).unwrap(),
        model: model().name,
        transport: Transport::ModbusTcp,
        host: Host::new("127.0.0.1").unwrap(),
        port: endpoint.address().port(),
        unit_id: unit,
        poll_interval_ms: u32::try_from(PERIOD.as_millis()).unwrap(),
    }
}

fn int(value: u16) -> Value {
    Value::Int(i64::from(value))
}

fn control(lease: &str, action: LiveAction) -> Vec<u8> {
    LiveControl {
        v: V1,
        lease: LeaseId::new(lease).unwrap(),
        action,
    }
    .encode()
    .unwrap()
}

fn spec(devices: &[&str], ttl_s: u32) -> LiveSpec {
    LiveSpec {
        devices: devices
            .iter()
            .map(|device| DeviceId::new(*device).unwrap())
            .collect(),
        interval_ms: LIVE_INTERVAL_MS,
        ttl_s,
    }
}

/// The service's wiring with a scripted cloud, a fake Modbus endpoint with units 1 and 2, and
/// the configuration (meter-a on unit 1, meter-b on unit 2) applied and polled once.
struct Service {
    bus: FakeModbus,
    broker: FakeBroker<DuplexStream>,
    polls: mpsc::UnboundedReceiver<Seen>,
    stats: upload::Stats,
    /// The leases as the polling sees them.
    watcher: live::Watcher,
    /// The next packet ID the cloud uses for a QoS 1 delivery.
    next_id: u16,
    tasks: Vec<JoinHandle<()>>,
    shutdown: CancellationToken,
    pipeline_stop: CancellationToken,
    _dir: ScratchDir,
}

impl Service {
    async fn start(name: &str) -> Self {
        let bus = FakeModbus::start(fake::Transport::ModbusTcp).await;
        for unit in [1, 2] {
            bus.add_unit(unit);
            bus.set_holding_registers(unit, 0, &[BASE + u16::from(unit)]);
        }
        let dir = ScratchDir::new(name);
        let (connector, mut brokers) = support::DuplexConnector::new();
        let shutdown = CancellationToken::new();
        let link_settings = LinkSettings {
            subscriptions: dispatch::subscriptions(),
            ..LinkSettings::new(gateway(), AgentVersion::new("0.1.0-test").unwrap())
        };
        let (link, incoming, supervisor) = link::link(link_settings, connector, shutdown.clone());
        let gateway_clock = GatewayClock::new(SystemWall, dir.path().join("no-timesync"));
        let mut upload_settings = upload::Settings::new(dir.path().join(upload::BUFFER_DIRECTORY));
        upload_settings.upload_interval = UPLOAD_INTERVAL;
        let pipeline_stop = CancellationToken::new();
        let (handles, pipeline) = upload::pipeline(
            upload_settings,
            link.clone(),
            gateway_clock.clone(),
            pipeline_stop.clone(),
        )
        .unwrap();
        let (inbox, deliveries) = desired::inbox();
        let (leases, watcher) = live::leases();
        let (sink, publisher) = live::publisher(
            link.clone(),
            watcher.clone(),
            Arc::new(handles.readings.clone()),
        );
        let (polls_sender, polls) = mpsc::unbounded_channel();
        let settings = Settings::default();
        let fleet = Fleet::new(
            Endpoints::new(settings.connect_timeout).exempt_for_tests([Ipv4Addr::LOCALHOST.into()]),
            settings,
            Arc::new(gateway_clock),
            Arc::new(Watched {
                inner: sink,
                polls: polls_sender,
            }),
        )
        .with_live(watcher.clone());
        let tasks = vec![
            tokio::spawn(dispatch::run(
                incoming,
                Handlers {
                    config: inbox,
                    telemetry_ack: handles.acks.clone(),
                    live: leases,
                },
            )),
            tokio::spawn(publisher.run()),
            tokio::spawn(desired::run(
                Applier::new(fleet, Store::new(dir.path())),
                link,
                deliveries,
                shutdown.clone(),
            )),
            tokio::spawn(pipeline.run()),
            tokio::spawn(supervisor.run()),
        ];
        let mut broker = FakeBroker::new(bounded("the connection", brokers.recv()).await.unwrap())
            .subscribing(dispatch::subscriptions().channels());
        broker.accept().await;
        let mut service = Self {
            bus,
            broker,
            polls,
            stats: handles.stats,
            watcher: watcher.clone(),
            next_id: 1,
            tasks,
            shutdown,
            pipeline_stop,
            _dir: dir,
        };
        let config = DesiredConfig {
            v: V1,
            rev: 1,
            version: ConfigVersion::new(1).unwrap(),
            models: vec![model()],
            devices: vec![
                device("meter-a", &service.bus, 1),
                device("meter-b", &service.bus, 2),
            ],
        }
        .encode()
        .unwrap();
        let id = service.deliver(Channel::Config, &config, true).await;
        // The PUBACK of the delivery and the configuration report, in either order.
        let (mut acked, mut reported) = (false, false);
        while !(acked && reported) {
            match bounded(
                "the PUBACK and the report",
                service.broker.read_answering_pings(),
            )
            .await
            {
                Some(Packet::PubAck { id: got, reason: 0 }) if got == id => acked = true,
                Some(Packet::Publish(report))
                    if report.topic == Channel::ConfigReport.topic(&gateway()) =>
                {
                    service.broker.send(&puback(report.id.unwrap(), 0)).await;
                    reported = true;
                }
                other => panic!("expected the PUBACK and the report, got {other:?}"),
            }
        }
        // The first round polls at once, on the regular schedule.
        let mut first = vec![service.poll().await, service.poll().await];
        first.sort_by(|a, b| a.device.cmp(&b.device));
        assert_eq!(
            first,
            [
                Seen {
                    device: "meter-a".to_owned(),
                    regular: true,
                    value: int(BASE + 1),
                },
                Seen {
                    device: "meter-b".to_owned(),
                    regular: true,
                    value: int(BASE + 2),
                },
            ]
        );
        service
    }

    /// The cloud publishes `payload` on `channel` at QoS 1; returns the packet ID.
    async fn deliver(&mut self, channel: Channel, payload: &[u8], retain: bool) -> u16 {
        let id = self.next_id;
        self.next_id += 1;
        self.broker
            .send(&publish(
                &channel.topic(&gateway()),
                payload,
                1,
                retain,
                Some(id),
            ))
            .await;
        id
    }

    /// The cloud sends a `live-control` message; the gateway acknowledges it at once.
    async fn live_control(&mut self, payload: &[u8]) {
        let id = self.deliver(Channel::LiveControl, payload, false).await;
        match bounded(
            "the PUBACK of the live-control message",
            self.broker.read_answering_pings(),
        )
        .await
        {
            Some(Packet::PubAck { id: got, reason: 0 }) if got == id => {}
            other => panic!("expected the PUBACK of the live-control message, got {other:?}"),
        }
    }

    /// Waits until the dispatcher has applied the last live-control message, as `applied` sees
    /// the coverage. The link acknowledges a message once it is queued for the dispatcher, so
    /// the PUBACK alone does not say it was applied.
    async fn until_leases(&self, what: &str, applied: impl Fn(&live::Coverage) -> bool) {
        bounded(what, async {
            while !applied(&self.watcher.coverage()) {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    async fn poll(&mut self) -> Seen {
        bounded("a poll", self.polls.recv()).await.unwrap()
    }

    /// The next packet the gateway sends: an `up/live` message (QoS 0, no retain), decoded.
    async fn live(&mut self) -> LiveSamples {
        match bounded("an up/live message", self.broker.read_answering_pings()).await {
            Some(Packet::Publish(message)) if message.topic == Channel::Live.topic(&gateway()) => {
                assert_eq!(
                    (message.qos, message.retain, message.id),
                    (0, false, None),
                    "QoS 0: nothing to acknowledge, nothing to wait for"
                );
                LiveSamples::decode(&message.payload).unwrap()
            }
            other => panic!("expected an up/live message, got {other:?}"),
        }
    }

    /// The next packet the gateway sends: a telemetry batch, acknowledged with a PUBACK.
    async fn batch(&mut self) -> TelemetryBatch {
        match bounded("a telemetry batch", self.broker.read_answering_pings()).await {
            Some(Packet::Publish(message))
                if message.topic == Channel::Telemetry.topic(&gateway()) =>
            {
                self.broker.send(&puback(message.id.unwrap(), 0)).await;
                TelemetryBatch::decode(&message.payload).unwrap()
            }
            other => panic!("expected a telemetry batch, got {other:?}"),
        }
    }

    fn pending_records(&self) -> u64 {
        self.stats.snapshot().pending_records
    }

    async fn stop(mut self) {
        self.shutdown.cancel();
        self.pipeline_stop.cancel();
        // The link's stop waits for the PUBACK of `offline` on a clock that does not move.
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

/// (device, point, values) of every series of `samples`.
fn values(series: &[protocol::telemetry::Series]) -> Vec<(String, Vec<Value>)> {
    let mut values: Vec<_> = series
        .iter()
        .map(|series| {
            (
                series.device.as_str().to_owned(),
                series
                    .samples
                    .iter()
                    .map(|sample| sample.value.clone())
                    .collect(),
            )
        })
        .collect();
    values.sort_by(|a, b| a.0.cmp(&b.0));
    values
}

/// Acceptance criteria 1 and 2: each sample of a leased device goes up on `up/live` right after
/// the poll and never into the buffer; once the lease's `ttl_s` has run out nothing more goes up
/// and the device is back on its regular interval. The telemetry batch holds the regular polls
/// only.
#[tokio::test(start_paused = true)]
async fn a_leased_devices_samples_go_up_at_once_outside_the_buffer_until_the_lease_ends() {
    let _manual = clock::manual();
    let mut service = Service::start("live-ttl").await;
    assert_eq!(service.pending_records(), 2, "the first round's readings");

    // 1 s in (a live interval after the first round, so the lease's first poll need not wait for
    // it), the cloud opens a 3 s lease on meter-a. The gateway polls it at once, without the
    // clock moving, and the sample goes straight to up/live.
    advance(Duration::from_millis(u64::from(LIVE_INTERVAL_MS))).await;
    service.bus.set_holding_registers(1, 0, &[BASE + 11]);
    service
        .live_control(&control(
            "chart-1",
            LiveAction::Start(spec(&["meter-a"], 3)),
        ))
        .await;
    assert_eq!(
        service.poll().await,
        Seen {
            device: "meter-a".to_owned(),
            regular: false,
            value: int(BASE + 11),
        }
    );
    let sent = service.live().await;
    assert_eq!(
        values(&sent.series),
        [("meter-a".to_owned(), vec![int(BASE + 11)])]
    );
    assert_eq!(sent.series[0].point.as_str(), "power");
    // Not in the buffer: the window still holds the first round only.
    assert_eq!(service.pending_records(), 2);

    // Every live interval (1 s) while the lease runs.
    for (second, value) in [(2, BASE + 21), (3, BASE + 31)] {
        service.bus.set_holding_registers(1, 0, &[value]);
        advance(Duration::from_millis(u64::from(LIVE_INTERVAL_MS))).await;
        assert_eq!(
            service.poll().await,
            Seen {
                device: "meter-a".to_owned(),
                regular: false,
                value: int(value),
            },
            "the live poll {second} s in"
        );
        assert_eq!(
            values(&service.live().await.series),
            [("meter-a".to_owned(), vec![int(value)])]
        );
        assert_eq!(service.pending_records(), 2, "{second} s in");
    }

    // The lease ends 4 s in: no poll then, and the next poll of each device is the regular one,
    // 6 s in, which goes into the window and not to up/live.
    service.bus.set_holding_registers(1, 0, &[BASE + 51]);
    service.bus.set_holding_registers(2, 0, &[BASE + 52]);
    advance(Duration::from_secs(1)).await;
    advance(Duration::from_secs(2)).await;
    let mut regular = vec![service.poll().await, service.poll().await];
    regular.sort_by(|a, b| a.device.cmp(&b.device));
    assert_eq!(
        regular,
        [
            Seen {
                device: "meter-a".to_owned(),
                regular: true,
                value: int(BASE + 51),
            },
            Seen {
                device: "meter-b".to_owned(),
                regular: true,
                value: int(BASE + 52),
            },
        ]
    );
    assert_eq!(service.pending_records(), 4);

    // The window closes 10 s in. The next packet the gateway sends is its batch (nothing went
    // to up/live in between), holding the regular polls only.
    advance(Duration::from_secs(4)).await;
    let batch = service.batch().await;
    assert_eq!(
        values(&batch.series),
        [
            ("meter-a".to_owned(), vec![int(BASE + 1), int(BASE + 51)]),
            ("meter-b".to_owned(), vec![int(BASE + 2), int(BASE + 52)]),
        ]
    );
    assert!(service.polls.try_recv().is_err(), "no other poll");
    service.stop().await;
}

/// A renewal moves the lease's end; `stop` ends it at once: no more polls at the live interval
/// and nothing more on up/live.
#[tokio::test(start_paused = true)]
async fn a_renewal_extends_the_lease_and_stop_ends_it_at_once() {
    let _manual = clock::manual();
    let mut service = Service::start("live-stop").await;

    let meter_b = DeviceId::new("meter-b").unwrap();
    // 1 s in, a live interval after the first round.
    advance(Duration::from_millis(u64::from(LIVE_INTERVAL_MS))).await;
    service
        .live_control(&control(
            "chart-1",
            LiveAction::Start(spec(&["meter-b"], 2)),
        ))
        .await;
    assert!(!service.poll().await.regular);
    service.live().await;
    // Renewed 2 s in, for 10 s from then: still running 3 s in, where the start alone would
    // have ended it.
    advance(Duration::from_secs(1)).await;
    assert!(!service.poll().await.regular);
    service.live().await;
    service
        .live_control(&control(
            "chart-1",
            LiveAction::Renew(spec(&["meter-b"], 10)),
        ))
        .await;
    service
        .until_leases("the renewal to be applied", |coverage| {
            coverage.covers(
                &meter_b,
                tokio::time::Instant::now() + Duration::from_secs(5),
            )
        })
        .await;
    advance(Duration::from_secs(1)).await;
    assert_eq!(
        service.poll().await,
        Seen {
            device: "meter-b".to_owned(),
            regular: false,
            value: int(BASE + 2),
        },
        "3 s in, the renewed lease still runs"
    );
    assert_eq!(
        values(&service.live().await.series),
        [("meter-b".to_owned(), vec![int(BASE + 2)])]
    );

    // Stopped 3 s in, 9 s before the renewed lease would end: no poll at the live interval any
    // more, the next ones are the regular polls 6 s in, and nothing more goes to up/live (the
    // next packet is the batch).
    service
        .live_control(&control("chart-1", LiveAction::Stop))
        .await;
    service
        .until_leases("the stop to be applied", |coverage| {
            !coverage.covers(&meter_b, tokio::time::Instant::now())
        })
        .await;
    advance(Duration::from_secs(1)).await;
    advance(Duration::from_secs(2)).await;
    let mut regular = [service.poll().await, service.poll().await];
    regular.sort_by(|a, b| a.device.cmp(&b.device));
    assert_eq!(
        regular
            .iter()
            .map(|seen| (seen.device.as_str(), seen.regular))
            .collect::<Vec<_>>(),
        [("meter-a", true), ("meter-b", true)]
    );
    advance(Duration::from_secs(4)).await;
    let batch = service.batch().await;
    assert_eq!(
        values(&batch.series),
        [
            ("meter-a".to_owned(), vec![int(BASE + 1), int(BASE + 1)]),
            ("meter-b".to_owned(), vec![int(BASE + 2), int(BASE + 2)]),
        ]
    );
    assert!(service.polls.try_recv().is_err(), "no other poll");
    service.stop().await;
}
