//! The migration Job (docs/architecture/deployment.md, "migration Job"; this crate's README).
//!
//! - [`run`]: one migration run. It takes a Postgres advisory lock first, so two Jobs never run at
//!   the same time; then fills the Secrets that hold the generated passwords ([`passwords`]),
//!   the Postgres part ([`postgres`]: the Job's bookkeeping, the per-database
//!   setup, every service's roles, schema, outbox tables and migrations, the per-tenant role
//!   hooks) and the ClickHouse part ([`clickhouse`]: the DDL and the collector's account).
//! - [`kube::wait_for_job`]: the init container that holds a service's pod until the release's
//!   migration Job has completed.
//! - [`clickhouse_admin::generate`]: the bundled ClickHouse's init container, which generates
//!   its bootstrap admin password into the Secret the chart declares before the server starts;
//!   the run reads it from there.
//!
//! Every step is idempotent: a second run on the same databases changes nothing, which
//! [`Report::changes`] states and the tests check against the catalogs.

pub mod clickhouse;
pub mod clickhouse_admin;
pub mod clickhouse_files;
pub mod config;
pub mod http;
pub mod kube;
pub mod passwords;
pub mod postgres;
pub mod registry;
pub mod secret;

use std::time::Duration;

use crate::config::RunConfig;
use crate::registry::Registry;

/// Why a run failed. Messages never contain passwords.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Postgres: {context}: {source}")]
    Postgres {
        context: String,
        #[source]
        source: sqlx::Error,
    },
    #[error("Postgres did not accept the migration role within {0:?}: {1}")]
    PostgresUnavailable(Duration, String),
    #[error("another migration run held the lock for longer than {0:?}")]
    LockTimeout(Duration),
    #[error("the migration lock's database session ended during the run; the run is incomplete")]
    LockLost,
    #[error("the migration role {role} cannot run migrations: {problem}")]
    MigrationRole { role: String, problem: String },
    #[error("role {role}: {problem}")]
    Role { role: String, problem: String },
    #[error("schema {schema}: {problem}")]
    Schema { schema: String, problem: String },
    #[error(
        "{component}/{name} changed after it was applied (applied files are append-only: add a \
         new file instead)"
    )]
    Changed { component: String, name: String },
    #[error(
        "{file} left its transaction or its role ({now}); migration files must not control \
         transactions or switch roles. It was not recorded as applied; anything it ran after \
         ending the transaction is already committed (docs/runbooks/migrations.md)"
    )]
    LeftItsContext { file: String, now: String },
    #[error("the registry is inconsistent: {0}")]
    Registry(String),
    #[error("tenant role hook {hook}: {problem}")]
    TenantHook { hook: String, problem: String },
    #[error(transparent)]
    Password(#[from] secret::PasswordError),
    #[error("no randomness for a password salt: {0}")]
    Random(String),
    #[error("ClickHouse: {0}")]
    ClickHouse(String),
    #[error("Secret {secret}: {problem}")]
    Secret { secret: String, problem: String },
    #[error("Kubernetes API: {0}")]
    Kube(String),
}

impl Error {
    pub(crate) fn postgres(context: impl Into<String>) -> impl FnOnce(sqlx::Error) -> Self {
        let context = context.into();
        move |source| Self::Postgres { context, source }
    }
}

/// What a run did. A run that found everything in place reports no changes.
#[derive(Debug, Default)]
pub struct Report {
    /// One line per change, for example `applied identity/0001_identity.sql` or
    /// `created role identity_app`; empty when the run changed nothing.
    pub changes: Vec<String>,
    /// Login roles of the registry that no Secret is bound to in `passwords.postgres` (their
    /// service is not part of this deployment): the run leaves them NOLOGIN.
    pub roles_without_password: Vec<String>,
}

impl Report {
    pub(crate) fn change(&mut self, change: String) {
        tracing::info!(change = %change, "migration change");
        self.changes.push(change);
    }
}

/// The SHA-256 of a file's text, in lower-case hex, as the Job records it: service-kit's, so
/// the records and the services' readiness check hash alike (Laysi/trustiot-v2#402).
#[must_use]
pub fn sha256_hex(text: &str) -> String {
    service_kit::migrations::sha256_hex(text)
}

/// One migration run: everything in `registry`, under the migration lock.
///
/// # Errors
///
/// When a database is not reachable in time, another run holds the lock for too long, or a step
/// fails. A failed Postgres file leaves nothing behind (one transaction per file); ClickHouse
/// statements are idempotent, so the next run repeats a half-applied ClickHouse file.
pub async fn run(config: &RunConfig, registry: &Registry) -> Result<Report, Error> {
    postgres::check_registry(registry)?;
    postgres::check_password_bindings(config, registry)?;
    let mut report = Report::default();
    let mut session = postgres::Session::connect(config).await?;
    session.lock(config.wait.lock).await?;
    session.check_migrator().await?;
    // Under the lock, so two runs never generate a password for the same key; after the
    // migration role passed its checks, so a refused run writes no Secret; before any role is
    // set, so a role is always set to the password its Secret holds.
    let passwords = match &config.passwords {
        Some(settings) => passwords::converge(settings, config.wait.databases, &mut report).await?,
        None => passwords::Passwords::default(),
    };
    session
        .migrate(config, registry, &passwords, &mut report)
        .await?;
    if let Some(settings) = &config.clickhouse {
        let client =
            clickhouse::ClickHouse::connect(config, settings, config.wait.databases).await?;
        client
            .migrate(registry, passwords.collector(), &mut report)
            .await?;
    } else {
        tracing::info!("no ClickHouse settings: the ClickHouse part is skipped");
    }
    // Everything ran while this session held the lock; if it ended, another run may have
    // started in between, so do not report success.
    session.unlock().await?;
    Ok(report)
}
