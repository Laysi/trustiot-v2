//! The assistant binary as the chart starts it (Laysi/trustiot-v2#592): the product's schema,
//! applied by the migration Job itself (`testkit::product`), and the service role
//! `assistant_app` with the settings the chart sets.
//!
//! - Readiness passes once `assistant_app` logs in and the database has every migration file
//!   this release expects, without the AI gateway or any service the tools call: none of them
//!   listens here, and the service calls none of them before a question.
//! - It stays unready while the role cannot log in, and while its migration record is missing.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpListener};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use testkit::postgres::TEST_PASSWORD;
use testkit::product::{ProductPostgres, product_postgres};

/// The assistant binary, built by Cargo for these tests.
const ASSISTANT: &str = env!("CARGO_BIN_EXE_assistant");

/// The longest a started service may take to listen, to become ready, or to log what a test
/// waits for. Generous: test hosts are often loaded (CLAUDE.md, "Test time limits").
const START_LIMIT: Duration = Duration::from_secs(60);

/// What service-kit logs when a readiness check starts failing, and once every check passes.
const CHECK_FAILED: &str = "readiness check failed";
/// What the migration check logs once every file is there.
const COMPLETE: &str =
    "the database has every migration this image expects (newer ones are forward compatible)";
/// What it logs at ERROR while files are missing or changed.
const BEHIND: &str = "the database lacks migrations this image expects, so the service stays \
                      unready; run the migration Job of this release (tools/migrate, \
                      docs/runbooks/migrations.md)";

const DELETE_RECORD: &str = "DELETE FROM assistant.trustiot_migrate_applied
    WHERE component = 'assistant' AND name = '0001_assistant.sql'";
/// Puts the record back as the Job wrote it, from its own table.
const RESTORE_RECORD: &str =
    "INSERT INTO assistant.trustiot_migrate_applied (component, name, sha256)
    SELECT component, name, sha256 FROM trustiot_migrate.applied
    WHERE component = 'assistant' AND name = '0001_assistant.sql'";

/// An origin where nothing listens: a port the OS handed out and that was closed again.
fn closed_origin() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    let address = listener.local_addr().expect("local address");
    drop(listener);
    format!("http://{address}")
}

/// The lines a child process printed, shared with the thread that reads them.
#[derive(Clone, Default)]
struct Output(Arc<Mutex<Vec<String>>>);

impl Output {
    fn collect(&self, stream: impl Read + Send + 'static) {
        let lines = Arc::clone(&self.0);
        thread::spawn(move || {
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                lines.lock().expect("output lock").push(line);
            }
        });
    }

    fn text(&self) -> String {
        self.0.lock().expect("output lock").join("\n")
    }

    /// The JSON lines whose `message` is `message`.
    fn messages(&self, message: &str) -> Vec<Value> {
        self.0
            .lock()
            .expect("output lock")
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| value["message"] == message)
            .collect()
    }
}

/// The assistant running as a child process.
struct Service {
    child: Child,
    stdout: Output,
    stderr: Output,
    health: SocketAddr,
}

impl Service {
    /// Starts the service on free local ports against `product` as `assistant_app` with
    /// `password`, with every address the chart sets pointing where nothing listens, and returns
    /// once it listens, ready or not. The environment is otherwise empty, so the developer's own
    /// variables cannot leak in.
    async fn spawn(product: &ProductPostgres, password: &str) -> Self {
        let mut command = Command::new(ASSISTANT);
        command
            .env_clear()
            .env("TRUSTIOT__HTTP__LISTEN_ADDR", "127.0.0.1:0")
            .env("TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR", "127.0.0.1:0")
            .env("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "100ms")
            .env("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "5s")
            .env("TRUSTIOT__DATABASE__USERNAME", "assistant_app")
            .env("TRUSTIOT__DATABASE__PASSWORD", password)
            .env("TRUSTIOT__AI_GATEWAY__URL", closed_origin())
            .env("TRUSTIOT__IDENTITY__EXCHANGE_URL", closed_origin())
            .env("TRUSTIOT__TELEMETRY_QUERY__URL", closed_origin())
            .env("TRUSTIOT__ASSETS__URL", closed_origin())
            .envs(product.platform_env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("start the assistant");
        let (stdout, stderr) = (Output::default(), Output::default());
        stdout.collect(child.stdout.take().expect("piped stdout"));
        stderr.collect(child.stderr.take().expect("piped stderr"));
        let deadline = Instant::now() + START_LIMIT;
        let listening = loop {
            if let Some(listening) = stdout.messages("listening").into_iter().next() {
                break listening;
            }
            if let Ok(Some(status)) = child.try_wait() {
                panic!(
                    "the assistant exited with {status}\nstdout:\n{}\nstderr:\n{}",
                    stdout.text(),
                    stderr.text()
                );
            }
            assert!(
                Instant::now() < deadline,
                "the assistant did not log `listening` within {START_LIMIT:?}\nstdout:\n{}\nstderr:\n{}",
                stdout.text(),
                stderr.text()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let health = listening["fields"]["health_address"]
            .as_str()
            .and_then(|address| address.parse().ok())
            .unwrap_or_else(|| panic!("no health_address in {listening}"));
        Self {
            child,
            stdout,
            stderr,
            health,
        }
    }

    /// The status `/readyz` answers now, if it answers. Readiness runs its checks only when it
    /// is asked, as the kubelet does.
    async fn readiness(&self) -> Option<u16> {
        reqwest::get(format!("http://{}/readyz", self.health))
            .await
            .ok()
            .map(|response| response.status().as_u16())
    }

    /// Asks `/readyz` until it answers `status`.
    async fn wait_readiness(&self, status: u16) {
        let what = format!("/readyz answering {status}");
        wait_for(&what, || async {
            (self.readiness().await == Some(status)).then_some(())
        })
        .await;
    }

    /// Asks `/readyz` until the service logs `message`, and returns that line.
    async fn wait_logged(&self, message: &str) -> Value {
        wait_for(message, || async {
            let _ = self.readiness().await;
            self.stdout.messages(message).into_iter().next()
        })
        .await
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Polls `check` every 100 ms until it returns `Some`, for at most [`START_LIMIT`]; fails naming
/// `what` was awaited.
async fn wait_for<T, F, Fut>(what: &str, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + START_LIMIT;
    loop {
        if let Some(found) = check().await {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: not within {START_LIMIT:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ready_once_the_service_role_logs_in_on_the_current_schema() {
    let Some(product) = product_postgres(&["assistant_app"]).await else {
        return;
    };
    let mut admin = product.superuser().await;

    // 1. The role logs in and every file is there: ready, with its four tools and the run
    //    delegation, while neither the AI gateway nor any service the tools call listens.
    let ready = Service::spawn(&product, TEST_PASSWORD).await;
    ready.wait_readiness(200).await;
    ready.wait_logged(COMPLETE).await;
    let built = ready.wait_logged("assistant ready").await;
    assert_eq!(built["fields"]["tools"], 4, "{built}");
    assert_eq!(built["fields"]["delegation"], true, "{built}");
    // No line carries the password.
    assert!(!ready.stdout.text().contains(TEST_PASSWORD));
    assert!(!ready.stderr.text().contains(TEST_PASSWORD));

    // 2. A wrong password: the process starts, the check fails with the login error, and it
    //    stays unready.
    let refused = Service::spawn(&product, "not-the-password").await;
    let failed = refused.wait_logged(CHECK_FAILED).await;
    assert_eq!(refused.readiness().await, Some(503), "{failed}");
    assert!(!refused.stdout.text().contains("not-the-password"));
    drop(refused);

    // 3. A database without 0001_assistant.sql: a new process stays unready and names the file;
    //    once the record is back it turns ready without a restart.
    sqlx::raw_sql(DELETE_RECORD)
        .execute(&mut admin)
        .await
        .expect("delete the record");
    let lacking = Service::spawn(&product, TEST_PASSWORD).await;
    let behind = lacking.wait_logged(BEHIND).await;
    assert_eq!(
        behind["fields"]["missing"], "assistant/0001_assistant.sql",
        "{behind}"
    );
    lacking.wait_readiness(503).await;
    sqlx::raw_sql(RESTORE_RECORD)
        .execute(&mut admin)
        .await
        .expect("restore the record");
    lacking.wait_readiness(200).await;
    drop(lacking);
    drop(ready);
}
