//! A throwaway Postgres that serves TLS with a certificate from a CA made for this test, so
//! the consumer's service-kit pools connect with `sslmode=verify-full` as services do.
//!
//! Follows `crates/service-kit/tests/support/postgres.rs` (same image, same TLS setup), until a
//! shared test crate exists; unlike the kit's, every test gets its own container, as the outbox
//! tests always had.

use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::Command;
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

/// How often a start retries with another port when the one it picked was taken in between, and
/// how often a restart retries when the port is briefly held by another process.
const PORT_ATTEMPTS: u32 = 8;

/// A running Postgres with TLS; the container and the CA file are removed when dropped.
pub struct TlsPostgres {
    pub address: SocketAddr,
    /// The CA that signed the server certificate, in PEM.
    pub ca_pem: String,
    /// The same CA in a file, for the example service's `ca_file` setting.
    pub ca_file: PathBuf,
    container: Container,
    /// Whether this test chose the host port, which is what lets [`Self::restart`] keep it.
    fixed_port: bool,
}

impl TlsPostgres {
    /// Starts a Postgres and runs `init` (static SQL) as the superuser on `postgres`. `None`
    /// (after printing why) when no container runtime is available outside CI.
    ///
    /// The runtime assigns the host port, which podman keeps across a restart and Docker does
    /// not: a test that restarts the container uses [`Self::start_restartable`].
    pub async fn start(init: &'static str) -> Option<Self> {
        Self::start_with(init, false).await
    }

    /// Like [`Self::start`], for a test that calls [`Self::restart`]. The test picks a free
    /// host port itself and publishes it explicitly, because only an explicit port is kept by
    /// both podman and Docker when the container restarts (Docker reassigns an ephemeral one,
    /// and every pool and option set made before the restart then points at nothing).
    pub async fn start_restartable(init: &'static str) -> Option<Self> {
        Self::start_with(init, true).await
    }

    async fn start_with(init: &'static str, fixed_port: bool) -> Option<Self> {
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
        let address = run_container(
            runtime,
            &container.name,
            &[&password_env, &cert_env, &key_env],
            fixed_port,
        );
        let ca_file =
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{}-ca.pem", container.name));
        std::fs::write(&ca_file, &ca_pem).expect("write the test CA");
        let postgres = Self {
            address,
            ca_pem,
            ca_file,
            container,
            fixed_port,
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

impl TlsPostgres {
    /// Kills the server (SIGKILL, so the next start is crash recovery) and starts it again,
    /// then checks that it is published on the same host port. Only for a Postgres made by
    /// [`Self::start_restartable`].
    ///
    /// The port is released while the container is down, so another process (an outgoing
    /// connection of another test, say) can take it just then; the restart is retried, up to
    /// [`PORT_ATTEMPTS`] times, when the runtime reports that.
    pub fn restart(&self) {
        assert!(
            self.fixed_port,
            "restart needs a Postgres made by start_restartable"
        );
        let runtime = self.container.runtime;
        let mut attempt = 1;
        loop {
            let output = Command::new(runtime)
                .args(["restart", "--time", "0", &self.container.name])
                .output()
                .unwrap_or_else(|error| panic!("{runtime} restart: {error}"));
            if output.status.success() {
                break;
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                port_is_taken(&stderr) && attempt < PORT_ATTEMPTS,
                "{runtime} restart failed (attempt {attempt} of {PORT_ATTEMPTS}): {stderr}"
            );
            attempt += 1;
            std::thread::sleep(Duration::from_millis(500));
        }
        let mapping = runtime_output(runtime, &["port", &self.container.name, "5432/tcp"]);
        assert_eq!(
            host_address(&mapping),
            self.address,
            "the host port changed across the restart: {mapping}"
        );
    }
}

/// Starts the container and returns the address it is published on: with `fixed_port`, a free
/// port picked here and requested as `-p 127.0.0.1:<port>:5432`, otherwise one the runtime
/// assigns.
///
/// Picking a port and publishing it are two steps, so another process can take the port in
/// between. The start then fails with a bind error (on Docker leaving a created container
/// behind, which is removed), and it is retried with a new port up to [`PORT_ATTEMPTS`] times.
/// Any other failure is not retried.
fn run_container(runtime: &'static str, name: &str, env: &[&str], fixed_port: bool) -> SocketAddr {
    let mut attempt = 1;
    loop {
        let publish = if fixed_port {
            format!("127.0.0.1:{}:5432", free_port())
        } else {
            "127.0.0.1::5432".to_owned()
        };
        let mut command = Command::new(runtime);
        command.args(["run", "--detach", "--name", name]);
        for value in env {
            command.args(["--env", value]);
        }
        let output = command
            .args(["--publish", &publish, "--entrypoint", "sh", POSTGRES_IMAGE])
            .args(["-c", START_WITH_TLS])
            .output()
            .unwrap_or_else(|error| panic!("{runtime} run: {error}"));
        if output.status.success() {
            let mapping = runtime_output(runtime, &["port", name, "5432/tcp"]);
            return host_address(&mapping);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            fixed_port && port_is_taken(&stderr) && attempt < PORT_ATTEMPTS,
            "{runtime} run failed (attempt {attempt} of {PORT_ATTEMPTS}): {stderr}"
        );
        let _ = Command::new(runtime)
            .args(["rm", "--force", "--volumes", name])
            .output();
        attempt += 1;
    }
}

/// A port on 127.0.0.1 that nothing listens on right now.
fn free_port() -> u16 {
    TcpListener::bind((SERVER_HOST, 0))
        .and_then(|listener| listener.local_addr())
        .expect("pick a free port")
        .port()
}

/// Whether a runtime's error message says the host port could not be bound.
fn port_is_taken(stderr: &str) -> bool {
    let stderr = stderr.to_lowercase();
    ["already in use", "already allocated", "address in use"]
        .iter()
        .any(|text| stderr.contains(text))
}

/// The host address in the output of `port <container> 5432/tcp`.
fn host_address(mapping: &str) -> SocketAddr {
    mapping
        .lines()
        .find_map(|line| line.trim().parse().ok())
        .unwrap_or_else(|| panic!("no host address for 5432: {mapping}"))
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
