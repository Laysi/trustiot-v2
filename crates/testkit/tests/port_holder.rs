//! `PortHolder`'s start (Laysi/trustiot-v2#857): picking host ports and publishing them are two
//! steps, so another process can take a picked port in between. The start then tries again with
//! new ports, a bounded number of times, and the ports it publishes are held: nothing else can
//! bind them while the holder runs, and they are free once it stops, as a container's own
//! published ports are while it is stopped. These tests take a port themselves to play the
//! other process. They start
//! throwaway containers with podman or docker; they are skipped without either unless `CI` is
//! set.

use std::net::TcpListener;

use testkit::container::{
    PORT_ATTEMPTS, PortHolder, container_runtime, runtime_output, unavailable,
};
use testkit::mqtt::mosquitto::MOSQUITTO_IMAGE;

/// The port inside the holder's network namespace; nothing listens on it, only the host side
/// matters here.
const INSIDE: u16 = 1883;

/// A port on 127.0.0.1 that this test holds, as another process would.
fn taken_port() -> (TcpListener, u16) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind a port");
    let port = listener.local_addr().expect("local address").port();
    (listener, port)
}

#[test]
fn a_picked_port_that_was_taken_is_replaced_by_a_new_one() {
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return;
    };
    let (_taken, port) = taken_port();
    let mut picks = 0_u32;
    let holder = PortHolder::start_with(runtime, "port-holder", MOSQUITTO_IMAGE, &[INSIDE], |n| {
        picks += 1;
        assert_eq!(n, 1);
        if picks == 1 {
            vec![port]
        } else {
            let (listener, free) = taken_port();
            drop(listener);
            vec![free]
        }
    });
    assert_eq!(picks, 2, "the start picks again once, after the taken port");
    assert_ne!(holder.host_port(INSIDE), port);
    // The published port is held on the host for as long as the holder runs.
    holder.assert_held();

    // A stopped container gives its published ports up, which is why the container under test
    // must not publish them itself: the check sees that.
    let _ = runtime_output(runtime, &["stop", "--time", "10", &holder.container.name]);
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| holder.assert_held()))
        .expect_err("the port of a stopped holder was still held");
    let message = failure
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_default();
    assert!(message.contains("could be bound"), "{message}");
}

#[test]
fn a_start_gives_up_after_the_attempts_when_every_port_is_taken() {
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return;
    };
    let (_taken, port) = taken_port();
    let mut picks = 0_u32;
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        PortHolder::start_with(runtime, "port-holder", MOSQUITTO_IMAGE, &[INSIDE], |_| {
            picks += 1;
            vec![port]
        })
    }))
    .expect_err("the holder started on a port the test holds");
    let message = failure
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_default();
    assert_eq!(picks, PORT_ATTEMPTS, "{message}");
    assert!(
        message.contains(&format!("attempt {PORT_ATTEMPTS} of {PORT_ATTEMPTS}")),
        "{message}"
    );
    let lower = message.to_lowercase();
    assert!(
        lower.contains("address already in use") || lower.contains("port is already allocated"),
        "the message holds the runtime's reason: {message}"
    );
}
