//! The one-minute and one-hour aggregate tables (`trustiot.telemetry_1m`, `trustiot.telemetry_1h`,
//! migrations/clickhouse/0010_telemetry_rollups.sql; Laysi/trustiot-v2#141): the statements the
//! ingest service and the consumers of the aggregates run against them. The reasoning is in
//! README.md, section 「彙總表」.
//!
//! # The I1 rewrite (contract for Laysi/trustiot-v2#641)
//!
//! Nothing in this repository deletes or rewrites telemetry rows yet: the ingest service's write
//! path is built in Laysi/trustiot-v2#91 and its I1 rewrite in Laysi/trustiot-v2#641, which must
//! follow the steps below and merges after this file. Until then every batch reaches ClickHouse
//! as INSERTs only (the lab dataset loader included), and the views aggregate each in full.
//!
//! The materialized views add every block inserted into `trustiot.telemetry` to the aggregates,
//! but a `DELETE FROM` does not reach them. When the ingest service rewrites a batch that has no
//! completion record (rebuild checklist I1), the rows of an earlier attempt that reached
//! ClickHouse stay counted in the aggregates, and an earlier attempt whose raw rows were written
//! but whose aggregate part was not (a crash between the two) is missing from them. So the
//! rewrite is, for a batch of one stream:
//!
//! 1. Make sure no earlier INSERT of this stream is still running (one ingest replica per stream).
//! 2. `DELETE FROM trustiot.telemetry` the batch's rows, synchronously
//!    (`lightweight_deletes_sync = 2`).
//! 3. Insert the whole batch with `insert_deduplicate = 0`. With deduplication on, a block whose
//!    content the table saw before the DELETE (the same rows, or the same
//!    `insert_deduplication_token`) is dropped without an error, and the batch's rows are lost.
//! 4. Run [`RECONCILE_1M`] and [`RECONCILE_1H`] with the batch's parameters ([`ReconcileParams`]).
//!    Each inserts, for the cells of this stream that the batch touches, the difference between
//!    the cells recomputed from the raw rows and the cells as they are; when they agree it
//!    inserts nothing.
//! 5. Write the completion record, then acknowledge.
//!
//! A crash anywhere before step 5 leaves no completion record, so the gateway sends the batch
//! again and the next attempt starts over at step 1: the reconcile works from the state of the
//! tables, not from what happened before, so running it again is harmless. A batch written once,
//! whose INSERT succeeded, needs no reconcile (the views run inside the INSERT); running it then
//! inserts nothing. Only the raw side of a cell is trusted, so the reconcile must not run while
//! another write to the same stream is in flight, which one replica per stream and one batch in
//! flight per stream (telemetry rule 2) already guarantee.
//!
//! The counts and the sum of a cell are corrected exactly. The minimum, maximum, first and last
//! of a cell are merged from the raw side and are not taken back: telemetry rule 2 resends a
//! batch unchanged, so the rows of an earlier attempt are rows of the rewritten batch and leave
//! those values as they are.
//!
//! The reconcile reaches back [`RECONCILE_WINDOW_DAYS`] receive days: a gateway keeps an
//! unacknowledged batch at most that long, so every earlier attempt of a batch was received
//! within it, and the raw data retention is longer (`retention::RawRetention`), so every raw row
//! received within it is still there. Older cells are never compared with raw rows that may have
//! expired.
//!
//! # Energy from cumulative points
//!
//! [`ENERGY_1M`] and [`ENERGY_1H`] compute a cumulative point's increase (energy from a meter's
//! counter) over a range of buckets from a tenant view, by interval differences, never by
//! summing readings: the increase within each bucket from its first and last reading, plus the
//! increase between consecutive buckets from the previous bucket's last reading to the next
//! bucket's first. A drop is a reset or, for a counter with a wrap value (`wraps_at` in the
//! device model), a wrap; within a bucket a drop shows as a last reading below the bucket's
//! maximum or a minimum below its first reading. The increase within a bucket is exact when the
//! bucket holds at most one reset or wrap, and after a reset its last reading stays below the
//! peak before it.

use protocol::telemetry::BUFFER_MAX_AGE_DAYS;

/// How many receive days before today (UTC) the reconcile compares: the gateway buffer's maximum
/// age.
pub const RECONCILE_WINDOW_DAYS: u32 = BUFFER_MAX_AGE_DAYS;

/// The reconcile statement of one aggregate table: `$table` is the table, `$bucket` the function
/// that truncates `ts` to its bucket, as in the table's materialized view. The query parameters
/// are those of [`ReconcileParams`].
macro_rules! reconcile {
    ($table:literal, $bucket:literal) => {
        concat!(
            "INSERT INTO trustiot.",
            $table,
            " (tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day,
    readings, numeric_readings, value_sum, value_min, value_max, first, last)
SETTINGS insert_deduplicate = 0, join_use_nulls = 0
SELECT
    tenant_id,
    device_id,
    point,
    bucket,
    gateway_id,
    stream_id,
    received_day,
    r.raw_readings - a.agg_readings AS readings,
    r.raw_numeric_readings - a.agg_numeric_readings AS numeric_readings,
    r.raw_value_sum - a.agg_value_sum AS value_sum,
    r.raw_value_min AS value_min,
    r.raw_value_max AS value_max,
    r.raw_first AS first,
    r.raw_last AS last
FROM
(
    SELECT
        tenant_id,
        device_id,
        point,
        bucket,
        gateway_id,
        stream_id,
        received_day,
        toInt64(count()) AS raw_readings,
        toInt64(count(value_number)) AS raw_numeric_readings,
        sum(ifNull(value_number, 0)) AS raw_value_sum,
        min(value_number) AS raw_value_min,
        max(value_number) AS raw_value_max,
        argMinState(reading, ts) AS raw_first,
        argMaxState(reading, ts) AS raw_last
    FROM
    (
        SELECT
            tenant_id,
            device_id,
            point,
            ",
            $bucket,
            "(ts) AS bucket,
            toString(gateway_id) AS gateway_id,
            toString(stream_id) AS stream_id,
            toDate(received_at) AS received_day,
            ts,
            multiIf(value_type = 'float', value_float, value_type = 'int', toFloat64(value_int), value_type = 'bool', toFloat64(value_bool), NULL) AS value_number,
            CAST((ts, value_type, value_float, value_int, value_bool, value_string) AS Tuple(ts DateTime64(3, 'UTC'), value_type Enum8('float' = 1, 'int' = 2, 'bool' = 3, 'string' = 4), value_float Float64, value_int Int64, value_bool Bool, value_string String)) AS reading
        FROM trustiot.telemetry
        WHERE tenant_id = {tenant:UUID}
            AND (device_id, point) IN {series:Array(Tuple(UUID, String))}
            AND ",
            $bucket,
            "(ts) BETWEEN ",
            $bucket,
            "({first_ts:DateTime64(3, 'UTC')}) AND ",
            $bucket,
            "({last_ts:DateTime64(3, 'UTC')})
            AND toString(gateway_id) = {gateway:String}
            AND toString(stream_id) = {stream:String}
            AND toDate(received_at) >= toDate(now('UTC')) - {window_days:UInt32}
    )
    GROUP BY tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day
) AS r
FULL OUTER JOIN
(
    SELECT
        tenant_id,
        device_id,
        point,
        bucket,
        gateway_id,
        stream_id,
        received_day,
        sum(readings) AS agg_readings,
        sum(numeric_readings) AS agg_numeric_readings,
        sum(value_sum) AS agg_value_sum
    FROM trustiot.",
            $table,
            "
    WHERE tenant_id = {tenant:UUID}
        AND (device_id, point) IN {series:Array(Tuple(UUID, String))}
        AND bucket BETWEEN ",
            $bucket,
            "({first_ts:DateTime64(3, 'UTC')}) AND ",
            $bucket,
            "({last_ts:DateTime64(3, 'UTC')})
        AND gateway_id = {gateway:String}
        AND stream_id = {stream:String}
        AND received_day >= toDate(now('UTC')) - {window_days:UInt32}
    GROUP BY tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day
) AS a USING (tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day)
WHERE r.raw_readings != a.agg_readings
    OR r.raw_numeric_readings != a.agg_numeric_readings
    OR abs(r.raw_value_sum - a.agg_value_sum) > 1e-9 * greatest(abs(r.raw_value_sum), abs(a.agg_value_sum), 1)"
        )
    };
}

/// Corrects the one-minute aggregates of one stream's batch after an I1 rewrite (see the module
/// documentation).
pub const RECONCILE_1M: &str = reconcile!("telemetry_1m", "toStartOfMinute");

/// Corrects the one-hour aggregates of one stream's batch after an I1 rewrite.
pub const RECONCILE_1H: &str = reconcile!("telemetry_1h", "toStartOfHour");

/// The query parameters of [`RECONCILE_1M`] and [`RECONCILE_1H`] for one batch, as ClickHouse's
/// HTTP interface takes them (`param_<name>` in the URL).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconcileParams {
    /// The tenant the batch was written for.
    pub tenant: String,
    /// The gateway and the stream of the batch, as the telemetry table holds them.
    pub gateway: String,
    pub stream: String,
    /// Every distinct device and point of the batch, as `(device, point)` pairs.
    pub series: Vec<(String, String)>,
    /// The earliest and the latest reading time of the batch, `YYYY-MM-DD hh:mm:ss.sss` in UTC.
    pub first_ts: String,
    pub last_ts: String,
}

impl ReconcileParams {
    /// The parameters as `param_<name>` pairs, [`RECONCILE_WINDOW_DAYS`] included.
    #[must_use]
    pub fn to_query_pairs(&self) -> Vec<(&'static str, String)> {
        let series = self
            .series
            .iter()
            .map(|(device, point)| format!("({}, {})", quoted(device), quoted(point)))
            .collect::<Vec<_>>()
            .join(", ");
        vec![
            ("param_tenant", self.tenant.clone()),
            ("param_gateway", self.gateway.clone()),
            ("param_stream", self.stream.clone()),
            ("param_series", format!("[{series}]")),
            ("param_first_ts", self.first_ts.clone()),
            ("param_last_ts", self.last_ts.clone()),
            ("param_window_days", RECONCILE_WINDOW_DAYS.to_string()),
        ]
    }
}

/// A string in the text form of a ClickHouse query parameter of type Array(Tuple(...)): single
/// quotes, with backslashes and single quotes escaped.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The increase of a cumulative point over a range of buckets of a tenant view: `$view` is the
/// view (parameters at [`ENERGY_1M`]).
macro_rules! energy {
    ($view:literal) => {
        concat!(
            "SELECT sum(inside) + sum(across)
FROM
(
    SELECT
        bucket,
        if(first.value_type = 'int', toFloat64(first.value_int), first.value_float) AS first_value,
        if(last.value_type = 'int', toFloat64(last.value_int), last.value_float) AS last_value,
        if(last_value < value_max OR value_min < first_value,
            if({wraps_at:Float64} > 0, {wraps_at:Float64} - first_value + last_value, value_max - first_value + last_value),
            last_value - first_value) AS inside,
        lagInFrame(last_value) OVER (ORDER BY bucket ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS previous_last,
        row_number() OVER (ORDER BY bucket ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS position,
        if(position = 1, 0,
            if(first_value >= previous_last, first_value - previous_last,
                if({wraps_at:Float64} > 0, first_value + {wraps_at:Float64} - previous_last, first_value))) AS across
    FROM trustiot.",
            $view,
            "
    WHERE device_id = {device:UUID} AND point = {point:String}
        AND bucket >= {from:DateTime('UTC')} AND bucket < {to:DateTime('UTC')}
)"
        )
    };
}

/// A cumulative point's increase from the one-minute aggregates. Parameters: `device` (UUID),
/// `point` (String), `from` and `to` (DateTime, UTC: the buckets from `from` up to but not
/// including `to`), and `wraps_at` (Float64, the counter's wrap value, 0 for one that does not
/// wrap). The answer is the increase from the first reading of the first bucket to the last
/// reading of the last bucket, 0 without readings.
pub const ENERGY_1M: &str = energy!("tenant_telemetry_1m");

/// A cumulative point's increase from the one-hour aggregates.
pub const ENERGY_1H: &str = energy!("tenant_telemetry_1h");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_reconciles_differ_only_in_table_and_bucket() {
        assert_eq!(
            RECONCILE_1M
                .replace("telemetry_1m", "telemetry_1h")
                .replace("toStartOfMinute", "toStartOfHour"),
            RECONCILE_1H
        );
        assert!(RECONCILE_1M.starts_with("INSERT INTO trustiot.telemetry_1m "));
        assert!(RECONCILE_1H.contains("FROM trustiot.telemetry_1h\n"));
        for statement in [RECONCILE_1M, RECONCILE_1H] {
            assert!(statement.contains("SETTINGS insert_deduplicate = 0, join_use_nulls = 0"));
            assert!(!statement.contains(';'));
        }
    }

    #[test]
    fn parameters_are_quoted_for_the_array_type() {
        let params = ReconcileParams {
            tenant: "t".to_owned(),
            gateway: "g".to_owned(),
            stream: "s".to_owned(),
            series: vec![
                ("d1".to_owned(), "energy".to_owned()),
                ("d2".to_owned(), "it's \\ odd".to_owned()),
            ],
            first_ts: "2026-10-25 00:00:00.000".to_owned(),
            last_ts: "2026-10-25 00:05:00.000".to_owned(),
        };
        let pairs = params.to_query_pairs();
        assert!(pairs.contains(&(
            "param_series",
            "[('d1', 'energy'), ('d2', 'it\\'s \\\\ odd')]".to_owned()
        )));
        assert!(pairs.contains(&("param_window_days", "30".to_owned())));
        assert_eq!(pairs.len(), 7);
    }

    #[test]
    fn the_energy_queries_read_the_tenant_views() {
        assert_eq!(ENERGY_1M.replace("_1m", "_1h"), ENERGY_1H);
        assert!(ENERGY_1H.contains("FROM trustiot.tenant_telemetry_1h\n"));
    }
}
