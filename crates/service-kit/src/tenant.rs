//! Tenant-isolated Postgres transactions (decision 29). Requires the `contract` feature.
//!
//! Every table with tenant data has row-level security on, and its policy compares `tenant_id`
//! with the transaction's tenant setting ([`TENANT_CONDITION`](crate::postgres::TENANT_CONDITION)).
//! The kit sets that setting once, as the first statement of the transaction, from a verified
//! identity; nothing else may set it. A query that forgets its tenant condition still sees only
//! the transaction's tenant, and a query outside a tenant transaction sees no tenant rows at all.
//!
//! - [`TenantDb`]: the service role's pool for request handlers. [`TenantDb::begin`] opens a
//!   transaction for the tenant of a request's [`UserIdentity`]; there is no other way to pick a
//!   tenant through it.
//! - [`BackgroundTenantDb`]: the same pool for background work that acts for one tenant at a
//!   time (scheduled jobs, event consumers), as the service's [`ServiceIdentity`]. It only comes
//!   with the pool, from [`TenantDb::connect_lazy_with_background`] at startup, so a handler
//!   that holds a [`TenantDb`] cannot get one and open a transaction for a tenant taken from
//!   its request.
//! - [`PlatformDb`]: the pool of the service's platform role, the only role whose policies let
//!   it read and write across tenants (the platform back office and cross-tenant jobs). Every
//!   transaction names a [`PlatformActor`] and a purpose and is logged.
//! - [`PlatformReader`]: read-only transactions of the platform role for routine reads that run
//!   every few seconds, such as an outbox consumer polling a producer's events. The actor and
//!   purpose are logged once, when the reader is made.
//!
//! Every pool verifies the server (`verify-full`) and checks every session before use
//! ([`UnsafeSession`], see [`postgres::connect_lazy`](crate::postgres::connect_lazy)).
//!
//! What row-level security does not cover (see decision 29's threat model): SQL injection, since
//! injected SQL runs as the service role and could change the setting. Services therefore only
//! run parameterized SQL, and `tools/sql-check` rejects string-built SQL and any SQL that sets
//! the tenant outside this module.

use std::fmt;
use std::ops::{Deref, DerefMut};

use contract::{ServiceName, TenantId, UserId};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use crate::identity::{ServiceIdentity, UserIdentity};
use crate::postgres::{self, InsecureConnection, SessionRole, UnsafeSession};

/// Sets the tenant for the rest of the transaction: `set_config(..., true)` is `SET LOCAL`, but
/// takes the value as a bound parameter.
const SET_TENANT: &str = "SELECT set_config('trustiot.tenant_id', $1, true)";

/// Why a tenant or platform transaction could not start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TenantDbError {
    /// The user has not picked or joined a tenant, so there is no tenant to act in. A
    /// tenant-scoped endpoint answers `permission_denied`.
    #[error("the request's identity has no tenant")]
    NoTenant,
    /// No session was available because the pool refused new sessions that row-level security
    /// would not protect (reported after the pool's acquire timeout).
    #[error("refused the database session: {0}")]
    UnsafeSession(UnsafeSession),
    /// The database failed.
    #[error("database error: {0}")]
    Database(sqlx::Error),
}

impl From<sqlx::Error> for TenantDbError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// A pool from [`postgres::connect_lazy`](crate::postgres::connect_lazy) and the sessions it
/// refused.
#[derive(Clone, Debug)]
struct CheckedPool {
    pool: PgPool,
    refusals: postgres::Refusals,
}

impl CheckedPool {
    fn connect_lazy(
        options: PgConnectOptions,
        pool: PgPoolOptions,
        role: SessionRole,
    ) -> Result<Self, InsecureConnection> {
        postgres::connect_lazy_recording(options, pool, role)
            .map(|(pool, refusals)| Self { pool, refusals })
    }

    /// Starts a transaction whose first statement sets the tenant.
    async fn begin_in(&self, tenant: TenantId) -> Result<TenantTransaction, TenantDbError> {
        let mut tx = self.begin().await?;
        sqlx::query(SET_TENANT)
            .bind(tenant.to_string())
            .execute(&mut *tx)
            .await?;
        Ok(TenantTransaction { tx, tenant })
    }

    /// A pool error, as the refused session when the pool timed out after refusing one.
    fn error(&self, error: sqlx::Error) -> TenantDbError {
        match (&error, self.refusals.last()) {
            (sqlx::Error::PoolTimedOut, Some(problem)) => TenantDbError::UnsafeSession(problem),
            _ => TenantDbError::Database(error),
        }
    }

    async fn begin(&self) -> Result<Transaction<'static, Postgres>, TenantDbError> {
        self.pool.begin().await.map_err(|error| self.error(error))
    }

    async fn ping(&self) -> Result<(), TenantDbError> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map_err(|error| self.error(error))?;
        Ok(())
    }
}

/// The service role's pool for tenant data. Cloning is cheap and shares the pool.
///
/// ```no_run
/// use service_kit::identity::UserIdentity;
/// use service_kit::tenant::{TenantDb, TenantDbError};
///
/// async fn device_names(db: &TenantDb, user: &UserIdentity) -> Result<Vec<String>, TenantDbError> {
///     let mut tx = db.begin(user).await?;
///     // No tenant condition needed: row-level security returns only the user's tenant's rows.
///     let names = sqlx::query_scalar("SELECT name FROM devices ORDER BY name")
///         .fetch_all(&mut *tx)
///         .await?;
///     tx.commit().await?;
///     Ok(names)
/// }
/// ```
///
/// The tenant is set once, by the kit, when the transaction starts. A transaction has no way to
/// change it, and there is no way to open one for a tenant other than the identity's:
///
/// ```compile_fail,E0599
/// # async fn switch(db: service_kit::tenant::TenantDb, user: service_kit::identity::UserIdentity,
/// #     other: contract::TenantId) {
/// let mut tx = db.begin(&user).await.unwrap();
/// tx.set_tenant(other);
/// # }
/// ```
///
/// ```compile_fail,E0061
/// # async fn other_tenant(db: service_kit::tenant::TenantDb,
/// #     user: service_kit::identity::UserIdentity, other: contract::TenantId) {
/// let tx = db.begin(&user, other).await;
/// # }
/// ```
///
/// A handler cannot reach the background path either: `TenantDb` has no method that takes a
/// tenant, and a [`BackgroundTenantDb`] only comes with a new pool.
///
/// ```compile_fail,E0599
/// # async fn from_request(db: service_kit::tenant::TenantDb, other: contract::TenantId) {
/// let actor = service_kit::identity::ServiceIdentity::new(contract::ServiceName::Assets);
/// let tx = db.begin_for_service(actor, other, "handler").await;
/// # }
/// ```
///
/// ```compile_fail,E0451
/// # fn forge(tx: sqlx::Transaction<'static, sqlx::Postgres>, tenant: contract::TenantId) {
/// let tx = service_kit::tenant::TenantTransaction { tx, tenant };
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct TenantDb {
    pool: CheckedPool,
}

impl TenantDb {
    /// A lazily connecting pool for the service role; see
    /// [`postgres::connect_lazy`](crate::postgres::connect_lazy).
    ///
    /// # Errors
    ///
    /// When `options` would not verify the server.
    pub fn connect_lazy(
        options: PgConnectOptions,
        pool: PgPoolOptions,
    ) -> Result<Self, InsecureConnection> {
        CheckedPool::connect_lazy(options, pool, SessionRole::Service).map(|pool| Self { pool })
    }

    /// Like [`TenantDb::connect_lazy`], plus a handle on the same pool for background work that
    /// acts for one tenant at a time as `actor`. Call it at startup, put the [`TenantDb`] in the
    /// router's state, and give the [`BackgroundTenantDb`] only to the background tasks.
    ///
    /// # Errors
    ///
    /// When `options` would not verify the server.
    pub fn connect_lazy_with_background(
        options: PgConnectOptions,
        pool: PgPoolOptions,
        actor: ServiceIdentity,
    ) -> Result<(Self, BackgroundTenantDb), InsecureConnection> {
        let db = Self::connect_lazy(options, pool)?;
        let background = BackgroundTenantDb {
            pool: db.pool.clone(),
            actor,
        };
        Ok((db, background))
    }

    /// The underlying pool, for tables without tenant data, job locks, and health checks. Queries
    /// on it run without a tenant, so they see no rows of tenant tables.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool.pool
    }

    /// Runs `SELECT 1` on a pooled session, for readiness: fails while the database is down or
    /// new sessions are refused ([`UnsafeSession`]).
    ///
    /// # Errors
    ///
    /// When the query fails.
    pub async fn ping(&self) -> Result<(), TenantDbError> {
        self.pool.ping().await
    }

    /// Starts a transaction in the tenant of the request's verified identity.
    ///
    /// # Errors
    ///
    /// [`TenantDbError::NoTenant`] when the identity has no tenant; otherwise when the database
    /// fails or refuses the session.
    pub async fn begin(&self, user: &UserIdentity) -> Result<TenantTransaction, TenantDbError> {
        let tenant = user.tenant_id().ok_or(TenantDbError::NoTenant)?;
        self.pool.begin_in(tenant).await
    }
}

/// The service role's pool for work the service does on its own behalf for one tenant at a
/// time: a scheduled job, or an event consumer handling an event of that tenant. Such work has
/// no user identity (see [`ServiceIdentity`]), so the tenant comes from the work itself: the
/// event's tenant, or a tenant found by a [`PlatformDb`] query. Only
/// [`TenantDb::connect_lazy_with_background`] makes one; keep it out of the router's state.
///
/// ```no_run
/// use contract::{ServiceName, TenantId};
/// use service_kit::identity::ServiceIdentity;
/// use service_kit::tenant::{BackgroundTenantDb, TenantDb};
/// use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
///
/// async fn purge_expired(jobs: BackgroundTenantDb, tenant: TenantId) {
///     let mut tx = jobs.begin(tenant, "purge-expired").await.unwrap();
///     sqlx::query("DELETE FROM sessions WHERE expires_at < now()")
///         .execute(&mut *tx)
///         .await
///         .unwrap();
///     tx.commit().await.unwrap();
/// }
///
/// fn connect(options: PgConnectOptions) -> (TenantDb, BackgroundTenantDb) {
///     let actor = ServiceIdentity::new(ServiceName::Assets);
///     TenantDb::connect_lazy_with_background(options, PgPoolOptions::new(), actor).unwrap()
/// }
/// ```
#[derive(Clone, Debug)]
pub struct BackgroundTenantDb {
    pool: CheckedPool,
    actor: ServiceIdentity,
}

impl BackgroundTenantDb {
    /// The service the work runs as.
    #[must_use]
    pub fn actor(&self) -> ServiceIdentity {
        self.actor
    }

    /// Starts a transaction in `tenant`. `purpose` names the work, for example
    /// `"purge-expired-sessions"`; it is logged at DEBUG with the service and the tenant.
    ///
    /// # Errors
    ///
    /// When the database fails or refuses the session.
    pub async fn begin(
        &self,
        tenant: TenantId,
        purpose: &'static str,
    ) -> Result<TenantTransaction, TenantDbError> {
        tracing::debug!(
            service = self.actor.service().as_str(),
            tenant_id = %tenant,
            purpose,
            "tenant transaction for background work"
        );
        self.pool.begin_in(tenant).await
    }

    /// Starts a transaction with no tenant: row-level security shows it no row of any tenant
    /// table and lets it write none. Use it for the service's tables without tenant data, such
    /// as an outbox consumer's bookkeeping, and for work that belongs to no tenant, such as a
    /// platform-wide event (one whose `tenant_id` is NULL). `purpose` is logged at DEBUG with
    /// the service.
    ///
    /// # Errors
    ///
    /// When the database fails or refuses the session.
    pub async fn begin_without_tenant(
        &self,
        purpose: &'static str,
    ) -> Result<Transaction<'static, Postgres>, TenantDbError> {
        tracing::debug!(
            service = self.actor.service().as_str(),
            purpose,
            "transaction without a tenant for background work"
        );
        self.pool.begin().await
    }
}

/// A transaction in one tenant. Use it as the executor of queries with `&mut *tx`; it rolls back
/// when dropped without [`TenantTransaction::commit`]. The tenant setting ends with the
/// transaction, so the pooled connection returns without it.
///
/// Write events to the outbox with the same `&mut *tx`, so they commit or roll back with the
/// data; [`TenantTransaction::tenant_id`] is the tenant the event belongs to.
pub struct TenantTransaction {
    tx: Transaction<'static, Postgres>,
    tenant: TenantId,
}

impl TenantTransaction {
    /// The tenant every query in this transaction is limited to.
    #[must_use]
    pub fn tenant_id(&self) -> TenantId {
        self.tenant
    }

    /// Commits the transaction.
    ///
    /// # Errors
    ///
    /// When the commit fails; the transaction is then rolled back.
    pub async fn commit(self) -> Result<(), sqlx::Error> {
        self.tx.commit().await
    }

    /// Rolls the transaction back.
    ///
    /// # Errors
    ///
    /// When the rollback fails; the connection is then closed.
    pub async fn rollback(self) -> Result<(), sqlx::Error> {
        self.tx.rollback().await
    }

    /// The sqlx transaction, for library code whose API takes one, such as `outbox::publish`
    /// and an outbox handler. The tenant setting stays as the kit set it, and the transaction
    /// still ends through this `TenantTransaction`: commit, rollback, or drop.
    pub fn transaction(&mut self) -> &mut Transaction<'static, Postgres> {
        &mut self.tx
    }
}

impl Deref for TenantTransaction {
    type Target = PgConnection;

    fn deref(&self) -> &PgConnection {
        &self.tx
    }
}

impl DerefMut for TenantTransaction {
    fn deref_mut(&mut self) -> &mut PgConnection {
        &mut self.tx
    }
}

impl fmt::Debug for TenantTransaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TenantTransaction")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

/// Who a platform transaction acts for: a platform administrator in the back office, or a
/// service's own cross-tenant work. Only these two constructors exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlatformActor(Actor);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Actor {
    Admin(UserId),
    Service(ServiceName),
}

impl PlatformActor {
    /// The platform administrator of a request; `None` when the user is not one (A2).
    #[must_use]
    pub fn admin(user: &UserIdentity) -> Option<Self> {
        user.is_platform_admin()
            .then(|| Self(Actor::Admin(user.user_id())))
    }

    /// A service doing cross-tenant work on its own behalf, such as finding the tenants a
    /// scheduled job has work for.
    #[must_use]
    pub fn service(service: ServiceIdentity) -> Self {
        Self(Actor::Service(service.service()))
    }
}

/// The pool of the service's platform role: the role whose `platform_access` policies let it
/// see every tenant's rows of the tables that opt in (see the policy template in the kit's
/// README). It has its own credentials, so the service role's pool cannot act across tenants.
/// The role has no `BYPASSRLS` and owns nothing; its sessions pass the same check as the
/// service role's.
#[derive(Clone, Debug)]
pub struct PlatformDb {
    pool: CheckedPool,
}

impl PlatformDb {
    /// A lazily connecting pool for the platform role; see
    /// [`postgres::connect_lazy`](crate::postgres::connect_lazy).
    ///
    /// # Errors
    ///
    /// When `options` would not verify the server.
    pub fn connect_lazy(
        options: PgConnectOptions,
        pool: PgPoolOptions,
    ) -> Result<Self, InsecureConnection> {
        CheckedPool::connect_lazy(options, pool, SessionRole::Platform).map(|pool| Self { pool })
    }

    /// Runs `SELECT 1` on a pooled session, for readiness.
    ///
    /// # Errors
    ///
    /// When the query fails or new sessions are refused.
    pub async fn ping(&self) -> Result<(), TenantDbError> {
        self.pool.ping().await
    }

    /// Starts a cross-tenant transaction. It is logged at INFO with the actor and `purpose`
    /// (`platform database access`); the operation log of Laysi/trustiot-v2#130 records the
    /// operations themselves.
    ///
    /// # Errors
    ///
    /// When the database fails or refuses the session.
    pub async fn begin(
        &self,
        actor: PlatformActor,
        purpose: &'static str,
    ) -> Result<PlatformTransaction, TenantDbError> {
        match actor.0 {
            Actor::Admin(user) => tracing::info!(
                actor = "platform_admin",
                user_id = %user,
                purpose,
                "platform database access"
            ),
            Actor::Service(service) => tracing::info!(
                actor = "service",
                service = service.as_str(),
                purpose,
                "platform database access"
            ),
        }
        let tx = self.pool.begin().await?;
        Ok(PlatformTransaction { tx, actor })
    }

    /// A handle for routine read-only work across tenants that opens a transaction every few
    /// seconds, such as an outbox consumer polling a producer's events, where an INFO line per
    /// transaction would bury the log. The actor and `purpose` are logged at INFO here, once
    /// (`platform database reader`); each transaction is logged at DEBUG. Its transactions are
    /// `READ ONLY`, so the database refuses every write through them.
    #[must_use]
    pub fn reader(&self, actor: PlatformActor, purpose: &'static str) -> PlatformReader {
        match actor.0 {
            Actor::Admin(user) => tracing::info!(
                actor = "platform_admin",
                user_id = %user,
                purpose,
                "platform database reader"
            ),
            Actor::Service(service) => tracing::info!(
                actor = "service",
                service = service.as_str(),
                purpose,
                "platform database reader"
            ),
        }
        PlatformReader {
            pool: self.pool.clone(),
            actor,
            purpose,
        }
    }
}

/// Read-only cross-tenant transactions of the platform role for one actor and purpose; see
/// [`PlatformDb::reader`]. Cloning is cheap and shares the pool.
#[derive(Clone, Debug)]
pub struct PlatformReader {
    pool: CheckedPool,
    actor: PlatformActor,
    purpose: &'static str,
}

impl PlatformReader {
    /// Who the reads are for.
    #[must_use]
    pub fn actor(&self) -> PlatformActor {
        self.actor
    }

    /// What the reads are for.
    #[must_use]
    pub fn purpose(&self) -> &'static str {
        self.purpose
    }

    /// Starts a `READ ONLY` cross-tenant transaction, logged at DEBUG with the purpose.
    ///
    /// # Errors
    ///
    /// When the database fails or refuses the session.
    pub async fn begin(&self) -> Result<PlatformTransaction, TenantDbError> {
        tracing::debug!(purpose = self.purpose, "platform database read");
        let tx = self
            .pool
            .pool
            .begin_with("BEGIN READ ONLY")
            .await
            .map_err(|error| self.pool.error(error))?;
        Ok(PlatformTransaction {
            tx,
            actor: self.actor,
        })
    }
}

/// A cross-tenant transaction of the platform role. Use it as the executor with `&mut *tx`; it
/// rolls back when dropped without [`PlatformTransaction::commit`].
pub struct PlatformTransaction {
    tx: Transaction<'static, Postgres>,
    actor: PlatformActor,
}

impl PlatformTransaction {
    /// Who the transaction acts for.
    #[must_use]
    pub fn actor(&self) -> PlatformActor {
        self.actor
    }

    /// Commits the transaction.
    ///
    /// # Errors
    ///
    /// When the commit fails.
    pub async fn commit(self) -> Result<(), sqlx::Error> {
        self.tx.commit().await
    }

    /// Rolls the transaction back.
    ///
    /// # Errors
    ///
    /// When the rollback fails.
    pub async fn rollback(self) -> Result<(), sqlx::Error> {
        self.tx.rollback().await
    }

    /// The sqlx transaction, for library code whose API takes one, such as `outbox::publish`
    /// writing a platform-wide event. The transaction still ends through this
    /// `PlatformTransaction`.
    pub fn transaction(&mut self) -> &mut Transaction<'static, Postgres> {
        &mut self.tx
    }
}

impl Deref for PlatformTransaction {
    type Target = PgConnection;

    fn deref(&self) -> &PgConnection {
        &self.tx
    }
}

impl DerefMut for PlatformTransaction {
    fn deref_mut(&mut self) -> &mut PgConnection {
        &mut self.tx
    }
}

impl fmt::Debug for PlatformTransaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlatformTransaction")
            .field("actor", &self.actor)
            .finish_non_exhaustive()
    }
}
