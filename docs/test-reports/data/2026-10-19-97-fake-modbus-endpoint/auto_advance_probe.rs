//! Measures how often a read times out spuriously against the fake endpoint, with a paused tokio
//! clock alone (auto-advance) and with `testkit::clock::manual`. Not part of the test suite: copy
//! it to `crates/testkit/tests/` and run
//! `cargo test -p testkit --test auto_advance_probe -- --nocapture`.
//!
//! Every read meets a 1 s delay and has a 3 s timeout, so a correct clock never times out.

use std::time::Duration;

use testkit::modbus::{FakeModbus, Fault, Transport};
use tokio_modbus::Slave;
use tokio_modbus::client::{Reader, tcp};

const ROUNDS: usize = 2000;

async fn spurious_timeouts(manual: bool) -> usize {
    let _clock = manual.then(testkit::clock::manual);
    let endpoint = FakeModbus::start(Transport::ModbusTcp).await;
    endpoint.set_holding_registers(1, 0, &[1, 2]);
    endpoint.inject(Fault::delay(Duration::from_secs(1)));
    let stream = tokio::net::TcpStream::connect(endpoint.address())
        .await
        .unwrap();
    let mut client = tcp::attach_slave(stream, Slave(1));
    let mut timeouts = 0;
    for round in 0..ROUNDS {
        let read = tokio::time::timeout(
            Duration::from_secs(3),
            client.read_holding_registers(0, 2),
        );
        let result = if manual {
            let (result, ()) = tokio::join!(read, async {
                endpoint.wait_for_requests(round + 1).await;
                tokio::time::advance(Duration::from_secs(1)).await;
            });
            result
        } else {
            read.await
        };
        if result.is_err() {
            timeouts += 1;
            // A late answer would be read as the next one; start over on a new connection.
            let stream = tokio::net::TcpStream::connect(endpoint.address())
                .await
                .unwrap();
            client = tcp::attach_slave(stream, Slave(1));
        }
    }
    timeouts
}

#[tokio::test(start_paused = true)]
async fn auto_advance() {
    let timeouts = spurious_timeouts(false).await;
    println!("PROBE auto-advance: {timeouts}/{ROUNDS} spurious timeouts");
}

#[tokio::test(start_paused = true)]
async fn manual_clock() {
    let timeouts = spurious_timeouts(true).await;
    println!("PROBE manual: {timeouts}/{ROUNDS} spurious timeouts");
}
