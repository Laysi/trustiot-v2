//! Receiving and applying the desired configuration (Laysi/trustiot-v2#94, rebuild checklist
//! E1) against testkit's fake Modbus endpoints: each acceptance criterion of the issue has a
//! test here.
//!
//! - removed devices are no longer polled and their state is cleared;
//! - a device removed during a request is stopped once its answer is in, so the device sharing
//!   its connection never gets that answer;
//! - a change leaves the other devices polling untouched: same schedule, same connection, values
//!   every poll;
//! - an invalid configuration is rejected with its issues and the previous one stays;
//! - the version rules of the protocol (same version again, older version, cleared message);
//! - after a restart without network the saved configuration is polled at once;
//! - after a restart the saved configuration is polled until the cloud's newer one arrives over
//!   the link, which is then applied, saved and reported.
//!
//! A restart is shown by stopping the agent's configuration side and starting a new one on the
//! same state directory, which is what a process starting after a power cut finds; a save cut
//! short by the power is covered by the store's unit tests. The clock moves only when a test
//! advances it (`testkit::clock::manual`), and only once every device has reported its poll, so
//! no timeout fires because the machine is slow.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod support;

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use agent::desired::fleet::{Fleet, Sink};
use agent::desired::store::{FILE_NAME, Store};
use agent::desired::{self, Applier};
use agent::link::{
    self, ConnectError, ConnectStage, Connector, LinkSettings, LinkState, Subscriptions,
};
use agent::modbus::endpoint::Endpoints;
use agent::modbus::{Clock, Timestamp};
use agent::polling::{Polled, Settings};
use protocol::config::{
    ByteOrder, ConfigOutcome, ConfigReport, DataPoint, DataType, DesiredConfig, Device,
    DeviceModel, FunctionCode, Host, Transport,
};
use protocol::topic::Channel;
use protocol::{
    AgentVersion, ConfigVersion, DeviceId, IssueCode, ModelName, PointKey, UnixMillis, V1, Value,
};
use support::{FakeBroker, Packet, gateway, puback, publish};
use testkit::clock::{self, bounded};
use testkit::modbus::{self as fake, ConnectionId, ConnectionState, FakeModbus};
use testkit::mqtt::ScratchDir;
use tokio::io::DuplexStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::advance;
use tokio_util::sync::CancellationToken;

/// Every device's poll interval unless a test says otherwise.
const PERIOD: Duration = Duration::from_secs(1);
/// What unit `u` holds in holding register 0: `BASE + u`.
const BASE: u16 = 2300;

fn settings() -> Settings {
    let settings = Settings::default();
    settings.validate().unwrap();
    settings
}

/// A synchronized clock that always reads the same time.
struct TestClock;

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        Timestamp {
            at: UnixMillis::new(1_760_054_400_000),
            synced: true,
        }
    }
}

#[derive(Debug)]
enum Record {
    Polled(DeviceId, Box<Polled>),
    Removed(DeviceId),
}

/// The sink of the tests: every call becomes a record.
struct Recorder(mpsc::UnboundedSender<Record>);

impl Sink for Recorder {
    fn polled(&self, device: &DeviceId, polled: Polled) {
        let _ = self
            .0
            .send(Record::Polled(device.clone(), Box::new(polled)));
    }

    fn removed(&self, device: &DeviceId) {
        let _ = self.0.send(Record::Removed(device.clone()));
    }
}

struct Records(mpsc::UnboundedReceiver<Record>);

impl Records {
    /// Waits for one poll of each of `devices` and nothing else; fails on a removal or a poll
    /// of another device. Every poll read the device's value.
    async fn round(&mut self, devices: &[&str]) -> BTreeMap<String, Polled> {
        let mut polls = BTreeMap::new();
        while polls.len() < devices.len() {
            let record = bounded(format!("polls of {devices:?}"), self.0.recv())
                .await
                .unwrap();
            let Record::Polled(device, polled) = record else {
                panic!("expected polls of {devices:?}, got {record:?}")
            };
            let device = device.as_str().to_owned();
            assert!(
                devices.contains(&device.as_str()),
                "{device} polled; expected only {devices:?}"
            );
            assert!(
                polled.poll.failed.is_empty() && polled.poll.readings.len() == 1,
                "{device}: {:?}",
                polled.poll
            );
            assert!(
                polls.insert(device.clone(), *polled).is_none(),
                "{device} polled twice in one round"
            );
        }
        polls
    }

    /// The removals recorded so far, sorted; fails on anything else.
    fn removed(&mut self) -> Vec<String> {
        let mut removed = Vec::new();
        while let Ok(record) = self.0.try_recv() {
            match record {
                Record::Removed(device) => removed.push(device.as_str().to_owned()),
                other @ Record::Polled(..) => panic!("expected only removals, got {other:?}"),
            }
        }
        removed.sort();
        removed
    }

    /// Waits for the removal of exactly `removed` and one poll of each of `polled`, in any
    /// order.
    async fn changes(&mut self, removed: &[&str], polled: &[&str]) {
        let mut seen_removed = Vec::new();
        let mut seen_polled = Vec::new();
        while seen_removed.len() < removed.len() || seen_polled.len() < polled.len() {
            match bounded("removals and polls", self.0.recv()).await.unwrap() {
                Record::Removed(device) => seen_removed.push(device.as_str().to_owned()),
                Record::Polled(device, polled) => {
                    assert!(polled.poll.failed.is_empty(), "{device}: {:?}", polled.poll);
                    seen_polled.push(device.as_str().to_owned());
                }
            }
        }
        seen_removed.sort();
        seen_polled.sort();
        assert_eq!(seen_removed, removed);
        assert_eq!(seen_polled, polled);
    }

    /// Nothing more was recorded.
    fn assert_idle(&mut self) {
        if let Ok(record) = self.0.try_recv() {
            panic!("nothing more expected, got {record:?}");
        }
    }
}

/// A fleet that reaches the fake endpoints on 127.0.0.1, and what its sink records.
fn fleet() -> (Fleet, Records) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let settings = settings();
    let fleet = Fleet::new(
        Endpoints::new(settings.connect_timeout).exempt_for_tests([Ipv4Addr::LOCALHOST.into()]),
        settings,
        Arc::new(TestClock),
        Arc::new(Recorder(sender)),
    );
    (fleet, Records(receiver))
}

/// A Modbus TCP endpoint hosting `units`, each with its value in holding register 0.
async fn endpoint(units: &[u8]) -> FakeModbus {
    let endpoint = FakeModbus::start(fake::Transport::ModbusTcp).await;
    for &unit in units {
        endpoint.add_unit(unit);
        endpoint.set_holding_registers(unit, 0, &[BASE + u16::from(unit)]);
    }
    endpoint
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

fn version(version: u64) -> ConfigVersion {
    ConfigVersion::new(version).unwrap()
}

fn payload(version_number: u64, devices: Vec<Device>) -> Vec<u8> {
    DesiredConfig {
        v: V1,
        rev: 1,
        version: version(version_number),
        models: vec![model()],
        devices,
    }
    .encode()
    .unwrap()
}

/// `payload` with `from` (which occurs exactly once) replaced by `to` of the same length: a
/// configuration that decodes but breaks a rule, which `encode` would refuse to produce.
fn patched(payload: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    assert_eq!(from.len(), to.len());
    let at: Vec<usize> = payload
        .windows(from.len())
        .enumerate()
        .filter_map(|(at, window)| (window == from).then_some(at))
        .collect();
    assert_eq!(at.len(), 1, "{from:?} must occur exactly once");
    let mut patched = payload.to_vec();
    patched[at[0]..at[0] + to.len()].copy_from_slice(to);
    patched
}

fn saved(dir: &ScratchDir) -> Vec<u8> {
    std::fs::read(dir.path().join(FILE_NAME)).unwrap()
}

/// The single value of a poll.
fn value(polled: &Polled) -> &Value {
    &polled.poll.readings[0].sample.value
}

#[tokio::test(start_paused = true)]
async fn a_removed_device_is_no_longer_polled_and_its_state_is_cleared() {
    let _manual = clock::manual();
    let bus = endpoint(&[1, 2]).await;
    let other = endpoint(&[1]).await;
    let dir = ScratchDir::new("desired-removed");
    let (fleet, mut records) = fleet();
    let mut applier = Applier::new(fleet, Store::new(dir.path()));

    let v1 = payload(
        1,
        vec![
            device("meter-a", &bus, 1),
            device("meter-b", &bus, 2),
            device("meter-c", &other, 1),
        ],
    );
    assert_eq!(
        applier.receive(v1).await,
        Some(ConfigReport::applied(version(1)))
    );
    records.round(&["meter-a", "meter-b", "meter-c"]).await;
    other
        .wait_for_connection(ConnectionId(0), ConnectionState::Open)
        .await;
    let other_requests = other.requests().len();

    let v2 = payload(2, vec![device("meter-a", &bus, 1)]);
    assert_eq!(
        applier.receive(v2.clone()).await,
        Some(ConfigReport::applied(version(2)))
    );
    // Both are stopped and cleared before the report goes out.
    assert_eq!(records.removed(), ["meter-b", "meter-c"]);
    let ids: Vec<&str> = applier.fleet().devices().map(DeviceId::as_str).collect();
    assert_eq!(ids, ["meter-a"]);
    assert_eq!(saved(&dir), v2);

    let from = bus.requests().len();
    for _ in 0..3 {
        advance(PERIOD).await;
        records.round(&["meter-a"]).await;
    }
    records.assert_idle();
    let units: Vec<u8> = bus.requests()[from..].iter().map(|r| r.unit).collect();
    assert_eq!(units, [1, 1, 1], "only meter-a (unit 1) is asked");
    // meter-c was the only device at the other endpoint: its connection is closed and nothing
    // more was asked there.
    other
        .wait_for_connection(ConnectionId(0), ConnectionState::Closed)
        .await;
    assert_eq!(other.requests().len(), other_requests);
    assert_eq!(applier.fleet().endpoints(), 1);
}

/// A device removed while its request is in flight is stopped once that request has its answer,
/// never in the middle of it: the device sharing its connection goes on reading its own values
/// over the same connection, and no answer is left for it to take by mistake.
#[tokio::test(start_paused = true)]
async fn a_device_removed_during_its_request_is_stopped_after_the_answer() {
    let _manual = clock::manual();
    let bus = endpoint(&[1, 2]).await;
    let dir = ScratchDir::new("desired-in-flight");
    let (fleet, mut records) = fleet();
    let mut applier = Applier::new(fleet, Store::new(dir.path()));

    let v1 = payload(
        1,
        vec![device("meter-a", &bus, 1), device("meter-b", &bus, 2)],
    );
    assert_eq!(
        applier.receive(v1).await,
        Some(ConfigReport::applied(version(1)))
    );
    records.round(&["meter-a", "meter-b"]).await;

    // meter-b's next request gets its answer only when the test lets it through.
    bus.inject(fake::Fault::hold().unit(2).once());
    let from = bus.requests().len();
    advance(PERIOD).await;
    let mut count = from + 1;
    let held = loop {
        let requests = bus.wait_for_requests(count).await;
        if let Some(held) = requests[from..].iter().find(|r| r.fault.is_some()) {
            break held.index;
        }
        count += 1;
    };

    let v2 = payload(2, vec![device("meter-a", &bus, 1)]);
    let receive = applier.receive(v2);
    tokio::pin!(receive);
    // Applying waits for the request in flight: it cannot end before the answer is let
    // through. (Aborting meter-b's task at once would let it end here.)
    for _ in 0..1000 {
        tokio::select! {
            biased;
            report = &mut receive => {
                panic!("applied while meter-b's request was in flight: {report:?}")
            }
            () = tokio::task::yield_now() => {}
        }
    }
    bus.release(held);
    assert_eq!(
        bounded("the configuration to be applied", receive).await,
        Some(ConfigReport::applied(version(2)))
    );

    // meter-b's last poll, if it got to report it, has its own value and comes before its
    // removal; meter-a's poll of this round has its own value too.
    let (mut removed, mut polled_a) = (false, false);
    while !(removed && polled_a) {
        match bounded("meter-b's removal and meter-a's poll", records.0.recv())
            .await
            .unwrap()
        {
            Record::Removed(device) => {
                assert_eq!(device.as_str(), "meter-b");
                removed = true;
            }
            Record::Polled(device, polled) => {
                let expected = match device.as_str() {
                    "meter-a" => {
                        polled_a = true;
                        BASE + 1
                    }
                    "meter-b" => {
                        assert!(!removed, "meter-b polled after its removal");
                        BASE + 2
                    }
                    other => panic!("{other} polled"),
                };
                assert!(polled.poll.failed.is_empty(), "{device}: {:?}", polled.poll);
                assert_eq!(value(&polled), &Value::Int(i64::from(expected)), "{device}");
            }
        }
    }

    let from = bus.requests().len();
    for _ in 0..3 {
        advance(PERIOD).await;
        let polls = records.round(&["meter-a"]).await;
        assert_eq!(value(&polls["meter-a"]), &Value::Int(i64::from(BASE + 1)));
    }
    records.assert_idle();
    let units: Vec<u8> = bus.requests()[from..].iter().map(|r| r.unit).collect();
    assert_eq!(units, [1, 1, 1], "only meter-a (unit 1) is asked");
    // One connection all along: nothing was cut off and reopened.
    assert_eq!(
        bus.connections().into_iter().collect::<Vec<_>>(),
        [(ConnectionId(0), ConnectionState::Open)]
    );
}

#[tokio::test(start_paused = true)]
async fn a_change_leaves_the_other_devices_polling_untouched() {
    let _manual = clock::manual();
    let bus = endpoint(&[1, 2, 3]).await;
    let dir = ScratchDir::new("desired-unchanged");
    let (fleet, mut records) = fleet();
    let mut applier = Applier::new(fleet, Store::new(dir.path()));

    let a = device("meter-a", &bus, 1);
    let b = device("meter-b", &bus, 2);
    assert_eq!(
        applier
            .receive(payload(1, vec![a.clone(), b.clone()]))
            .await,
        Some(ConfigReport::applied(version(1)))
    );
    let first = records.round(&["meter-a", "meter-b"]).await;
    advance(PERIOD).await;
    records.round(&["meter-a", "meter-b"]).await;
    let mut a_scheduled = first["meter-a"].scheduled + PERIOD;

    // meter-b polls half as often now and meter-c is new; meter-a is as it was.
    let slower_b = Device {
        poll_interval_ms: 2000,
        ..b
    };
    let c = device("meter-c", &bus, 3);
    assert_eq!(
        applier
            .receive(payload(2, vec![a.clone(), slower_b, c]))
            .await,
        Some(ConfigReport::applied(version(2)))
    );
    // The changed and the new device start at once; meter-a does not poll out of turn, and no
    // device is cleared: meter-b keeps its last values until its next poll.
    records.round(&["meter-b", "meter-c"]).await;
    assert_eq!(records.removed(), Vec::<String>::new());

    for round in 1..=4 {
        advance(PERIOD).await;
        let devices: &[&str] = if round % 2 == 0 {
            &["meter-a", "meter-b", "meter-c"]
        } else {
            &["meter-a", "meter-c"]
        };
        let polls = records.round(devices).await;
        let polled = &polls["meter-a"];
        a_scheduled += PERIOD;
        // Its schedule goes on from before the change, with no poll skipped.
        assert_eq!(polled.scheduled, a_scheduled, "round {round}");
        assert_eq!(polled.missed, 0, "round {round}");
        assert_eq!(
            value(polled),
            &Value::Int(i64::from(BASE + 1)),
            "round {round}"
        );
        assert!(polled.watchdog.is_none());
    }
    // The endpoint's one connection was never replaced: the change did not reconnect anyone.
    assert_eq!(
        bus.connections(),
        BTreeMap::from([(ConnectionId(0), ConnectionState::Open)])
    );
    records.assert_idle();
}

#[tokio::test(start_paused = true)]
async fn an_invalid_configuration_is_rejected_with_its_issues_and_the_previous_one_stays() {
    let _manual = clock::manual();
    let bus = endpoint(&[1]).await;
    let dir = ScratchDir::new("desired-invalid");
    let (fleet, mut records) = fleet();
    let mut applier = Applier::new(fleet, Store::new(dir.path()));
    let v1 = payload(1, vec![device("meter-a", &bus, 1)]);
    assert_eq!(
        applier.receive(v1.clone()).await,
        Some(ConfigReport::applied(version(1)))
    );
    records.round(&["meter-a"]).await;

    // Version 2 moves meter-a to a model that is not in the configuration ("ghost" for
    // "meter": CBOR text of five characters is 0x65 and the text).
    let v2 = patched(
        &payload(2, vec![device("meter-a", &bus, 1)]),
        b"\x65model\x65meter",
        b"\x65model\x65ghost",
    );
    let report = applier.receive(v2).await.unwrap();
    assert_eq!(
        (
            report.version,
            report.outcome.clone(),
            report.active_version
        ),
        (version(2), ConfigOutcome::Rejected, Some(version(1)))
    );
    assert_eq!(report.issues.len(), 1, "{:?}", report.issues);
    assert_eq!(report.issues[0].code, IssueCode::UnknownModel);
    assert_eq!(report.issues[0].path, "devices[0].model");
    // The report goes out as it is.
    assert_eq!(
        ConfigReport::decode(&report.encode().unwrap()).unwrap(),
        report
    );

    // A payload that does not decode is rejected for the version it carries.
    let only_a_version = b"\xa2\x61v\x01\x67version\x03".to_vec();
    let report = applier.receive(only_a_version).await.unwrap();
    assert_eq!(
        (
            report.version,
            report.outcome.clone(),
            report.active_version
        ),
        (version(3), ConfigOutcome::Rejected, Some(version(1)))
    );
    assert_eq!(report.issues[0].code, IssueCode::DecodeFailed);

    // Version 1 stays: active, saved, and polled on its schedule.
    assert_eq!(applier.active_version(), Some(version(1)));
    assert_eq!(saved(&dir), v1);
    for _ in 0..2 {
        advance(PERIOD).await;
        records.round(&["meter-a"]).await;
    }
    records.assert_idle();
}

#[tokio::test(start_paused = true)]
async fn versions_follow_the_protocol_rules() {
    let _manual = clock::manual();
    let bus = endpoint(&[1, 2]).await;
    let dir = ScratchDir::new("desired-versions");
    let (fleet, mut records) = fleet();
    let mut applier = Applier::new(fleet, Store::new(dir.path()));
    let a = device("meter-a", &bus, 1);

    // Nothing to answer: no version to report, no configuration to change.
    assert_eq!(applier.receive(Vec::new()).await, None);
    assert_eq!(applier.receive(b"\xff\x00".to_vec()).await, None);
    assert_eq!(applier.active_version(), None);

    let v3 = payload(3, vec![a.clone()]);
    assert_eq!(
        applier.receive(v3.clone()).await,
        Some(ConfigReport::applied(version(3)))
    );
    records.round(&["meter-a"]).await;

    // The same version again (every reconnect delivers it): reported again, not applied again,
    // even with other content.
    assert_eq!(
        applier.receive(v3.clone()).await,
        Some(ConfigReport::applied(version(3)))
    );
    let other_content = payload(3, vec![a.clone(), device("meter-b", &bus, 2)]);
    assert_eq!(
        applier.receive(other_content).await,
        Some(ConfigReport::applied(version(3)))
    );
    // The active version that does not decode is rejected, and version 3 stays.
    let report = applier
        .receive(b"\xa2\x61v\x01\x67version\x03".to_vec())
        .await
        .unwrap();
    assert_eq!(
        (
            report.version,
            report.outcome.clone(),
            report.active_version
        ),
        (version(3), ConfigOutcome::Rejected, Some(version(3)))
    );
    assert_eq!(report.issues[0].code, IssueCode::DecodeFailed);
    // An older version is refused as stale; a cleared retained message changes nothing.
    assert_eq!(
        applier.receive(payload(2, vec![])).await,
        Some(ConfigReport::stale(version(2), version(3)))
    );
    assert_eq!(applier.receive(Vec::new()).await, None);

    assert_eq!(applier.active_version(), Some(version(3)));
    assert_eq!(saved(&dir), v3);
    assert_eq!(applier.fleet().len(), 1);
    records.assert_idle();
    advance(PERIOD).await;
    records.round(&["meter-a"]).await;
    records.assert_idle();
}

/// A connection that never comes up: the gateway has no network.
struct NoNetwork;

impl Connector for NoNetwork {
    type Stream = DuplexStream;

    async fn connect(&self) -> Result<DuplexStream, ConnectError> {
        Err(ConnectError::new(
            ConnectStage::Tcp,
            "network is unreachable",
        ))
    }
}

fn link_settings() -> LinkSettings {
    LinkSettings {
        subscriptions: Subscriptions::new([Channel::Config]).unwrap(),
        ..LinkSettings::new(gateway(), AgentVersion::new("0.1.0-test").unwrap())
    }
}

/// The configuration side of a running agent, as `main` starts it.
struct Running {
    link: link::Link,
    shutdown: CancellationToken,
    service: JoinHandle<()>,
    supervisor: JoinHandle<()>,
    records: Records,
}

impl Running {
    fn start<C: Connector>(dir: &ScratchDir, connector: C) -> Self {
        let shutdown = CancellationToken::new();
        let (link, mut incoming, supervisor) =
            link::link(link_settings(), connector, shutdown.clone());
        let (inbox, deliveries) = desired::inbox();
        tokio::spawn(async move {
            while let Some(message) = incoming.recv().await {
                assert_eq!(message.channel, Channel::Config);
                inbox.deliver(message.payload);
            }
        });
        let (fleet, records) = fleet();
        let service = tokio::spawn(desired::run(
            Applier::new(fleet, Store::new(dir.path())),
            link.clone(),
            deliveries,
            shutdown.clone(),
        ));
        Self {
            link,
            shutdown,
            supervisor: tokio::spawn(supervisor.run()),
            service,
            records,
        }
    }

    async fn wait_state(&self, what: &str, wanted: impl FnMut(&LinkState) -> bool) {
        let mut state = self.link.state();
        bounded(what, state.wait_for(wanted)).await.unwrap();
    }

    /// Stops it as the process ending would: no device is polled afterwards.
    async fn stop(self) {
        self.shutdown.cancel();
        bounded("the configuration side to stop", self.service)
            .await
            .unwrap();
        self.supervisor.abort();
    }
}

#[tokio::test(start_paused = true)]
async fn after_a_restart_without_network_the_saved_configuration_is_polled() {
    let _manual = clock::manual();
    let bus = endpoint(&[1, 2]).await;
    let dir = ScratchDir::new("desired-offline");
    let v1 = payload(
        1,
        vec![device("meter-a", &bus, 1), device("meter-b", &bus, 2)],
    );
    {
        let (fleet, mut records) = fleet();
        let mut applier = Applier::new(fleet, Store::new(dir.path()));
        assert_eq!(
            applier.receive(v1.clone()).await,
            Some(ConfigReport::applied(version(1)))
        );
        records.round(&["meter-a", "meter-b"]).await;
        applier.stop().await;
    }

    // The next start finds no network: the link keeps failing, and the devices of the saved
    // configuration are polled from the start all the same.
    let mut running = Running::start(&dir, NoNetwork);
    running
        .wait_state("the failed connection", |state| {
            matches!(state, LinkState::Disconnected { .. })
        })
        .await;
    let polls = running.records.round(&["meter-a", "meter-b"]).await;
    assert_eq!(value(&polls["meter-b"]), &Value::Int(i64::from(BASE + 2)));
    for _ in 0..3 {
        advance(PERIOD).await;
        running.records.round(&["meter-a", "meter-b"]).await;
    }
    assert!(!matches!(
        *running.link.state().borrow(),
        LinkState::Connected { .. }
    ));
    running.stop().await;
    assert_eq!(saved(&dir), v1);
}

/// The next packet that is not a PINGREQ; the PUBACK for the delivery and the report may come
/// in either order.
async fn report_and_ack(broker: &mut FakeBroker<DuplexStream>, delivery_id: u16) -> ConfigReport {
    let mut acked = false;
    let mut report = None;
    while !(acked && report.is_some()) {
        match bounded("the PUBACK and the report", broker.read_answering_pings()).await {
            Some(Packet::PubAck { id, reason }) => {
                assert_eq!((id, reason), (delivery_id, 0));
                acked = true;
            }
            Some(Packet::Publish(publish)) => {
                assert_eq!(publish.topic, Channel::ConfigReport.topic(&gateway()));
                assert!(publish.qos == 1 && !publish.retain, "{publish:?}");
                broker.send(&puback(publish.id.unwrap(), 0)).await;
                assert!(report.is_none(), "one report per delivery");
                report = Some(ConfigReport::decode(&publish.payload).unwrap());
            }
            other => panic!("expected the PUBACK and the report, got {other:?}"),
        }
    }
    report.unwrap()
}

#[tokio::test(start_paused = true)]
async fn after_a_restart_the_gateway_converges_to_the_latest_version() {
    let _manual = clock::manual();
    let bus = endpoint(&[1, 2, 3]).await;
    let dir = ScratchDir::new("desired-restart");
    let a = device("meter-a", &bus, 1);
    let v1 = payload(1, vec![a.clone(), device("meter-b", &bus, 2)]);
    {
        let (fleet, mut records) = fleet();
        let mut applier = Applier::new(fleet, Store::new(dir.path()));
        assert_eq!(
            applier.receive(v1).await,
            Some(ConfigReport::applied(version(1)))
        );
        records.round(&["meter-a", "meter-b"]).await;
        applier.stop().await;
    }

    // While the gateway was off, the cloud published version 2: meter-b removed, meter-c added.
    let v2 = payload(2, vec![a, device("meter-c", &bus, 3)]);
    let (connector, mut brokers) = support::DuplexConnector::new();
    let mut running = Running::start(&dir, connector);
    // The saved version is polled before the connection is up.
    running.records.round(&["meter-a", "meter-b"]).await;
    let mut broker = FakeBroker::new(bounded("the connection", brokers.recv()).await.unwrap())
        .subscribing(&[Channel::Config]);
    broker.accept().await;
    // The broker delivers the retained configuration right after the SUBACK.
    broker
        .send(&publish(
            &Channel::Config.topic(&gateway()),
            &v2,
            1,
            true,
            Some(1),
        ))
        .await;
    assert_eq!(
        report_and_ack(&mut broker, 1).await,
        ConfigReport::applied(version(2))
    );
    // meter-b was cleared before the report; meter-c started polling at once, maybe before.
    running.records.changes(&["meter-b"], &["meter-c"]).await;
    assert_eq!(saved(&dir), v2);
    advance(PERIOD).await;
    running.records.round(&["meter-a", "meter-c"]).await;

    // Every reconnect delivers the retained message again: reported again, nothing re-applied.
    broker
        .send(&publish(
            &Channel::Config.topic(&gateway()),
            &v2,
            1,
            true,
            Some(2),
        ))
        .await;
    assert_eq!(
        report_and_ack(&mut broker, 2).await,
        ConfigReport::applied(version(2))
    );
    running.records.assert_idle();
    running.stop().await;

    // Another restart, now without network, polls version 2.
    let mut running = Running::start(&dir, NoNetwork);
    running.records.round(&["meter-a", "meter-c"]).await;
    running.stop().await;
}
