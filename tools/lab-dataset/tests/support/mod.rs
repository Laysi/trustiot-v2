//! What the anomaly tests and the ClickHouse load test share.

use chrono::{DateTime, Utc};
use lab_dataset::{Anomaly, time};

/// A dataset time in the canonical spelling.
pub fn at(text: &str) -> DateTime<Utc> {
    time::parse(text).expect("a canonical time")
}

/// The anomalies the tests insert: one of each kind, a stop across the 32-bit counter's wrap,
/// and a stop of the energy meter whose readings arrived as backfill (out of order).
pub fn anomalies() -> Vec<Anomaly> {
    vec![
        Anomaly::spike(
            "power-spike",
            "meter-1",
            &["power_kw"],
            at("2026-01-05T22:20:00.000Z"),
            at("2026-01-05T22:30:00.999Z"),
            480.5,
        )
        .expecting("power-high", true),
        Anomaly::outage(
            "meter-stop",
            "meter-1",
            &["energy_kwh", "power_kw"],
            at("2026-01-06T00:50:00.000Z"),
            at("2026-01-06T01:00:00.999Z"),
        )
        .expecting("machine-stopped", true)
        .expecting("power-high", false),
        Anomaly::outage(
            "pulse-stop",
            "meter-2",
            &["pulses", "power_kw"],
            at("2026-01-05T23:50:00.000Z"),
            at("2026-01-06T00:00:00.999Z"),
        )
        .expecting("machine-stopped", true),
        Anomaly::changeover(
            "press-changeover",
            "press-1",
            &["running", "mode", "cycle_count"],
            at("2026-01-06T01:30:00.000Z"),
            at("2026-01-06T01:40:00.999Z"),
            "changeover",
        )
        .expecting("changeover-detected", true)
        .expecting("machine-stopped", false),
    ]
}
