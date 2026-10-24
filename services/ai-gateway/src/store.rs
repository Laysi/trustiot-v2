//! The model roles' stored settings in Postgres (README.md, 「資料庫」): the platform
//! administrator's model lists that replace a role's configured models, the settings generation
//! every forwarded request reads, and the append-only log of every change.
//!
//! Every change runs in one transaction: it first increments the settings generation, whose row
//! lock makes concurrent writers queue, then locks the role's row and compares the version the
//! caller read, writes the role and one change record, and commits. The new generation is the
//! change's version, so versions only go up, across resets too. A change is checked against the
//! configuration ([`RoleRules`]) before the transaction starts; a refused change writes nothing.
//!
//! Every query runs in a transaction that bounds it on the server too (`SET LOCAL lock_timeout`
//! and `statement_timeout`): a forwarded request's reads until its deadline, never longer than
//! `database.timeout`; a change's statements `database.timeout` each. A query the gateway has
//! given up on, such as a read blocked by a migration's lock or a change waiting behind another
//! writer, does not keep a server process waiting after the caller has its answer.

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, Transaction};
use tokio::time::Instant;

use crate::config::{Config, DEFAULT_DATABASE_TIMEOUT, ModelsError, RoleRules, is_name};

/// The longest operator name a change records, in characters.
pub const MAX_OPERATOR_CHARS: usize = 128;

const READ_GENERATION: &str = "SELECT generation FROM ai_gateway.settings_generation";
/// Bounds the rest of the transaction on the server, like `SET LOCAL`: `$1` is milliseconds.
const SET_TIMEOUTS: &str = "SELECT set_config('lock_timeout', $1, true), \
     set_config('statement_timeout', $1, true)";
const BEGIN_READ: &str = "BEGIN READ ONLY";
const BUMP_GENERATION: &str = "UPDATE ai_gateway.settings_generation \
     SET generation = generation + 1 RETURNING generation";
/// The role's version and whether it has stored models (they are NULL after a reset).
const LOCK_ROLE: &str = "SELECT version, models IS NOT NULL FROM ai_gateway.role_models \
     WHERE role = $1 FOR UPDATE";
/// Sets the models, or clears them (`$2` NULL) for a reset: the row stays with its new version, so
/// a role's version never goes back to 0.
const UPSERT_ROLE: &str = "INSERT INTO ai_gateway.role_models (role, models, version, updated_by) \
     VALUES ($1, $2, $3, $4) \
     ON CONFLICT (role) DO UPDATE SET models = EXCLUDED.models, version = EXCLUDED.version, \
     updated_at = now(), updated_by = EXCLUDED.updated_by";
const LOG_CHANGE: &str = "INSERT INTO ai_gateway.role_model_changes \
     (version, role, action, models, changed_by) VALUES ($1, $2, $3, $4, $5)";
/// Every stored role. The time is formatted in UTC by the database (RFC 3339 with microseconds),
/// so the gateway needs no date type.
const LOAD_ROLES: &str = "SELECT role, models, version, \
     to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'), updated_by \
     FROM ai_gateway.role_models ORDER BY role";
/// A row of [`LOAD_ROLES`]: role, models, version, updated_at, updated_by.
type RoleRow = (String, Option<Vec<String>>, i64, String, String);
/// The settings and their generation from one snapshot of the database.
const BEGIN_SNAPSHOT: &str = "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY";

/// A role's row: its stored model list, or none since a reset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRole {
    pub role: String,
    /// `<provider>/<model>` in fallback order; `None` after a reset (the role uses its
    /// configured models).
    pub models: Option<Vec<String>>,
    /// The settings generation of the change or reset that wrote it: the version a change must
    /// expect. A role without a row is at version 0.
    pub version: i64,
    /// When it was written, RFC 3339 in UTC.
    pub updated_at: String,
    /// Who asked for it.
    pub updated_by: String,
}

/// Every stored role, read in one snapshot together with the generation they belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub generation: i64,
    pub roles: Vec<StoredRole>,
}

/// What a change did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Changed {
    /// Written, with this version (the new settings generation).
    Written { version: i64 },
    /// A reset of a role without stored models (never set, or already reset): nothing was
    /// written.
    Unchanged,
}

/// Why a change was not made. Nothing was written in every case.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The role is not configured; only the chart adds roles.
    #[error("no such model role")]
    UnknownRole,
    /// Not a role name (reset accepts any valid name, so a removed role's models can be cleared).
    #[error("not a model role name")]
    InvalidRole,
    #[error("the operator must be 1 to {MAX_OPERATOR_CHARS} characters without control characters")]
    InvalidOperator,
    #[error("the expected version must not be negative")]
    InvalidVersion,
    #[error("{0}")]
    InvalidModels(ModelsError),
    /// The role's version is no longer the one the caller read: someone else changed it first.
    #[error("the model role was changed since version {expected}; it is at version {current}")]
    Conflict { expected: i64, current: i64 },
    #[error("the database failed: {0}")]
    Database(#[from] sqlx::Error),
}

/// The gateway's pool of the service role `ai_gateway_app`.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
    /// `database.timeout`: the longest server-side `lock_timeout` and `statement_timeout` of
    /// any statement.
    timeout: Duration,
}

impl Store {
    /// A lazy pool (`service_kit::postgres::connect_lazy`: `verify-full` and the session check)
    /// of `database.max_connections` connections, each waited for at most `database.timeout`.
    ///
    /// # Errors
    ///
    /// When the settings would not verify the server.
    pub fn connect_lazy(
        config: &Config,
    ) -> Result<Self, service_kit::postgres::InsecureConnection> {
        let pool = service_kit::postgres::connect_lazy(
            config.postgres().connect_options(),
            PgPoolOptions::new()
                .max_connections(config.database.max_connections)
                .min_connections(0)
                .acquire_timeout(config.database.timeout),
        )?;
        Ok(Self::with_timeout(pool, config.database.timeout))
    }

    /// A store on an existing pool of the service role, with the default `database.timeout`
    /// (tests).
    pub fn from_pool(pool: PgPool) -> Self {
        Self::with_timeout(pool, DEFAULT_DATABASE_TIMEOUT)
    }

    /// A store on an existing pool whose statements the server ends after `timeout` at most.
    pub fn with_timeout(pool: PgPool, timeout: Duration) -> Self {
        Self { pool, timeout }
    }

    /// `database.timeout` from now.
    fn deadline(&self) -> Instant {
        Instant::now() + self.timeout
    }

    /// Opens a transaction with `begin` and bounds it on the server until `deadline`.
    async fn begin(
        &self,
        begin: &'static str,
        deadline: Instant,
    ) -> Result<Transaction<'static, Postgres>, sqlx::Error> {
        let mut transaction = self.pool.begin_with(begin).await?;
        self.bound(&mut transaction, deadline).await?;
        Ok(transaction)
    }

    /// Sets the transaction's `lock_timeout` and `statement_timeout` to the time left until
    /// `deadline`, at most `database.timeout` and at least 1 ms (0 would turn them off). Each
    /// applies per statement, so a transaction of several statements sets them again before
    /// each one; the server then never runs past the caller's deadline.
    async fn bound(
        &self,
        transaction: &mut Transaction<'static, Postgres>,
        deadline: Instant,
    ) -> Result<(), sqlx::Error> {
        let left = deadline
            .saturating_duration_since(Instant::now())
            .min(self.timeout);
        sqlx::query(SET_TIMEOUTS)
            .bind(left.as_millis().max(1).to_string())
            .execute(&mut **transaction)
            .await?;
        Ok(())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The settings generation, bounded by `database.timeout` (readiness, tests).
    ///
    /// # Errors
    ///
    /// When the database cannot be reached or refuses the query.
    pub async fn generation(&self) -> Result<i64, sqlx::Error> {
        self.generation_by(self.deadline()).await
    }

    /// The settings generation: one short query, which every forwarded request runs, in a
    /// read-only transaction the server ends at `deadline` or after `database.timeout`,
    /// whichever comes first.
    ///
    /// # Errors
    ///
    /// When the database cannot be reached or refuses the query.
    pub async fn generation_by(&self, deadline: Instant) -> Result<i64, sqlx::Error> {
        let mut transaction = self.begin(BEGIN_READ, deadline).await?;
        let generation = sqlx::query_scalar(READ_GENERATION)
            .fetch_one(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(generation)
    }

    /// [`Store::load_by`], bounded by `database.timeout` (tests).
    ///
    /// # Errors
    ///
    /// When the database cannot be reached or refuses a query.
    pub async fn load(&self) -> Result<Stored, sqlx::Error> {
        self.load_by(self.deadline()).await
    }

    /// Every stored role and the generation they belong to, in one read-only `REPEATABLE READ`
    /// transaction, so the roles are exactly those of that generation. The server ends each
    /// statement at `deadline` or after `database.timeout`, whichever comes first.
    ///
    /// # Errors
    ///
    /// When the database cannot be reached or refuses a query.
    pub async fn load_by(&self, deadline: Instant) -> Result<Stored, sqlx::Error> {
        let mut transaction = self.begin(BEGIN_SNAPSHOT, deadline).await?;
        let generation: i64 = sqlx::query_scalar(READ_GENERATION)
            .fetch_one(&mut *transaction)
            .await?;
        self.bound(&mut transaction, deadline).await?;
        let rows: Vec<RoleRow> = sqlx::query_as(LOAD_ROLES)
            .fetch_all(&mut *transaction)
            .await?;
        transaction.commit().await?;
        let roles = rows
            .into_iter()
            .map(
                |(role, models, version, updated_at, updated_by)| StoredRole {
                    role,
                    models,
                    version,
                    updated_at,
                    updated_by,
                },
            )
            .collect();
        Ok(Stored { generation, roles })
    }

    /// Replaces the model list of `role` with `models`, if the role is still at
    /// `expected_version` (its row's version, or 0 when it has none). The list must pass the
    /// configuration's checks and use the role's own protocol ([`RoleRules::check`]).
    ///
    /// # Errors
    ///
    /// [`StoreError`]; nothing is written then.
    pub async fn set_models(
        &self,
        rules: &RoleRules,
        role: &str,
        models: &[String],
        expected_version: i64,
        operator: &str,
    ) -> Result<Changed, StoreError> {
        match rules.check(role, models) {
            None => return Err(StoreError::UnknownRole),
            Some(Err(error)) => return Err(StoreError::InvalidModels(error)),
            Some(Ok(())) => {}
        }
        check_change(expected_version, operator)?;
        self.write(role, Some(models), expected_version, operator)
            .await
    }

    /// Clears the stored model list of `role`, which goes back to its configured models, if the
    /// role is still at `expected_version`. The row stays with the reset's version, so a change
    /// based on a read from before the reset is a conflict. Any valid role name is accepted, so
    /// the models of a role the chart no longer has can be cleared. A role without stored models
    /// is left as it is ([`Changed::Unchanged`]) when its version matches.
    ///
    /// # Errors
    ///
    /// [`StoreError`]; nothing is written then.
    pub async fn reset(
        &self,
        role: &str,
        expected_version: i64,
        operator: &str,
    ) -> Result<Changed, StoreError> {
        if !is_name(role) {
            return Err(StoreError::InvalidRole);
        }
        check_change(expected_version, operator)?;
        self.write(role, None, expected_version, operator).await
    }

    /// One change in one transaction. Dropping the transaction on an error rolls it back.
    async fn write(
        &self,
        role: &str,
        models: Option<&[String]>,
        expected_version: i64,
        operator: &str,
    ) -> Result<Changed, StoreError> {
        // Each statement waits at most `database.timeout` on the server.
        let mut transaction = self.begin("BEGIN", self.deadline()).await?;
        // First, so that writers queue on this row and versions follow commit order.
        let version: i64 = sqlx::query_scalar(BUMP_GENERATION)
            .fetch_one(&mut *transaction)
            .await?;
        let row: Option<(i64, bool)> = sqlx::query_as(LOCK_ROLE)
            .bind(role)
            .fetch_optional(&mut *transaction)
            .await?;
        let (current, has_models) = row.unwrap_or((0, false));
        if current != expected_version {
            transaction.rollback().await?;
            return Err(StoreError::Conflict {
                expected: expected_version,
                current,
            });
        }
        if models.is_none() && !has_models {
            // Nothing to reset: the generation goes back too.
            transaction.rollback().await?;
            return Ok(Changed::Unchanged);
        }
        sqlx::query(UPSERT_ROLE)
            .bind(role)
            .bind(models)
            .bind(version)
            .bind(operator)
            .execute(&mut *transaction)
            .await?;
        let action = if models.is_some() { "set" } else { "reset" };
        sqlx::query(LOG_CHANGE)
            .bind(version)
            .bind(role)
            .bind(action)
            .bind(models)
            .bind(operator)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(Changed::Written { version })
    }
}

fn check_change(expected_version: i64, operator: &str) -> Result<(), StoreError> {
    if expected_version < 0 {
        return Err(StoreError::InvalidVersion);
    }
    let chars = operator.chars().count();
    if chars == 0 || chars > MAX_OPERATOR_CHARS || operator.chars().any(char::is_control) {
        return Err(StoreError::InvalidOperator);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operators_and_versions_are_checked() {
        assert!(check_change(0, "admin:1").is_ok());
        assert!(check_change(7, &"a".repeat(MAX_OPERATOR_CHARS)).is_ok());
        assert!(matches!(
            check_change(-1, "admin"),
            Err(StoreError::InvalidVersion)
        ));
        for operator in [
            String::new(),
            "a".repeat(MAX_OPERATOR_CHARS + 1),
            "a\nb".to_owned(),
        ] {
            assert!(matches!(
                check_change(0, &operator),
                Err(StoreError::InvalidOperator)
            ));
        }
    }
}
