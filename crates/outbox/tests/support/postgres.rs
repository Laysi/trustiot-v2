//! A throwaway Postgres that serves TLS with a certificate from a CA made for this test, so
//! the consumer's service-kit pools connect with `sslmode=verify-full` as services do.
//!
//! Follows `crates/service-kit/tests/support/postgres.rs` (same image, same TLS setup), until a
//! shared test crate exists; unlike the kit's, every test gets its own container, as the outbox
//! tests always had.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use sqlx::postgres::{PgConnectOptions, PgSslMode};
use sqlx::{Connection, PgConnection};

use super::{Container, container_runtime, runtime_output, unavailable, unique};

/// Same image and digest as the kit's tests.
const POSTGRES_IMAGE: &str = "docker.io/library/postgres:18.6-alpine@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873";

/// The password of the superuser and of every role the tests create.
pub const TEST_PASSWORD: &str = "outbox-test";

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

/// A running Postgres with TLS; the container and the CA file are removed when dropped.
pub struct TlsPostgres {
    pub address: SocketAddr,
    /// The CA that signed the server certificate, in PEM.
    pub ca_pem: String,
    /// The same CA in a file, for the example service's `ca_file` setting.
    pub ca_file: PathBuf,
    _container: Container,
}

impl TlsPostgres {
    /// Starts a Postgres and runs `init` (static SQL) as the superuser on `postgres`. `None`
    /// (after printing why) when no container runtime is available outside CI.
    pub async fn start(init: &'static str) -> Option<Self> {
        let Some(runtime) = container_runtime() else {
            unavailable("podman or docker");
            return None;
        };
        let (ca_pem, certificate, key) = certificates();
        let container = Container {
            runtime,
            name: format!("outbox-test-{}", unique("postgres").replace('_', "-")),
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
        let mapping = runtime_output(runtime, &["port", &container.name, "5432/tcp"]);
        let address: SocketAddr = mapping
            .lines()
            .find_map(|line| line.trim().parse().ok())
            .unwrap_or_else(|| panic!("no host address for 5432: {mapping}"));
        let ca_file =
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{}-ca.pem", container.name));
        std::fs::write(&ca_file, &ca_pem).expect("write the test CA");
        let postgres = Self {
            address,
            ca_pem,
            ca_file,
            _container: container,
        };
        // The image runs initdb with networking off and then restarts; wait for the real server.
        let started = Instant::now();
        let mut connection = loop {
            match PgConnection::connect_with(&postgres.options("postgres")).await {
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
        Some(postgres)
    }

    /// Options to connect as `role` to the `postgres` database, verifying the server with the
    /// test CA.
    pub fn options(&self, role: &str) -> PgConnectOptions {
        PgConnectOptions::new_without_pgpass()
            .host(SERVER_HOST)
            .port(self.address.port())
            .username(role)
            .password(TEST_PASSWORD)
            .database("postgres")
            .ssl_mode(PgSslMode::VerifyFull)
            .ssl_root_cert_from_pem(self.ca_pem.clone().into_bytes())
    }
}

impl Drop for TlsPostgres {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.ca_file);
    }
}

/// A CA, and a server certificate and key for [`SERVER_HOST`] signed by it, all in PEM.
fn certificates() -> (String, String, String) {
    let ca_key = KeyPair::generate().expect("CA key");
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "outbox test CA");
    let ca_pem = params.self_signed(&ca_key).expect("CA certificate").pem();
    let issuer = Issuer::new(params, ca_key);
    let key = KeyPair::generate().expect("server key");
    let server = CertificateParams::new(vec![SERVER_HOST.to_owned()]).expect("server params");
    let certificate = server.signed_by(&key, &issuer).expect("server certificate");
    (ca_pem, certificate.pem(), key.serialize_pem())
}
