//! The rows of `gateway_ops.gateway_presence` (services/gateway-ops/README.md, "在線狀態").
//!
//! A message names its gateway by its topic, before any tenant is known, so the presence task
//! first asks, as the platform role and read only, which binding of which tenant is the
//! gateway's latest ([`PresenceStore::binding`]), and then writes as the service role in that
//! tenant only (decision 29): row-level security keeps every write in the tenant that holds the
//! gateway. The write's transaction checks again, under a share lock, that the binding has not
//! ended since, so a gateway rebound in between never writes into its former tenant's row. The heartbeat timeout's search across tenants ([`PresenceStore::overdue`]) is a
//! read of the platform role too; each timeout it finds is written in its row's tenant, on the
//! condition that the row did not change since the search.

use std::time::Duration;

use chrono::{DateTime, Utc};
use contract::{GatewayBindingId, ServiceName, TenantId};
use protocol::{ConfigVersion, GatewayId, SessionId, UnixMillis};
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{
    BackgroundTenantDb, PlatformActor, PlatformDb, PlatformReader, TenantDbError, TenantTransaction,
};
use sqlx::Row;

use super::rules::{self, Decision, Ignored, Offline, Signal, State, Stored};

/// The purpose the reads and writes name in the log.
pub const PURPOSE: &str = "gateway-presence";

/// The longest heartbeat interval the timeout allows for: a gateway announcing a longer one is
/// stored with this one. The agent's own setting goes up to the same hour (edge/agent/README.md).
pub const MAX_INTERVAL_S: u32 = 3600;

/// The heartbeat interval the timeout assumes for a gateway that has sent no heartbeat in its
/// session yet: the agent's default (edge/agent/README.md).
pub const DEFAULT_INTERVAL_S: u32 = 60;

/// How many announced intervals pass without a heartbeat before the gateway is offline
/// (`heartbeat_timeout`), plus the grace (`presence.heartbeat_grace`).
pub const MISSED_HEARTBEATS: u32 = 2;

/// Most rows one search for overdue gateways returns; a full page is followed by another.
pub const OVERDUE_PAGE: i64 = 100;

/// The gateway's latest binding by the assets service's event order, whichever tenant holds it,
/// and whether it still holds. `$1` hardware ID.
const BINDING: &str = "SELECT tenant_id::text AS tenant_id, binding_id::text AS binding_id,
    unbound_at IS NULL AS bound
FROM gateway_ops.gateway_bindings
WHERE hardware_id = $1
ORDER BY position_tx DESC, position_id DESC
LIMIT 1";

/// Whether the binding of the gateway still holds, share-locked until the transaction ends so
/// its unbinding waits for the write. `$1` binding, `$2` hardware ID. Row-level security limits
/// it to the transaction's tenant.
const STILL_BOUND: &str = "SELECT 1 FROM gateway_ops.gateway_bindings
WHERE binding_id = $1::uuid AND hardware_id = $2 AND unbound_at IS NULL
FOR SHARE";

/// The row of a binding, locked until the transaction ends. `$1` binding. Row-level security
/// limits it to the transaction's tenant.
const SELECT_ROW: &str = "SELECT state, session, offline_reason, heartbeat_sent_ms
FROM gateway_ops.gateway_presence
WHERE binding_id = $1::uuid
FOR UPDATE";

/// The first row of a binding; returns a row only when it wrote. `$1` tenant, `$2` binding,
/// `$3` hardware ID, `$4` state, `$5` session, `$6` offline reason, `$7` agent version, `$8`
/// whether the message is a heartbeat, `$9` its interval, `$10` its configuration version,
/// `$11` the session's latest heartbeat's `sent_at`.
const INSERT_ROW: &str = "INSERT INTO gateway_ops.gateway_presence
    (tenant_id, binding_id, hardware_id, state, session, offline_reason, changed_at, last_seen_at,
     agent_version, last_heartbeat_at, heartbeat_interval_s, config_version, heartbeat_sent_ms)
VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6, now(), now(), $7,
    CASE WHEN $8 THEN now() END, CASE WHEN $8 THEN $9::integer END,
    CASE WHEN $8 THEN $10::bigint END, $11::bigint)
ON CONFLICT (tenant_id, binding_id) DO NOTHING
RETURNING 1";

/// Writes a decision on the locked row. `$1` binding, `$2` state, `$3` session, `$4` offline
/// reason, `$5` whether the state changed, `$6` agent version (NULL: the message has none), `$7`
/// whether the message is a heartbeat, `$8` its interval, `$9` its configuration version, `$10`
/// the session's latest heartbeat's `sent_at` afterwards. A new session starts without the agent
/// version and heartbeat fields of the one before: they describe the session that sent them.
const UPDATE_ROW: &str = "UPDATE gateway_ops.gateway_presence
SET state = $2, session = $3, offline_reason = $4,
    changed_at = CASE WHEN $5 THEN now() ELSE changed_at END,
    last_seen_at = now(),
    agent_version = CASE WHEN $6 IS NOT NULL THEN $6 WHEN session <> $3 THEN NULL
        ELSE agent_version END,
    last_heartbeat_at = CASE WHEN $7 THEN now() WHEN session <> $3 THEN NULL
        ELSE last_heartbeat_at END,
    heartbeat_interval_s = CASE WHEN $7 THEN $8 WHEN session <> $3 THEN NULL
        ELSE heartbeat_interval_s END,
    config_version = CASE WHEN $7 THEN $9 WHEN session <> $3 THEN NULL ELSE config_version END,
    heartbeat_sent_ms = $10::bigint
WHERE binding_id = $1::uuid";

/// The online rows whose session has not been heard from for [`MISSED_HEARTBEATS`] of their
/// announced intervals (at most [`MAX_INTERVAL_S`], [`DEFAULT_INTERVAL_S`] before the first
/// heartbeat) plus the grace, counted from no earlier than the moment the task could hear them
/// again: `$1` seconds ago. `$2` grace in seconds, `$3` page size. The `last_seen_at` bound
/// lets the partial index narrow the search; it is implied by the full condition.
const OVERDUE: &str = "SELECT tenant_id::text AS tenant_id, binding_id::text AS binding_id,
    hardware_id, session, last_seen_at, heartbeat_interval_s
FROM gateway_ops.gateway_presence
WHERE state = 'online'
    AND last_seen_at < now() - make_interval(secs => $2 + 2)
    AND GREATEST(last_seen_at, now() - make_interval(secs => $1))
        + make_interval(secs => LEAST(COALESCE(heartbeat_interval_s, 60), 3600) * 2 + $2)
        < now()
ORDER BY last_seen_at
LIMIT $3";

/// Ends an overdue session, unless its row changed since the search. `$1` binding, `$2` session,
/// `$3` the `last_seen_at` the search saw; returns a row only when it wrote.
const TIME_OUT: &str = "UPDATE gateway_ops.gateway_presence
SET state = 'offline', offline_reason = 'heartbeat_timeout', changed_at = now()
WHERE binding_id = $1::uuid AND state = 'online' AND session = $2 AND last_seen_at = $3
RETURNING 1";

/// The gateway's latest binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    /// The tenant that holds it.
    pub tenant: TenantId,
    /// The binding.
    pub binding: GatewayBindingId,
    /// Whether it still holds.
    pub bound: bool,
}

/// What [`PresenceStore::apply`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    /// What the rules decided.
    pub decision: Decision,
    /// The state the row had before, `None` without a row.
    pub before: Option<State>,
}

/// An online gateway whose heartbeats are overdue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overdue {
    /// The tenant of its binding.
    pub tenant: TenantId,
    /// Its binding.
    pub binding: GatewayBindingId,
    /// The gateway.
    pub hardware_id: GatewayId,
    /// The session that went quiet.
    pub session: String,
    /// When it was last heard from.
    pub last_seen_at: DateTime<Utc>,
    /// The interval it announced, if it sent a heartbeat.
    pub interval_s: Option<u32>,
}

/// Why a read or write failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database failed.
    #[error(transparent)]
    Database(#[from] TenantDbError),
    /// A stored row breaks the table's rules, which cannot happen.
    #[error("a stored gateway presence row breaks the rules: {0}")]
    Corrupt(&'static str),
}

impl From<sqlx::Error> for StoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(TenantDbError::Database(error))
    }
}

impl StoreError {
    /// What failed, for the log, without the database's message.
    #[must_use]
    pub fn log_kind(&self) -> String {
        match self {
            Self::Database(TenantDbError::Database(sqlx::Error::Database(database))) => database
                .code()
                .map_or_else(|| "database".to_owned(), |code| format!("SQLSTATE {code}")),
            Self::Database(TenantDbError::Database(sqlx::Error::PoolTimedOut)) => {
                "pool timed out".to_owned()
            }
            Self::Database(TenantDbError::Database(_)) => "database client".to_owned(),
            Self::Database(other) => other.to_string(),
            Self::Corrupt(what) => format!("corrupt row: {what}"),
        }
    }
}

/// The reads and writes; see the module. Cloning is cheap and shares the pools.
#[derive(Clone, Debug)]
pub struct PresenceStore {
    reader: PlatformReader,
    db: BackgroundTenantDb,
}

fn uuid_column(row: &sqlx::postgres::PgRow, column: &str) -> Result<String, StoreError> {
    Ok(row.try_get::<String, _>(column)?)
}

fn tenant_of(row: &sqlx::postgres::PgRow) -> Result<(TenantId, GatewayBindingId), StoreError> {
    let tenant = uuid_column(row, "tenant_id")?
        .parse()
        .map_err(|_| StoreError::Corrupt("tenant_id"))?;
    let binding = uuid_column(row, "binding_id")?
        .parse()
        .map_err(|_| StoreError::Corrupt("binding_id"))?;
    Ok((tenant, binding))
}

/// The value stored for an announced interval: at least 1 (the protocol's rule), at most
/// [`MAX_INTERVAL_S`].
#[must_use]
pub fn stored_interval(interval_s: u32) -> i32 {
    // MAX_INTERVAL_S fits an i32.
    i32::try_from(interval_s.clamp(1, MAX_INTERVAL_S)).unwrap_or(i32::MAX)
}

/// The stored `heartbeat_sent_ms`; a time past `i64::MAX` milliseconds is clamped.
fn sent_column(sent_at: UnixMillis) -> i64 {
    i64::try_from(sent_at.get()).unwrap_or(i64::MAX)
}

fn version_column(version: ConfigVersion) -> i64 {
    // A version above i64::MAX cannot be stored; the gateway's claim is clamped, as it is only
    // shown.
    i64::try_from(version.get()).unwrap_or(i64::MAX)
}

fn read_stored(row: &sqlx::postgres::PgRow) -> Result<Stored, StoreError> {
    let state: String = row.try_get("state")?;
    let session: String = row.try_get("session")?;
    let reason: Option<String> = row.try_get("offline_reason")?;
    let sent: Option<i64> = row.try_get("heartbeat_sent_ms")?;
    let session = SessionId::new(session).map_err(|_| StoreError::Corrupt("session"))?;
    let heartbeat_sent_at = sent
        .map(|sent| {
            u64::try_from(sent)
                .map(UnixMillis::new)
                .map_err(|_| StoreError::Corrupt("heartbeat_sent_ms"))
        })
        .transpose()?;
    let state = match (state.as_str(), reason.as_deref()) {
        ("online", None) => State::Online,
        ("offline", Some(reason)) => {
            State::Offline(Offline::parse(reason).ok_or(StoreError::Corrupt("offline_reason"))?)
        }
        _ => return Err(StoreError::Corrupt("state")),
    };
    Ok(Stored {
        state,
        session,
        heartbeat_sent_at,
    })
}

impl PresenceStore {
    /// Reads on the platform role's pool `platform`, as the gateway-ops service on its own
    /// behalf, and writes through the service role's `db`, both with purpose [`PURPOSE`].
    #[must_use]
    pub fn new(platform: &PlatformDb, db: BackgroundTenantDb) -> Self {
        let actor = PlatformActor::service(ServiceIdentity::new(ServiceName::GatewayOps));
        Self {
            reader: platform.reader(actor, PURPOSE),
            db,
        }
    }

    /// The gateway's latest binding; `None` when it was never bound.
    ///
    /// # Errors
    ///
    /// When the database fails or the row is corrupt.
    pub async fn binding(&self, gateway: &GatewayId) -> Result<Option<Binding>, StoreError> {
        let mut tx = self.reader.begin().await?;
        let row = sqlx::query(BINDING)
            .bind(gateway.as_str())
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let (tenant, binding) = tenant_of(&row)?;
        Ok(Some(Binding {
            tenant,
            binding,
            bound: row.try_get("bound")?,
        }))
    }

    /// Applies `signal` of `gateway` to the row of `binding` (a binding of the gateway in its
    /// tenant) by the rules ([`rules::decide`]), in one transaction that holds the row's lock,
    /// so the replicas apply one message at a time per gateway. `replayed`: the broker sent the
    /// message as a retained one because of a new subscription; such a message never writes the
    /// binding's first row ([`Ignored::Replayed`]), as it may be from an earlier binding.
    ///
    /// `None` when the binding no longer holds: nothing is written.
    ///
    /// # Errors
    ///
    /// When the database fails or the stored row is corrupt; nothing is written then.
    pub async fn apply(
        &self,
        binding: &Binding,
        gateway: &GatewayId,
        signal: &Signal,
        replayed: bool,
    ) -> Result<Option<Applied>, StoreError> {
        let mut tx = self.db.begin(binding.tenant, PURPOSE).await?;
        let holds: Option<i32> = sqlx::query_scalar(STILL_BOUND)
            .bind(binding.binding.to_string())
            .bind(gateway.as_str())
            .fetch_optional(&mut *tx)
            .await?;
        if holds.is_none() {
            tx.commit().await?;
            return Ok(None);
        }
        let mut stored = Self::locked(&mut tx, binding).await?;
        if stored.is_none() && replayed {
            tx.commit().await?;
            return Ok(Some(Applied {
                decision: Decision::Ignore(Ignored::Replayed),
                before: None,
            }));
        }
        if stored.is_none() {
            let decision = rules::decide(None, signal);
            let Decision::Write(write) = &decision else {
                // A first message is always written.
                return Err(StoreError::Corrupt("first message not written"));
            };
            let (agent_version, heartbeat) = fields(signal);
            let inserted: Option<i32> = sqlx::query_scalar(INSERT_ROW)
                .bind(binding.tenant.to_string())
                .bind(binding.binding.to_string())
                .bind(gateway.as_str())
                .bind(write.state.column())
                .bind(write.session.as_str())
                .bind(write.state.reason().map(Offline::as_str))
                .bind(agent_version)
                .bind(heartbeat.is_some())
                .bind(heartbeat.map(|(interval, _)| interval))
                .bind(heartbeat.and_then(|(_, version)| version))
                .bind(write.heartbeat_sent_at.map(sent_column))
                .fetch_optional(&mut *tx)
                .await?;
            if inserted.is_some() {
                tx.commit().await?;
                return Ok(Some(Applied {
                    decision,
                    before: None,
                }));
            }
            // Another replica wrote the first row since: decide against it.
            stored = Self::locked(&mut tx, binding).await?;
        }
        let stored = stored.ok_or(StoreError::Corrupt("row vanished"))?;
        let decision = rules::decide(Some(&stored), signal);
        if let Decision::Write(write) = &decision {
            let (agent_version, heartbeat) = fields(signal);
            sqlx::query(UPDATE_ROW)
                .bind(binding.binding.to_string())
                .bind(write.state.column())
                .bind(write.session.as_str())
                .bind(write.state.reason().map(Offline::as_str))
                .bind(write.changed)
                .bind(agent_version)
                .bind(heartbeat.is_some())
                .bind(heartbeat.map(|(interval, _)| interval))
                .bind(heartbeat.and_then(|(_, version)| version))
                .bind(write.heartbeat_sent_at.map(sent_column))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(Some(Applied {
            decision,
            before: Some(stored.state),
        }))
    }

    async fn locked(
        tx: &mut TenantTransaction,
        binding: &Binding,
    ) -> Result<Option<Stored>, StoreError> {
        let row = sqlx::query(SELECT_ROW)
            .bind(binding.binding.to_string())
            .fetch_optional(&mut **tx)
            .await?;
        row.as_ref().map(read_stored).transpose()
    }

    /// The online gateways whose heartbeats are overdue (see [`OVERDUE`]), the longest quiet
    /// first, at most [`OVERDUE_PAGE`]. `heard_for` is how long the task has been able to hear
    /// heartbeats without a gap: no gateway is overdue before that much time has passed since
    /// the gap, whatever its row says.
    ///
    /// # Errors
    ///
    /// When the database fails or a row is corrupt.
    pub async fn overdue(
        &self,
        heard_for: Duration,
        grace: Duration,
    ) -> Result<Vec<Overdue>, StoreError> {
        let mut tx = self.reader.begin().await?;
        let rows = sqlx::query(OVERDUE)
            .bind(heard_for.as_secs_f64())
            .bind(grace.as_secs_f64())
            .bind(OVERDUE_PAGE)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        rows.iter()
            .map(|row| {
                let (tenant, binding) = tenant_of(row)?;
                let hardware_id: String = row.try_get("hardware_id")?;
                let interval: Option<i32> = row.try_get("heartbeat_interval_s")?;
                Ok(Overdue {
                    tenant,
                    binding,
                    hardware_id: GatewayId::new(hardware_id)
                        .map_err(|_| StoreError::Corrupt("hardware_id"))?,
                    session: row.try_get("session")?,
                    last_seen_at: row.try_get("last_seen_at")?,
                    interval_s: interval.and_then(|interval| u32::try_from(interval).ok()),
                })
            })
            .collect()
    }

    /// Marks `overdue` offline (`heartbeat_timeout`) in its tenant, unless its row changed
    /// since it was found; `true` when it wrote.
    ///
    /// # Errors
    ///
    /// When the database fails; nothing is written then.
    pub async fn time_out(&self, overdue: &Overdue) -> Result<bool, StoreError> {
        let mut tx = self.db.begin(overdue.tenant, PURPOSE).await?;
        let written: Option<i32> = sqlx::query_scalar(TIME_OUT)
            .bind(overdue.binding.to_string())
            .bind(&overdue.session)
            .bind(overdue.last_seen_at)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(written.is_some())
    }
}

/// The agent version and the heartbeat fields (interval, configuration version) a message
/// carries.
fn fields(signal: &Signal) -> (Option<&str>, Option<(i32, Option<i64>)>) {
    match signal {
        Signal::Online { agent_version, .. } => (Some(agent_version.as_str()), None),
        Signal::Offline { .. } => (None, None),
        Signal::Heartbeat {
            agent_version,
            interval_s,
            config_version,
            ..
        } => (
            Some(agent_version.as_str()),
            Some((
                stored_interval(*interval_s),
                config_version.map(version_column),
            )),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_are_stored_within_the_timeouts_range() {
        assert_eq!(stored_interval(0), 1);
        assert_eq!(stored_interval(60), 60);
        assert_eq!(stored_interval(u32::MAX), 3600);
    }

    /// The SQL states these numbers, which it cannot name.
    #[test]
    fn the_queries_use_the_constants() {
        assert_eq!(MAX_INTERVAL_S, 3600);
        assert_eq!(DEFAULT_INTERVAL_S, 60);
        assert_eq!(MISSED_HEARTBEATS, 2);
        assert!(OVERDUE.contains("COALESCE(heartbeat_interval_s, 60), 3600) * 2 + $2"));
        assert!(OVERDUE.contains("secs => $2 + 2)"));
    }
}
