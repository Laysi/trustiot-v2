//! The telemetry pipeline (Laysi/trustiot-v2#90): readings to batches, the offline buffer in
//! memory and on disk, and the upload with the cloud's end-to-end acknowledgement, through the
//! real link against a scripted broker (`support::FakeBroker`) over in-memory streams.
//!
//! Every test runs on a paused clock, so an hour (or thirty days) of disconnection passes in
//! moments and every timer fires exactly when nothing else can happen; the buffer's files are
//! real files in a scratch directory. A test wall clock follows tokio's clock and can be
//! stepped. Two kinds of cloud answer: [`Cloud`] serves every connection like the ingest service
//! does (a high-water mark per stream, `committed` or `duplicate`), and the ack-table tests
//! answer each packet themselves.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod support;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent::clock::{GatewayClock, WallClock};
use agent::desired;
use agent::dispatch::{self, Handlers};
use agent::link::{self, ConnectError, ConnectStage, Connector, Link, LinkSettings, LinkState};
use agent::upload::{self, Handles, Settings};
use protocol::health::Heartbeat;
use protocol::telemetry::{AckOutcome, Reading, Sample, TelemetryAck, TelemetryBatch};
use protocol::topic::Channel;
use protocol::{AgentVersion, DeviceId, IssueCode, PointKey, Seq, StreamId, UnixMillis, V1, Value};
use support::{FakeBroker, Packet, gateway, pingresp, puback, publish};
use testkit::clock::bounded;
use testkit::mqtt::ScratchDir;
use tokio::io::DuplexStream;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// The test wall clock's time when the test starts: 2027-01-15T08:00:00Z.
const START_MS: u64 = 1_800_000_000_000;
const DAY: Duration = Duration::from_secs(86_400);

/// A wall clock that runs with tokio's (paused) clock and that the test can step.
#[derive(Clone)]
struct TestWall {
    start: Instant,
    step_ms: Arc<AtomicI64>,
}

impl TestWall {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            step_ms: Arc::new(AtomicI64::new(0)),
        }
    }

    fn step(&self, by_ms: i64) {
        self.step_ms.fetch_add(by_ms, Ordering::Relaxed);
    }

    fn now_ms(&self) -> u64 {
        let elapsed = i64::try_from(self.start.elapsed().as_millis()).unwrap();
        u64::try_from(
            i64::try_from(START_MS).unwrap() + elapsed + self.step_ms.load(Ordering::Relaxed),
        )
        .unwrap()
    }
}

impl WallClock for TestWall {
    fn now(&self) -> UnixMillis {
        UnixMillis::new(self.now_ms())
    }
}

/// The network between the agent and the cloud: the test takes it down and up.
#[derive(Clone)]
struct Network {
    online: Arc<AtomicBool>,
    /// Bumped to cut the connection that is up.
    cut: Arc<watch::Sender<u64>>,
}

impl Network {
    fn new() -> Self {
        Self {
            online: Arc::new(AtomicBool::new(true)),
            cut: Arc::new(watch::channel(0).0),
        }
    }

    /// No new connections, and the current one is cut.
    fn down(&self) {
        self.online.store(false, Ordering::Relaxed);
        self.cut.send_modify(|generation| *generation += 1);
    }

    fn up(&self) {
        self.online.store(true, Ordering::Relaxed);
    }
}

/// Hands every connection's broker side to the cloud while the network is up.
struct SwitchConnector {
    network: Network,
    brokers: mpsc::UnboundedSender<DuplexStream>,
}

impl Connector for SwitchConnector {
    type Stream = DuplexStream;

    async fn connect(&self) -> Result<DuplexStream, ConnectError> {
        if !self.network.online.load(Ordering::Relaxed) {
            return Err(ConnectError::new(
                ConnectStage::Tcp,
                "the test's network is down",
            ));
        }
        let (client, broker) = tokio::io::duplex(1024 * 1024);
        self.brokers
            .send(broker)
            .map_err(|_| ConnectError::new(ConnectStage::Tcp, "the test stopped accepting"))?;
        Ok(client)
    }
}

/// One agent: the link, the pipeline and the dispatcher, wired as `main.rs` wires them.
struct Agent {
    link: Link,
    handles: Handles,
    clock: GatewayClock,
    stop: CancellationToken,
    shutdown: CancellationToken,
    supervisor: JoinHandle<()>,
    pipeline: JoinHandle<()>,
    dispatcher: JoinHandle<()>,
}

fn start_agent(settings: Settings, connector: impl Connector, clock: GatewayClock) -> Agent {
    let link_settings = LinkSettings {
        subscriptions: dispatch::subscriptions(),
        // The longest delay: an hour offline is a few dozen attempts, not thousands.
        jitter: || u64::MAX,
        ..LinkSettings::new(gateway(), AgentVersion::new("0.1.0-test").unwrap())
    };
    let shutdown = CancellationToken::new();
    let stop = CancellationToken::new();
    let (link, incoming, supervisor) = link::link(link_settings, connector, shutdown.clone());
    let (handles, pipeline) =
        upload::pipeline(settings, link.clone(), clock.clone(), stop.clone()).unwrap();
    let dispatcher = tokio::spawn(dispatch::run(
        incoming,
        Handlers {
            // The scripted cloud publishes no configuration.
            config: desired::inbox().0,
            telemetry_ack: handles.acks.clone(),
            // Nor a live lease.
            live: agent::live::leases().0,
        },
    ));
    Agent {
        link,
        handles,
        clock,
        stop,
        shutdown,
        supervisor: tokio::spawn(supervisor.run()),
        pipeline: tokio::spawn(pipeline.run()),
        dispatcher,
    }
}

impl Agent {
    /// A graceful stop: the pipeline flushes memory to disk, the link disconnects.
    async fn stop(self) {
        self.stop.cancel();
        bounded("the pipeline to stop", self.pipeline)
            .await
            .unwrap();
        self.shutdown.cancel();
        bounded("the link to stop", self.supervisor).await.unwrap();
        bounded("the dispatcher to stop", self.dispatcher)
            .await
            .unwrap();
    }

    /// A forced end, like SIGKILL or a power cut for what is in memory: nothing is flushed.
    async fn kill(self) {
        self.pipeline.abort();
        self.supervisor.abort();
        self.dispatcher.abort();
        let _ = self.pipeline.await;
        let _ = self.supervisor.await;
        let _ = self.dispatcher.await;
    }

    async fn wait_connected(&self) -> protocol::SessionId {
        let mut state = self.link.state();
        let state = bounded(
            "the link to connect",
            state.wait_for(|state| matches!(state, LinkState::Connected { .. })),
        )
        .await
        .unwrap()
        .clone();
        match state {
            LinkState::Connected { session } => session,
            _ => unreachable!(),
        }
    }
}

/// A readings that identifies itself: (device, point, time, value, clock state).
type Key = (String, String, u64, i64, bool);

fn key_of(reading: &Reading) -> Key {
    let Value::Int(value) = reading.sample.value else {
        panic!("the tests send integers")
    };
    (
        reading.device.to_string(),
        reading.point.to_string(),
        reading.sample.at.get(),
        value,
        reading.clock_synced,
    )
}

/// Every reading of a batch, as keys.
fn keys_of(batch: &TelemetryBatch) -> Vec<Key> {
    batch
        .series
        .iter()
        .flat_map(|series| {
            series.samples.iter().map(|sample| {
                let Value::Int(value) = sample.value else {
                    panic!("the tests send integers")
                };
                (
                    series.device.to_string(),
                    series.point.to_string(),
                    sample.at.get(),
                    value,
                    batch.clock_synced,
                )
            })
        })
        .collect()
}

/// Feeds readings every second, as the polling will: `devices` × `points` readings stamped by
/// the gateway's clock, each with a value of its own.
struct Generator {
    generated: Arc<Mutex<Vec<Reading>>>,
    task: JoinHandle<()>,
}

fn generate(agent: &Agent, devices: usize, points: usize, seconds: u64) -> Generator {
    generate_with(agent, devices, points, seconds, |_| None)
}

/// Like [`generate`], with a text value instead of an integer where `text` says so.
fn generate_with(
    agent: &Agent,
    devices: usize,
    points: usize,
    seconds: u64,
    text: impl Fn(u64) -> Option<String> + Send + 'static,
) -> Generator {
    let readings = agent.handles.readings.clone();
    let clock = agent.clock.clone();
    let generated = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&generated);
    let task = tokio::spawn(async move {
        // Half a second off the windows' ticks, so a reading never ties with a window's end.
        let mut tick = tokio::time::interval_at(
            Instant::now() + Duration::from_millis(500),
            Duration::from_secs(1),
        );
        let mut value = 0_i64;
        for second in 0..seconds {
            tick.tick().await;
            let now = agent::modbus::Clock::now(&clock);
            let mut batch = Vec::new();
            for device in 0..devices {
                for point in 0..points {
                    value += 1;
                    let sample_value = match text(second) {
                        Some(text) if device == 0 && point == 0 => Value::Text(text),
                        _ => Value::Int(value),
                    };
                    batch.push(Reading {
                        device: DeviceId::new(format!("device-{device}")).unwrap(),
                        point: PointKey::new(format!("point-{point}")).unwrap(),
                        sample: Sample {
                            at: now.at,
                            value: sample_value,
                        },
                        clock_synced: now.synced,
                    });
                }
            }
            sink.lock().unwrap().extend(
                batch
                    .iter()
                    .filter(|reading| matches!(reading.sample.value, Value::Int(_)))
                    .cloned(),
            );
            readings.push(batch);
        }
    });
    Generator { generated, task }
}

impl Generator {
    async fn finish(self) -> BTreeSet<Key> {
        bounded("the generator to finish", self.task).await.unwrap();
        self.generated.lock().unwrap().iter().map(key_of).collect()
    }
}

/// What the cloud saw.
#[derive(Default)]
struct CloudState {
    /// The committed high-water mark per stream (this gateway only).
    high_water: HashMap<StreamId, u64>,
    /// Every telemetry PUBLISH, in arrival order.
    received: Vec<(StreamId, u64)>,
    /// The batches committed, first commits only.
    committed: Vec<TelemetryBatch>,
    duplicates: usize,
    heartbeats: Vec<Heartbeat>,
}

impl CloudState {
    fn committed_keys(&self) -> Vec<Key> {
        self.committed.iter().flat_map(keys_of).collect()
    }
}

/// How the cloud answers one batch.
struct Reply {
    puback: u8,
    outcome: Option<AckOutcome>,
    /// Added to the wall clock for `server_time`.
    server_offset_ms: i64,
}

type Policy = Box<dyn FnMut(&TelemetryBatch, bool) -> Reply + Send>;

/// The ingest service's rule: new above the high-water mark, otherwise a duplicate.
fn ingest() -> Policy {
    Box::new(|batch, new| Reply {
        puback: 0,
        outcome: Some(if new {
            AckOutcome::Committed {
                records: u32::try_from(batch.record_count()).unwrap(),
            }
        } else {
            AckOutcome::Duplicate {
                records: u32::try_from(batch.record_count()).unwrap(),
            }
        }),
        server_offset_ms: 0,
    })
}

/// The broker and the cloud behind it, serving every connection.
struct Cloud {
    state: Arc<Mutex<CloudState>>,
    task: JoinHandle<()>,
}

impl Drop for Cloud {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Cloud {
    fn start(
        mut brokers: mpsc::UnboundedReceiver<DuplexStream>,
        network: &Network,
        wall: TestWall,
        policy: Policy,
    ) -> Self {
        let state = Arc::new(Mutex::new(CloudState::default()));
        let shared = Arc::clone(&state);
        let policy = Arc::new(Mutex::new(policy));
        let mut cut = network.cut.subscribe();
        let task = tokio::spawn(async move {
            while let Some(stream) = brokers.recv().await {
                cut.borrow_and_update();
                let mut broker =
                    FakeBroker::new(stream).subscribing(dispatch::subscriptions().channels());
                broker.accept().await;
                let mut next_id = 1_u16;
                loop {
                    let packet = tokio::select! {
                        packet = broker.read() => packet,
                        _ = cut.changed() => break,
                    };
                    match packet {
                        None | Some(Packet::Disconnect { .. }) => break,
                        Some(Packet::PingReq) => broker.send(&pingresp()).await,
                        Some(Packet::PubAck { .. }) => {}
                        Some(Packet::Publish(message))
                            if message.topic == Channel::Heartbeat.topic(&gateway()) =>
                        {
                            let heartbeat = Heartbeat::decode(&message.payload).unwrap();
                            shared.lock().unwrap().heartbeats.push(heartbeat);
                        }
                        // The link's own `offline` on a graceful stop.
                        Some(Packet::Publish(message))
                            if message.topic == Channel::Status.topic(&gateway()) =>
                        {
                            if let Some(id) = message.id {
                                broker.send(&puback(id, 0)).await;
                            }
                        }
                        Some(Packet::Publish(message)) => {
                            assert_eq!(message.topic, Channel::Telemetry.topic(&gateway()));
                            let batch = TelemetryBatch::decode(&message.payload).unwrap();
                            let reply = {
                                let mut state = shared.lock().unwrap();
                                state.received.push((batch.stream.clone(), batch.seq.get()));
                                let high =
                                    state.high_water.get(&batch.stream).copied().unwrap_or(0);
                                let reply =
                                    (policy.lock().unwrap())(&batch, batch.seq.get() > high);
                                if reply.puback < 0x80 {
                                    match reply.outcome {
                                        Some(AckOutcome::Committed { .. }) => {
                                            state
                                                .high_water
                                                .insert(batch.stream.clone(), batch.seq.get());
                                            state.committed.push(batch.clone());
                                        }
                                        Some(AckOutcome::Duplicate { .. }) => state.duplicates += 1,
                                        _ => {}
                                    }
                                }
                                reply
                            };
                            broker
                                .send(&puback(message.id.unwrap(), reply.puback))
                                .await;
                            if reply.puback < 0x80
                                && let Some(outcome) = reply.outcome
                            {
                                let server_time = u64::try_from(
                                    i64::try_from(wall.now_ms()).unwrap() + reply.server_offset_ms,
                                )
                                .unwrap();
                                let ack = TelemetryAck {
                                    v: V1,
                                    stream: batch.stream.clone(),
                                    seq: batch.seq,
                                    outcome,
                                    server_time: UnixMillis::new(server_time),
                                };
                                let payload = protocol::encode(&ack).unwrap();
                                broker
                                    .send(&publish(
                                        &Channel::TelemetryAck.topic(&gateway()),
                                        &payload,
                                        1,
                                        false,
                                        Some(next_id),
                                    ))
                                    .await;
                                next_id = next_id.wrapping_add(1).max(1);
                            }
                        }
                        Some(other) => panic!("the cloud did not expect {other:?}"),
                    }
                }
            }
        });
        Self { state, task }
    }

    fn with<T>(&self, read: impl FnOnce(&CloudState) -> T) -> T {
        read(&self.state.lock().unwrap())
    }
}

/// Waits (on the paused clock, in steps of `step`) until `condition` holds.
async fn until_every(step: Duration, what: &str, mut condition: impl FnMut() -> bool) {
    bounded(what, async {
        while !condition() {
            tokio::time::sleep(step).await;
        }
    })
    .await;
}

async fn until(what: &str, condition: impl FnMut() -> bool) {
    until_every(Duration::from_millis(100), what, condition).await;
}

/// Like [`until`], but fails unless `condition` holds within `limit` on the paused clock: for
/// a wait that must not be satisfied by letting the paused clock run on for days.
async fn within(limit: Duration, what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    bounded(what, async {
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "{what}: not within {limit:?} of the paused clock"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
}

/// The default settings, except that the window closes every upload interval even while
/// uploads do not go through (normally every five minutes then): for the tests of the buffer's
/// order and limits, which need many batches during an outage.
fn every_interval(directory: PathBuf) -> Settings {
    let mut settings = Settings::new(directory);
    settings.outage_window = settings.upload_interval;
    settings
}

/// A clock whose timesync flag is `flag`.
fn clock_with(wall: &TestWall, flag: &Path) -> GatewayClock {
    GatewayClock::new(wall.clone(), flag.to_owned())
}

/// The segment files in a buffer directory.
fn segments(dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("seg-") && name.ends_with(".log"))
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Per stream, the sequence numbers in the order the cloud received them never go down (a
/// resend repeats the same number).
fn assert_ascending_per_stream(received: &[(StreamId, u64)]) {
    let mut last: HashMap<&StreamId, u64> = HashMap::new();
    for (stream, seq) in received {
        let previous = last.insert(stream, *seq).unwrap_or(0);
        assert!(
            *seq >= previous,
            "stream {stream}: seq {seq} after {previous}: {received:?}"
        );
    }
}

/// Acceptance criterion 1: an hour without network (readings every second from a few devices,
/// a memory limit that holds only a few batches, so most go to disk), then the network returns:
/// the cloud gets every reading exactly once, each stream in ascending sequence numbers, with
/// the timestamps unchanged, and the buffer ends empty.
#[tokio::test(start_paused = true)]
async fn an_hour_offline_then_every_reading_arrives_once_in_order() {
    let scratch = ScratchDir::new("agent-upload");
    let flag = scratch.write("synchronized", "");
    let wall = TestWall::new();
    let network = Network::new();
    let (brokers_in, brokers) = mpsc::unbounded_channel();
    let cloud = Cloud::start(brokers, &network, wall.clone(), ingest());
    let mut settings = every_interval(scratch.path().join("buffer"));
    settings.memory_limit = 8 * 1024;
    let agent = start_agent(
        settings,
        SwitchConnector {
            network: network.clone(),
            brokers: brokers_in,
        },
        clock_with(&wall, &flag),
    );
    agent.wait_connected().await;
    // A minute online, an hour offline, then two minutes online again.
    let generator = generate(&agent, 3, 2, 60 + 3600 + 120);
    until("the first batches to be committed", || {
        cloud.with(|cloud| cloud.committed.len() >= 3)
    })
    .await;
    tokio::time::sleep_until(agent_start(&wall) + Duration::from_secs(60)).await;
    network.down();
    tokio::time::sleep(Duration::from_secs(3600)).await;
    let offline = agent.handles.stats.snapshot();
    assert!(
        offline.pending_batches >= 350,
        "an hour of batches waits: {offline:?}"
    );
    assert!(offline.disk_bytes > 0, "most of them on disk: {offline:?}");
    assert!(
        !segments(&scratch.path().join("buffer")).is_empty(),
        "segment files hold them"
    );
    assert_eq!(offline.dropped_records, 0);
    network.up();
    let generated = generator.finish().await;
    until_every(
        Duration::from_secs(1),
        "every reading to be committed",
        || {
            cloud.with(|cloud| cloud.committed_keys().len()) >= generated.len()
                && agent.handles.stats.snapshot().pending_batches == 0
        },
    )
    .await;

    let (committed, received, duplicates) = cloud.with(|cloud| {
        (
            cloud.committed_keys(),
            cloud.received.clone(),
            cloud.duplicates,
        )
    });
    assert_eq!(committed.len(), generated.len(), "no reading twice");
    assert_eq!(
        committed.into_iter().collect::<BTreeSet<_>>(),
        generated,
        "every reading, with its timestamp and value unchanged"
    );
    assert_ascending_per_stream(&received);
    let streams: BTreeSet<&StreamId> = received.iter().map(|(stream, _)| stream).collect();
    assert_eq!(streams.len(), 1, "one process, one stream");
    assert_eq!(
        duplicates, 0,
        "nothing was committed before it was sent again"
    );
    let stats = agent.handles.stats.snapshot();
    assert_eq!(
        (
            stats.pending_records,
            stats.pending_batches,
            stats.disk_bytes,
            stats.dropped_records
        ),
        (0, 0, 0, 0),
        "{stats:?}"
    );
    assert!(segments(&scratch.path().join("buffer")).is_empty());
    agent.stop().await;
}

/// The instant the test wall clock started (the agent starts with it).
fn agent_start(wall: &TestWall) -> Instant {
    wall.start
}

/// Acceptance criterion 2: batches on disk survive a forced end (nothing flushed) and go out
/// again with their own stream and sequence numbers; the batches that were only in memory are
/// lost, and the new process's batches are on a new stream that the cloud (high-water mark per
/// gateway and stream) takes as new.
#[tokio::test(start_paused = true)]
async fn after_a_forced_end_disk_batches_go_again_and_new_ones_use_a_new_stream() {
    let scratch = ScratchDir::new("agent-upload");
    let flag = scratch.write("synchronized", "");
    let wall = TestWall::new();
    let network = Network::new();
    let (brokers_in, brokers) = mpsc::unbounded_channel();
    let cloud = Cloud::start(brokers, &network, wall.clone(), ingest());
    let mut settings = every_interval(scratch.path().join("buffer"));
    // Two windows' batches fit in memory; the rest go to disk.
    settings.memory_limit = 2 * 1024;
    let first = start_agent(
        settings.clone(),
        SwitchConnector {
            network: network.clone(),
            brokers: brokers_in.clone(),
        },
        clock_with(&wall, &flag),
    );
    first.wait_connected().await;
    let generator = generate(&first, 3, 2, 30 + 120);
    until("the first batches to be committed", || {
        cloud.with(|cloud| cloud.committed.len() >= 2)
    })
    .await;
    tokio::time::sleep_until(wall.start + Duration::from_secs(30)).await;
    network.down();
    let before_restart = generator.finish().await;
    // Every reading has become a batch.
    until("the last window to close", || {
        let stats = first.handles.stats.snapshot();
        stats.pending_records > 0 && first.handles.stats.snapshot().pending_batches >= 12
    })
    .await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    let killed = first.handles.stats.snapshot();
    assert!(killed.disk_bytes > 0, "{killed:?}");
    let (first_stream, high_water) = cloud.with(|cloud| {
        let stream = cloud.committed[0].stream.clone();
        let high = cloud.high_water[&stream];
        (stream, high)
    });
    first.kill().await;

    network.up();
    let second = start_agent(
        settings,
        SwitchConnector {
            network: network.clone(),
            brokers: brokers_in,
        },
        clock_with(&wall, &flag),
    );
    second.wait_connected().await;
    let generator = generate(&second, 3, 2, 30);
    let after_restart = generator.finish().await;
    until_every(
        Duration::from_secs(1),
        "the second process to send everything",
        || {
            second.handles.stats.snapshot().pending_batches == 0
                && cloud.with(|cloud| {
                    cloud
                        .committed_keys()
                        .iter()
                        .filter(|key| after_restart.contains(*key))
                        .count()
                }) == after_restart.len()
        },
    )
    .await;

    let (committed, received, duplicates) = cloud.with(|cloud| {
        (
            cloud.committed.clone(),
            cloud.received.clone(),
            cloud.duplicates,
        )
    });
    assert_ascending_per_stream(&received);
    assert_eq!(duplicates, 0, "nothing new was mistaken for a resend");
    let second_streams: BTreeSet<StreamId> = committed
        .iter()
        .filter(|batch| keys_of(batch).iter().all(|key| after_restart.contains(key)))
        .map(|batch| batch.stream.clone())
        .collect();
    assert_eq!(second_streams.len(), 1);
    assert!(
        !second_streams.contains(&first_stream),
        "a restart starts a new stream"
    );
    // The first stream's batches after the restart are the ones on disk: their sequence
    // numbers continue above a gap, the batches lost from memory.
    let resent: Vec<u64> = committed
        .iter()
        .filter(|batch| batch.stream == first_stream)
        .map(|batch| batch.seq.get())
        .filter(|seq| *seq > high_water)
        .collect();
    assert!(!resent.is_empty(), "the batches on disk went out");
    assert!(
        resent.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "contiguous: {resent:?}"
    );
    assert!(
        resent[0] > high_water + 1,
        "the batches in memory (from {}) are lost, so the disk's start above them: {resent:?}",
        high_water + 1
    );
    let committed_keys: BTreeSet<Key> = committed.iter().flat_map(keys_of).collect();
    let lost: Vec<&Key> = before_restart.difference(&committed_keys).collect();
    assert!(!lost.is_empty(), "the memory batches were lost");
    assert!(
        lost.len() <= 3 * 60,
        "at most the memory limit's worth: {}",
        lost.len()
    );
    // What was lost is the oldest stretch after the network went down: every reading on disk
    // is newer than every lost one.
    let newest_lost = lost.iter().map(|key| key.2).max().unwrap();
    let resent_oldest = committed
        .iter()
        .filter(|batch| batch.stream == first_stream && batch.seq.get() > high_water)
        .flat_map(keys_of)
        .map(|key| key.2)
        .min()
        .unwrap();
    assert!(newest_lost < resent_oldest);
    second.stop().await;
}

/// Acceptance criterion 4: a restart gets a new stream, and so does a fresh state directory (a
/// reinstall or a new SD card); nothing reuses a sequence number of an earlier stream.
#[tokio::test(start_paused = true)]
async fn a_restart_and_a_fresh_state_directory_each_get_a_new_stream() {
    let scratch = ScratchDir::new("agent-upload");
    let flag = scratch.write("synchronized", "");
    let wall = TestWall::new();
    let network = Network::new();
    let (brokers_in, brokers) = mpsc::unbounded_channel();
    let cloud = Cloud::start(brokers, &network, wall.clone(), ingest());
    let mut streams = Vec::new();
    for directory in ["state-a", "state-a", "state-b"] {
        std::fs::create_dir_all(scratch.path().join(directory)).unwrap();
        let agent = start_agent(
            Settings::new(scratch.path().join(directory).join("buffer")),
            SwitchConnector {
                network: network.clone(),
                brokers: brokers_in.clone(),
            },
            clock_with(&wall, &flag),
        );
        agent.wait_connected().await;
        let before = cloud.with(|cloud| cloud.committed.len());
        generate(&agent, 1, 1, 5).finish().await;
        until("a batch to be committed", || {
            cloud.with(|cloud| cloud.committed.len() > before)
        })
        .await;
        let batch = cloud.with(|cloud| cloud.committed.last().unwrap().clone());
        assert_eq!(batch.seq, Seq::FIRST, "a new stream starts at 1");
        streams.push(batch.stream);
        agent.stop().await;
    }
    assert_ne!(streams[0], streams[1], "a restart");
    assert_ne!(streams[2], streams[0], "a fresh state directory");
    assert_ne!(streams[2], streams[1]);
    assert_eq!(cloud.with(|cloud| cloud.duplicates), 0);
    for directory in ["state-a", "state-b"] {
        assert!(
            scratch.path().join(directory).join("buffer").is_dir(),
            "the buffer directory was made under {directory}"
        );
    }
}

/// Captures what the agent logs on this thread (the pipeline's task runs here).
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Captured {
    fn install(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .finish(),
        )
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// Acceptance criterion 3, first half: memory is full and the disk fails (here its directory
/// cannot be created): the oldest batches are dropped, counted and logged, nothing panics, and
/// the newest data keeps flowing once the network is back.
#[tokio::test(start_paused = true)]
async fn memory_full_with_a_failing_disk_drops_the_oldest_and_keeps_going() {
    let captured = Captured::default();
    let _guard = captured.install();
    let scratch = ScratchDir::new("agent-upload");
    let flag = scratch.write("synchronized", "");
    let wall = TestWall::new();
    let network = Network::new();
    network.down();
    let (brokers_in, brokers) = mpsc::unbounded_channel();
    let cloud = Cloud::start(brokers, &network, wall.clone(), ingest());
    // The state directory does not exist, so the buffer directory cannot be made.
    let mut settings = every_interval(scratch.path().join("missing").join("buffer"));
    settings.memory_limit = 2 * 1024;
    let agent = start_agent(
        settings,
        SwitchConnector {
            network: network.clone(),
            brokers: brokers_in,
        },
        clock_with(&wall, &flag),
    );
    let generator = generate(&agent, 3, 2, 300);
    let offline = generator.finish().await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    let stats = agent.handles.stats.snapshot();
    assert!(stats.dropped_records > 0, "{stats:?}");
    assert_eq!(stats.disk_bytes, 0);
    assert_eq!(
        stats.pending_records + stats.dropped_records,
        offline.len() as u64,
        "every reading is either waiting or counted as dropped: {stats:?}"
    );
    let log = captured.text();
    assert!(log.contains("cannot open the buffer directory"), "{log}");
    assert!(
        log.contains("dropped buffered telemetry") && log.contains("reason=memory_full"),
        "{log}"
    );

    // Newer data keeps flowing: what is left is the newest, and it arrives.
    network.up();
    agent.wait_connected().await;
    let more = generate(&agent, 3, 2, 30).finish().await;
    until_every(Duration::from_secs(1), "the buffer to drain", || {
        agent.handles.stats.snapshot().pending_batches == 0
            && cloud.with(|cloud| cloud.committed_keys().len()) as u64
                >= stats.pending_records + more.len() as u64
    })
    .await;
    let committed: BTreeSet<Key> = cloud.with(|cloud| cloud.committed_keys().into_iter().collect());
    assert!(more.is_subset(&committed));
    let kept: Vec<&Key> = offline.intersection(&committed).collect();
    let dropped: Vec<&Key> = offline.difference(&committed).collect();
    assert_eq!(dropped.len() as u64, stats.dropped_records);
    let oldest_kept = kept.iter().map(|key| key.2).min().unwrap();
    assert!(
        dropped.iter().all(|key| key.2 < oldest_kept),
        "the oldest were dropped"
    );
    agent.stop().await;
}

/// Acceptance criterion 3, second half: the disk limit is reached: the oldest batches are
/// dropped and counted, nothing panics, and the newest data arrives.
#[tokio::test(start_paused = true)]
async fn a_full_disk_drops_the_oldest_and_keeps_going() {
    let captured = Captured::default();
    let _guard = captured.install();
    let scratch = ScratchDir::new("agent-upload");
    let flag = scratch.write("synchronized", "");
    let wall = TestWall::new();
    let network = Network::new();
    network.down();
    let (brokers_in, brokers) = mpsc::unbounded_channel();
    let cloud = Cloud::start(brokers, &network, wall.clone(), ingest());
    let buffer = scratch.path().join("buffer");
    let mut settings = every_interval(buffer.clone());
    settings.memory_limit = 4 * 1024;
    settings.disk_limit = 64 * 1024;
    let agent = start_agent(
        settings,
        SwitchConnector {
            network: network.clone(),
            brokers: brokers_in,
        },
        clock_with(&wall, &flag),
    );
    // About 5 KB per window: four hundred seconds are about 200 KB, more than both limits.
    let offline = generate(&agent, 10, 5, 400).finish().await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    let stats = agent.handles.stats.snapshot();
    assert!(stats.dropped_records > 0, "{stats:?}");
    assert!(
        stats.disk_bytes > 0 && stats.disk_bytes <= 64 * 1024,
        "{stats:?}"
    );
    let on_disk: u64 = segments(&buffer)
        .iter()
        .map(|path| std::fs::metadata(path).unwrap().len())
        .sum();
    assert_eq!(
        on_disk, stats.disk_bytes,
        "the files are what the buffer counts"
    );
    assert_eq!(
        stats.pending_records + stats.dropped_records,
        offline.len() as u64
    );
    let log = captured.text();
    assert!(log.contains("reason=disk_full"), "{log}");

    network.up();
    agent.wait_connected().await;
    until_every(Duration::from_secs(1), "the buffer to drain", || {
        agent.handles.stats.snapshot().pending_batches == 0
    })
    .await;
    let committed: BTreeSet<Key> = cloud.with(|cloud| cloud.committed_keys().into_iter().collect());
    let dropped: Vec<&Key> = offline.difference(&committed).collect();
    assert_eq!(dropped.len() as u64, stats.dropped_records);
    let oldest_kept = offline
        .intersection(&committed)
        .map(|key| key.2)
        .min()
        .unwrap();
    assert!(
        dropped.iter().all(|key| key.2 < oldest_kept),
        "the oldest were dropped"
    );
    assert!(segments(&buffer).is_empty());
    agent.stop().await;
}

/// Acceptance criterion 5, the batch age: the wall clock steps back ten days across a restart.
/// The batch on disk still goes after thirty days of real (monotonic) time, not forty; the
/// heartbeat counts it as dropped.
#[tokio::test(start_paused = true)]
async fn a_backward_clock_step_does_not_keep_a_batch_past_thirty_days() {
    let scratch = ScratchDir::new("agent-upload");
    let flag = scratch.write("synchronized", "");
    let wall = TestWall::new();
    let network = Network::new();
    network.down();
    let (brokers_in, _brokers) = mpsc::unbounded_channel();
    let buffer = scratch.path().join("buffer");
    let mut settings = every_interval(buffer.clone());
    settings.memory_limit = 0;
    let first = start_agent(
        settings.clone(),
        SwitchConnector {
            network: network.clone(),
            brokers: brokers_in.clone(),
        },
        clock_with(&wall, &flag),
    );
    let generated = generate(&first, 1, 1, 5).finish().await;
    until("the batch to be stored", || {
        first.handles.stats.snapshot().pending_batches == 1
    })
    .await;
    first.stop().await;
    assert_eq!(segments(&buffer).len(), 1);

    // The clock steps back ten days while the agent is down.
    wall.step(-i64::try_from((10 * DAY).as_millis()).unwrap());
    let second = start_agent(
        settings,
        SwitchConnector {
            network: network.clone(),
            brokers: brokers_in,
        },
        clock_with(&wall, &flag),
    );
    until("the batch to be loaded", || {
        second.handles.stats.snapshot().pending_batches == 1
    })
    .await;
    tokio::time::advance(30 * DAY - Duration::from_secs(120)).await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    let stats = second.handles.stats.snapshot();
    assert_eq!(
        (stats.pending_batches, stats.dropped_records),
        (1, 0),
        "not yet thirty days: {stats:?}"
    );
    // Thirty days and a minute after it was formed, in monotonic time: it goes at the next
    // window (every 10 s), not ten days later when the stepped-back wall clock gets there.
    tokio::time::advance(Duration::from_secs(120)).await;
    within(
        Duration::from_secs(30),
        "the batch to expire at thirty days",
        || second.handles.stats.snapshot().pending_batches == 0,
    )
    .await;
    let stats = second.handles.stats.snapshot();
    assert_eq!(stats.dropped_records, generated.len() as u64);
    assert_eq!(stats.disk_bytes, 0);
    assert!(segments(&buffer).is_empty());
    second.stop().await;
}

/// Acceptance criterion 5, the clock state: before any comparison with the cloud, timesyncd's
/// flag decides, and a change splits the window into one batch per state.
#[tokio::test(start_paused = true)]
async fn a_change_of_the_sync_state_splits_the_batch() {
    let scratch = ScratchDir::new("agent-upload");
    let flag = scratch.path().join("synchronized");
    let wall = TestWall::new();
    let network = Network::new();
    let (brokers_in, brokers) = mpsc::unbounded_channel();
    let cloud = Cloud::start(brokers, &network, wall.clone(), ingest());
    let agent = start_agent(
        Settings::new(scratch.path().join("buffer")),
        SwitchConnector {
            network: network.clone(),
            brokers: brokers_in,
        },
        clock_with(&wall, &flag),
    );
    agent.wait_connected().await;
    let generator = generate(&agent, 2, 2, 9);
    // timesyncd synchronizes after five readings, before the first window closes.
    tokio::time::sleep_until(wall.start + Duration::from_secs(5)).await;
    std::fs::write(&flag, "").unwrap();
    let generated = generator.finish().await;
    until("both batches", || {
        cloud.with(|cloud| cloud.committed.len() >= 2)
    })
    .await;
    let committed = cloud.with(|cloud| cloud.committed.clone());
    assert_eq!(
        committed
            .iter()
            .map(|batch| (batch.seq.get(), batch.clock_synced))
            .collect::<Vec<_>>(),
        [(1, false), (2, true)]
    );
    let unsynced = keys_of(&committed[0]);
    let synced = keys_of(&committed[1]);
    assert_eq!(unsynced.len(), 5 * 4);
    assert_eq!(synced.len(), 4 * 4);
    assert!(unsynced.iter().all(|key| key.2 < wall.start_ms() + 5_000));
    assert!(synced.iter().all(|key| key.2 > wall.start_ms() + 5_000));
    let all: BTreeSet<Key> = unsynced.into_iter().chain(synced).collect();
    assert_eq!(all, generated, "timestamps and values unchanged");
    agent.stop().await;
}

impl TestWall {
    fn start_ms(&self) -> u64 {
        START_MS
    }
}

/// Acceptance criterion 5, the cloud's time: an acknowledgement whose `server_time` is outside
/// the window around the batch turns `clock_synced` false for the batches and the heartbeat,
/// and a good one turns it back; the timestamps themselves are never changed.
#[tokio::test(start_paused = true)]
async fn the_clouds_time_decides_clock_synced_for_batches_and_heartbeats() {
    let scratch = ScratchDir::new("agent-upload");
    let flag = scratch.write("synchronized", "");
    let wall = TestWall::new();
    let network = Network::new();
    let (brokers_in, brokers) = mpsc::unbounded_channel();
    let offset = Arc::new(AtomicI64::new(0));
    let policy_offset = Arc::clone(&offset);
    let mut standard = ingest();
    let cloud = Cloud::start(
        brokers,
        &network,
        wall.clone(),
        Box::new(move |batch, new| Reply {
            server_offset_ms: policy_offset.load(Ordering::Relaxed),
            ..standard(batch, new)
        }),
    );
    let agent = start_agent(
        Settings::new(scratch.path().join("buffer")),
        SwitchConnector {
            network: network.clone(),
            brokers: brokers_in,
        },
        clock_with(&wall, &flag),
    );
    let heartbeats = tokio::spawn(agent::heartbeat::run(
        agent.link.clone(),
        agent::heartbeat::HeartbeatSettings {
            interval: Duration::from_secs(10),
            agent_version: AgentVersion::new("0.1.0-test").unwrap(),
            certificate_not_after: watch::channel(UnixMillis::new(START_MS + 365 * 86_400_000)).1,
            buffer: agent.handles.stats.clone(),
            clock: agent.clock.clone(),
        },
        Instant::now(),
        agent.shutdown.clone(),
    ));
    agent.wait_connected().await;
    let generator = generate(&agent, 1, 2, 75);
    let committed = |count: usize| {
        let cloud = &cloud;
        move || cloud.with(|cloud| cloud.committed.len() >= count)
    };
    until("two good batches", committed(2)).await;
    assert!(agent.clock.synced());
    // The cloud's clock is an hour ahead from now on: the third batch's ack shows the gateway's
    // clock an hour behind.
    offset.store(3_600_000, Ordering::Relaxed);
    until("the third batch", committed(3)).await;
    assert!(!agent.clock.synced(), "the ack was outside the window");
    let off_at = wall.now_ms();
    until("a heartbeat while the clock is off", || {
        cloud.with(|cloud| {
            cloud
                .heartbeats
                .iter()
                .any(|beat| beat.sent_at.get() > off_at)
        })
    })
    .await;
    let beat = cloud.with(|cloud| {
        cloud
            .heartbeats
            .iter()
            .find(|beat| beat.sent_at.get() > off_at)
            .cloned()
            .unwrap()
    });
    assert!(!beat.clock_synced, "{beat:?}");
    until("the fourth batch", committed(4)).await;
    // The cloud's clock agrees again from the fifth batch on.
    offset.store(0, Ordering::Relaxed);
    until("the fifth batch", committed(5)).await;
    assert!(agent.clock.synced());
    let back_at = wall.now_ms();
    until("the seventh batch", committed(7)).await;
    until("a heartbeat once the clock agrees again", || {
        cloud.with(|cloud| {
            cloud
                .heartbeats
                .iter()
                .any(|beat| beat.sent_at.get() > back_at)
        })
    })
    .await;
    let generated = generator.finish().await;
    until("every batch", || {
        cloud.with(|cloud| cloud.committed_keys().len() >= generated.len())
    })
    .await;
    let batches = cloud.with(|cloud| cloud.committed.clone());
    let states: Vec<bool> = batches.iter().map(|batch| batch.clock_synced).collect();
    // Batches 1-3 were read before the third ack, batches 4 and 5 between it and the fifth
    // ack, the rest after.
    assert_eq!(&states[..3], [true, true, true], "{states:?}");
    assert_eq!(&states[3..5], [false, false], "{states:?}");
    assert!(states[5..].iter().all(|synced| *synced), "{states:?}");
    let beat = cloud.with(|cloud| {
        cloud
            .heartbeats
            .iter()
            .find(|beat| beat.sent_at.get() > back_at)
            .cloned()
            .unwrap()
    });
    assert!(beat.clock_synced, "{beat:?}");
    assert!(beat.buffer.pending_records > 0 || beat.buffer.pending_batches == 0);
    let committed: BTreeSet<Key> = batches.iter().flat_map(keys_of).collect();
    assert_eq!(committed, generated, "timestamps are never rewritten");
    heartbeats.abort();
    agent.stop().await;
}

/// The ack-table tests: one connection the test answers packet by packet.
struct Manual {
    agent: Agent,
    brokers: mpsc::UnboundedReceiver<DuplexStream>,
    wall: TestWall,
    _scratch: ScratchDir,
    next_id: u16,
}

fn manual() -> Manual {
    manual_with(|_| {})
}

fn manual_with(adjust: impl FnOnce(&mut Settings)) -> Manual {
    let scratch = ScratchDir::new("agent-upload");
    let flag = scratch.write("synchronized", "");
    let wall = TestWall::new();
    let (connector, brokers) = support::DuplexConnector::new();
    let mut settings = every_interval(scratch.path().join("buffer"));
    adjust(&mut settings);
    let agent = start_agent(settings, connector, clock_with(&wall, &flag));
    Manual {
        agent,
        brokers,
        wall,
        _scratch: scratch,
        next_id: 1,
    }
}

impl Manual {
    /// One reading, stamped now.
    fn reading(&self, value: i64) {
        let now = agent::modbus::Clock::now(&self.agent.clock);
        self.agent.handles.readings.push([Reading {
            device: DeviceId::new("device-0").unwrap(),
            point: PointKey::new("point-0").unwrap(),
            sample: Sample {
                at: now.at,
                value: Value::Int(value),
            },
            clock_synced: now.synced,
        }]);
    }

    async fn broker(&mut self) -> FakeBroker<DuplexStream> {
        let stream = bounded("a connection", self.brokers.recv()).await.unwrap();
        let mut broker = FakeBroker::new(stream).subscribing(dispatch::subscriptions().channels());
        broker.accept().await;
        self.agent.wait_connected().await;
        broker
    }

    /// Readings for `batches` windows, one batch each.
    async fn feed(&self, batches: u64) {
        generate(&self.agent, 1, 1, batches * 10).finish().await;
    }

    async fn ack(
        &mut self,
        broker: &mut FakeBroker<DuplexStream>,
        stream: &StreamId,
        seq: Seq,
        outcome: AckOutcome,
    ) {
        let server_time = self.agent.clock.wall();
        self.ack_at(broker, stream, seq, outcome, server_time).await;
    }

    /// An acknowledgement whose `server_time` is `server_time`.
    async fn ack_at(
        &mut self,
        broker: &mut FakeBroker<DuplexStream>,
        stream: &StreamId,
        seq: Seq,
        outcome: AckOutcome,
        server_time: UnixMillis,
    ) {
        let ack = TelemetryAck {
            v: V1,
            stream: stream.clone(),
            seq,
            outcome,
            server_time,
        };
        broker
            .send(&publish(
                &Channel::TelemetryAck.topic(&gateway()),
                &protocol::encode(&ack).unwrap(),
                1,
                false,
                Some(self.next_id),
            ))
            .await;
        self.next_id += 1;
    }
}

/// The next telemetry PUBLISH, answering PINGREQs and skipping PUBACKs of the acks.
async fn next_batch(broker: &mut FakeBroker<DuplexStream>) -> (support::Publish, TelemetryBatch) {
    loop {
        match bounded("a telemetry batch", broker.read()).await {
            Some(Packet::Publish(message))
                if message.topic == Channel::Telemetry.topic(&gateway()) =>
            {
                let batch = TelemetryBatch::decode(&message.payload).unwrap();
                return (message, batch);
            }
            Some(Packet::PingReq) => broker.send(&pingresp()).await,
            Some(Packet::PubAck { .. }) => {}
            other => panic!("expected a telemetry batch, got {other:?}"),
        }
    }
}

/// Asserts that no telemetry PUBLISH comes within `quiet` (on the paused clock).
async fn nothing_for(broker: &mut FakeBroker<DuplexStream>, quiet: Duration) {
    let deadline = Instant::now() + quiet;
    loop {
        match tokio::time::timeout_at(deadline, broker.read()).await {
            Err(_) => return,
            Ok(Some(Packet::PingReq)) => broker.send(&pingresp()).await,
            Ok(Some(Packet::PubAck { .. })) => {}
            Ok(other) => panic!("expected nothing for {quiet:?}, got {other:?}"),
        }
    }
}

/// Acceptance criterion 6: every row of the acknowledgement table, on one connection.
#[tokio::test(start_paused = true)]
async fn every_acknowledgement_outcome_is_handled_as_the_table_says() {
    let mut harness = manual();
    let mut broker = harness.broker().await;
    harness.feed(6).await;
    let ack_timeout = upload::ACK_TIMEOUT;

    // committed: released, the next batch follows.
    let (message, first) = next_batch(&mut broker).await;
    assert_eq!(first.seq.get(), 1);
    broker.send(&puback(message.id.unwrap(), 0)).await;
    harness
        .ack(
            &mut broker,
            &first.stream,
            first.seq,
            AckOutcome::Committed { records: 10 },
        )
        .await;
    let (message, second) = next_batch(&mut broker).await;
    assert_eq!(
        (second.stream.clone(), second.seq.get()),
        (first.stream.clone(), 2)
    );
    let stream = first.stream.clone();

    // A mismatched ack (another seq, another stream) is ignored; with no ack the same bytes go
    // again after the acknowledgement timeout, not before.
    broker.send(&puback(message.id.unwrap(), 0)).await;
    let published = Instant::now();
    harness
        .ack(
            &mut broker,
            &stream,
            Seq::new(7).unwrap(),
            AckOutcome::Committed { records: 10 },
        )
        .await;
    harness
        .ack(
            &mut broker,
            &StreamId::new("other").unwrap(),
            second.seq,
            AckOutcome::Committed { records: 10 },
        )
        .await;
    let (again, resent) = next_batch(&mut broker).await;
    assert!(
        published.elapsed() >= ack_timeout,
        "{:?}",
        published.elapsed()
    );
    assert!(published.elapsed() < ack_timeout + Duration::from_secs(1));
    assert_eq!(again.payload, message.payload, "the same bytes");
    assert_eq!(resent.seq.get(), 2);

    // duplicate: released.
    broker.send(&puback(again.id.unwrap(), 0)).await;
    harness
        .ack(
            &mut broker,
            &stream,
            Seq::new(2).unwrap(),
            AckOutcome::Duplicate { records: 10 },
        )
        .await;
    let (message, third) = next_batch(&mut broker).await;
    assert_eq!(third.seq.get(), 3);

    // retry: kept, sent again after after_ms.
    broker.send(&puback(message.id.unwrap(), 0)).await;
    let asked = Instant::now();
    harness
        .ack(
            &mut broker,
            &stream,
            third.seq,
            AckOutcome::Retry { after_ms: 5_000 },
        )
        .await;
    let (again, resent) = next_batch(&mut broker).await;
    assert_eq!(resent.seq.get(), 3);
    assert_eq!(again.payload, message.payload);
    assert!(
        asked.elapsed() >= Duration::from_secs(5),
        "{:?}",
        asked.elapsed()
    );
    assert!(asked.elapsed() < Duration::from_secs(6));

    // An unknown status: kept, sent again after the acknowledgement timeout.
    broker.send(&puback(again.id.unwrap(), 0)).await;
    let asked = Instant::now();
    let unknown = minicbor_unknown_ack(&stream, 3, harness.agent.clock.wall());
    broker
        .send(&publish(
            &Channel::TelemetryAck.topic(&gateway()),
            &unknown,
            1,
            false,
            Some(harness.next_id),
        ))
        .await;
    harness.next_id += 1;
    let (again, resent) = next_batch(&mut broker).await;
    assert_eq!(resent.seq.get(), 3);
    assert!(asked.elapsed() >= ack_timeout, "{:?}", asked.elapsed());

    // rejected: released (not counted as dropped), the next batch follows.
    broker.send(&puback(again.id.unwrap(), 0)).await;
    harness
        .ack(
            &mut broker,
            &stream,
            Seq::new(3).unwrap(),
            AckOutcome::Rejected {
                code: IssueCode::InvalidValue,
                message: "the test rejects it".to_owned(),
            },
        )
        .await;
    let (message, fourth) = next_batch(&mut broker).await;
    assert_eq!(fourth.seq.get(), 4);
    assert_eq!(harness.agent.handles.stats.snapshot().dropped_records, 0);

    // PUBACK refused (0x87): kept, sent again after 1 s, then 2 s.
    broker.send(&puback(message.id.unwrap(), 0x87)).await;
    let refused = Instant::now();
    let (again, _) = next_batch(&mut broker).await;
    assert!(
        refused.elapsed() >= Duration::from_secs(1) && refused.elapsed() < Duration::from_secs(2)
    );
    assert_eq!(again.payload, message.payload);
    broker.send(&puback(again.id.unwrap(), 0x87)).await;
    let refused = Instant::now();
    let (again, _) = next_batch(&mut broker).await;
    assert!(
        refused.elapsed() >= Duration::from_secs(2) && refused.elapsed() < Duration::from_secs(3)
    );
    broker.send(&puback(again.id.unwrap(), 0)).await;
    harness
        .ack(
            &mut broker,
            &stream,
            fourth.seq,
            AckOutcome::Committed { records: 10 },
        )
        .await;

    // Connection lost after the PUBACK, before the ack: the same batch first on the next
    // connection.
    let (message, fifth) = next_batch(&mut broker).await;
    assert_eq!(fifth.seq.get(), 5);
    broker.send(&puback(message.id.unwrap(), 0)).await;
    drop(broker);
    let mut broker = harness.broker().await;
    let (again, resent) = next_batch(&mut broker).await;
    assert_eq!(
        (resent.seq.get(), again.payload.clone()),
        (5, message.payload)
    );

    // Connection lost before the PUBACK: the same again.
    drop(broker);
    let mut broker = harness.broker().await;
    let (again, resent) = next_batch(&mut broker).await;
    assert_eq!(resent.seq.get(), 5);
    broker.send(&puback(again.id.unwrap(), 0)).await;
    harness
        .ack(
            &mut broker,
            &stream,
            resent.seq,
            AckOutcome::Committed { records: 10 },
        )
        .await;
    let (message, sixth) = next_batch(&mut broker).await;
    assert_eq!(sixth.seq.get(), 6);
    broker.send(&puback(message.id.unwrap(), 0)).await;
    harness
        .ack(
            &mut broker,
            &stream,
            sixth.seq,
            AckOutcome::Committed { records: 10 },
        )
        .await;
    nothing_for(&mut broker, Duration::from_secs(30)).await;
    let stats = harness.agent.handles.stats.snapshot();
    assert_eq!(
        (stats.pending_batches, stats.dropped_records),
        (0, 0),
        "{stats:?}"
    );
    harness.agent.stop().await;
}

/// A `TelemetryAck` whose status a newer cloud might send: a CBOR map written by hand.
fn minicbor_unknown_ack(stream: &StreamId, seq: u64, server_time: UnixMillis) -> Vec<u8> {
    fn text(out: &mut Vec<u8>, value: &str) {
        assert!(value.len() < 24);
        out.push(0x60 + u8::try_from(value.len()).unwrap());
        out.extend_from_slice(value.as_bytes());
    }
    fn uint(out: &mut Vec<u8>, value: u64) {
        out.push(0x1B);
        out.extend_from_slice(&value.to_be_bytes());
    }
    let mut out = vec![0xA5];
    text(&mut out, "v");
    out.push(0x01);
    text(&mut out, "stream");
    let stream = stream.as_str();
    out.push(0x78);
    out.push(u8::try_from(stream.len()).unwrap());
    out.extend_from_slice(stream.as_bytes());
    text(&mut out, "seq");
    uint(&mut out, seq);
    text(&mut out, "outcome");
    out.push(0xA1);
    text(&mut out, "status");
    text(&mut out, "postponed");
    text(&mut out, "server_time");
    uint(&mut out, server_time.get());
    let decoded: TelemetryAck = protocol::decode(&out).unwrap();
    assert_eq!(decoded.outcome, AckOutcome::Unknown);
    out
}

/// Acceptance criterion 8: a corrupt record on disk is never sent (it is dropped and counted),
/// a torn one at the end of a segment neither, and files that are not the buffer's are left
/// alone.
#[tokio::test(start_paused = true)]
async fn a_corrupt_or_torn_record_is_never_sent_and_unknown_files_stay() {
    let scratch = ScratchDir::new("agent-upload");
    let flag = scratch.write("synchronized", "");
    let wall = TestWall::new();
    let network = Network::new();
    network.down();
    let (brokers_in, brokers) = mpsc::unbounded_channel();
    let buffer = scratch.path().join("buffer");
    let mut settings = every_interval(buffer.clone());
    settings.memory_limit = 0;
    let first = start_agent(
        settings.clone(),
        SwitchConnector {
            network: network.clone(),
            brokers: brokers_in.clone(),
        },
        clock_with(&wall, &flag),
    );
    // Three windows; the second carries a text value the test finds on disk.
    let generated = generate_with(&first, 1, 2, 30, |second| {
        (10..20)
            .contains(&second)
            .then(|| format!("corrupt-me-{second:02}"))
    })
    .finish()
    .await;
    until("three batches on disk", || {
        first.handles.stats.snapshot().pending_batches == 3
    })
    .await;
    first.stop().await;
    let files = segments(&buffer);
    assert_eq!(files.len(), 1);
    let mut bytes = std::fs::read(&files[0]).unwrap();
    let marker = b"corrupt-me-15";
    let at = bytes
        .windows(marker.len())
        .position(|window| window == marker)
        .unwrap();
    bytes[at] ^= 0x20;
    // A torn record after the last one, as a crash in the middle of a write leaves it.
    bytes.extend_from_slice(&[0x52, 0x45, 0x43, 0x31, 0, 1, 0]);
    std::fs::write(&files[0], &bytes).unwrap();
    std::fs::write(buffer.join("notes.txt"), "an operator's notes").unwrap();

    network.up();
    let cloud = Cloud::start(brokers, &network, wall.clone(), ingest());
    let second = start_agent(
        settings,
        SwitchConnector {
            network: network.clone(),
            brokers: brokers_in,
        },
        clock_with(&wall, &flag),
    );
    second.wait_connected().await;
    until_every(Duration::from_secs(1), "the buffer to drain", || {
        let stats = second.handles.stats.snapshot();
        stats.pending_batches == 0 && stats.dropped_records > 0
    })
    .await;
    let committed = cloud.with(|cloud| cloud.committed.clone());
    let seqs: Vec<u64> = committed.iter().map(|batch| batch.seq.get()).collect();
    assert_eq!(seqs, [1, 3], "the corrupt batch is never sent");
    assert!(cloud.with(|cloud| cloud.received.iter().all(|(_, seq)| *seq != 2)));
    let committed_keys: BTreeSet<Key> = committed.iter().flat_map(keys_of).collect();
    assert!(committed_keys.is_subset(&generated));
    let stats = second.handles.stats.snapshot();
    assert_eq!(
        stats.dropped_records, 20,
        "the corrupt batch's readings count as dropped"
    );
    assert_eq!(
        std::fs::read_to_string(buffer.join("notes.txt")).unwrap(),
        "an operator's notes"
    );
    assert!(segments(&buffer).is_empty());
    second.stop().await;
}

/// Codex review before the commit (2026-10-28): batches found on disk were sent before their
/// age was checked, because expiry ran only when a window closed, an upload interval after the
/// start. A gateway back after thirty-one days, with the cloud reachable from the start, sends
/// none of its expired backlog: the batches are dropped and counted, and new readings flow.
#[tokio::test(start_paused = true)]
async fn an_expired_backlog_found_at_start_is_never_sent() {
    let scratch = ScratchDir::new("agent-upload");
    let flag = scratch.write("synchronized", "");
    let wall = TestWall::new();
    let network = Network::new();
    network.down();
    let (brokers_in, brokers) = mpsc::unbounded_channel();
    let buffer = scratch.path().join("buffer");
    let mut settings = every_interval(buffer.clone());
    settings.memory_limit = 0;
    let first = start_agent(
        settings.clone(),
        SwitchConnector {
            network: network.clone(),
            brokers: brokers_in.clone(),
        },
        clock_with(&wall, &flag),
    );
    let backlog = generate(&first, 1, 2, 30).finish().await;
    until("three batches on disk", || {
        first.handles.stats.snapshot().pending_batches == 3
    })
    .await;
    first.stop().await;

    // The gateway was off for thirty-one days; the cloud is reachable as soon as it starts.
    wall.step(i64::try_from((31 * DAY).as_millis()).unwrap());
    network.up();
    let cloud = Cloud::start(brokers, &network, wall.clone(), ingest());
    let second = start_agent(
        settings,
        SwitchConnector {
            network: network.clone(),
            brokers: brokers_in,
        },
        clock_with(&wall, &flag),
    );
    second.wait_connected().await;
    within(
        Duration::from_secs(5),
        "the expired backlog to be dropped",
        || {
            let stats = second.handles.stats.snapshot();
            stats.pending_batches == 0 && stats.dropped_records == backlog.len() as u64
        },
    )
    .await;
    let fresh = generate(&second, 1, 2, 10).finish().await;
    until("the new readings to be committed", || {
        cloud.with(|cloud| cloud.committed_keys().len()) >= fresh.len()
    })
    .await;
    let (committed, received) =
        cloud.with(|cloud| (cloud.committed.clone(), cloud.received.clone()));
    assert_eq!(
        received.len(),
        committed.len(),
        "nothing else was published"
    );
    let sent: BTreeSet<Key> = committed.iter().flat_map(keys_of).collect();
    assert_eq!(sent, fresh, "only the new readings, none of the backlog");
    assert!(segments(&buffer).is_empty());
    second.stop().await;
}

/// Codex review before the commit (2026-10-28): a batch in flight that crosses thirty days on
/// the age clock while it waits for its acknowledgement, or for a `retry`, is not sent again.
/// The windows here close only when a reading arrives (a 40-day interval, a window limit of one
/// reading), so no window's expiry can remove the batch first.
#[tokio::test(start_paused = true)]
async fn a_batch_in_flight_that_crosses_thirty_days_is_not_sent_again() {
    let mut harness = manual_with(|settings| {
        settings.upload_interval = 40 * DAY;
        settings.window_limit = 1;
    });
    let mut broker = harness.broker().await;

    // Waiting for the acknowledgement: its timeout ends after thirty days.
    harness.reading(1);
    let (message, first) = next_batch(&mut broker).await;
    broker.send(&puback(message.id.unwrap(), 0)).await;
    nothing_for(&mut broker, Duration::from_secs(1)).await;
    tokio::time::advance(30 * DAY + Duration::from_secs(120)).await;
    nothing_for(&mut broker, Duration::from_secs(120)).await;
    let stats = harness.agent.handles.stats.snapshot();
    assert_eq!(
        (stats.pending_batches, stats.dropped_records),
        (0, 1),
        "{stats:?}"
    );

    // Waiting to retry: the retry comes due after thirty days.
    harness.reading(2);
    let (message, second) = next_batch(&mut broker).await;
    assert_eq!(
        (second.stream.clone(), second.seq.get()),
        (first.stream.clone(), first.seq.get() + 1)
    );
    broker.send(&puback(message.id.unwrap(), 0)).await;
    harness
        .ack(
            &mut broker,
            &second.stream,
            second.seq,
            AckOutcome::Retry { after_ms: 600_000 },
        )
        .await;
    nothing_for(&mut broker, Duration::from_secs(1)).await;
    tokio::time::advance(30 * DAY + Duration::from_secs(60)).await;
    nothing_for(&mut broker, Duration::from_secs(900)).await;
    let stats = harness.agent.handles.stats.snapshot();
    assert_eq!(
        (stats.pending_batches, stats.dropped_records),
        (0, 2),
        "{stats:?}"
    );
    harness.agent.stop().await;
}

/// The same when the buffer directory opens only later (it cannot be opened at the start and
/// is tried again every minute): its expired backlog is dropped as soon as it is loaded, and
/// none of it is ever sent, while new readings flow from memory meanwhile.
#[tokio::test(start_paused = true)]
async fn an_expired_backlog_found_when_the_disk_opens_late_is_never_sent() {
    let scratch = ScratchDir::new("agent-upload");
    let flag = scratch.write("synchronized", "");
    let wall = TestWall::new();
    let network = Network::new();
    network.down();
    let (brokers_in, brokers) = mpsc::unbounded_channel();
    let state = scratch.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let mut settings = every_interval(state.join("buffer"));
    settings.memory_limit = 0;
    let first = start_agent(
        settings.clone(),
        SwitchConnector {
            network: network.clone(),
            brokers: brokers_in.clone(),
        },
        clock_with(&wall, &flag),
    );
    let backlog = generate(&first, 1, 2, 30).finish().await;
    until("three batches on disk", || {
        first.handles.stats.snapshot().pending_batches == 3
    })
    .await;
    first.stop().await;

    // Thirty-one days later the state directory is not there yet when the agent starts.
    let away = scratch.path().join("state-away");
    std::fs::rename(&state, &away).unwrap();
    wall.step(i64::try_from((31 * DAY).as_millis()).unwrap());
    network.up();
    let cloud = Cloud::start(brokers, &network, wall.clone(), ingest());
    settings.memory_limit = upload::DEFAULT_MEMORY_LIMIT;
    let second = start_agent(
        settings,
        SwitchConnector {
            network: network.clone(),
            brokers: brokers_in,
        },
        clock_with(&wall, &flag),
    );
    second.wait_connected().await;
    let fresh = generate(&second, 1, 2, 10).finish().await;
    until("the new readings to be committed", || {
        cloud.with(|cloud| cloud.committed_keys().len()) >= fresh.len()
    })
    .await;
    std::fs::rename(&away, &state).unwrap();
    within(
        Duration::from_secs(90),
        "the backlog to be loaded and dropped",
        || second.handles.stats.snapshot().dropped_records == backlog.len() as u64,
    )
    .await;
    tokio::time::sleep(Duration::from_secs(30)).await;
    let (committed, received) =
        cloud.with(|cloud| (cloud.committed.clone(), cloud.received.clone()));
    assert_eq!(
        received.len(),
        committed.len(),
        "nothing else was published"
    );
    let sent: BTreeSet<Key> = committed.iter().flat_map(keys_of).collect();
    assert_eq!(sent, fresh, "only the new readings, none of the backlog");
    assert_eq!(second.handles.stats.snapshot().pending_batches, 0);
    second.stop().await;
}

/// Independent review of Laysi/trustiot-v2#90 (2026-10-28), finding 3: while the link is down
/// the collection window closes every five minutes rather than every upload interval, so six
/// hours offline with a reading every second become about 72 batches instead of 2,160, and
/// catching up takes as many round trips. With the link back and no backlog, the window closes
/// every upload interval again.
#[tokio::test(start_paused = true)]
async fn a_long_outage_forms_one_batch_per_five_minutes() {
    let scratch = ScratchDir::new("agent-upload");
    let flag = scratch.write("synchronized", "");
    let wall = TestWall::new();
    let network = Network::new();
    network.down();
    let (brokers_in, brokers) = mpsc::unbounded_channel();
    let cloud = Cloud::start(brokers, &network, wall.clone(), ingest());
    let agent = start_agent(
        Settings::new(scratch.path().join("buffer")),
        SwitchConnector {
            network: network.clone(),
            brokers: brokers_in,
        },
        clock_with(&wall, &flag),
    );
    let generated = generate(&agent, 1, 1, 6 * 3600).finish().await;
    let offline = agent.handles.stats.snapshot();
    // A window closes at the first tick five minutes after the last one closed: 71 or 72 in
    // six hours, depending on whether the last tick came before the last reading.
    assert!(
        (71..=72).contains(&offline.pending_batches),
        "six hours offline: {offline:?}"
    );
    assert_eq!(offline.dropped_records, 0);

    network.up();
    until_every(
        Duration::from_secs(1),
        "every reading to be committed",
        || {
            cloud.with(|cloud| cloud.committed_keys().len()) >= generated.len()
                && agent.handles.stats.snapshot().pending_batches == 0
        },
    )
    .await;
    let (committed, received) = cloud.with(|cloud| (cloud.committed_keys(), cloud.received.len()));
    assert_eq!(committed.len(), generated.len(), "no reading twice");
    assert_eq!(committed.into_iter().collect::<BTreeSet<_>>(), generated);
    assert!(received <= 74, "{received} publishes to catch up six hours");

    // Back online with nothing waiting: a batch every upload interval again.
    let before = cloud.with(|cloud| cloud.committed.len());
    let more = generate(&agent, 1, 1, 60).finish().await;
    until("the minute's readings to be committed", || {
        let keys: BTreeSet<Key> = cloud.with(|cloud| cloud.committed_keys().into_iter().collect());
        more.is_subset(&keys)
    })
    .await;
    let batches = cloud.with(|cloud| cloud.committed.len()) - before;
    assert!(
        (5..=7).contains(&batches),
        "a minute online: {batches} batches"
    );
    agent.stop().await;
}

/// Independent review, finding 7: the gateway's clock is an hour ahead. A batch goes out on one
/// connection, which drops after the PUBACK; two hours later it goes again on a new connection
/// and the cloud acknowledges it at once. The ack can only answer the publish on its own
/// connection, so the window starts there and the clock is found off; counted from the first
/// publish, two hours earlier, the cloud's time would fall inside it.
#[tokio::test(start_paused = true)]
async fn a_clock_an_hour_ahead_is_found_off_when_the_ack_answers_a_publish_on_a_new_connection() {
    const HOUR_MS: u64 = 3_600_000;
    let mut harness = manual();
    harness.wall.step(i64::try_from(HOUR_MS).unwrap());
    let mut broker = harness.broker().await;
    harness.feed(1).await;
    let (message, batch) = next_batch(&mut broker).await;
    broker.send(&puback(message.id.unwrap(), 0)).await;
    assert!(
        harness.agent.clock.synced(),
        "timesyncd decides before the first comparison"
    );
    drop(broker);
    harness.wall.step(i64::try_from(2 * HOUR_MS).unwrap());
    let mut broker = harness.broker().await;
    let (again, resent) = next_batch(&mut broker).await;
    assert_eq!(
        (resent.stream.clone(), resent.seq),
        (batch.stream.clone(), batch.seq)
    );
    broker.send(&puback(again.id.unwrap(), 0)).await;
    // The cloud's clock is right: an hour behind the gateway's.
    let server_time = UnixMillis::new(harness.agent.clock.wall().get() - HOUR_MS);
    harness
        .ack_at(
            &mut broker,
            &batch.stream,
            batch.seq,
            AckOutcome::Committed { records: 10 },
            server_time,
        )
        .await;
    within(Duration::from_secs(5), "the batch to be released", || {
        harness.agent.handles.stats.snapshot().pending_batches == 0
    })
    .await;
    assert!(
        !harness.agent.clock.synced(),
        "a clock an hour ahead counts as not synchronized"
    );
    harness.agent.stop().await;
}

/// Codex review before the commit (tenth round): a publish the broker refuses never reaches
/// the cloud, so its time must not widen the window the acknowledgement is compared with. The
/// gateway's clock is 5 s ahead of the cloud's; the broker refuses the batch three times (the
/// uploader waits 1, 2 and 4 s) and accepts the fourth publish on the same connection. The
/// cloud's time in the ack lies 5 s before that publish, outside the 2 s margin: the clock
/// counts as not synchronized. Compared from the first, refused, publish 7 s earlier, it
/// counted as synchronized.
#[tokio::test(start_paused = true)]
async fn a_refused_publish_does_not_widen_the_window_of_the_clock_comparison() {
    const AHEAD_MS: u64 = 5_000;
    let mut harness = manual();
    let mut broker = harness.broker().await;
    harness.feed(1).await;
    for _ in 0..3 {
        let (message, _) = next_batch(&mut broker).await;
        broker.send(&puback(message.id.unwrap(), 0x87)).await;
    }
    let (message, batch) = next_batch(&mut broker).await;
    broker.send(&puback(message.id.unwrap(), 0)).await;
    assert!(
        harness.agent.clock.synced(),
        "timesyncd decides before the first comparison"
    );
    // The cloud's clock is right: 5 s behind the gateway's.
    let server_time = UnixMillis::new(harness.agent.clock.wall().get() - AHEAD_MS);
    harness
        .ack_at(
            &mut broker,
            &batch.stream,
            batch.seq,
            AckOutcome::Committed { records: 10 },
            server_time,
        )
        .await;
    within(Duration::from_secs(5), "the batch to be released", || {
        harness.agent.handles.stats.snapshot().pending_batches == 0
    })
    .await;
    assert!(
        !harness.agent.clock.synced(),
        "a clock 5 s ahead counts as not synchronized, whatever the broker refused before"
    );
    harness.agent.stop().await;
}

/// Codex review before the commit (after the move to main `371572a`): an acknowledgement of the
/// batch can be handled before its send has ended, that is, before the uploader has recorded the
/// publish the broker accepted. It used to release the batch at once without comparing the
/// cloud's time. The gateway's clock is an hour ahead and timesyncd's flag says synchronized; the
/// cloud's acknowledgement (with the true time) comes before the broker's PUBACK. The batch stays
/// until the PUBACK ends the send, and then the acknowledgement is compared: the clock counts as
/// not synchronized.
#[tokio::test(start_paused = true)]
async fn an_acknowledgement_before_the_puback_waits_for_the_send_and_is_compared() {
    const HOUR_MS: u64 = 3_600_000;
    let mut harness = manual();
    harness.wall.step(i64::try_from(HOUR_MS).unwrap());
    let mut broker = harness.broker().await;
    harness.feed(1).await;
    let (message, batch) = next_batch(&mut broker).await;
    assert!(
        harness.agent.clock.synced(),
        "timesyncd decides before the first comparison"
    );
    // The cloud's clock is right: an hour behind the gateway's.
    let server_time = UnixMillis::new(harness.agent.clock.wall().get() - HOUR_MS);
    harness
        .ack_at(
            &mut broker,
            &batch.stream,
            batch.seq,
            AckOutcome::Committed { records: 10 },
            server_time,
        )
        .await;
    // The paused clock moves only once every task is idle, so the acknowledgement has reached
    // the uploader by the end of this.
    nothing_for(&mut broker, Duration::from_secs(1)).await;
    assert_eq!(
        harness.agent.handles.stats.snapshot().pending_batches,
        1,
        "the batch is kept while its send waits for the PUBACK"
    );
    broker.send(&puback(message.id.unwrap(), 0)).await;
    within(Duration::from_secs(5), "the batch to be released", || {
        harness.agent.handles.stats.snapshot().pending_batches == 0
    })
    .await;
    assert!(
        !harness.agent.clock.synced(),
        "the acknowledgement is compared once the send ends: a clock an hour ahead is off"
    );
    nothing_for(&mut broker, Duration::from_secs(30)).await;
    harness.agent.stop().await;
}

/// Like [`next_batch`], keeping the heartbeats that come first.
async fn next_batch_keeping(
    broker: &mut FakeBroker<DuplexStream>,
    heartbeats: &mut Vec<Heartbeat>,
) -> (support::Publish, TelemetryBatch) {
    loop {
        match bounded("a telemetry batch", broker.read()).await {
            Some(Packet::Publish(message))
                if message.topic == Channel::Telemetry.topic(&gateway()) =>
            {
                let batch = TelemetryBatch::decode(&message.payload).unwrap();
                return (message, batch);
            }
            Some(Packet::Publish(message))
                if message.topic == Channel::Heartbeat.topic(&gateway()) =>
            {
                heartbeats.push(Heartbeat::decode(&message.payload).unwrap());
            }
            Some(Packet::PingReq) => broker.send(&pingresp()).await,
            Some(Packet::PubAck { .. }) => {}
            other => panic!("expected a telemetry batch, got {other:?}"),
        }
    }
}

/// Like [`nothing_for`], keeping the heartbeats.
async fn no_batch_for(
    broker: &mut FakeBroker<DuplexStream>,
    quiet: Duration,
    heartbeats: &mut Vec<Heartbeat>,
) {
    let deadline = Instant::now() + quiet;
    loop {
        match tokio::time::timeout_at(deadline, broker.read()).await {
            Err(_) => return,
            Ok(Some(Packet::Publish(message)))
                if message.topic == Channel::Heartbeat.topic(&gateway()) =>
            {
                heartbeats.push(Heartbeat::decode(&message.payload).unwrap());
            }
            Ok(Some(Packet::PingReq)) => broker.send(&pingresp()).await,
            Ok(Some(Packet::PubAck { .. })) => {}
            Ok(other) => panic!("expected nothing for {quiet:?}, got {other:?}"),
        }
    }
}

/// The first heartbeat sent after `after_ms` on the gateway's clock, reading until it comes.
async fn heartbeat_after(
    broker: &mut FakeBroker<DuplexStream>,
    after_ms: u64,
    heartbeats: &mut Vec<Heartbeat>,
) -> Heartbeat {
    loop {
        if let Some(beat) = heartbeats.iter().find(|beat| beat.sent_at.get() > after_ms) {
            return beat.clone();
        }
        match bounded("a heartbeat", broker.read()).await {
            Some(Packet::Publish(message))
                if message.topic == Channel::Heartbeat.topic(&gateway()) =>
            {
                heartbeats.push(Heartbeat::decode(&message.payload).unwrap());
            }
            Some(Packet::PingReq) => broker.send(&pingresp()).await,
            Some(Packet::PubAck { .. }) => {}
            other => panic!("expected a heartbeat, got {other:?}"),
        }
    }
}

/// Codex review before the commit (after the Laysi/trustiot-v2#94 integration): the window an
/// acknowledgement is compared with spans the whole round trip, so a slow one let a clock that
/// is off by less than the round trip count as synchronized. The gateway's clock is 20 s ahead
/// and timesyncd has not synchronized it; the cloud answers 30 s after the publish with the
/// true time, which lies inside the 34 s window. Over [`SYNC_ROUND_TRIP_LIMIT`], the ack is not
/// compared: the next batch and the heartbeat stay not synchronized. Controls on the same
/// connection: with timesyncd's flag present (so that only a comparison can say otherwise), a
/// prompt ack finds the 20 s-ahead clock off; with the flag gone and the clock right, a prompt
/// ack finds it synchronized.
///
/// [`SYNC_ROUND_TRIP_LIMIT`]: agent::clock::SYNC_ROUND_TRIP_LIMIT
#[tokio::test(start_paused = true)]
async fn a_slow_acknowledgement_never_judges_a_clock_20_s_ahead_synchronized() {
    const AHEAD_MS: u64 = 20_000;
    const SLOW: Duration = Duration::from_secs(30);
    assert!(SLOW > agent::clock::SYNC_ROUND_TRIP_LIMIT && SLOW < upload::ACK_TIMEOUT);
    let scratch = ScratchDir::new("agent-upload");
    let flag = scratch.path().join("synchronized");
    let wall = TestWall::new();
    wall.step(i64::try_from(AHEAD_MS).unwrap());
    let (connector, brokers) = support::DuplexConnector::new();
    let agent = start_agent(
        every_interval(scratch.path().join("buffer")),
        connector,
        clock_with(&wall, &flag),
    );
    let mut harness = Manual {
        agent,
        brokers,
        wall,
        _scratch: scratch,
        next_id: 1,
    };
    let heartbeat = tokio::spawn(agent::heartbeat::run(
        harness.agent.link.clone(),
        agent::heartbeat::HeartbeatSettings {
            interval: Duration::from_secs(10),
            agent_version: AgentVersion::new("0.1.0-test").unwrap(),
            certificate_not_after: watch::channel(UnixMillis::new(START_MS + 365 * 86_400_000)).1,
            buffer: harness.agent.handles.stats.clone(),
            clock: harness.agent.clock.clone(),
        },
        Instant::now(),
        harness.agent.shutdown.clone(),
    ));
    let mut broker = harness.broker().await;
    let mut beats = Vec::new();
    // The true time: the gateway's clock less the 20 s it is ahead.
    let truth = |harness: &Manual| UnixMillis::new(harness.agent.clock.wall().get() - AHEAD_MS);

    harness.reading(1);
    let (message, first) = next_batch_keeping(&mut broker, &mut beats).await;
    assert!(!first.clock_synced, "no timesyncd flag, nothing compared");
    broker.send(&puback(message.id.unwrap(), 0)).await;
    no_batch_for(&mut broker, SLOW, &mut beats).await;
    let server_time = truth(&harness);
    harness
        .ack_at(
            &mut broker,
            &first.stream,
            first.seq,
            AckOutcome::Committed { records: 1 },
            server_time,
        )
        .await;
    within(
        Duration::from_secs(5),
        "the first batch to be released",
        || harness.agent.handles.stats.snapshot().pending_batches == 0,
    )
    .await;
    assert!(
        !harness.agent.clock.synced(),
        "an ack 30 s after the publish is not compared, so it cannot judge a clock 20 s ahead synchronized"
    );
    let acked_at = harness.wall.now_ms();
    let beat = heartbeat_after(&mut broker, acked_at, &mut beats).await;
    assert!(!beat.clock_synced, "{beat:?}");
    harness.reading(2);
    let (message, second) = next_batch_keeping(&mut broker, &mut beats).await;
    assert!(!second.clock_synced, "the slow ack was not compared");
    broker.send(&puback(message.id.unwrap(), 0)).await;

    // Control: the same clock, a prompt ack. timesyncd's flag is there now, so only the
    // comparison can make the clock count as not synchronized.
    std::fs::write(&flag, "").unwrap();
    assert!(
        harness.agent.clock.synced(),
        "nothing has been compared yet: the flag decides"
    );
    let server_time = truth(&harness);
    harness
        .ack_at(
            &mut broker,
            &second.stream,
            second.seq,
            AckOutcome::Committed { records: 1 },
            server_time,
        )
        .await;
    within(
        Duration::from_secs(5),
        "the second batch to be released",
        || harness.agent.handles.stats.snapshot().pending_batches == 0,
    )
    .await;
    assert!(
        !harness.agent.clock.synced(),
        "a prompt ack finds the clock 20 s ahead"
    );

    // Control: the clock is right and the flag is gone, so only the comparison can make the
    // clock count as synchronized.
    std::fs::remove_file(&flag).unwrap();
    harness.wall.step(-i64::try_from(AHEAD_MS).unwrap());
    harness.reading(3);
    let (message, third) = next_batch_keeping(&mut broker, &mut beats).await;
    assert!(!third.clock_synced, "read while the clock was found off");
    broker.send(&puback(message.id.unwrap(), 0)).await;
    let server_time = harness.agent.clock.wall();
    harness
        .ack_at(
            &mut broker,
            &third.stream,
            third.seq,
            AckOutcome::Committed { records: 1 },
            server_time,
        )
        .await;
    within(
        Duration::from_secs(5),
        "the third batch to be released",
        || harness.agent.handles.stats.snapshot().pending_batches == 0,
    )
    .await;
    assert!(
        harness.agent.clock.synced(),
        "a prompt ack finds the right clock synchronized"
    );
    heartbeat.abort();
    harness.agent.stop().await;
}
