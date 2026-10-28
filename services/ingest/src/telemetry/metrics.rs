//! The telemetry path's metrics (services/ingest/README.md, "指標"), exported through
//! service-kit's meter provider like every service's: OTLP to the collector while
//! `telemetry.otlp_endpoint` is set, nothing otherwise. The alert rules of
//! Laysi/trustiot-v2#183 ("資料接收失敗與積壓") are built on them.

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};

/// What became of a message on the telemetry channel: the `outcome` attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Written and acknowledged `committed`.
    Committed,
    /// At or below the stream's high-water mark: acknowledged `duplicate`.
    Duplicate,
    /// Can never be accepted: acknowledged `rejected`.
    Rejected,
    /// Not written now: acknowledged `retry`.
    Retry,
    /// A retained message (the gateway set the retain flag): dropped without an answer.
    RetainDropped,
    /// Delivered with QoS 0: dropped without an answer, the gateway sends it again.
    QosDropped,
    /// Not even a batch header can be read, or the topic is not a telemetry topic: dropped
    /// without an answer, since there is nothing to address one to.
    Undecodable,
    /// Over its gateway's limits: acknowledged `retry` without being handled, or dropped without
    /// an answer when it could not have one or too many such answers were being sent.
    Throttled,
}

impl Outcome {
    /// The attribute's value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Duplicate => "duplicate",
            Self::Rejected => "rejected",
            Self::Retry => "retry",
            Self::RetainDropped => "retain_dropped",
            Self::QosDropped => "qos0_dropped",
            Self::Undecodable => "undecodable",
            Self::Throttled => "throttled",
        }
    }
}

/// The telemetry path's instruments.
#[derive(Clone, Debug)]
pub struct Metrics {
    batches: Counter<u64>,
    records: Counter<u64>,
    duration: Histogram<f64>,
    failures: Counter<u64>,
}

impl Metrics {
    /// Creates the instruments. Call it after the kit started telemetry (in `start`), or they are
    /// not exported.
    #[must_use]
    pub fn new() -> Self {
        let meter = opentelemetry::global::meter("ingest");
        Self {
            batches: meter
                .u64_counter("ingest.telemetry.batches")
                .with_unit("{batch}")
                .with_description("Messages on the telemetry channel, by outcome")
                .build(),
            records: meter
                .u64_counter("ingest.telemetry.records")
                .with_unit("{reading}")
                .with_description("Readings written to ClickHouse and acknowledged committed")
                .build(),
            duration: meter
                .f64_histogram("ingest.telemetry.duration")
                .with_unit("s")
                .with_description(
                    "Time from a message's arrival to its acknowledgement being sent, by outcome",
                )
                .build(),
            failures: meter
                .u64_counter("ingest.telemetry.write_failures")
                .with_unit("{failure}")
                .with_description(
                    "Statements of the telemetry path that failed, by store and step; each one \
                     answers retry",
                )
                .build(),
        }
    }

    pub(crate) fn handled(&self, outcome: Outcome, elapsed: Duration) {
        let attributes = [KeyValue::new("outcome", outcome.as_str())];
        self.batches.add(1, &attributes);
        self.duration.record(elapsed.as_secs_f64(), &attributes);
    }

    pub(crate) fn committed(&self, records: u32) {
        self.records.add(u64::from(records), &[]);
    }

    pub(crate) fn failed(&self, store: &'static str, step: &'static str) {
        self.failures.add(
            1,
            &[KeyValue::new("store", store), KeyValue::new("step", step)],
        );
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}
