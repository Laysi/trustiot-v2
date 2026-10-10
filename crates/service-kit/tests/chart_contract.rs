//! The Helm chart's contract with services (deploy/chart/README.md, "chart 與服務的約定"),
//! checked against the kit's defaults so neither side drifts unnoticed: the HTTP port, the probe
//! paths, and the time the chart allows between SIGTERM and SIGKILL. Reads the `serviceDefaults`
//! block of deploy/chart/values.yaml line by line; no YAML parser is needed for these keys.

use std::time::Duration;

use service_kit::health::{LIVENESS_PATH, READINESS_PATH};
use service_kit::http::HttpConfig;
use service_kit::shutdown::ShutdownConfig;

/// What a service needs after the drain before it exits: cutting off work that overran the drain
/// (at most 0.5 s) and exporting buffered telemetry (at most 2.5 s) (crates/service-kit/README.md).
const EXIT_BUDGET: Duration = Duration::from_secs(3);

/// The trimmed lines of the chart's `serviceDefaults` block.
fn service_defaults() -> Vec<String> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../deploy/chart/values.yaml"
    );
    let text = std::fs::read_to_string(path).expect("read deploy/chart/values.yaml");
    let block: Vec<String> = text
        .lines()
        .skip_while(|line| *line != "serviceDefaults:")
        .skip(1)
        .take_while(|line| line.is_empty() || line.starts_with(' '))
        .map(|line| line.trim().to_owned())
        .collect();
    assert!(!block.is_empty(), "no serviceDefaults block in {path}");
    block
}

/// The value of the first line that starts with `key`.
fn value_of(lines: &[String], key: &str) -> Option<String> {
    lines
        .iter()
        .find_map(|line| line.strip_prefix(key))
        .map(|value| value.trim().to_owned())
}

/// The value of the first line that starts with `key`, at or after the line `after`.
fn value_after(block: &[String], after: &str, key: &str) -> String {
    let start = block
        .iter()
        .position(|line| line == after)
        .unwrap_or_else(|| panic!("no `{after}` in serviceDefaults"));
    value_of(&block[start..], key)
        .unwrap_or_else(|| panic!("no `{key}` after `{after}` in serviceDefaults"))
}

#[test]
fn the_kit_listens_on_the_chart_port() {
    let block = service_defaults();
    let port: u16 = value_after(&block, "ports:", "containerPort:")
        .parse()
        .expect("containerPort is a number");
    assert_eq!(HttpConfig::default().listen_addr.port(), port);
    assert_eq!(value_after(&block, "ports:", "- name:"), "http");
}

#[test]
fn the_chart_probes_the_kit_health_paths() {
    let block = service_defaults();
    assert_eq!(value_after(&block, "startup:", "path:"), LIVENESS_PATH);
    assert_eq!(value_after(&block, "liveness:", "path:"), LIVENESS_PATH);
    assert_eq!(value_after(&block, "readiness:", "path:"), READINESS_PATH);
}

#[test]
fn the_default_drain_fits_the_chart_drain_seconds() {
    let block = service_defaults();
    let drain_seconds: u64 = value_of(&block, "drainSeconds:")
        .expect("drainSeconds in serviceDefaults")
        .parse()
        .expect("drainSeconds is a number");
    let shutdown = ShutdownConfig::default();
    let needed = shutdown.drain_delay + shutdown.drain_timeout + EXIT_BUDGET;
    assert!(
        needed <= Duration::from_secs(drain_seconds),
        "the kit needs up to {needed:?} after SIGTERM, but the chart sends SIGKILL after \
         {drain_seconds} s"
    );
}
