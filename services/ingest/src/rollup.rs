//! The one-minute and one-hour aggregate tables (`trustiot.telemetry_1m`, `trustiot.telemetry_1h`,
//! migrations/clickhouse/0010_telemetry_rollups.sql; Laysi/trustiot-v2#141): the statements the
//! ingest service and the consumers of the aggregates run against them, and the backfill the
//! migration Job runs (tools/migrate compiles this file in as its `rollup` module). The
//! reasoning is in README.md, section 「彙總表」.
//!
//! # The I1 rewrite (contract for Laysi/trustiot-v2#641)
//!
//! Nothing in this repository writes, deletes or rewrites telemetry rows yet: the ingest
//! service's write path and its I1 rewrite are built in Laysi/trustiot-v2#641 (part of
//! Laysi/trustiot-v2#91), which must follow the steps below and merges after this file. Until
//! then every batch reaches ClickHouse as INSERTs only (the lab dataset loader included), and
//! the views aggregate each in full.
//!
//! The materialized views add every block inserted into `trustiot.telemetry` to the aggregates,
//! but a `DELETE FROM` does not reach them. When the ingest service rewrites a batch that has no
//! completion record (rebuild checklist I1), the rows of an earlier attempt that reached
//! ClickHouse stay counted in the aggregates, and an earlier attempt whose raw rows were written
//! but whose aggregate part was not (a crash between the two) is missing from them. So the
//! rewrite is, for a batch of one stream:
//!
//! 1. Run every statement that writes the stream with [`stream_settings`]: the INSERTs of every
//!    attempt (the first included) and steps 2 to 4 below. They carry the stream's own query ID
//!    ([`stream_query_id`]) with `replace_running_query = 1`, a `max_execution_time` of
//!    [`STATEMENT_MAX_SECONDS`], and `async_insert = 0`. A statement of an earlier attempt of the
//!    same stream that still runs on the server (its client gave up on it, or crashed) is then
//!    stopped before the new statement starts: ClickHouse 26.8 cancels the running query that
//!    has the same query ID and the same account, and waits up to
//!    `replace_running_query_max_wait_ms` ([`REPLACE_WAIT_MS`]) for it to end. The blocks it
//!    committed before it stopped stay and count like any other rows of the batch; nothing of it
//!    lands once it has ended. If it has not ended by then, the new statement fails with
//!    `QUERY_WITH_SAME_ID_IS_ALREADY_RUNNING`, as it does when another account runs a query with
//!    that ID: the old statement has been told to stop but keeps running, and keeps the query
//!    ID, until its next cancellation check, so the attempt stops there, without a completion
//!    record, and the next attempt waits for it again. The client's request timeout must be at
//!    least [`CLIENT_TIMEOUT_SECONDS`], longer than that wait and the run together, so the server
//!    has ended a statement before the client gives up on it. One ingest replica per stream at
//!    a time still holds; this makes sure no statement of an earlier attempt overlaps the next.
//!
//!    The INSERTs must be synchronous (`async_insert = 0`; the server's default is 1), because
//!    on ClickHouse 26.8 an asynchronous insert escapes this step. One that waits for its flush
//!    (`wait_for_async_insert = 1`) cannot be interrupted: a cancel or `max_execution_time` ends
//!    the statement only after its buffered rows have landed. One that does not wait gives up
//!    the query ID at once, so its rows and their view parts can land after the rewrite's
//!    DELETE, and the raw rows and the aggregates then both count them twice, consistently,
//!    where no reconcile can see it. Switching the write path to asynchronous inserts, for
//!    example for throughput, requires verifying this contract again first. With synchronous
//!    inserts each batch INSERT makes its own raw, one-minute and one-hour parts; what that
//!    costs in throughput is measured in the deferred verification of Laysi/trustiot-v2#641.
//!
//!    No service forwards a query ID that a caller supplies to ClickHouse. A statement held
//!    under a stream's query ID stalls that stream's writes until it ends; that fails safe (no
//!    completion record is written, and the batch is sent again). The gateway ID in the query
//!    ID comes from the topic the message arrived on (the gateway's certificate), never from the
//!    payload, so a gateway can name only its own streams.
//! 2. `DELETE FROM trustiot.telemetry` the batch's rows, synchronously
//!    (`lightweight_deletes_sync = 2`).
//! 3. Insert the whole batch with [`REWRITE_SETTINGS`] (`deduplicate_insert = 'disable'`). With
//!    deduplication on, a block whose content the table saw before the DELETE (the same rows, or
//!    the same `insert_deduplication_token`) is dropped without an error, and the batch's rows
//!    are lost. On ClickHouse 26.8 `insert_deduplicate = 0` does not turn it off:
//!    `deduplicate_insert` (default `enable`) supersedes it, for synchronous and asynchronous
//!    inserts alike, and consults it only when set to `backward_compatible_choice`.
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
//! another write to the same stream is in flight, which one replica per stream, one batch in
//! flight per stream (telemetry rule 2) and step 1 guarantee.
//!
//! The counts and the sum of a cell are corrected exactly. The minimum, maximum, first and last
//! of a cell are merged from the raw side and are not taken back: telemetry rule 2 resends a
//! batch unchanged, so the rows of an earlier attempt are rows of the rewritten batch and leave
//! those values as they are.
//!
//! The reconcile reaches back [`RECONCILE_WINDOW_DAYS`] (31) receive days, one more than a
//! gateway keeps an unacknowledged batch (owner decision of 2026-10-26): a batch first received
//! at 23:59 can be rewritten after the midnight that ends the buffer's last day for it (the time
//! a resend and the rewrite take, or a gateway clock that drifts), and a window of the buffer's
//! age alone would leave out the first attempt's receive day, whose cells would stay counted
//! twice. The raw data retention is at least two days longer than the window
//! (`retention::MIN_RAW_RETENTION_DAYS`, 33 days), so every raw row received within it is still
//! there: the two days keep a reconcile clear of raw rows that expire around midnight and of TTL
//! merges on a replica whose clock runs ahead, and a statement reads the parts the tables had
//! when it started. Older cells are never compared with raw rows that may have expired.
//!
//! # Backfill and checks
//!
//! The backfill views of 0010_telemetry_rollups.sql give, for a range of receive days, the
//! difference between the cells recomputed from the raw rows and the cells as they are, and
//! whether the aggregates hold the cell at all. They leave out every receive day before today -
//! `raw_retention_days` + 2 (UTC), on both sides, so a cell whose raw rows may have expired is
//! never compared.
//!
//! - [`BACKFILL_1M`] and [`BACKFILL_1H`] insert the cells the aggregates lack. The migration
//!   Job runs them day by day after it has applied the files and set the TTLs, to fill the
//!   aggregates from the rows the telemetry table held before the views existed. They add
//!   cells and never change one, so running them again, or after `trustiot_migrate.applied` was
//!   lost, takes nothing away, whatever the raw retention was.
//! - [`REPAIR_1M`] and [`REPAIR_1H`] insert every difference, a correction of a cell whose raw
//!   rows were deleted without a reconcile included: an administrator's repair of a range of
//!   days after a check (docs/runbooks/migrations.md). With a `raw_retention_days` longer than
//!   the raw rows were kept they erase cells whose raw rows expired, so the runbook says which
//!   value to pass.
//!
//! Both count a block whose raw part landed but whose view part had not yet as missing, so
//! nobody may write telemetry while they run: true when the Job first applies the file, since
//! no release before it has an ingest writer.
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
//! bucket holds at most one reset or wrap, and after it the bucket's last reading stays below
//! the peak before it.

/// How many receive days before today (UTC) the reconcile compares: defined with the raw data
/// retention it bounds (`retention::MIN_RAW_RETENTION_DAYS` is two days longer).
pub use crate::retention::RECONCILE_WINDOW_DAYS;

/// The settings of the rewrite's INSERT (step 3 of the module documentation), as name and value:
/// the batch's rows must land even when the table saw the same block before the DELETE.
pub const REWRITE_SETTINGS: [(&str, &str); 1] = [("deduplicate_insert", "disable")];

/// How long one statement that writes a stream may run on the server, in seconds:
/// `max_execution_time` in [`stream_settings`].
pub const STATEMENT_MAX_SECONDS: u32 = 20;

/// How long a statement that writes a stream waits for a still-running statement of the same
/// stream to stop, in milliseconds: `replace_running_query_max_wait_ms` in [`stream_settings`]
/// (ClickHouse's default, set so the contract does not depend on it).
pub const REPLACE_WAIT_MS: u32 = 5_000;

/// The shortest request timeout, in seconds, the ingest service's ClickHouse client may use for
/// the statements that write a stream: longer than [`REPLACE_WAIT_MS`] and
/// [`STATEMENT_MAX_SECONDS`] together, so the server has ended a statement before the client
/// gives up on it.
pub const CLIENT_TIMEOUT_SECONDS: u32 = 30;

/// The query ID every statement that writes one stream runs under (step 1 of the module
/// documentation). Gateway and stream IDs never contain `/` (the protocol's identifier rule), so
/// two streams never share one.
///
/// `gateway` is the gateway the message's topic names (its certificate), never an ID from the
/// payload. No service forwards a caller-supplied query ID to ClickHouse: a statement held under
/// a stream's ID stalls the stream's writes until it ends, which fails safe (no completion
/// record, and the batch is sent again).
#[must_use]
pub fn stream_query_id(gateway: &str, stream: &str) -> String {
    format!("trustiot-rewrite/{gateway}/{stream}")
}

/// The settings of every statement that writes one stream: each attempt's INSERT, the rewrite's
/// DELETE and INSERT and the reconcile (step 1 of the module documentation), as ClickHouse's HTTP
/// interface takes them in the URL: the stream's query ID ([`stream_query_id`], with the same
/// rules), `replace_running_query = 1` with a wait of [`REPLACE_WAIT_MS`], a
/// `max_execution_time` of [`STATEMENT_MAX_SECONDS`], and `async_insert = 0`. An asynchronous
/// insert is neither stopped by the replacement nor bounded by `max_execution_time` while it
/// waits for its flush, and one that does not wait lands after the statements that follow it;
/// switching to asynchronous inserts requires verifying step 1 again first.
///
/// Only the tests call it so far: no code writes telemetry yet. It is the contract for the
/// ingest service's write path and its I1 rewrite (Laysi/trustiot-v2#641), which must send every
/// statement of a stream with these settings, every INSERT and the rewrite's DELETE, INSERT and
/// reconcile, over a client whose request timeout is at least [`CLIENT_TIMEOUT_SECONDS`].
#[must_use]
pub fn stream_settings(gateway: &str, stream: &str) -> [(&'static str, String); 5] {
    [
        ("query_id", stream_query_id(gateway, stream)),
        ("replace_running_query", "1".to_owned()),
        (
            "replace_running_query_max_wait_ms",
            REPLACE_WAIT_MS.to_string(),
        ),
        ("max_execution_time", STATEMENT_MAX_SECONDS.to_string()),
        ("async_insert", "0".to_owned()),
    ]
}

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
SETTINGS deduplicate_insert_select = 'disable', join_use_nulls = 0
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

/// An INSERT into one aggregate table from its backfill view: `$table` is the table, `$filter`
/// what it takes of the view's rows.
macro_rules! from_backfill_view {
    ($table:literal, $filter:literal) => {
        concat!(
            "INSERT INTO trustiot.",
            $table,
            " (tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day,
    readings, numeric_readings, value_sum, value_min, value_max, first, last)
SETTINGS deduplicate_insert_select = 'disable'
SELECT tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day, readings,
    numeric_readings, value_sum, value_min, value_max, first, last
FROM trustiot.",
            $table,
            "_backfill(first_day = {first_day:Date}, last_day = {last_day:Date},
    raw_retention_days = {raw_retention_days:UInt32})",
            $filter
        )
    };
}

/// Inserts into the one-minute table the cells it lacks, recomputed from the raw rows, for the
/// receive days from `first_day` to `last_day` (Date) that are not before today -
/// `raw_retention_days` + 2 (UTC); `raw_retention_days` (UInt32) is the raw data retention,
/// never more than the telemetry table's TTL has been since the cells were written. It leaves
/// every cell the table holds as it is. The account that runs it must be the one the backfill
/// view's `migration_account` policy names, or it reads no rows and inserts nothing.
pub const BACKFILL_1M: &str = from_backfill_view!("telemetry_1m", "\nWHERE aggregated = 0");

/// The same for the one-hour table.
pub const BACKFILL_1H: &str = from_backfill_view!("telemetry_1h", "\nWHERE aggregated = 0");

/// Inserts into the one-minute table every difference between the cells recomputed from the raw
/// rows and the cells as they are, for the same receive days as [`BACKFILL_1M`] and with the
/// same parameters: the cells the table lacks, and the corrections of the cells it holds, down
/// to taking out a cell whose raw rows were deleted. A difference may equal an earlier one, so
/// ClickHouse must not drop it as a duplicate (`deduplicate_insert_select = 'disable'`; on 26.8
/// `insert_deduplicate` does not turn that off).
pub const REPAIR_1M: &str = from_backfill_view!("telemetry_1m", "");

/// The same for the one-hour table.
pub const REPAIR_1H: &str = from_backfill_view!("telemetry_1h", "");

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
            // A correction equal to an earlier one must not be dropped as a duplicate.
            assert!(
                statement
                    .contains("SETTINGS deduplicate_insert_select = 'disable', join_use_nulls = 0")
            );
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
        // One more receive day than the gateway buffer's 30 (owner decision of 2026-10-26).
        assert!(pairs.contains(&("param_window_days", "31".to_owned())));
        assert_eq!(pairs.len(), 7);
    }

    #[test]
    fn a_rewrite_statement_ends_on_the_server_before_its_client_gives_up() {
        let settings = stream_settings("gw-1", "s.2");
        assert_eq!(
            settings.each_ref().map(|(name, _)| *name),
            [
                "query_id",
                "replace_running_query",
                "replace_running_query_max_wait_ms",
                "max_execution_time",
                "async_insert"
            ]
        );
        assert_eq!(settings[0].1, "trustiot-rewrite/gw-1/s.2");
        assert_eq!(settings[1].1, "1");
        assert_eq!(settings[2].1.parse::<u32>().unwrap(), REPLACE_WAIT_MS);
        assert_eq!(settings[3].1.parse::<u32>().unwrap(), STATEMENT_MAX_SECONDS);
        // Synchronous: an asynchronous insert escapes the replacement and the time limit.
        assert_eq!(settings[4].1, "0");
        assert!(
            u64::from(REPLACE_WAIT_MS) + 1_000 * u64::from(STATEMENT_MAX_SECONDS)
                < 1_000 * u64::from(CLIENT_TIMEOUT_SECONDS),
            "the wait and the run together must end before the client's timeout"
        );
    }

    #[test]
    fn every_stream_has_its_own_query_id() {
        // The identifier rule allows '.', '_' and '-' but not '/', so moving a separator between
        // the gateway and the stream ID never gives another stream's ID.
        let ids = [
            stream_query_id("a-b", "c"),
            stream_query_id("a", "b-c"),
            stream_query_id("a", "b"),
            stream_query_id("b", "a"),
        ];
        let distinct: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(distinct.len(), ids.len(), "{ids:?}");
    }

    #[test]
    fn the_backfills_and_repairs_differ_only_in_table_and_filter() {
        assert_eq!(
            BACKFILL_1M.replace("telemetry_1m", "telemetry_1h"),
            BACKFILL_1H
        );
        assert_eq!(REPAIR_1M.replace("telemetry_1m", "telemetry_1h"), REPAIR_1H);
        assert!(BACKFILL_1M.starts_with("INSERT INTO trustiot.telemetry_1m "));
        assert!(BACKFILL_1H.contains("FROM trustiot.telemetry_1h_backfill(first_day = "));
        for (backfill, repair) in [(BACKFILL_1M, REPAIR_1M), (BACKFILL_1H, REPAIR_1H)] {
            // The backfill only adds the cells the table lacks.
            assert_eq!(backfill, format!("{repair}\nWHERE aggregated = 0"));
            assert!(repair.contains("SETTINGS deduplicate_insert_select = 'disable'\n"));
            assert!(repair.ends_with("raw_retention_days = {raw_retention_days:UInt32})"));
            assert!(!backfill.contains(';'));
        }
    }

    #[test]
    fn the_energy_queries_read_the_tenant_views() {
        assert_eq!(ENERGY_1M.replace("_1m", "_1h"), ENERGY_1H);
        assert!(ENERGY_1H.contains("FROM trustiot.tenant_telemetry_1h\n"));
    }
}
