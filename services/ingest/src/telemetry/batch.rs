//! A decoded batch, checked for what the protocol leaves to the cloud, and its rows in the
//! telemetry table (services/ingest/README.md, "寫入流程").
//!
//! The protocol accepts any device ID and any reading time; the table holds devices as UUIDs
//! (the assets service creates them so) and times as `DateTime64(3, 'UTC')`, which ends with
//! 2299. A batch with a device ID that is not a UUID, or a reading time past the column's range,
//! can never be written, so it is rejected whole: answering `retry` would hold the gateway's
//! stream up for good.

use std::collections::BTreeSet;

use chrono::DateTime;
use protocol::telemetry::TelemetryBatch;
use protocol::{GatewayId, Issue, IssueCode, PointKey, Seq, StreamId, Value};
use serde::Serialize;
use uuid::Uuid;

use crate::rollup::ReconcileParams;

/// The latest reading time the telemetry table holds: 2299-12-31T23:59:59.999Z, the end of
/// ClickHouse's `DateTime64` range, in milliseconds since the Unix epoch.
pub const MAX_READING_MILLIS: u64 = 10_413_791_999_999;

/// The length of a UUID in its hyphenated form, the only form a device ID takes.
const HYPHENATED_UUID_LEN: usize = 36;

/// A batch that may be written: its devices are UUIDs and its reading times fit the table.
#[derive(Clone, Debug)]
pub struct Prepared {
    /// The gateway of the topic the batch arrived on (its certificate), never from the payload.
    pub gateway: GatewayId,
    /// The batch's stream and sequence number.
    pub stream: StreamId,
    pub seq: Seq,
    /// How many readings the batch holds: the rows it becomes.
    pub records: u32,
    series: Vec<Series>,
    first_ms: u64,
    last_ms: u64,
}

#[derive(Clone, Debug)]
struct Series {
    device: Uuid,
    point: PointKey,
    samples: Vec<(u64, Value)>,
}

/// Checks `batch` from `gateway` and keeps what its rows need. The error is the first problem
/// found, for a `rejected` acknowledgement.
///
/// # Errors
///
/// An [`Issue`] for a device ID that is not a hyphenated UUID or a reading time past
/// [`MAX_READING_MILLIS`].
pub fn prepare(gateway: GatewayId, batch: TelemetryBatch) -> Result<Prepared, Issue> {
    let records = u32::try_from(batch.record_count()).map_err(|_| {
        // A batch within the channel's 256 KiB holds far fewer readings.
        Issue::new(
            IssueCode::TooMany,
            "series",
            "the batch holds too many readings",
        )
    })?;
    let (mut first_ms, mut last_ms) = (u64::MAX, 0);
    let mut series = Vec::with_capacity(batch.series.len());
    for (index, entry) in batch.series.into_iter().enumerate() {
        let device = device_uuid(entry.device.as_str()).ok_or_else(|| {
            Issue::new(
                IssueCode::InvalidValue,
                format!("series[{index}].device"),
                "a device ID must be a UUID (the assets service creates devices so)",
            )
        })?;
        let mut samples = Vec::with_capacity(entry.samples.len());
        for (sample_index, sample) in entry.samples.into_iter().enumerate() {
            let at = sample.at.get();
            if at > MAX_READING_MILLIS {
                return Err(Issue::new(
                    IssueCode::OutOfRange,
                    format!("series[{index}].samples[{sample_index}]"),
                    "the reading time is after 2299-12-31T23:59:59.999Z",
                ));
            }
            first_ms = first_ms.min(at);
            last_ms = last_ms.max(at);
            samples.push((at, sample.value));
        }
        series.push(Series {
            device,
            point: entry.point,
            samples,
        });
    }
    Ok(Prepared {
        gateway,
        stream: batch.stream,
        seq: batch.seq,
        records,
        series,
        first_ms,
        last_ms,
    })
}

/// `id` as a UUID when it is one in the hyphenated form (either case).
fn device_uuid(id: &str) -> Option<Uuid> {
    if id.len() != HYPHENATED_UUID_LEN {
        return None;
    }
    Uuid::try_parse(id).ok()
}

/// `millis` as ClickHouse's text form of `DateTime64(3, 'UTC')`: `YYYY-MM-DD hh:mm:ss.sss`.
/// `None` past the range of [`MAX_READING_MILLIS`] (never for a prepared batch's times or the
/// service's own clock).
#[must_use]
pub fn datetime64(millis: u64) -> Option<String> {
    if millis > MAX_READING_MILLIS {
        return None;
    }
    let time = DateTime::from_timestamp_millis(i64::try_from(millis).ok()?)?;
    Some(time.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
}

/// One row of `trustiot.telemetry` in `JSONEachRow`. The value columns other than the one of
/// the value's type keep their defaults (the table's `value_matches_type` constraint).
#[derive(Serialize)]
struct Row<'a> {
    tenant_id: &'a str,
    gateway_id: &'a str,
    stream_id: &'a str,
    batch_seq: u64,
    device_id: &'a str,
    point: &'a str,
    ts: String,
    received_at: &'a str,
    value_type: &'static str,
    value_float: f64,
    value_int: i64,
    value_bool: bool,
    value_string: &'a str,
}

impl Prepared {
    /// The batch's rows for `tenant`, received at `received_at` (the text form of
    /// [`datetime64`]), one `JSONEachRow` line each.
    ///
    /// # Errors
    ///
    /// When a value cannot be written as JSON, which a decoded batch never holds (floats are
    /// finite).
    pub fn rows(&self, tenant: &str, received_at: &str) -> Result<String, serde_json::Error> {
        let (gateway, stream) = (self.gateway.as_str(), self.stream.as_str());
        let mut out = String::new();
        for series in &self.series {
            let device = series.device.hyphenated().to_string();
            for (at, value) in &series.samples {
                let ts = datetime64(*at).unwrap_or_default();
                let mut row = Row {
                    tenant_id: tenant,
                    gateway_id: gateway,
                    stream_id: stream,
                    batch_seq: self.seq.get(),
                    device_id: &device,
                    point: series.point.as_str(),
                    ts,
                    received_at,
                    value_type: "float",
                    value_float: 0.0,
                    value_int: 0,
                    value_bool: false,
                    value_string: "",
                };
                match value {
                    Value::Float(number) => row.value_float = *number,
                    Value::Int(number) => {
                        row.value_type = "int";
                        row.value_int = *number;
                    }
                    Value::Bool(flag) => {
                        row.value_type = "bool";
                        row.value_bool = *flag;
                    }
                    Value::Text(text) => {
                        row.value_type = "string";
                        row.value_string = text;
                    }
                }
                out.push_str(&serde_json::to_string(&row)?);
                out.push('\n');
            }
        }
        Ok(out)
    }

    /// The reconcile's parameters for this batch written for `tenant` (src/rollup.rs).
    #[must_use]
    pub fn reconcile_params(&self, tenant: &str) -> ReconcileParams {
        let series: BTreeSet<(String, String)> = self
            .series
            .iter()
            .map(|series| {
                (
                    series.device.hyphenated().to_string(),
                    series.point.as_str().to_owned(),
                )
            })
            .collect();
        ReconcileParams {
            tenant: tenant.to_owned(),
            gateway: self.gateway.as_str().to_owned(),
            stream: self.stream.as_str().to_owned(),
            series: series.into_iter().collect(),
            first_ts: datetime64(self.first_ms).unwrap_or_default(),
            last_ts: datetime64(self.last_ms).unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use protocol::telemetry::{Sample, Series as WireSeries};
    use protocol::{DeviceId, UnixMillis, V1};

    use super::*;

    const DEVICE: &str = "0b9c6f3e-2d4a-4c8e-9a51-6f0d2b7c1e44";

    fn batch(device: &str, samples: Vec<(u64, Value)>) -> TelemetryBatch {
        TelemetryBatch {
            v: V1,
            stream: StreamId::new("s-1").unwrap(),
            seq: Seq::new(7).unwrap(),
            clock_synced: true,
            series: vec![WireSeries {
                device: DeviceId::new(device).unwrap(),
                point: PointKey::new("energy").unwrap(),
                samples: samples
                    .into_iter()
                    .map(|(at, value)| Sample {
                        at: UnixMillis::new(at),
                        value,
                    })
                    .collect(),
            }],
        }
    }

    fn gateway() -> GatewayId {
        GatewayId::new("gw-1").unwrap()
    }

    #[test]
    fn the_last_reading_time_is_the_end_of_2299() {
        assert_eq!(
            datetime64(MAX_READING_MILLIS).as_deref(),
            Some("2299-12-31 23:59:59.999")
        );
        assert_eq!(datetime64(MAX_READING_MILLIS + 1), None);
        assert_eq!(datetime64(0).as_deref(), Some("1970-01-01 00:00:00.000"));
        assert_eq!(
            datetime64(1_761_350_400_123).as_deref(),
            Some("2025-10-25 00:00:00.123")
        );
    }

    #[test]
    fn devices_must_be_hyphenated_uuids() {
        for good in [DEVICE, &DEVICE.to_uppercase()] {
            assert!(prepare(gateway(), batch(good, vec![(1, Value::Int(1))])).is_ok());
        }
        for bad in ["meter-1", "0b9c6f3e2d4a4c8e9a516f0d2b7c1e44"] {
            let issue = prepare(gateway(), batch(bad, vec![(1, Value::Int(1))])).unwrap_err();
            assert_eq!(issue.code, IssueCode::InvalidValue);
            assert_eq!(issue.path, "series[0].device");
        }
    }

    #[test]
    fn reading_times_past_the_table_are_rejected() {
        let issue = prepare(
            gateway(),
            batch(
                DEVICE,
                vec![(1, Value::Int(1)), (MAX_READING_MILLIS + 1, Value::Int(2))],
            ),
        )
        .unwrap_err();
        assert_eq!(issue.code, IssueCode::OutOfRange);
        assert_eq!(issue.path, "series[0].samples[1]");
    }

    #[test]
    fn rows_put_each_value_in_its_column_and_take_the_tenant_given() {
        let prepared = prepare(
            gateway(),
            batch(
                &DEVICE.to_uppercase(),
                vec![
                    (1_000, Value::Float(1.5)),
                    (2_000, Value::Int(-3)),
                    (3_000, Value::Bool(true)),
                    (4_000, Value::Text("a\u{1b}[31m\"b".to_owned())),
                ],
            ),
        )
        .unwrap();
        assert_eq!(prepared.records, 4);
        let rows = prepared
            .rows("tenant-x", "2026-10-29 00:00:00.000")
            .unwrap();
        let rows: Vec<serde_json::Value> = rows
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 4);
        for row in &rows {
            assert_eq!(row["tenant_id"], "tenant-x");
            assert_eq!(row["gateway_id"], "gw-1");
            assert_eq!(row["stream_id"], "s-1");
            assert_eq!(row["batch_seq"], 7);
            // Lowercase, as ClickHouse prints UUIDs.
            assert_eq!(row["device_id"], DEVICE);
            assert_eq!(row["received_at"], "2026-10-29 00:00:00.000");
        }
        assert_eq!(rows[0]["value_type"], "float");
        assert_eq!(rows[0]["value_float"], 1.5);
        assert_eq!(rows[1]["value_type"], "int");
        assert_eq!(rows[1]["value_int"], -3);
        assert_eq!(rows[1]["value_float"], 0.0);
        assert_eq!(rows[2]["value_type"], "bool");
        assert_eq!(rows[2]["value_bool"], true);
        assert_eq!(rows[3]["value_type"], "string");
        // Control characters are stored as they are, escaped only by JSON.
        assert_eq!(rows[3]["value_string"], "a\u{1b}[31m\"b");
        assert_eq!(rows[3]["ts"], "1970-01-01 00:00:04.000");

        let params = prepared.reconcile_params("tenant-x");
        assert_eq!(
            params.series,
            vec![(DEVICE.to_owned(), "energy".to_owned())]
        );
        assert_eq!(params.first_ts, "1970-01-01 00:00:01.000");
        assert_eq!(params.last_ts, "1970-01-01 00:00:04.000");
    }
}
