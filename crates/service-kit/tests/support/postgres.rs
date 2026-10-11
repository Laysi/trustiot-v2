//! A throwaway Postgres that requires nothing but serves TLS with a certificate from a CA made
//! for this test process, so tests can connect with `sslmode=verify-full` as services do. One
//! container is shared by the tests of a binary that run at the same time and removed when the
//! last of them drops it.

use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use sqlx::postgres::{PgConnectOptions, PgSslMode};
use sqlx::{Connection, PgConnection};

use super::{Container, container_runtime, runtime_output, unavailable, unique_name};

/// Same image as tests/job_lock.rs.
pub const POSTGRES_IMAGE: &str = "docker.io/library/postgres:18.6-alpine@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873";

/// The password of the superuser and of every role the tests create.
pub const TEST_PASSWORD: &str = "service-kit-test";

/// The server certificate names only this address, which is where the container is published.
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

impl TlsPostgres {
    /// Options to connect as `role` to `database`, verifying the server with the test CA.
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
    let container = Container {
        runtime,
        name: unique_name("postgres-tls"),
    };
    let cert_env = format!("TEST_TLS_CERT={certificate}");
    let key_env = format!("TEST_TLS_KEY={key}");
    let password_env = format!("POSTGRES_PASSWORD={TEST_PASSWORD}");
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
