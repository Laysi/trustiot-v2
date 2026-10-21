//! The product's Postgres schema in a throwaway database, applied by the migration Job itself
//! (`tools/migrate`, [`migrate::run`] with [`PRODUCT`]): every service's roles, schema, outbox
//! tables, policies and migrations, exactly as a deployment gets them. Services whose tests need
//! more than their own migration files (an outbox producer's or consumer's tables, another
//! service's schema) start from here instead of applying files by hand.
//!
//! Roles are cluster-wide and the Job refuses a migration role that owns more than one database,
//! so the product is applied once per Postgres container, into [`DATABASE`]. The tests of one
//! binary share the container ([`crate::postgres::tls_postgres`]) and so the database: give
//! them data of their own (new tenants, new users), never expect an empty table.
//!
//! The Job leaves a login role without a bound Secret NOLOGIN, as in a deployment that does not
//! run its service; the tests name the roles their services log in with, and only those get
//! LOGIN with [`TEST_PASSWORD`](crate::postgres::TEST_PASSWORD), as the chart's Secrets would
//! give them.

use std::path::PathBuf;
use std::sync::{Arc, Weak};

use migrate::config::{Platform, PostgresSettings, RunConfig, WaitSettings};
use migrate::registry::{PRODUCT, Service};
use service_kit::postgres::SslMode;
use sqlx::{Connection, PgConnection};

use crate::postgres::{SERVER_HOST, TEST_PASSWORD, TlsPostgres, tls_postgres};

/// The migration role, as the chart's bundled Postgres makes it: it owns the database and may
/// create roles, and is no superuser.
pub const MIGRATOR: &str = "trustiot_owner";

/// The `init` of [`tls_postgres`] for every test binary that uses this module: the migration
/// role with the test password.
pub const PRODUCT_INIT: &str =
    "CREATE ROLE trustiot_owner LOGIN CREATEROLE PASSWORD 'testkit-postgres'";

/// The database the product is applied to.
pub const DATABASE: &str = "trustiot";

const CREATE_DATABASE: &str = "CREATE DATABASE trustiot OWNER trustiot_owner";

/// Gives one role LOGIN with the test password, as a superuser. A temporary function, so the role
/// name is a bound parameter and the statement stays static.
const LOGIN_FUNCTION: &str = "CREATE FUNCTION pg_temp.testkit_allow_login(role_name text)
RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    EXECUTE format('ALTER ROLE %I LOGIN PASSWORD %L', role_name, 'testkit-postgres');
END
$$";
const ALLOW_LOGIN: &str = "SELECT pg_temp.testkit_allow_login($1)";

/// A Postgres container with the product's schema in [`DATABASE`].
#[derive(Debug)]
pub struct ProductPostgres {
    /// The container; connect with [`TlsPostgres::options`] and [`DATABASE`].
    pub postgres: Arc<TlsPostgres>,
    /// The test CA in a file, for services that read `TRUSTIOT_POSTGRES_CA_FILE`.
    pub ca_file: PathBuf,
}

impl ProductPostgres {
    /// The environment that points a service at this database (the platform settings the chart
    /// sets), without the role and password.
    #[must_use]
    pub fn platform_env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("TRUSTIOT_POSTGRES_HOST", SERVER_HOST.to_owned()),
            (
                "TRUSTIOT_POSTGRES_PORT",
                self.postgres.address.port().to_string(),
            ),
            ("TRUSTIOT_POSTGRES_DATABASE", DATABASE.to_owned()),
            ("TRUSTIOT_POSTGRES_SSLMODE", "verify-full".to_owned()),
            (
                "TRUSTIOT_POSTGRES_CA_FILE",
                self.ca_file.display().to_string(),
            ),
        ]
    }

    /// A plain connection (no session check) as the superuser, which row-level security does
    /// not limit, for setup and inspection.
    ///
    /// # Panics
    ///
    /// When the connection fails.
    pub async fn superuser(&self) -> PgConnection {
        self.postgres.connect("postgres", DATABASE).await
    }
}

impl Drop for ProductPostgres {
    fn drop(&mut self) {
        if let Some(dir) = self.ca_file.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

static SHARED: tokio::sync::Mutex<Weak<ProductPostgres>> =
    tokio::sync::Mutex::const_new(Weak::new());

/// The shared Postgres with the product's schema, set up on first use: the migration role
/// ([`PRODUCT_INIT`]), the database, one run of the migration Job, and LOGIN for `logins`. Later
/// callers get the same container; `logins` are given LOGIN on every call. Every caller in a test
/// binary must use this function (not [`tls_postgres`] with another `init`).
///
/// `None` when no container runtime is available outside CI (see [`tls_postgres`]).
///
/// # Panics
///
/// When a role in `logins` is not a login role of the product's registry, or the setup or the
/// migration run fails.
pub async fn product_postgres(logins: &[&str]) -> Option<Arc<ProductPostgres>> {
    let login_roles: Vec<String> = PRODUCT
        .services
        .iter()
        .flat_map(Service::login_roles)
        .collect();
    for role in logins {
        assert!(
            login_roles.iter().any(|known| known == role),
            "{role} is not a login role of tools/migrate's registry ({login_roles:?})"
        );
    }
    let mut shared = SHARED.lock().await;
    let product = match shared.upgrade() {
        Some(running) => running,
        None => {
            let postgres = tls_postgres(PRODUCT_INIT).await?;
            let product = Arc::new(apply(postgres).await);
            *shared = Arc::downgrade(&product);
            product
        }
    };
    // The lock stays held while the roles get LOGIN: concurrent tests in one binary would
    // otherwise run ALTER ROLE on the same role at once, which Postgres refuses with "tuple
    // concurrently updated".
    let mut admin = product.superuser().await;
    sqlx::raw_sql(LOGIN_FUNCTION)
        .execute(&mut admin)
        .await
        .expect("create the login helper");
    for role in logins {
        sqlx::query(ALLOW_LOGIN)
            .bind(role)
            .execute(&mut admin)
            .await
            .unwrap_or_else(|error| panic!("let {role} log in: {error}"));
    }
    admin.close().await.expect("close");
    drop(shared);
    Some(product)
}

/// Creates the database and runs the migration Job on it.
async fn apply(postgres: Arc<TlsPostgres>) -> ProductPostgres {
    let mut admin = PgConnection::connect_with(&postgres.superuser("postgres"))
        .await
        .expect("connect as the superuser");
    sqlx::raw_sql(CREATE_DATABASE)
        .execute(&mut admin)
        .await
        .expect("create the product database");
    admin.close().await.expect("close");

    let dir = std::env::temp_dir().join(format!(
        "testkit-product-{}-{}",
        std::process::id(),
        postgres.address.port()
    ));
    std::fs::create_dir_all(&dir).expect("create a directory for the CA file");
    let ca_file = dir.join("postgres-ca.crt");
    std::fs::write(&ca_file, &postgres.ca_pem).expect("write the CA file");
    let config = RunConfig {
        platform: Platform {
            postgres_host: SERVER_HOST.to_owned(),
            postgres_port: postgres.address.port(),
            postgres_database: DATABASE.to_owned(),
            postgres_sslmode: SslMode::VerifyFull,
            postgres_ca_file: ca_file.clone(),
            clickhouse_url: None,
            clickhouse_ca_file: None,
        },
        postgres: PostgresSettings {
            username: MIGRATOR.to_owned(),
            password: TEST_PASSWORD.to_owned(),
        },
        clickhouse: None,
        passwords: None,
        wait: WaitSettings::default(),
    };
    migrate::run(&config, &PRODUCT)
        .await
        .unwrap_or_else(|error| panic!("the migration Job's run on the test database: {error}"));
    ProductPostgres { postgres, ca_file }
}
