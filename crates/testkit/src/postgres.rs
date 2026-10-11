//! A throwaway Postgres that serves TLS with a certificate from a CA made for the test process,
//! so tests connect with `sslmode=verify-full` as services do (service-kit refuses anything
//! else). The tests of one binary that run at the same time share one container; it is removed
//! when the last of them drops it. Give each test its own database.

use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use sqlx::postgres::{PgConnectOptions, PgSslMode};
use sqlx::{Connection, PgConnection};

use crate::container::{Container, container_runtime, runtime_output, unavailable, unique_name};

/// The image, pinned by digest; the same as service-kit's tests.
pub const POSTGRES_IMAGE: &str = "docker.io/library/postgres:18.6-alpine@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873";

/// The password of the superuser. Give the roles a test creates this password too, so
/// [`TlsPostgres::options`] can log in as them.
pub const TEST_PASSWORD: &str = "testkit-postgres";

/// The server certificate names only this address, where the container is published.
pub const SERVER_HOST: &str = "127.0.0.1";

/// Moves the generated certificate and key into place for the postgres user (the key must be
/// readable only by it) and starts the image's entrypoint with TLS on.
const START_WITH_TLS: &str = "set -eu
mkdir -p /etc/postgres-tls
printf '%s' \"$TEST_TLS_CERT\" > /etc/postgres-tls/server.crt
(umask 077; printf '%s' \"$TEST_TLS_KEY\" > /etc/postgres-tls/server.key)
chown -R postgres:postgres /etc/postgres-tls
exec docker-entrypoint.sh postgres -c ssl=on \
  -c ssl_cert_file=/etc/postgres-tls/server.crt -c ssl_key_file=/etc/postgres-tls/server.key";

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

impl TlsPostgres {
    /// Options to connect as `role` (with [`TEST_PASSWORD`]) to `database`, verifying the server
    /// with the test CA.
    #[must_use]
    pub fn options(&self, role: &str, database: &str) -> PgConnectOptions {
        PgConnectOptions::new_without_pgpass()
            .host(SERVER_HOST)
            .port(self.address.port())
            .username(role)
            .password(TEST_PASSWORD)
            .database(database)
            .ssl_mode(PgSslMode::VerifyFull)
            .ssl_root_cert_from_pem(self.ca_pem.clone().into_bytes())
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
/// When the container or `init` fails, or the server does not start within 90 seconds.
pub async fn tls_postgres(init: &'static str) -> Option<Arc<TlsPostgres>> {
    let mut shared = SHARED.lock().await;
    if let Some(running) = shared.upgrade() {
        return Some(running);
    }
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return None;
    };
    let ca = TestCa::new("testkit test CA");
    let (certificate, key) = ca.server_certificate(&[SERVER_HOST]);
    let container = Container {
        runtime,
        name: unique_name("postgres-tls"),
    };
    let cert_env = format!("TEST_TLS_CERT={certificate}");
    let key_env = format!("TEST_TLS_KEY={key}");
    let password_env = format!("POSTGRES_PASSWORD={TEST_PASSWORD}");
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
    let address = container.host_address("5432/tcp");
    let postgres = TlsPostgres {
        address,
        ca_pem: ca.pem,
        _container: container,
    };
    // The image runs initdb with networking off and then restarts; wait for the real server.
    let started = Instant::now();
    let mut connection = loop {
        match PgConnection::connect_with(&postgres.superuser("postgres")).await {
            Ok(connection) => break connection,
            Err(error) => assert!(
                started.elapsed() < Duration::from_secs(90),
                "Postgres with TLS did not start in time: {error}"
            ),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    sqlx::raw_sql(init)
        .execute(&mut connection)
        .await
        .expect("initialize the test cluster");
    connection.close().await.expect("close");
    let postgres = Arc::new(postgres);
    *shared = Arc::downgrade(&postgres);
    Some(postgres)
}
