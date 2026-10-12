//! A throwaway Postgres that serves TLS with a certificate from a CA made for the test process,
//! so tests connect with `sslmode=verify-full` as services do (service-kit refuses anything
//! else). The tests of one binary that run at the same time share one container; it is removed
//! when the last of them drops it. Give each test its own database.
//!
//! The container starts through [`start`], the same file that service-kit's, outbox's and
//! identity's tests include: the wait is bounded by measured limits, a container whose published
//! port is dead is replaced, and a failure says what it waited for and shows the container's state
//! and log tail.

use std::net::SocketAddr;
use std::sync::{Arc, Weak};

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use sqlx::postgres::{PgConnectOptions, PgSslMode};
use sqlx::{Connection, PgConnection};

use crate::container::{Container, container_runtime, runtime_output, unavailable, unique_name};

// This file lives in service-kit's tests so that service-kit, which testkit depends on, can use
// it too (a dev-dependency on testkit would make a cycle, and with it service-kit's
// `--no-default-features` tests would build with the default features). It needs the
// `Container` and `unique_name` imported above.
#[path = "../../service-kit/tests/support/start.rs"]
pub mod start;

use self::start::{START_LIMITS, START_WITH_TLS, StartLimits, log_in_once, start_postgres_within};

/// The image, pinned by digest; the same as service-kit's tests.
pub use self::start::POSTGRES_IMAGE;

/// The password of the superuser. Give the roles a test creates this password too, so
/// [`TlsPostgres::options`] can log in as them.
pub const TEST_PASSWORD: &str = "testkit-postgres";

/// The server certificate names only this address, where the container is published.
pub const SERVER_HOST: &str = "127.0.0.1";

/// A CA made for one test process.
pub struct TestCa {
    /// The CA certificate in PEM.
    pub pem: String,
    issuer: Issuer<'static, KeyPair>,
}

impl TestCa {
    /// A new self-signed CA named `name`.
    ///
    /// # Panics
    ///
    /// When key or certificate generation fails.
    #[must_use]
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
    ///
    /// # Panics
    ///
    /// When key or certificate generation fails.
    #[must_use]
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

impl std::fmt::Debug for TestCa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestCa").finish_non_exhaustive()
    }
}

/// A running Postgres with TLS.
#[derive(Debug)]
pub struct TlsPostgres {
    /// Where the server listens.
    pub address: SocketAddr,
    /// The CA that signed the server certificate.
    pub ca_pem: String,
    _container: Container,
}

/// Options to connect to the Postgres published on `port`, verifying the server with the CA in
/// `ca_pem`.
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
    /// Options to connect as `role` (with [`TEST_PASSWORD`]) to `database`, verifying the server
    /// with the test CA.
    #[must_use]
    pub fn options(&self, role: &str, database: &str) -> PgConnectOptions {
        tls_options(&self.ca_pem, self.address.port(), role, database)
    }

    /// The superuser, on `database`.
    #[must_use]
    pub fn superuser(&self, database: &str) -> PgConnectOptions {
        self.options("postgres", database)
    }

    /// A plain connection (no session check) as `role` to `database`, for setup and inspection.
    ///
    /// # Panics
    ///
    /// When the connection fails.
    pub async fn connect(&self, role: &str, database: &str) -> PgConnection {
        PgConnection::connect_with(&self.options(role, database))
            .await
            .unwrap_or_else(|error| panic!("connect as {role} to {database}: {error}"))
    }
}

static SHARED: tokio::sync::Mutex<Weak<TlsPostgres>> = tokio::sync::Mutex::const_new(Weak::new());

/// The shared Postgres, started on first use; `init` (static SQL, for example the roles the
/// tests need) runs once as the superuser on the `postgres` database. Every caller in a test
/// binary must pass the same `init`.
///
/// `None` when no container runtime is available outside CI (after printing why: the test then
/// returns early); with `CI` set that fails the test instead.
///
/// # Panics
///
/// When the container or `init` fails, or the server cannot be logged in to within the limits of
/// [`start::START_LIMITS`]. The message then says what was awaited and for how long, and shows
/// the state and the last log lines of the container (never its environment, which holds the
/// TLS key and the password).
pub async fn tls_postgres(init: &'static str) -> Option<Arc<TlsPostgres>> {
    let mut shared = SHARED.lock().await;
    if let Some(running) = shared.upgrade() {
        return Some(running);
    }
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return None;
    };
    let postgres =
        start_tls_postgres_within(START_LIMITS, runtime, "postgres-tls", |_address| false)
            .await
            .unwrap_or_else(|message| panic!("{message}"));
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

/// Starts one TLS Postgres container with `runtime`, named from `kind`, through
/// [`start::start_postgres_within`] with `limits`, and returns it once the host can log in with
/// `verify-full`. [`tls_postgres`] calls it with [`start::START_LIMITS`] and no dead port; a test
/// of the start itself passes shorter limits, and `published_port_dead` to make the host's login
/// to an address fail, as it does when Docker did not publish the port: the container is then
/// replaced by another one.
///
/// # Errors
///
/// The message of [`start::start_postgres_within`]: what was awaited, for how long, the last
/// error, and the state and last log lines of the container.
pub async fn start_tls_postgres_within(
    limits: StartLimits,
    runtime: &'static str,
    kind: &str,
    published_port_dead: impl Fn(SocketAddr) -> bool,
) -> Result<TlsPostgres, String> {
    let ca = TestCa::new("testkit test CA");
    let (certificate, key) = ca.server_certificate(&[SERVER_HOST]);
    let cert_env = format!("TEST_TLS_CERT={certificate}");
    let key_env = format!("TEST_TLS_KEY={key}");
    let password_env = format!("POSTGRES_PASSWORD={TEST_PASSWORD}");
    let started = start_postgres_within(
        limits,
        runtime,
        kind,
        "Postgres with TLS",
        |container| {
            let _id = runtime_output(
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
        |address| {
            let dead = published_port_dead(address);
            let options = tls_options(&ca.pem, address.port(), "postgres", "postgres");
            async move {
                if dead {
                    return Err("simulated: nothing listens on the published port".to_owned());
                }
                log_in_once(options).await
            }
        },
    )
    .await?;
    Ok(TlsPostgres {
        address: started.address,
        ca_pem: ca.pem,
        _container: started.container,
    })
}
