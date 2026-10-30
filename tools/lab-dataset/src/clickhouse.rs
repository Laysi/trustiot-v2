//! Writing to and reading from ClickHouse's telemetry table and batch completion table
//! (services/ingest/migrations/clickhouse/0002_telemetry.sql, 0012_gateway_stream_ids.sql) over
//! the HTTP interface, with the migration Job's HTTP client: `http://`, or `https://` trusting
//! only a named CA (`migrate::http`).
//!
//! [`ClickHouse::commit_batch`] is the only code that writes a completion record: when the
//! completion table's key or columns change, it changes there (and in the ingest service),
//! nowhere else. The reads (`lab-dataset export`) are the two `SELECT` statements below, static
//! text with query parameters; [`ClickHouse::select`] refuses any other statement.
//!
//! The gateway and stream IDs are UUIDs written as text, which the protocol's ID rule accepts
//! (the same text 0012_gateway_stream_ids.sql gives the rows written before it).

use std::fmt::{self, Display};
use std::path::Path;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use migrate::http;
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use crate::Error;
use crate::index::ValueType;
use crate::segment::Value;
use crate::time;

/// Inserts one batch's rows. The statement goes in the URL (`query`), the rows in the body.
const INSERT_TELEMETRY: &str = "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, \
     stream_id, batch_seq, device_id, point, ts, received_at, value_type, value_float, \
     value_int, value_bool, value_string) FORMAT JSONEachRow";

/// Inserts one batch's completion record into the table keyed by gateway plus stream.
const INSERT_BATCH_COMMIT: &str = "INSERT INTO trustiot.stream_commits (tenant_id, binding_id, \
     gateway_id, stream_id, seq, row_count, committed_at) FORMAT JSONEachRow";

/// Reads the rows one tenant's gateway sent that were received in a time window (`received_at`
/// from `from_ms` up to, not including, `to_ms`, Unix milliseconds), in arrival order. No
/// setting is sent with the read: an account with `readonly` may not change one, so the 64-bit
/// integers come back as ClickHouse quotes them by default and [`StoredRow`] reads either form.
const SELECT_TELEMETRY: &str = "SELECT stream_id, batch_seq, toString(device_id) AS device_id, \
     point, toUnixTimestamp64Milli(ts) AS ts_ms, \
     toUnixTimestamp64Milli(received_at) AS received_ms, toString(value_type) AS value_type, \
     value_float, value_int, value_bool, value_string \
     FROM trustiot.telemetry \
     WHERE tenant_id = {tenant:UUID} AND gateway_id = {gateway:String} \
     AND received_at >= fromUnixTimestamp64Milli({from_ms:Int64}, 'UTC') \
     AND received_at < fromUnixTimestamp64Milli({to_ms:Int64}, 'UTC') \
     ORDER BY received_at, stream_id, batch_seq, device_id, point, ts \
     FORMAT JSONEachRow";

/// Reads the completion records of one stream's batches `first` to `last`.
const SELECT_COMMITS: &str = "SELECT seq, row_count FROM trustiot.stream_commits \
     WHERE tenant_id = {tenant:UUID} AND gateway_id = {gateway:String} \
     AND stream_id = {stream:String} AND seq >= {first:UInt64} AND seq <= {last:UInt64} \
     ORDER BY seq FORMAT JSONEachRow";

/// A number that ClickHouse writes as a JSON number or, for 64-bit integers, as a string.
fn number_or_text<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + FromStr,
    T::Err: Display,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either<T> {
        Number(T),
        Text(String),
    }
    match Either::<T>::deserialize(deserializer)? {
        Either::Number(number) => Ok(number),
        Either::Text(text) => text.parse().map_err(serde::de::Error::custom),
    }
}

/// One row of the telemetry table as [`ClickHouse::telemetry_rows`] reads it. A float that is
/// not a finite number (ClickHouse writes it as `null`) reads as `None`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredRow {
    pub stream_id: String,
    #[serde(deserialize_with = "number_or_text")]
    pub batch_seq: u64,
    pub device_id: Uuid,
    pub point: String,
    #[serde(deserialize_with = "number_or_text")]
    pub ts_ms: i64,
    #[serde(deserialize_with = "number_or_text")]
    pub received_ms: i64,
    pub value_type: ValueType,
    pub value_float: Option<f64>,
    #[serde(deserialize_with = "number_or_text")]
    pub value_int: i64,
    pub value_bool: bool,
    pub value_string: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCommit {
    #[serde(deserialize_with = "number_or_text")]
    seq: u64,
    #[serde(deserialize_with = "number_or_text")]
    row_count: u64,
}

/// ClickHouse error texts are cut to this many characters in errors.
const ERROR_TEXT_LIMIT: usize = 2000;

/// An account on a ClickHouse server: where the loader writes.
pub struct ClickHouse {
    client: http::Client,
    url: String,
    user: String,
    password: String,
}

impl fmt::Debug for ClickHouse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClickHouse")
            .field("url", &self.url)
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

/// One row of `trustiot.telemetry` in `JSONEachRow`. Every value column is written; the ones
/// that do not hold the reading carry the column default, as `value_matches_type` requires.
#[derive(Serialize)]
pub(crate) struct TelemetryRow<'a> {
    pub tenant_id: Uuid,
    pub gateway_id: Uuid,
    pub stream_id: Uuid,
    pub batch_seq: u64,
    pub device_id: Uuid,
    pub point: &'a str,
    pub ts: String,
    pub received_at: String,
    pub value_type: &'static str,
    pub value_float: f64,
    pub value_int: i64,
    pub value_bool: bool,
    pub value_string: &'a str,
}

impl<'a> TelemetryRow<'a> {
    #[expect(
        clippy::too_many_arguments,
        reason = "one argument per column the dataset does not carry itself"
    )]
    pub(crate) fn new(
        tenant_id: Uuid,
        gateway_id: Uuid,
        stream_id: Uuid,
        batch_seq: u64,
        device_id: Uuid,
        point: &'a str,
        ts: DateTime<Utc>,
        received_at: DateTime<Utc>,
        value: &'a Value,
    ) -> Self {
        let mut row = Self {
            tenant_id,
            gateway_id,
            stream_id,
            batch_seq,
            device_id,
            point,
            ts: time::clickhouse(ts),
            received_at: time::clickhouse(received_at),
            value_type: value.value_type().as_str(),
            value_float: 0.0,
            value_int: 0,
            value_bool: false,
            value_string: "",
        };
        match value {
            Value::Float(float) => row.value_float = *float,
            Value::Int(int) => row.value_int = *int,
            Value::Bool(flag) => row.value_bool = *flag,
            Value::String(text) => row.value_string = text,
        }
        row
    }
}

/// A batch's completion record (`trustiot.stream_commits`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchCommit {
    pub tenant_id: Uuid,
    /// The gateway binding the batch was written under; never the nil UUID (`binding_id_set`).
    pub binding_id: Uuid,
    pub gateway_id: Uuid,
    pub stream_id: Uuid,
    pub seq: u64,
    pub row_count: u64,
    /// When every row of the batch was on disk; never earlier than the rows' `received_at`.
    pub committed_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct BatchCommitRow {
    tenant_id: Uuid,
    binding_id: Uuid,
    gateway_id: Uuid,
    stream_id: Uuid,
    seq: u64,
    row_count: u64,
    committed_at: String,
}

/// `JSONEachRow` text of `rows`: one JSON object per line.
pub(crate) fn json_lines<T: Serialize>(rows: &[T]) -> Result<String, Error> {
    let mut body = String::new();
    for row in rows {
        let line = serde_json::to_string(row)
            .map_err(|error| Error::ClickHouse(format!("cannot encode a row: {error}")))?;
        body.push_str(&line);
        body.push('\n');
    }
    Ok(body)
}

/// The rows of a `JSONEachRow` answer, one per line.
fn json_lines_of<T: for<'de> Deserialize<'de>>(body: &str) -> Result<Vec<T>, Error> {
    body.lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(number, line)| {
            serde_json::from_str(line).map_err(|error| {
                Error::ClickHouse(format!("row {} of the answer: {error}", number + 1))
            })
        })
        .collect()
}

impl ClickHouse {
    /// An account on the server at `url` (`http://host:port` or `https://host:port`, no
    /// credentials in it). An `https://` URL needs `ca_file`, the only CA the client trusts;
    /// an `http://` URL refuses one, so a CA never silently goes unused.
    ///
    /// # Errors
    ///
    /// When the URL is neither form, carries credentials, a query or a fragment, or the CA file
    /// cannot be used. The messages never repeat the URL, which could hold a password.
    pub fn new(
        url: &str,
        user: &str,
        password: &str,
        ca_file: Option<&Path>,
    ) -> Result<Self, Error> {
        let url = url.trim_end_matches('/');
        let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
        // The authority ends at the first `/`, `?` or `#`; `user:password@` would sit inside it.
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        if authority.contains('@') {
            return Err(Error::ClickHouse(
                "the ClickHouse URL must not carry credentials (user:password@); they come from \
                 the user and password settings"
                    .to_owned(),
            ));
        }
        if authority.is_empty() {
            return Err(Error::ClickHouse(
                "the ClickHouse URL has no host".to_owned(),
            ));
        }
        if rest.contains(['?', '#']) {
            return Err(Error::ClickHouse(
                "the ClickHouse URL must not carry a query or a fragment".to_owned(),
            ));
        }
        let client = match (Some(scheme).filter(|scheme| !scheme.is_empty()), ca_file) {
            (Some("https"), Some(ca)) => {
                http::trusting(ca).map_err(|error| Error::ClickHouse(error.to_string()))?
            }
            (Some("https"), None) => {
                return Err(Error::ClickHouse(
                    "an https:// ClickHouse needs a CA file, the only CA the loader trusts"
                        .to_owned(),
                ));
            }
            (Some("http"), None) => http::plain(),
            (Some("http"), Some(_)) => {
                return Err(Error::ClickHouse(
                    "a CA file was given for an http:// URL; use https:// or drop the CA file"
                        .to_owned(),
                ));
            }
            _ => {
                return Err(Error::ClickHouse(
                    "the ClickHouse URL is not http:// or https://".to_owned(),
                ));
            }
        };
        if user.is_empty() {
            return Err(Error::ClickHouse("the ClickHouse user is empty".to_owned()));
        }
        Ok(Self {
            client,
            url: format!("{url}/"),
            user: user.to_owned(),
            password: password.to_owned(),
        })
    }

    /// Runs one `INSERT ... FORMAT JSONEachRow` with `body` as its data, synchronously: no
    /// asynchronous insert, so the call returns once the rows are written, in call order.
    async fn insert(&self, statement: &'static str, body: String) -> Result<(), Error> {
        let response = self
            .client
            .post(
                &self.url,
                &[
                    ("query", statement),
                    ("async_insert", "0"),
                    ("wait_end_of_query", "1"),
                ],
                http::Auth::Basic {
                    username: &self.user,
                    password: &self.password,
                },
                body,
            )
            .await
            .map_err(|error| Error::ClickHouse(error.to_string()))?;
        if response.status.is_success() {
            Ok(())
        } else {
            Err(Error::ClickHouse(format!(
                "HTTP {}: {}",
                response.status.as_u16(),
                response
                    .body
                    .chars()
                    .take(ERROR_TEXT_LIMIT)
                    .collect::<String>()
            )))
        }
    }

    /// Runs one static `SELECT` and returns its `JSONEachRow` text. `parameters` are the values
    /// of the statement's `{name:Type}` placeholders, sent as query parameters, never spliced
    /// into the text.
    async fn select(
        &self,
        statement: &'static str,
        parameters: &[(&str, &str)],
    ) -> Result<String, Error> {
        if !statement.starts_with("SELECT ") {
            return Err(Error::ClickHouse(
                "only a SELECT statement is sent to read".to_owned(),
            ));
        }
        let names: Vec<String> = parameters
            .iter()
            .map(|(name, _)| format!("param_{name}"))
            .collect();
        let query: Vec<(&str, &str)> = names
            .iter()
            .map(String::as_str)
            .zip(parameters.iter().map(|(_, value)| *value))
            .collect();
        let response = self
            .client
            .post(
                &self.url,
                &query,
                http::Auth::Basic {
                    username: &self.user,
                    password: &self.password,
                },
                statement.to_owned(),
            )
            .await
            .map_err(|error| Error::ClickHouse(error.to_string()))?;
        if response.status.is_success() {
            Ok(response.body)
        } else {
            Err(Error::ClickHouse(format!(
                "HTTP {}: {}",
                response.status.as_u16(),
                response
                    .body
                    .chars()
                    .take(ERROR_TEXT_LIMIT)
                    .collect::<String>()
            )))
        }
    }

    /// The rows `gateway` of `tenant` sent that were received from `from` up to, not
    /// including, `to`, in arrival order.
    pub(crate) async fn telemetry_rows(
        &self,
        tenant: Uuid,
        gateway: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<StoredRow>, Error> {
        let body = self
            .select(
                SELECT_TELEMETRY,
                &[
                    ("tenant", &tenant.to_string()),
                    ("gateway", gateway),
                    ("from_ms", &from.timestamp_millis().to_string()),
                    ("to_ms", &to.timestamp_millis().to_string()),
                ],
            )
            .await?;
        json_lines_of(&body)
    }

    /// The completion records of `stream`'s batches `first` to `last`, as (sequence number, rows
    /// the batch wrote). A batch written twice (a replacing table keeps both until a merge) shows
    /// up twice.
    pub(crate) async fn commits(
        &self,
        tenant: Uuid,
        gateway: &str,
        stream: &str,
        first: u64,
        last: u64,
    ) -> Result<Vec<(u64, u64)>, Error> {
        let body = self
            .select(
                SELECT_COMMITS,
                &[
                    ("tenant", &tenant.to_string()),
                    ("gateway", gateway),
                    ("stream", stream),
                    ("first", &first.to_string()),
                    ("last", &last.to_string()),
                ],
            )
            .await?;
        Ok(json_lines_of::<StoredCommit>(&body)?
            .into_iter()
            .map(|commit| (commit.seq, commit.row_count))
            .collect())
    }

    /// Writes the rows of one batch in one INSERT, as the ingest service writes a batch.
    pub(crate) async fn write_rows(&self, rows: &[TelemetryRow<'_>]) -> Result<(), Error> {
        self.insert(INSERT_TELEMETRY, json_lines(rows)?).await
    }

    /// Writes a batch's completion record, after its rows.
    pub(crate) async fn commit_batch(&self, commit: &BatchCommit) -> Result<(), Error> {
        let row = BatchCommitRow {
            tenant_id: commit.tenant_id,
            binding_id: commit.binding_id,
            gateway_id: commit.gateway_id,
            stream_id: commit.stream_id,
            seq: commit.seq,
            row_count: commit.row_count,
            committed_at: time::clickhouse(commit.committed_at),
        };
        self.insert(INSERT_BATCH_COMMIT, json_lines(&[row])?).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_fill_only_their_value_column() {
        let id = Uuid::from_u128(1);
        let ts = time::parse("2026-01-05T22:00:00.120Z").unwrap();
        let text = Value::String("auto".to_owned());
        let row = TelemetryRow::new(id, id, id, 7, id, "mode", ts, ts, &text);
        let line = json_lines(&[row]).unwrap();
        assert_eq!(
            line,
            "{\"tenant_id\":\"00000000-0000-0000-0000-000000000001\",\
             \"gateway_id\":\"00000000-0000-0000-0000-000000000001\",\
             \"stream_id\":\"00000000-0000-0000-0000-000000000001\",\"batch_seq\":7,\
             \"device_id\":\"00000000-0000-0000-0000-000000000001\",\"point\":\"mode\",\
             \"ts\":\"2026-01-05 22:00:00.120\",\"received_at\":\"2026-01-05 22:00:00.120\",\
             \"value_type\":\"string\",\"value_float\":0.0,\"value_int\":0,\
             \"value_bool\":false,\"value_string\":\"auto\"}\n"
        );
        let float = Value::Float(15_230.4);
        let row = TelemetryRow::new(id, id, id, 7, id, "energy", ts, ts, &float);
        assert_eq!(row.value_type, "float");
        assert!((row.value_float - 15_230.4).abs() < f64::EPSILON);
        assert_eq!(
            (row.value_int, row.value_bool, row.value_string),
            (0, false, "")
        );
    }

    #[test]
    fn the_read_statements_are_selects_with_bound_parameters() {
        for statement in [SELECT_TELEMETRY, SELECT_COMMITS] {
            assert!(statement.starts_with("SELECT "), "{statement}");
            assert!(statement.ends_with("FORMAT JSONEachRow"), "{statement}");
            assert!(statement.contains("{tenant:UUID}"), "{statement}");
            // Nothing that writes, changes a setting or switches the role.
            let upper = statement.to_uppercase();
            for word in [
                "INSERT", "ALTER", "DELETE", "DROP", "SET ", "SETTINGS", "SYSTEM",
            ] {
                assert!(!upper.contains(word), "{word} in {statement}");
            }
        }
    }

    #[tokio::test]
    async fn only_a_select_is_ever_sent_to_read() {
        // The guard answers before any connection is made, so an unreachable server is fine.
        let clickhouse =
            ClickHouse::new("http://127.0.0.1:1", "reader", "secret-value", None).unwrap();
        for statement in [
            INSERT_TELEMETRY,
            INSERT_BATCH_COMMIT,
            "DROP TABLE trustiot.telemetry",
        ] {
            let error = clickhouse
                .select(statement, &[])
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("only a SELECT"), "{statement}: {error}");
        }
    }

    #[test]
    fn urls_and_ca_files_must_agree() {
        assert!(ClickHouse::new("http://127.0.0.1:8123", "u", "p", None).is_ok());
        assert!(ClickHouse::new("https://127.0.0.1:8443", "u", "p", None).is_err());
        assert!(
            ClickHouse::new("http://127.0.0.1:8123", "u", "p", Some(Path::new("ca.pem"))).is_err()
        );
        assert!(ClickHouse::new("ftp://127.0.0.1", "u", "p", None).is_err());
        assert!(ClickHouse::new("http://127.0.0.1:8123", "", "p", None).is_err());
        let debug = format!(
            "{:?}",
            ClickHouse::new("http://127.0.0.1:8123", "u", "secret-value", None).unwrap()
        );
        assert!(!debug.contains("secret-value"), "{debug}");
        // Credentials, a query or a fragment in the URL are refused, without echoing the URL.
        for url in [
            "http://u:secret-value@127.0.0.1:8123",
            "https://secret-value@127.0.0.1:8443/",
            "http://127.0.0.1:8123/?password=secret-value",
            "http://127.0.0.1:8123#secret-value",
            "http:///path",
        ] {
            let error = ClickHouse::new(url, "u", "p", None)
                .unwrap_err()
                .to_string();
            assert!(!error.contains("secret-value"), "{url}: {error}");
            assert!(
                error.contains("credentials") || error.contains("query") || error.contains("host"),
                "{url}: {error}"
            );
        }
        assert!(ClickHouse::new("http://127.0.0.1:8123/clickhouse/", "u", "p", None).is_ok());
    }
}
