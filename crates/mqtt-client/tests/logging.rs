//! No log line at any level holds any part of a password (Laysi/trustiot-v2#631). Every
//! `tracing` event at TRACE and every `log` record (the bridge services install too) of this
//! test process is captured while a client logs in, is refused with a wrong password, publishes
//! and receives; every event the caller sees is logged with its `Debug` form as well. No
//! 5-character piece of either password may appear in the output.
//!
//! Its own test binary: the capturing subscriber is the process's global default.

#![allow(
    clippy::unwrap_used,
    reason = "tests unwrap (clippy.toml allows it in tests)"
)]

mod support;

use std::io::Write;
use std::sync::{Arc, Mutex};

use mqtt_client::{Client, Event, Password, PublishOptions};
use support::{Broker, Scratch, expect_connected, expect_event, random_password};

/// Window length of the leak check: any piece this long of a password counts as a leak.
const WINDOW: usize = 5;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Every piece of `password` that is [`WINDOW`] characters long and occurs in `output`.
fn leaks(output: &str, password: &str) -> Vec<String> {
    let chars: Vec<char> = password.chars().collect();
    chars
        .windows(WINDOW)
        .map(|window| window.iter().collect::<String>())
        .filter(|piece| output.contains(piece.as_str()))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_log_line_holds_a_piece_of_a_password() {
    let capture = Capture::default();
    let writer = capture.clone();
    tracing_log::LogTracer::init().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let Some(broker) = Broker::start() else {
        return;
    };
    let scratch = Scratch::new();
    let password = random_password();
    let wrong_password = random_password();
    broker.create_account(&password);
    let config = broker.config(scratch.write("password", &password));

    // The password type's own Debug output, and the settings'.
    let read = Password::read(&config.password_file).unwrap();
    tracing::trace!(?read, ?config, "the password and the settings");
    assert_eq!(format!("{read:?}"), "Password(<redacted>)");

    let (client, mut events) = Client::start(&config, vec!["test/echo".to_owned()]).unwrap();
    expect_connected(&mut events, "the connection").await;
    client
        .publish("test/echo", "hello", PublishOptions::default())
        .await
        .unwrap();
    let message = expect_event(&mut events, "the echoed message", |event| {
        matches!(event, Event::Message(_))
    })
    .await;
    tracing::trace!(?message, "event");
    let refused = client
        .publish("forbidden/topic", "x", PublishOptions::default())
        .await;
    tracing::trace!(?refused, "publish result");

    let wrong = broker.config(scratch.write("wrong", &wrong_password));
    let (wrong_client, mut wrong_events) = Client::start(&wrong, Vec::new()).unwrap();
    let refusal = expect_event(&mut wrong_events, "the refusal", |event| {
        matches!(event, Event::Disconnected { .. })
    })
    .await;
    tracing::trace!(?refusal, "event");
    wrong_client.shutdown().await;
    client.shutdown().await;
    while let Some(event) = events.recv().await {
        tracing::trace!(?event, "event");
    }

    let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    support::evidence(format!(
        "captured {} log lines, {} bytes, at TRACE",
        output.lines().count(),
        output.len()
    ));
    // The capture works: the client's own lines at every level are there.
    for expected in [
        "connecting to the mqtt broker",
        "connected to the mqtt broker",
        "reconnecting",
        "publish",
        "service:test",
        "Password(<redacted>)",
    ] {
        assert!(output.contains(expected), "{expected:?} is not in the log");
    }
    assert_eq!(leaks(&output, &password), Vec::<String>::new());
    assert_eq!(leaks(&output, &wrong_password), Vec::<String>::new());
    support::evidence(format!(
        "no {WINDOW}-character piece of either password in the log"
    ));
}
