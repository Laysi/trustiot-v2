//! The telemetry path's ClickHouse statements (services/ingest/README.md, "寫入流程" and
//! "I1 重寫與對帳"). Every statement here reads or writes one stream, and every one carries that
//! stream's settings ([`rollup::stream_settings`]): its own query ID with
//! `replace_running_query`, `max_execution_time` and synchronous inserts. A statement of an
//! earlier attempt at the same stream that still runs on the server is stopped before the next
//! one starts, so no two attempts' statements overlap, whichever replica made them.

use std::collections::BTreeSet;
use std::fmt;

use crate::clickhouse::{ClickHouse, QueryError, Request};
use crate::rollup::{self, RECONCILE_1H, RECONCILE_1M, REWRITE_SETTINGS, ReconcileParams};
use crate::telemetry::batch::Prepared;

/// A stream's high-water mark (the highest committed sequence number, 0 without a record) and,
/// when the asked sequence number has a record, the rows it was committed with. Keyed by the
/// topic's gateway and the stream (telemetry rule 3); a record may be there twice
/// (`ReplacingMergeTree` merges only eventually), which neither value minds.
const HIGH_WATER_MARK: &str =
    "SELECT max(seq), anyIf(row_count, seq = {seq:UInt64}), countIf(seq = {seq:UInt64})
FROM trustiot.stream_commits
WHERE gateway_id = {gateway:String} AND stream_id = {stream:String}
FORMAT TabSeparated";

/// The tenants of the rows an earlier attempt of the batch left. Any tenant: the gateway may have
/// been bound to another tenant since that attempt, and its rows must not stay next to this
/// attempt's.
///
/// The condition is the batch's gateway, stream and sequence number, bounded by what the table
/// can prune with. Every row of the batch has one of its (device, point) pairs and a reading time
/// between its first and last: the second to fourth columns of the sort key `(tenant_id,
/// device_id, point, ts)`, which ClickHouse uses without the first (generic exclusion search,
/// effective because a tenant's devices are few next to its granules), so the statement reads
/// the granules of those devices around those times. And every earlier attempt was received
/// within the reconcile window of src/rollup.rs (one day more than a gateway keeps a batch), the
/// bound on `received_at` that selects partitions. [`DELETE_ROWS`] has the same condition.
const LEFTOVER_TENANTS: &str = "SELECT DISTINCT tenant_id FROM trustiot.telemetry
WHERE (device_id, point) IN {series:Array(Tuple(UUID, String))}
    AND ts BETWEEN {first_ts:DateTime64(3, 'UTC')} AND {last_ts:DateTime64(3, 'UTC')}
    AND gateway_id = {gateway:String}
    AND stream_id = {stream:String}
    AND batch_seq = {seq:UInt64}
    AND received_at >= toDateTime64(toDate(now('UTC')) - {window_days:UInt32}, 3, 'UTC')
ORDER BY tenant_id
FORMAT TabSeparated";

/// Deletes the rows an earlier attempt of the batch left (lightweight delete, synchronous with
/// [`DELETE_SETTINGS`]), with the condition of [`LEFTOVER_TENANTS`].
const DELETE_ROWS: &str = "DELETE FROM trustiot.telemetry
WHERE (device_id, point) IN {series:Array(Tuple(UUID, String))}
    AND ts BETWEEN {first_ts:DateTime64(3, 'UTC')} AND {last_ts:DateTime64(3, 'UTC')}
    AND gateway_id = {gateway:String}
    AND stream_id = {stream:String}
    AND batch_seq = {seq:UInt64}
    AND received_at >= toDateTime64(toDate(now('UTC')) - {window_days:UInt32}, 3, 'UTC')";

/// How many rows of the batch the table holds, for any tenant, with the condition of
/// [`LEFTOVER_TENANTS`]: compared with the batch's record count right before the completion
/// record is written.
const BATCH_ROWS: &str = "SELECT count() FROM trustiot.telemetry
WHERE (device_id, point) IN {series:Array(Tuple(UUID, String))}
    AND ts BETWEEN {first_ts:DateTime64(3, 'UTC')} AND {last_ts:DateTime64(3, 'UTC')}
    AND gateway_id = {gateway:String}
    AND stream_id = {stream:String}
    AND batch_seq = {seq:UInt64}
    AND received_at >= toDateTime64(toDate(now('UTC')) - {window_days:UInt32}, 3, 'UTC')
FORMAT TabSeparated";

/// Waits until the deleted rows are gone on every replica before the statement answers.
const DELETE_SETTINGS: (&str, &str) = ("lightweight_deletes_sync", "2");

/// The batch's rows, as `JSONEachRow` in the body.
const INSERT_ROWS: &str = "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, stream_id, batch_seq, device_id, point, ts, received_at, value_type, value_float, value_int, value_bool, value_string) FORMAT JSONEachRow";

/// Parses floats exactly (the server's default is a faster parser that may be off in the last
/// bit), so a reading lands with the value the gateway sent.
const PRECISE_FLOATS: (&str, &str) = ("precise_float_parsing", "1");

/// The completion record, as `JSONEachRow` in the body.
const INSERT_COMMIT: &str = "INSERT INTO trustiot.stream_commits (tenant_id, binding_id, gateway_id, stream_id, seq, row_count, committed_at) FORMAT JSONEachRow";

/// Which statement failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    HighWaterMark,
    LeftoverRows,
    Delete,
    Insert,
    Reconcile1m,
    Reconcile1h,
    RowCount,
    Commit,
}

impl Step {
    /// The step's name in logs and metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HighWaterMark => "high_water_mark",
            Self::LeftoverRows => "leftover_rows",
            Self::Delete => "delete",
            Self::Insert => "insert",
            Self::Reconcile1m => "reconcile_1m",
            Self::Reconcile1h => "reconcile_1h",
            Self::RowCount => "row_count",
            Self::Commit => "commit",
        }
    }
}

/// A statement that failed, safe to log: the step and ClickHouse's error code, never the
/// response body (it can quote the statement's parameters).
#[derive(Debug)]
pub struct StoreError {
    pub step: Step,
    pub code: Option<u32>,
    pub unreachable: bool,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.unreachable, self.code) {
            (true, _) => write!(f, "{}: no response from ClickHouse", self.step.as_str()),
            (false, Some(code)) => write!(f, "{}: ClickHouse error {code}", self.step.as_str()),
            (false, None) => write!(f, "{}: ClickHouse refused", self.step.as_str()),
        }
    }
}

impl std::error::Error for StoreError {}

/// A stream's high-water mark and the record of one sequence number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HighWaterMark {
    /// The highest committed sequence number of the stream, 0 without a record.
    pub max_seq: u64,
    /// The rows the asked sequence number was committed with, when it has a record.
    pub committed_rows: Option<u64>,
}

/// The telemetry statements of one batch.
pub struct Store<'a> {
    clickhouse: &'a ClickHouse,
    batch: &'a Prepared,
    settings: Vec<(&'static str, String)>,
    /// The batch's series and reading times in parts that each fit one URL field
    /// ([`ReconcileParams::split`]), without a tenant: every statement that names the batch's
    /// series runs once per part.
    parts: Vec<ReconcileParams>,
}

impl<'a> Store<'a> {
    #[must_use]
    pub fn new(clickhouse: &'a ClickHouse, batch: &'a Prepared) -> Self {
        Self {
            clickhouse,
            batch,
            parts: batch.reconcile_params("").split(),
            settings: rollup::stream_settings(batch.gateway.as_str(), batch.stream.as_str())
                .into_iter()
                .collect(),
        }
    }

    /// The stream's settings plus `extra`.
    fn settings_with(&self, extra: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
        let mut settings = self.settings.clone();
        settings.extend(
            extra
                .iter()
                .map(|(name, value)| (*name, (*value).to_owned())),
        );
        settings
    }

    async fn run(
        &self,
        step: Step,
        statement: &str,
        params: &[(&str, &str)],
        settings: &[(&'static str, String)],
        data: Option<&str>,
    ) -> Result<String, StoreError> {
        self.clickhouse
            .execute(&Request {
                statement,
                params,
                settings,
                data,
            })
            .await
            .map_err(|error| StoreError {
                step,
                code: error.code(),
                unreachable: matches!(error, QueryError::Unreachable(_)),
            })
    }

    fn malformed(step: Step) -> StoreError {
        StoreError {
            step,
            code: None,
            unreachable: false,
        }
    }

    /// The stream's high-water mark and the record of the batch's sequence number.
    ///
    /// # Errors
    ///
    /// When the statement fails or answers something else than three numbers.
    pub async fn high_water_mark(&self) -> Result<HighWaterMark, StoreError> {
        let seq = self.batch.seq.get().to_string();
        let params = [
            ("gateway", self.batch.gateway.as_str()),
            ("stream", self.batch.stream.as_str()),
            ("seq", seq.as_str()),
        ];
        let answer = self
            .run(
                Step::HighWaterMark,
                HIGH_WATER_MARK,
                &params,
                &self.settings,
                None,
            )
            .await?;
        let numbers: Vec<u64> = answer
            .trim()
            .split('\t')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|_| Self::malformed(Step::HighWaterMark))?;
        let [max_seq, row_count, records] = numbers[..] else {
            return Err(Self::malformed(Step::HighWaterMark));
        };
        Ok(HighWaterMark {
            max_seq,
            committed_rows: (records > 0).then_some(row_count),
        })
    }

    /// The parameters of [`LEFTOVER_TENANTS`], [`DELETE_ROWS`] and [`BATCH_ROWS`] for one part of
    /// the batch's series: those series and the batch's reading times as the reconcile takes
    /// them, its stream and sequence number, and the window.
    fn condition_params(&self, part: &ReconcileParams) -> Vec<(&'static str, String)> {
        // The tenant is not part of the condition.
        let mut params: Vec<(&'static str, String)> = part
            .to_query_pairs()
            .into_iter()
            .filter(|(name, _)| *name != "param_tenant")
            .map(|(name, value)| (name.strip_prefix("param_").unwrap_or(name), value))
            .collect();
        params.push(("seq", self.batch.seq.get().to_string()));
        params
    }

    /// Runs `statement` with the condition's parameters once per part of the batch's series,
    /// and returns the answers in order.
    async fn run_per_part(
        &self,
        step: Step,
        statement: &str,
        settings: &[(&'static str, String)],
    ) -> Result<Vec<String>, StoreError> {
        let mut answers = Vec::with_capacity(self.parts.len());
        for part in &self.parts {
            let owned = self.condition_params(part);
            let params: Vec<(&str, &str)> = owned
                .iter()
                .map(|(name, value)| (*name, value.as_str()))
                .collect();
            answers.push(self.run(step, statement, &params, settings, None).await?);
        }
        Ok(answers)
    }

    /// The tenants whose rows of the batch an earlier attempt left, none when there are none.
    ///
    /// # Errors
    ///
    /// When a statement fails or answers something else than tenant IDs.
    pub async fn leftover_tenants(&self) -> Result<Vec<String>, StoreError> {
        let answers = self
            .run_per_part(Step::LeftoverRows, LEFTOVER_TENANTS, &self.settings)
            .await?;
        let mut tenants = BTreeSet::new();
        for line in answers.iter().flat_map(|answer| answer.lines()) {
            let tenant =
                uuid::Uuid::try_parse(line).map_err(|_| Self::malformed(Step::LeftoverRows))?;
            tenants.insert(tenant.hyphenated().to_string());
        }
        Ok(tenants.into_iter().collect())
    }

    /// Deletes the rows of the batch that an earlier attempt left, for any tenant, and returns
    /// once they are gone.
    ///
    /// # Errors
    ///
    /// When a statement fails.
    pub async fn delete_rows(&self) -> Result<(), StoreError> {
        let settings = self.settings_with(&[DELETE_SETTINGS]);
        self.run_per_part(Step::Delete, DELETE_ROWS, &settings)
            .await
            .map(drop)
    }

    /// How many rows of the batch the table holds, for any tenant.
    ///
    /// # Errors
    ///
    /// When a statement fails or answers something else than a count.
    pub async fn batch_rows(&self) -> Result<u64, StoreError> {
        let answers = self
            .run_per_part(Step::RowCount, BATCH_ROWS, &self.settings)
            .await?;
        answers.iter().try_fold(0_u64, |total, answer| {
            answer
                .trim()
                .parse::<u64>()
                .ok()
                .and_then(|rows| total.checked_add(rows))
                .ok_or_else(|| Self::malformed(Step::RowCount))
        })
    }

    /// Inserts the batch's rows (`rows`, from [`Prepared::rows`]) in one statement, with
    /// deduplication off ([`REWRITE_SETTINGS`]): a block the table saw before a delete must land
    /// again, and the high-water mark, not the deduplication window, keeps a batch from being
    /// written twice.
    ///
    /// # Errors
    ///
    /// When the statement fails.
    pub async fn insert_rows(&self, rows: &str) -> Result<(), StoreError> {
        let mut extra: Vec<(&'static str, &str)> = REWRITE_SETTINGS.to_vec();
        extra.push(PRECISE_FLOATS);
        let settings = self.settings_with(&extra);
        self.run(Step::Insert, INSERT_ROWS, &[], &settings, Some(rows))
            .await
            .map(drop)
    }

    /// Runs [`RECONCILE_1M`] and [`RECONCILE_1H`] for the batch written for `tenant`, once per
    /// part of its series.
    ///
    /// # Errors
    ///
    /// When a statement fails.
    pub async fn reconcile(&self, tenant: &str) -> Result<(), StoreError> {
        for part in &self.parts {
            let part = ReconcileParams {
                tenant: tenant.to_owned(),
                ..part.clone()
            };
            let pairs = part.to_query_pairs();
            // to_query_pairs names them as the URL does (`param_<name>`); execute adds the
            // prefix.
            let params: Vec<(&str, &str)> = pairs
                .iter()
                .map(|(name, value)| (name.strip_prefix("param_").unwrap_or(name), value.as_str()))
                .collect();
            for (step, statement) in [
                (Step::Reconcile1m, RECONCILE_1M),
                (Step::Reconcile1h, RECONCILE_1H),
            ] {
                self.run(step, statement, &params, &self.settings, None)
                    .await?;
            }
        }
        Ok(())
    }

    /// Writes the completion record: `record` is its `JSONEachRow` line.
    ///
    /// # Errors
    ///
    /// When the statement fails.
    pub async fn commit(&self, record: &str) -> Result<(), StoreError> {
        self.run(
            Step::Commit,
            INSERT_COMMIT,
            &[],
            &self.settings,
            Some(record),
        )
        .await
        .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_batch_condition_bounds_the_rows_read() {
        // The three statements have the same condition.
        let condition = |statement: &'static str| {
            let start = statement.find("WHERE").unwrap();
            statement
                .get(start..)
                .unwrap()
                .trim_end_matches("\nFORMAT TabSeparated")
                .trim_end_matches("\nORDER BY tenant_id")
        };
        assert_eq!(condition(LEFTOVER_TENANTS), condition(DELETE_ROWS));
        assert_eq!(condition(BATCH_ROWS), condition(DELETE_ROWS));
        assert!(BATCH_ROWS.starts_with("SELECT count() FROM trustiot.telemetry\nWHERE "));
        // Any tenant's rows: the gateway may have been bound to another one since.
        assert!(!DELETE_ROWS.contains("tenant_id"));
        assert!(DELETE_ROWS.contains("(device_id, point) IN {series:"));
        assert!(DELETE_ROWS.contains("ts BETWEEN {first_ts:"));
        assert!(DELETE_ROWS.contains("received_at >= toDateTime64("));
        assert!(DELETE_ROWS.starts_with("DELETE FROM trustiot.telemetry\n"));
    }
}
