//! When the shared ClickHouse container helper retries a failed start: only when the runtime says
//! a host port it chose was taken, and only a bounded number of times. Needs no container.

mod common;

use common::retry_start;
use testkit::container::PORT_ATTEMPTS;

/// What rootless podman prints when another process took the port between choosing and binding
/// it (Laysi/trustiot-v2#914).
const ROOTLESSPORT: &str =
    "Error: rootlessport listen tcp 127.0.0.1:40481: bind: address already in use";
/// What rootful podman and Docker print.
const ALREADY_ALLOCATED: &str = "docker: Error response from daemon: driver failed programming \
     external connectivity: Bind for 127.0.0.1:8123 failed: port is already allocated.";

#[test]
fn a_taken_host_port_is_retried() {
    assert!(retry_start(ROOTLESSPORT, 1));
    assert!(retry_start(ALREADY_ALLOCATED, 1));
    assert!(
        retry_start(&ROOTLESSPORT.to_uppercase(), 1),
        "case is ignored"
    );
}

#[test]
fn any_other_failure_is_not_retried() {
    for stderr in [
        "Error: unable to copy from source docker://clickhouse: pull access denied",
        "Error: creating container storage: the container name is already in use",
        "Error: crun: memory limit is invalid",
        "",
    ] {
        assert!(!retry_start(stderr, 1), "retried: {stderr}");
    }
}

#[test]
fn the_retries_stop_after_the_attempt_limit() {
    assert!(retry_start(ROOTLESSPORT, PORT_ATTEMPTS - 1));
    assert!(
        !retry_start(ROOTLESSPORT, PORT_ATTEMPTS),
        "attempt {PORT_ATTEMPTS} of {PORT_ATTEMPTS} is the last"
    );
}
