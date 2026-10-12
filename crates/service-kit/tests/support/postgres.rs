//! Throwaway Postgres containers for the tests. A test binary starts one container, which the
//! tests of that binary that run at the same time share; the last test to drop its handle
//! removes it.
//!
//! - [`test_database`]: a plain Postgres (or the server named by `TEST_DATABASE_URL`) with a
//!   database of its own for each test.
//! - [`tls_postgres`]: a Postgres that requires nothing but serves TLS with a certificate from a
//!   CA made for this test process, so tests can connect with `sslmode=verify-full` as services
//!   do.
//!
//! Both start their container with [`start_postgres`] (`start.rs`), which says what it waited
//! for when it gives up.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use sqlx::postgres::{PgConnectOptions, PgSslMode};
use sqlx::{Connection, PgConnection};

use super::start::{POSTGRES_IMAGE, START_WITH_TLS, log_in_once, start_postgres};
use super::{Container, container_runtime, runtime_output, unavailable};

/// The password of the superuser and of every role the tests create.
pub const TEST_PASSWORD: &str = "service-kit-test";

/// The server certificate names only this address, which is where the container is published.
pub const SERVER_HOST: &str = "127.0.0.1";

/// A CA certificate in PEM, made for one test process.
pub struct TestCa {
    pub pem: String,
    issuer: Issuer<'static, KeyPair>,
}

impl TestCa {
    pub fn new(name: &str) -> Self {
        let key = KeyPair::generate().expect("CA key");
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(DnType::CommonName, name);
        let pem = params.self_signed(&key).expect("CA certificate").pem();
        Self {
            pem,
            issuer: Issuer::new(params, key),
        }
    }

    /// A server certificate for `names` (host names or IP addresses) and its key, in PEM.
    pub fn server_certificate(&self, names: &[&str]) -> (String, String) {
        let key = KeyPair::generate().expect("server key");
        let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        let params = CertificateParams::new(names).expect("server params");
        let certificate = params
            .signed_by(&key, &self.issuer)
            .expect("server certificate");
        (certificate.pem(), key.serialize_pem())
    }
}

/// A running Postgres with TLS.
pub struct TlsPostgres {
    pub address: SocketAddr,
    /// The CA that signed the server certificate.
    pub ca_pem: String,
    _container: Container,
}

/// Options to connect to the TLS Postgres published on `port`, verifying the server with the CA
/// in `ca_pem`.
fn tls_options(ca_pem: &str, port: u16, role: &str, database: &str) -> PgConnectOptions {
    PgConnectOptions::new_without_pgpass()
        .host(SERVER_HOST)
        .port(port)
        .username(role)
        .password(TEST_PASSWORD)
        .database(database)
        .ssl_mode(PgSslMode::VerifyFull)
        .ssl_root_cert_from_pem(ca_pem.to_owned().into_bytes())
}

impl TlsPostgres {
    /// Options to connect as `role` to `database`, verifying the server with the test CA.
    pub fn options(&self, role: &str, database: &str) -> PgConnectOptions {
        tls_options(&self.ca_pem, self.address.port(), role, database)
    }

    /// The superuser, on `database`.
    pub fn superuser(&self, database: &str) -> PgConnectOptions {
        self.options("postgres", database)
    }
}

static SHARED: tokio::sync::Mutex<Weak<TlsPostgres>> = tokio::sync::Mutex::const_new(Weak::new());

/// The shared Postgres, started on first use with `init` (static SQL) run by the superuser on
/// the `postgres` database. `None` (after printing why) when no container runtime is available
/// outside CI; in CI that fails the test instead. Every caller of a binary must pass the same
/// `init`.
pub async fn tls_postgres(init: &'static str) -> Option<Arc<TlsPostgres>> {
    let mut shared = SHARED.lock().await;
    if let Some(running) = shared.upgrade() {
        return Some(running);
    }
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return None;
    };
    let ca = TestCa::new("service-kit test CA");
    let (certificate, key) = ca.server_certificate(&[SERVER_HOST]);
    let cert_env = format!("TEST_TLS_CERT={certificate}");
    let key_env = format!("TEST_TLS_KEY={key}");
    let password_env = format!("POSTGRES_PASSWORD={TEST_PASSWORD}");
    let started = start_postgres(
        runtime,
        "postgres-tls",
        "Postgres with TLS",
        |container| {
            runtime_output(
                runtime,
                &[
                    "run",
                    "--detach",
                    "--name",
                    &container.name,
                    "--memory",
                    "512m",
                    "--env",
                    &password_env,
                    "--env",
                    &cert_env,
                    "--env",
                    &key_env,
                    "--publish",
                    "127.0.0.1::5432",
                    "--entrypoint",
                    "sh",
                    POSTGRES_IMAGE,
                    "-c",
                    START_WITH_TLS,
                ],
            );
        },
        |address| log_in_once(tls_options(&ca.pem, address.port(), "postgres", "postgres")),
    )
    .await
    .unwrap_or_else(|message| panic!("{message}"));
    let postgres = TlsPostgres {
        address: started.address,
        ca_pem: ca.pem,
        _container: started.container,
    };
    let mut connection = PgConnection::connect_with(&postgres.superuser("postgres"))
        .await
        .expect("connect to the started Postgres");
    sqlx::raw_sql(init)
        .execute(&mut connection)
        .await
        .expect("initialize the test cluster");
    connection.close().await.expect("close");
    let postgres = Arc::new(postgres);
    *shared = Arc::downgrade(&postgres);
    Some(postgres)
}

/// The password of the superuser of the plain Postgres.
const PLAIN_PASSWORD: &str = "service-kit-test";

/// Connections the shared plain Postgres accepts. Every test of a binary opens a few pools of up
/// to four connections each at the same time; the default of 100 is not enough for ten of them.
const PLAIN_MAX_CONNECTIONS: &str = "400";

/// A running plain Postgres, shared by the tests of a binary.
struct PlainPostgres {
    address: SocketAddr,
    /// Index in [`TEST_DATABASES`] of the next database to hand out.
    next_database: AtomicUsize,
    _container: Container,
}

impl PlainPostgres {
    fn options(&self, database: &str) -> PgConnectOptions {
        plain_options(self.address, database)
    }

    fn url_of(&self, database: &str) -> String {
        format!(
            "postgres://postgres:{PLAIN_PASSWORD}@{}/{database}",
            self.address
        )
    }
}

pub fn plain_options(address: SocketAddr, database: &str) -> PgConnectOptions {
    PgConnectOptions::new_without_pgpass()
        .host(&address.ip().to_string())
        .port(address.port())
        .username("postgres")
        .password(PLAIN_PASSWORD)
        .database(database)
}

/// The shared plain Postgres, or why starting it failed. The failure is kept so that the other
/// tests of the binary fail at once with the same message, instead of each waiting out the
/// start timeout again.
enum PlainState {
    Idle,
    Running(Weak<PlainPostgres>),
    Failed(String),
}

static PLAIN: tokio::sync::Mutex<PlainState> = tokio::sync::Mutex::const_new(PlainState::Idle);

/// A database for one test. The server (and with it every test's database) goes away when the
/// last test of the binary drops its handle.
pub struct TestDatabase {
    /// `postgres://` URL of the database, with credentials.
    pub url: String,
    _server: Option<Arc<PlainPostgres>>,
}

/// A database for one test, or `None` (after printing why) when there is no `TEST_DATABASE_URL`
/// and no container runtime outside CI; in CI that fails the test instead.
///
/// With `TEST_DATABASE_URL` set, that URL is used as it is. Otherwise a Postgres container is
/// started on first use and shared by the tests of the binary, and each test gets a database of
/// its own in it, so tests cannot see each other's advisory locks, which belong to a database.
/// When the tests of a binary do not overlap (`--test-threads=1`), the container is removed after
/// each test and started again for the next.
pub async fn test_database() -> Option<TestDatabase> {
    if let Some(url) = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|url| !url.is_empty())
    {
        return Some(TestDatabase { url, _server: None });
    }
    let server = plain_postgres().await?;
    let &(name, create) = TEST_DATABASES
        .get(server.next_database.fetch_add(1, Ordering::Relaxed))
        .unwrap_or_else(|| {
            panic!(
                "more tests share the server than the {} databases of TEST_DATABASES",
                TEST_DATABASES.len()
            )
        });
    let mut connection = PgConnection::connect_with(&server.options("postgres"))
        .await
        .expect("connect to the shared Postgres");
    sqlx::raw_sql(create)
        .execute(&mut connection)
        .await
        .expect("create the test's database");
    connection.close().await.expect("close");
    Some(TestDatabase {
        url: server.url_of(name),
        _server: Some(server),
    })
}

/// The databases the tests of a binary can take, each with its `CREATE DATABASE`. The statements
/// are literals because the workspace's SQL check (`tools/sql-check`) accepts no SQL built from
/// strings, and a database name cannot be a bind parameter. Add entries when a binary has more
/// tests that use a database than there are here.
const TEST_DATABASES: [(&str, &str); 32] = [
    ("test_db_00", "CREATE DATABASE test_db_00"),
    ("test_db_01", "CREATE DATABASE test_db_01"),
    ("test_db_02", "CREATE DATABASE test_db_02"),
    ("test_db_03", "CREATE DATABASE test_db_03"),
    ("test_db_04", "CREATE DATABASE test_db_04"),
    ("test_db_05", "CREATE DATABASE test_db_05"),
    ("test_db_06", "CREATE DATABASE test_db_06"),
    ("test_db_07", "CREATE DATABASE test_db_07"),
    ("test_db_08", "CREATE DATABASE test_db_08"),
    ("test_db_09", "CREATE DATABASE test_db_09"),
    ("test_db_10", "CREATE DATABASE test_db_10"),
    ("test_db_11", "CREATE DATABASE test_db_11"),
    ("test_db_12", "CREATE DATABASE test_db_12"),
    ("test_db_13", "CREATE DATABASE test_db_13"),
    ("test_db_14", "CREATE DATABASE test_db_14"),
    ("test_db_15", "CREATE DATABASE test_db_15"),
    ("test_db_16", "CREATE DATABASE test_db_16"),
    ("test_db_17", "CREATE DATABASE test_db_17"),
    ("test_db_18", "CREATE DATABASE test_db_18"),
    ("test_db_19", "CREATE DATABASE test_db_19"),
    ("test_db_20", "CREATE DATABASE test_db_20"),
    ("test_db_21", "CREATE DATABASE test_db_21"),
    ("test_db_22", "CREATE DATABASE test_db_22"),
    ("test_db_23", "CREATE DATABASE test_db_23"),
    ("test_db_24", "CREATE DATABASE test_db_24"),
    ("test_db_25", "CREATE DATABASE test_db_25"),
    ("test_db_26", "CREATE DATABASE test_db_26"),
    ("test_db_27", "CREATE DATABASE test_db_27"),
    ("test_db_28", "CREATE DATABASE test_db_28"),
    ("test_db_29", "CREATE DATABASE test_db_29"),
    ("test_db_30", "CREATE DATABASE test_db_30"),
    ("test_db_31", "CREATE DATABASE test_db_31"),
];

async fn plain_postgres() -> Option<Arc<PlainPostgres>> {
    let mut state = PLAIN.lock().await;
    match &*state {
        PlainState::Running(weak) => {
            if let Some(running) = weak.upgrade() {
                return Some(running);
            }
        }
        PlainState::Failed(message) => panic!("{message}"),
        PlainState::Idle => {}
    }
    let Some(runtime) = container_runtime() else {
        unavailable("TEST_DATABASE_URL, podman, or docker");
        return None;
    };
    let password_env = format!("POSTGRES_PASSWORD={PLAIN_PASSWORD}");
    let max_connections = format!("max_connections={PLAIN_MAX_CONNECTIONS}");
    let started = start_postgres(
        runtime,
        "postgres",
        "Postgres",
        |container| {
            runtime_output(
                runtime,
                &[
                    "run",
                    "--detach",
                    "--name",
                    &container.name,
                    "--memory",
                    "1g",
                    "--env",
                    &password_env,
                    "--publish",
                    "127.0.0.1::5432",
                    POSTGRES_IMAGE,
                    "postgres",
                    "-c",
                    &max_connections,
                ],
            );
        },
        |address| log_in_once(plain_options(address, "postgres")),
    )
    .await;
    let started = match started {
        Ok(started) => started,
        Err(message) => {
            *state = PlainState::Failed(message.clone());
            panic!("{message}");
        }
    };
    let postgres = Arc::new(PlainPostgres {
        address: started.address,
        next_database: AtomicUsize::new(0),
        _container: started.container,
    });
    *state = PlainState::Running(Arc::downgrade(&postgres));
    Some(postgres)
}
