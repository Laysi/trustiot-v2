//! The systemd watchdog follows the connection task, not only its thread
//! (Laysi/trustiot-v2#86): every wait of the real connection path is declared with the latest
//! moment it may end, a long but legitimate wait keeps the watchdog fed, and a task stuck in a
//! wait stops the notifications although its thread keeps counting ticks.
//!
//! Every test runs on a paused clock, like `tests/link.rs`.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod support;

use std::future::Future;
use std::os::unix::net::UnixDatagram;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use agent::backoff::BackoffSettings;
use agent::link::{
    self, ConnectError, Connector, INCOMING_QUEUE, LinkSettings, Published, Subscriptions,
};
use agent::liveness::{self, DELIVERY_LIMIT, Liveness, Phase, Progress, SLACK};
use agent::sdnotify::{Notifier, PROGRESS_TICK};
use agent::tpm::RESPONSE_TIMEOUT;
use protocol::AgentVersion;
use protocol::topic::Channel;
use support::{
    DuplexConnector, FakeBroker, GATEWAY, Packet, connack, disconnect, gateway, puback, publish,
    suback,
};
use testkit::clock::bounded;
use testkit::mqtt::ScratchDir;
use tokio::io::DuplexStream;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// The default settings, with every backoff delay the longest of 60 s and a subscription to
/// `log-request`.
fn settings() -> LinkSettings {
    LinkSettings {
        subscriptions: Subscriptions::new([Channel::LogRequest]).unwrap(),
        reconnect: BackoffSettings {
            base: Duration::from_secs(60),
            cap: Duration::from_secs(60),
        },
        jitter: || u64::MAX,
        ..LinkSettings::new(gateway(), AgentVersion::new("0.1.0-test").unwrap())
    }
}

/// Waits until the connection task declares `phase`.
async fn until_phase(progress: &Progress, phase: Phase) {
    bounded(format!("the phase {phase:?}"), async {
        while progress.phase() != phase {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

/// The wait declared now is `phase`, allowed exactly `allowed` from now: still alive at the
/// deadline plus SLACK, stuck just after.
fn assert_allowed(progress: &Progress, phase: Phase, allowed: Duration) {
    let now = Instant::now();
    assert_eq!(progress.phase(), phase);
    assert_eq!(
        progress.check(now + allowed + SLACK),
        Liveness::Alive,
        "{phase:?} at its deadline plus SLACK"
    );
    assert_eq!(
        progress.check(now + allowed + SLACK + Duration::from_millis(1)),
        Liveness::Overstayed {
            phase,
            lasted: allowed + SLACK + Duration::from_millis(1),
            allowed
        },
        "{phase:?} just after its deadline plus SLACK"
    );
}

/// A connector that never opens a stream: TCP, a TLS handshake or the WebSocket upgrade stuck.
struct HangingConnector;

impl Connector for HangingConnector {
    type Stream = DuplexStream;

    async fn connect(&self) -> Result<DuplexStream, ConnectError> {
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn opening_a_connection_may_take_the_connect_timeout_and_one_tpm_response() {
    let shutdown = CancellationToken::new();
    let (_link, _incoming, supervisor) = link::link(settings(), HangingConnector, shutdown.clone());
    let progress = supervisor.progress();
    let task = tokio::spawn(supervisor.run());
    until_phase(&progress, Phase::Connecting).await;
    // A TPM signature blocks the thread, so the 30 s timeout may fire that much later.
    assert_allowed(
        &progress,
        Phase::Connecting,
        Duration::from_secs(30) + RESPONSE_TIMEOUT,
    );
    shutdown.cancel();
    bounded("the supervisor", task).await.unwrap();
    assert_eq!(progress.phase(), Phase::Stopped);
    assert_eq!(
        progress.check(Instant::now() + Duration::from_secs(86_400)),
        Liveness::Alive,
        "a stopped task waits for nothing"
    );
}

/// Each wait along a connection's life, with the deadline the connection itself enforces.
#[tokio::test(start_paused = true)]
async fn every_wait_of_a_connection_is_declared_with_its_deadline() {
    let shutdown = CancellationToken::new();
    let (connector, mut brokers) = DuplexConnector::new();
    let (link, mut incoming, supervisor) = link::link(settings(), connector, shutdown.clone());
    let progress = supervisor.progress();
    let task = tokio::spawn(supervisor.run());

    let mut broker = FakeBroker::new(bounded("a connection", brokers.recv()).await.unwrap());
    let Some(Packet::Connect(_)) = broker.read().await else {
        panic!("expected CONNECT")
    };
    until_phase(&progress, Phase::AwaitConnack).await;
    assert_allowed(&progress, Phase::AwaitConnack, Duration::from_secs(30));

    broker.send(&connack(0, &[])).await;
    let Some(Packet::Publish(online)) = broker.read().await else {
        panic!("expected online")
    };
    until_phase(&progress, Phase::SettingUp).await;
    // The next timer is the PUBACK's deadline, a keepalive after the write.
    assert_allowed(&progress, Phase::SettingUp, Duration::from_secs(60));
    broker.send(&puback(online.id.unwrap(), 0)).await;
    let Some(Packet::Subscribe { id, filters }) = broker.read().await else {
        panic!("expected SUBSCRIBE")
    };
    assert_eq!(filters, [(Channel::LogRequest.topic(&gateway()), 1)]);
    until_phase(&progress, Phase::SettingUp).await;
    // The next timer is the PINGREQ, a keepalive after the last packet.
    assert_allowed(&progress, Phase::SettingUp, Duration::from_secs(60));

    broker.send(&suback(id, &vec![1; filters.len()])).await;
    until_phase(&progress, Phase::Connected).await;
    assert_allowed(&progress, Phase::Connected, Duration::from_secs(60));

    // The incoming queue full: the wait for the application has its own limit, which writes
    // in between (a PINGREQ, a publish) do not extend.
    let requests = Channel::LogRequest.topic(&gateway());
    let count = u16::try_from(INCOMING_QUEUE).unwrap() + 1;
    for id in 1..=count {
        broker
            .send(&publish(&requests, &id.to_be_bytes(), 1, false, Some(id)))
            .await;
    }
    for id in 1..count {
        assert_eq!(broker.read().await, Some(Packet::PubAck { id, reason: 0 }));
    }
    until_phase(&progress, Phase::Delivering).await;
    let delivering = Instant::now();
    assert_allowed(&progress, Phase::Delivering, DELIVERY_LIMIT);
    tokio::time::sleep(Duration::from_secs(100)).await;
    assert_eq!(broker.read().await, Some(Packet::PingReq));
    assert_eq!(
        bounded("a live message", link.publish(Channel::Live, vec![1])).await,
        Ok(Published::Sent)
    );
    assert!(matches!(broker.read().await, Some(Packet::Publish(_))));
    until_phase(&progress, Phase::Delivering).await;
    let late = Duration::from_millis(1);
    assert_eq!(
        progress.check(delivering + DELIVERY_LIMIT + SLACK + late),
        Liveness::Overstayed {
            phase: Phase::Delivering,
            lasted: DELIVERY_LIMIT + SLACK + late,
            allowed: DELIVERY_LIMIT
        },
        "the limit counts from the first moment the message waited"
    );
    assert_eq!(
        bounded("a message", incoming.recv()).await.unwrap().payload,
        1_u16.to_be_bytes()
    );
    assert_eq!(
        broker.read().await,
        Some(Packet::PubAck {
            id: count,
            reason: 0
        })
    );

    // The connection ends: the backoff (60 s here) is the next wait.
    broker.send(&disconnect(0x8B)).await;
    until_phase(&progress, Phase::Backoff).await;
    let now = Instant::now();
    assert_eq!(
        progress.check(now + Duration::from_secs(59) + SLACK),
        Liveness::Alive
    );
    assert!(matches!(
        progress.check(now + Duration::from_secs(60) + SLACK + late),
        Liveness::Overstayed {
            phase: Phase::Backoff,
            ..
        }
    ));

    // A stop waits for the PUBACK of offline/shutdown for at most 5 s.
    let mut broker = FakeBroker::new(bounded("a connection", brokers.recv()).await.unwrap())
        .subscribing(&[Channel::LogRequest]);
    broker.accept().await;
    until_phase(&progress, Phase::Connected).await;
    shutdown.cancel();
    let Some(Packet::Publish(offline)) = broker.read_answering_pings().await else {
        panic!("expected offline/shutdown")
    };
    until_phase(&progress, Phase::Stopping).await;
    assert_allowed(&progress, Phase::Stopping, Duration::from_secs(5));
    broker.send(&puback(offline.id.unwrap(), 0)).await;
    assert_eq!(broker.read().await, Some(Packet::Disconnect { reason: 0 }));
    bounded("the supervisor", task).await.unwrap();
    assert_eq!(progress.phase(), Phase::Stopped);
}

/// A write may take 1.5 keepalives, longer than the connected wait before it: it is declared
/// on its own, so a slow uplink does not get the agent restarted.
#[tokio::test(start_paused = true)]
async fn a_write_may_take_its_own_timeout() {
    let shutdown = CancellationToken::new();
    let (connector, mut brokers) = DuplexConnector::with_buffer(64);
    let (link, _incoming, supervisor) = link::link(settings(), connector, shutdown.clone());
    let progress = supervisor.progress();
    let _task = tokio::spawn(supervisor.run());
    let mut broker = FakeBroker::new(bounded("a connection", brokers.recv()).await.unwrap())
        .subscribing(&[Channel::LogRequest]);
    broker.accept().await;
    until_phase(&progress, Phase::Connected).await;
    // The broker stops reading; the message does not fit the stream's 64 bytes.
    let _stuck = tokio::spawn(async move { link.publish(Channel::Live, vec![0; 4096]).await });
    until_phase(&progress, Phase::Writing).await;
    assert_allowed(&progress, Phase::Writing, Duration::from_secs(90));
}

/// The supervisor's future, polled until the test freezes it; from then on it is never polled
/// again, like a task stuck in an await that never ends.
struct Freezable<F> {
    inner: Pin<Box<F>>,
    frozen: Arc<AtomicBool>,
}

impl<F: Future> Future for Freezable<F> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.frozen.load(Ordering::Relaxed) {
            return Poll::Pending;
        }
        self.inner.as_mut().poll(cx).map(|_| ())
    }
}

/// The agent's watchdog wiring: the connection thread's ticker, which keeps counting, and the
/// watchdog task notifying a socket the test reads every 15 s.
struct Watchdog {
    _dir: ScratchDir,
    receiver: UnixDatagram,
    started: Instant,
    tasks: [tokio::task::JoinHandle<()>; 2],
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Watchdog {
    fn start(progress: Arc<Progress>) -> Self {
        let dir = ScratchDir::new("link-watchdog");
        let path = dir.path().join("notify.sock");
        let receiver = UnixDatagram::bind(&path).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let ticks = Arc::new(AtomicU64::new(0));
        let counting = Arc::clone(&ticks);
        let ticker = tokio::spawn(async move {
            let mut interval = tokio::time::interval(PROGRESS_TICK);
            loop {
                interval.tick().await;
                counting.fetch_add(1, Ordering::Relaxed);
            }
        });
        let feeder = tokio::spawn(liveness::watchdog(
            Arc::new(Notifier::to_path(&path).unwrap()),
            Duration::from_secs(15),
            ticks,
            progress,
            GATEWAY.to_owned(),
        ));
        Self {
            _dir: dir,
            receiver,
            started: Instant::now(),
            tasks: [ticker, feeder],
        }
    }

    /// What the watchdog task sent for each of the first `rounds` notifications: `W` for
    /// WATCHDOG=1, otherwise the STATUS line.
    async fn rounds(&self, rounds: u32) -> Vec<String> {
        let mut sent = Vec::new();
        for round in 1..=rounds {
            tokio::time::sleep_until(
                self.started + Duration::from_secs(15) * round + Duration::from_millis(1),
            )
            .await;
            let mut buffer = [0_u8; 512];
            let mut this_round = Vec::new();
            while let Ok(len) = self.receiver.recv(&mut buffer) {
                let text = String::from_utf8_lossy(&buffer[..len]).into_owned();
                this_round.push(if text == "WATCHDOG=1\n" {
                    "W".to_owned()
                } else {
                    text
                });
            }
            sent.push(this_round.join(""));
        }
        sent
    }
}

/// A broker that never sends CONNACK: the agent gives up after 30 s and tries again, and the
/// watchdog stays fed throughout. Frozen in that same wait (the supervisor no longer polled,
/// as a wait without its timeout would be), the watchdog stops once the wait is 30 s plus
/// SLACK old, although the thread keeps counting ticks, and STATUS says why.
#[tokio::test(start_paused = true)]
async fn a_connection_task_stuck_in_a_wait_stops_the_watchdog() {
    for freeze in [false, true] {
        let shutdown = CancellationToken::new();
        let (connector, mut brokers) = DuplexConnector::new();
        let (_link, _incoming, supervisor) = link::link(settings(), connector, shutdown.clone());
        let progress = supervisor.progress();
        let frozen = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(Freezable {
            inner: Box::pin(supervisor.run()),
            frozen: Arc::clone(&frozen),
        });
        let watchdog = Watchdog::start(Arc::clone(&progress));
        // Every connection takes the CONNECT and never answers.
        let mut broker = FakeBroker::new(bounded("a connection", brokers.recv()).await.unwrap());
        let Some(Packet::Connect(_)) = broker.read().await else {
            panic!("expected CONNECT")
        };
        until_phase(&progress, Phase::AwaitConnack).await;
        frozen.store(freeze, Ordering::Relaxed);
        let sent = watchdog.rounds(5).await;
        if freeze {
            let stuck = |lasted: u64| {
                format!(
                    "STATUS=gateway {GATEWAY}: stuck waiting for CONNACK for {lasted} s (allowed 40 s); withholding the watchdog\n"
                )
            };
            assert_eq!(sent, ["W", "W", &stuck(45), &stuck(60), &stuck(75)]);
        } else {
            assert_eq!(
                sent, ["W"; 5],
                "a wait within its deadline keeps the watchdog fed"
            );
            assert!(
                !matches!(progress.phase(), Phase::AwaitConnack | Phase::Starting),
                "the agent gave up on CONNACK and went on: {:?}",
                progress.phase()
            );
        }
        task.abort();
        drop(broker);
    }
}
