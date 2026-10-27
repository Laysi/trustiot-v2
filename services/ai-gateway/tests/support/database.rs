//! A database for one test, set up as the migration Job sets one up (tools/migrate): the roles of
//! decision 29, crates/service-kit/sql/database.sql and the schema `ai_gateway` run by its
//! owner, this release's migration files, and the records of every file the gateway expects, so
//! its `migrations` readiness check passes. The Postgres is service-kit's throwaway TLS one
//! (`postgres::tls_postgres`), shared by the tests of a binary; each test creates its own
//! database with a static `CREATE DATABASE … OWNER ai_gateway_owner`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ai_gateway::Store;
use ai_gateway::config::RoleRules;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};

use super::postgres::{SERVER_HOST, TEST_PASSWORD, TlsPostgres, tls_postgres};

/// The owner and the service role, created once per Postgres. The migration Job creates them so
/// in a deployment, with generated passwords.
const ROLES: &str = "
CREATE ROLE ai_gateway_owner LOGIN PASSWORD 'service-kit-test';
CREATE ROLE ai_gateway_app LOGIN PASSWORD 'service-kit-test';
";

const DATABASE_TEMPLATE: &str = include_str!("../../../../crates/service-kit/sql/database.sql");
/// The migration Job creates the schema, owned by the owner role, before it runs the files.
const SCHEMA: &str = "CREATE SCHEMA ai_gateway";
const MIGRATION: &str = include_str!("../../migrations/postgres/0001_model_roles.sql");

/// Runs service-kit's migration records template for schema `ai_gateway` (`example_schema`)
/// and the service role `ai_gateway_app` (`example_app`). The template is a bound parameter and
/// the names are fixed, so no SQL is built in Rust.
const RECORDS_FUNCTION: &str = "CREATE FUNCTION pg_temp.ai_gateway_migration_records(template text)
RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    EXECUTE replace(replace(template, 'example_schema', 'ai_gateway'), 'example_app', 'ai_gateway_app');
END
$$";
const RUN_RECORDS_FUNCTION: &str = "SELECT pg_temp.ai_gateway_migration_records($1)";
const RECORD: &str = "INSERT INTO ai_gateway.trustiot_migrate_applied (component, name, sha256)
VALUES ($1, $2, $3)";

/// The connections a test's own pool of the service role opens.
const TEST_POOL_CONNECTIONS: u32 = 4;
/// How long a test's own pool waits for a connection: a bound only, far above what a loaded
/// machine needs (CLAUDE.md, "Test time limits").
const TEST_POOL_WAIT: Duration = Duration::from_secs(120);

/// One test's database.
pub struct Database {
    pub postgres: Arc<TlsPostgres>,
    pub name: &'static str,
    /// The test CA in a file, for `AI_GATEWAY_POSTGRES_CA_FILE`.
    pub ca_file: PathBuf,
}

impl Database {
    /// Creates database `name` with `create` and sets it up; `None` when no container runtime
    /// is available outside CI (the test is skipped).
    pub async fn new(create: &'static str, name: &'static str) -> Option<Self> {
        let postgres = tls_postgres(ROLES).await?;
        let mut admin = PgConnection::connect_with(&postgres.superuser("postgres"))
            .await
            .expect("connect as the superuser");
        sqlx::raw_sql(create)
            .execute(&mut admin)
            .await
            .expect("create the database");
        admin.close().await.expect("close");
        let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("ai-gateway-db-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("create the test directory");
        let ca_file = directory.join("postgres-ca.pem");
        std::fs::write(&ca_file, &postgres.ca_pem).expect("write the CA");
        let database = Self {
            postgres,
            name,
            ca_file,
        };
        let mut owner = database.connect("ai_gateway_owner").await;
        for (what, sql) in [
            ("the database template", DATABASE_TEMPLATE),
            ("the schema", SCHEMA),
            ("the migration", MIGRATION),
            ("the records helper", RECORDS_FUNCTION),
        ] {
            sqlx::raw_sql(sql)
                .execute(&mut owner)
                .await
                .unwrap_or_else(|error| panic!("run {what}: {error}"));
        }
        sqlx::query(RUN_RECORDS_FUNCTION)
            .bind(service_kit::migrations::RECORDS_TEMPLATE)
            .execute(&mut owner)
            .await
            .expect("create the migration records");
        for record in ai_gateway::schema::SCHEMA.expected() {
            sqlx::query(RECORD)
                .bind(&record.component)
                .bind(&record.name)
                .bind(&record.sha256)
                .execute(&mut owner)
                .await
                .unwrap_or_else(|error| panic!("record {record}: {error}"));
        }
        owner.close().await.expect("close");
        Some(database)
    }

    /// A connection as `role` (`ai_gateway_owner` or `ai_gateway_app`).
    pub async fn connect(&self, role: &str) -> PgConnection {
        PgConnection::connect_with(&self.postgres.options(role, self.name))
            .await
            .unwrap_or_else(|error| panic!("connect as {role}: {error}"))
    }

    /// The test's own pool of the service role, through service-kit's session check as the
    /// gateway's.
    pub fn app_pool(&self) -> PgPool {
        service_kit::postgres::connect_lazy(
            self.postgres.options("ai_gateway_app", self.name),
            PgPoolOptions::new()
                .max_connections(TEST_POOL_CONNECTIONS)
                .acquire_timeout(TEST_POOL_WAIT),
        )
        .expect("a verify-full pool")
    }

    /// A store on [`Database::app_pool`]: what the management API will call.
    pub fn store(&self) -> Store {
        Store::from_pool(self.app_pool())
    }

    /// The settings the chart gives the gateway for this database.
    pub fn vars(&self) -> Vec<(&'static str, String)> {
        vec![
            ("AI_GATEWAY_POSTGRES_HOST", SERVER_HOST.to_owned()),
            (
                "AI_GATEWAY_POSTGRES_PORT",
                self.postgres.address.port().to_string(),
            ),
            ("AI_GATEWAY_POSTGRES_DATABASE", self.name.to_owned()),
            ("AI_GATEWAY_POSTGRES_SSLMODE", "verify-full".to_owned()),
            (
                "AI_GATEWAY_POSTGRES_CA_FILE",
                self.ca_file.display().to_string(),
            ),
            (
                "AI_GATEWAY__DATABASE__USERNAME",
                "ai_gateway_app".to_owned(),
            ),
            ("AI_GATEWAY__DATABASE__PASSWORD", TEST_PASSWORD.to_owned()),
        ]
    }
}

/// `models` as owned strings.
pub fn models(models: &[&str]) -> Vec<String> {
    models.iter().map(|model| (*model).to_owned()).collect()
}

/// Sets `role`'s models through the store, expecting `expected_version`; returns the new
/// version.
pub async fn set_models(
    store: &Store,
    rules: &RoleRules,
    role: &str,
    list: &[&str],
    expected_version: i64,
) -> i64 {
    match store
        .set_models(rules, role, &models(list), expected_version, "test-admin")
        .await
        .unwrap_or_else(|error| panic!("set the models of {role}: {error}"))
    {
        ai_gateway::store::Changed::Written { version } => version,
        ai_gateway::store::Changed::Unchanged => panic!("setting models always writes"),
    }
}
