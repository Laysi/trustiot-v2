//! The real binary under service-kit. Against a provider stand-in over TLS that trusts only a
//! test CA (`SSL_CERT_FILE`): it reads the key from the variable its configuration names, uses
//! it, and never writes it, or its database password, to its logs (at trace level, every crate
//! included) or its responses. It does not start without its key, or when a role's limits do
//! not fit in the request timeout. With a real Postgres (tests/support/database.rs): two
//! processes on one database both use a change for their next request, a restarted process uses
//! the last change, a stored model list that the configuration no longer allows is ignored
//! with an ERROR, and settings restored from an older backup are adopted with a WARN. Through the
//! admin port (src/admin.rs): a change made on one process is used by the other's next request
//! and after a restart, the change log and the log name the actor, and each port serves only its
//! own routes. The database tests skip without podman or docker, and fail without them when
//! `CI` is set.

mod support;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use ai_gateway::config::{Protocol, RoleRules};
use support::database::{Database, set_models};
use support::postgres::TEST_PASSWORD;
use support::upstream::TestCa;
use support::{Server, Step, TENANT, Vars, fixture, http_response, json_reply, key_text};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};

const GATEWAY: &str = env!("CARGO_BIN_EXE_ai-gateway");

/// How long the binary may take to start, answer or exit on a loaded machine (CLAUDE.md, "Test
/// time limits"): reaching it means it is stuck.
const PROCESS_LIMIT: Duration = Duration::from_secs(120);

const KEY_VARIABLE: &str = "GATEWAY_TEST_ANTHROPIC_KEY";

const BODY: &str = r#"{"max_tokens":16,"messages":[{"role":"user","content":"Hi"}]}"#;

/// The ERROR a stored model list that no longer fits is logged with.
const IGNORED: &str = "ignored the stored models of a model role; it uses its configured models";

/// The WARN a settings generation that went back is logged with.
const WENT_BACK: &str = "the settings generation in the database went back, as after restoring a \
     backup; using the stored settings";

/// Puts the database back to how it was after the first change (version 1), as restoring a backup
/// taken then would: the rows are written anew, which the triggers that keep the generation and
/// the versions rising do not stop. Run by the owner role, which owns the tables.
const RESTORE_FIRST_CHANGE: &str = "
DELETE FROM ai_gateway.role_model_changes WHERE version > 1;
DELETE FROM ai_gateway.role_models;
INSERT INTO ai_gateway.role_models (role, models, version, updated_by)
VALUES ('main', ARRAY['second/model-two'], 1, 'test-admin');
DELETE FROM ai_gateway.settings_generation;
INSERT INTO ai_gateway.settings_generation (singleton, generation) VALUES (true, 1);
";

/// The test CA's certificate in a file of this test's own, for `SSL_CERT_FILE`.
fn ca_file(ca: &TestCa, name: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("ai-gateway-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("create the test directory");
    let path = directory.join("ca.pem");
    std::fs::write(&path, &ca.pem).expect("write the CA certificate");
    path
}

/// The binary with only these settings in its environment, logging everything. `vars` come
/// after the defaults, so they replace them.
fn spawn(vars: &[(String, String)]) -> Child {
    let mut command = Command::new(GATEWAY);
    command
        .env_clear()
        .env("AI_GATEWAY__HTTP__LISTEN_ADDR", "127.0.0.1:0")
        .env("AI_GATEWAY__HTTP__HEALTH_LISTEN_ADDR", "127.0.0.1:0")
        .env("AI_GATEWAY__ADMIN__LISTEN_ADDR", "127.0.0.1:0")
        .env("AI_GATEWAY__SHUTDOWN__DRAIN_DELAY", "0s")
        .env("AI_GATEWAY__TELEMETRY__LOG_FILTER", "trace")
        .env(
            "AI_GATEWAY__PROVIDERS__ANTHROPIC__PROTOCOL",
            "anthropic-messages",
        )
        .env("AI_GATEWAY__PROVIDERS__ANTHROPIC__KEY_ENV", KEY_VARIABLE)
        .env(
            "AI_GATEWAY__ROLES__MAIN__MODELS",
            "anthropic/claude-opus-5-5",
        );
    for (name, value) in vars {
        command.env(name, value);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start the ai-gateway binary")
}

fn owned(vars: &[(&str, String)]) -> Vec<(String, String)> {
    vars.iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect()
}

/// Reads JSON log lines until one has `message`; every line read goes into `seen`.
async fn wait_for(
    stdout: &mut BufReader<ChildStdout>,
    seen: &mut Vec<String>,
    message: &str,
) -> serde_json::Value {
    let found = tokio::time::timeout(PROCESS_LIMIT, async {
        let mut line = String::new();
        loop {
            line.clear();
            if stdout.read_line(&mut line).await.expect("read stdout") == 0 {
                return None;
            }
            seen.push(line.trim_end().to_owned());
            let value: serde_json::Value = serde_json::from_str(&line).expect("a JSON log line");
            if value["message"] == message {
                return Some(value);
            }
        }
    })
    .await;
    match found {
        Ok(Some(value)) => value,
        Ok(None) => panic!(
            "the binary exited before logging `{message}`:\n{}",
            seen.join("\n")
        ),
        Err(_) => panic!(
            "the binary did not log `{message}` within {PROCESS_LIMIT:?}:\n{}",
            seen.join("\n")
        ),
    }
}

/// Reads `source` to its end in the background, so a binary that logs at trace level never
/// blocks on a full pipe.
fn drain<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut source: R,
) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        let mut text = String::new();
        source
            .read_to_string(&mut text)
            .await
            .expect("read the binary's output");
        text
    })
}

/// Stops the binary and returns the rest of its output.
async fn finish(mut child: Child, outputs: Vec<tokio::task::JoinHandle<String>>) -> String {
    child.start_kill().expect("stop the binary");
    tokio::time::timeout(PROCESS_LIMIT, async {
        child.wait().await.expect("wait for the binary");
        let mut text = String::new();
        for output in outputs {
            text.push_str(&output.await.expect("the reader task"));
            text.push('\n');
        }
        text
    })
    .await
    .unwrap_or_else(|_| panic!("the binary did not exit within {PROCESS_LIMIT:?}"))
}

/// A running binary, listening.
struct Running {
    child: Child,
    address: String,
    /// The admin API's port.
    admin_address: String,
    seen: Vec<String>,
    outputs: Vec<tokio::task::JoinHandle<String>>,
}

impl Running {
    async fn start(vars: &[(String, String)]) -> Self {
        let mut child = spawn(vars);
        let stderr = drain(child.stderr.take().expect("piped stderr"));
        let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut seen = Vec::new();
        // The admin port is bound while the service starts, before the API port.
        let admin = wait_for(&mut stdout, &mut seen, "serving the admin API").await;
        let admin_address = admin["fields"]["address"]
            .as_str()
            .expect("the admin address")
            .to_owned();
        let listening = wait_for(&mut stdout, &mut seen, "listening").await;
        let address = listening["fields"]["address"]
            .as_str()
            .expect("the API address")
            .to_owned();
        Self {
            child,
            address,
            admin_address,
            seen,
            outputs: vec![drain(stdout), stderr],
        }
    }

    /// A request to `path` on `address` with `headers` and a JSON `body`: the status and the
    /// body.
    async fn request(
        address: &str,
        method: reqwest::Method,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<serde_json::Value>,
    ) -> (u16, String) {
        let mut request = reqwest::Client::new().request(method, format!("http://{address}{path}"));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        let response = tokio::time::timeout(PROCESS_LIMIT, request.send())
            .await
            .expect("the gateway answers in time")
            .expect("send the request");
        let status = response.status().as_u16();
        let text = tokio::time::timeout(PROCESS_LIMIT, response.text())
            .await
            .expect("the body arrives in time")
            .expect("read the body");
        (status, text)
    }

    /// A call to role `main`: the status and the model that answered.
    async fn call(&self) -> (u16, Option<String>) {
        let response = tokio::time::timeout(
            PROCESS_LIMIT,
            reqwest::Client::new()
                .post(format!(
                    "http://{}/internal/roles/main/v1/messages",
                    self.address
                ))
                .header("content-type", "application/json")
                .header("ai-gateway-tenant", TENANT)
                .body(BODY)
                .send(),
        )
        .await
        .expect("the gateway answers in time")
        .expect("send the request");
        let model = response
            .headers()
            .get("ai-gateway-model")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        (response.status().as_u16(), model)
    }

    /// Stops the binary; its whole log, one JSON line each.
    async fn stop(self) -> Vec<serde_json::Value> {
        let rest = finish(self.child, self.outputs).await;
        self.seen
            .iter()
            .map(String::as_str)
            .chain(rest.lines())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

fn message_reply() -> Vec<Step> {
    vec![Step::Send(json_reply(
        &fixture("anthropic/message_end_turn.json"),
        "request-id",
        "req_process",
    ))]
}

fn model_of(server: &Server, index: usize) -> String {
    server.request(index).json()["model"]
        .as_str()
        .expect("a model in the request body")
        .to_owned()
}

/// A gateway on `database` with providers `first` and, when `second` is given, `second`, both
/// Anthropic Messages at their scripted servers; role `main` is configured with
/// `first/model-one`.
fn settings(database: &Database, first: &Server, second: Option<&Server>) -> Vec<(String, String)> {
    let mut vars = Vars::new()
        .database(database)
        .set("SSL_CERT_FILE", database.ca_file.display().to_string())
        .set(KEY_VARIABLE, key_text())
        .set("AI_GATEWAY__HTTP__REQUEST_TIMEOUT", "10m")
        .set("AI_GATEWAY__TELEMETRY__LOG_FILTER", "info")
        .set(
            "AI_GATEWAY__PROVIDERS__FIRST__PROTOCOL",
            "anthropic-messages",
        )
        .set("AI_GATEWAY__PROVIDERS__FIRST__KEY_ENV", KEY_VARIABLE)
        .set(
            "AI_GATEWAY__PROVIDERS__FIRST__BASE_URL",
            format!("http://{}", first.addr),
        )
        .set("AI_GATEWAY__ROLES__MAIN__MODELS", "first/model-one");
    if let Some(second) = second {
        vars = vars
            .set(
                "AI_GATEWAY__PROVIDERS__SECOND__PROTOCOL",
                "anthropic-messages",
            )
            .set("AI_GATEWAY__PROVIDERS__SECOND__KEY_ENV", KEY_VARIABLE)
            .set(
                "AI_GATEWAY__PROVIDERS__SECOND__BASE_URL",
                format!("http://{}", second.addr),
            );
    }
    vars.pairs().to_vec()
}

/// What the store checks a change against for [`settings`] with both providers.
fn rules() -> RoleRules {
    RoleRules {
        providers: [
            ("anthropic".to_owned(), Protocol::AnthropicMessages),
            ("first".to_owned(), Protocol::AnthropicMessages),
            ("second".to_owned(), Protocol::AnthropicMessages),
        ]
        .into(),
        roles: [("main".to_owned(), Protocol::AnthropicMessages)].into(),
    }
}

#[tokio::test]
async fn the_key_is_used_but_never_logged_or_returned() {
    let Some(database) = Database::new(
        "CREATE DATABASE ai_gateway_key OWNER ai_gateway_owner",
        "ai_gateway_key",
    )
    .await
    else {
        return;
    };
    let key = key_text();
    let ca = TestCa::generate();
    let echo = |status: u16| {
        let body = serde_json::json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": format!("the key {key} is wrong")},
        })
        .to_string();
        http_response(
            status,
            &[("content-type", "application/json")],
            body.as_bytes(),
        )
    };
    let message = fixture("anthropic/message_end_turn.json");
    let server = Server::start_tls(
        vec![
            vec![Step::Send(json_reply(
                &message,
                "request-id",
                "req_process_1",
            ))],
            vec![Step::Send(echo(400))],
            vec![Step::Send(echo(401))],
        ],
        ca.acceptor(&["127.0.0.1"]),
    )
    .await;
    let mut vars = owned(&database.vars());
    vars.extend(owned(&[
        ("SSL_CERT_FILE", ca_file(&ca, "key").display().to_string()),
        (KEY_VARIABLE, key.clone()),
        (
            "AI_GATEWAY__PROVIDERS__ANTHROPIC__BASE_URL",
            format!("https://127.0.0.1:{}", server.addr.port()),
        ),
        ("AI_GATEWAY__HTTP__REQUEST_TIMEOUT", "10m".to_owned()),
    ]));
    let mut child = spawn(&vars);
    let stderr = drain(child.stderr.take().expect("piped stderr"));
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut seen = Vec::new();
    let ready = wait_for(&mut stdout, &mut seen, "model role ready").await;
    assert_eq!(
        ready["fields"]["models"], "anthropic/claude-opus-5-5",
        "{ready}"
    );
    let listening = wait_for(&mut stdout, &mut seen, "listening").await;
    let address = listening["fields"]["address"]
        .as_str()
        .expect("the API address")
        .to_owned();
    let stdout = drain(stdout);

    let client = reqwest::Client::new();
    let url = format!("http://{address}/internal/roles/main/v1/messages");
    let mut bodies = Vec::new();
    for expected in [200, 400, 502] {
        let response = tokio::time::timeout(
            PROCESS_LIMIT,
            client
                .post(&url)
                .header("content-type", "application/json")
                .header("ai-gateway-tenant", TENANT)
                .body(BODY)
                .send(),
        )
        .await
        .expect("the gateway answers in time")
        .expect("send the request");
        let status = response.status().as_u16();
        let headers = format!("{:?}", response.headers());
        let body = response.text().await.expect("the body");
        assert_eq!(status, expected, "{body}");
        bodies.push(format!("{headers}\n{body}"));
    }
    assert_eq!(
        bodies[0].split_once('\n').map(|(_, body)| body),
        Some(message.as_str())
    );
    assert!(
        bodies[1].contains("the key [REDACTED] is wrong"),
        "{}",
        bodies[1]
    );

    // The key went to the provider, in every request.
    for index in 0..3 {
        assert_eq!(server.request(index).header("x-api-key"), Some(key.clone()));
    }

    let rest = finish(child, vec![stdout, stderr]).await;
    let log = format!("{}\n{rest}", seen.join("\n"));
    assert!(
        log.contains("\"level\":\"TRACE\"") || log.contains("\"level\":\"DEBUG\""),
        "the binary logged at trace level:\n{log}"
    );
    assert!(log.contains("model call completed"), "{log}");
    assert!(log.contains("model call failed"), "{log}");
    assert!(!log.contains(&key), "the key is in the logs:\n{log}");
    assert!(
        !log.contains(TEST_PASSWORD),
        "the database password is in the logs:\n{log}"
    );
    for body in &bodies {
        assert!(!body.contains(&key), "the key is in a response: {body}");
    }
}

#[tokio::test]
async fn the_binary_does_not_start_without_its_key_or_with_limits_that_cannot_be_kept() {
    let ca = TestCa::generate();
    let certificates = ca_file(&ca, "startup").display().to_string();
    let key = key_text();
    // Database settings that point nowhere: the pool connects lazily, so they are not used.
    let database = Vars::new().pairs().to_vec();
    for (vars, expected) in [
        (
            owned(&[
                ("SSL_CERT_FILE", certificates.clone()),
                ("AI_GATEWAY__HTTP__REQUEST_TIMEOUT", "10m".to_owned()),
            ]),
            KEY_VARIABLE,
        ),
        (
            // service-kit's default request timeout (30 s) is shorter than a role's default
            // response limit (300 s).
            owned(&[
                ("SSL_CERT_FILE", certificates.clone()),
                (KEY_VARIABLE, key.clone()),
            ]),
            "AI_GATEWAY__HTTP__REQUEST_TIMEOUT",
        ),
        (
            // The database settings are required.
            owned(&[
                ("SSL_CERT_FILE", certificates.clone()),
                (KEY_VARIABLE, key.clone()),
                ("AI_GATEWAY__HTTP__REQUEST_TIMEOUT", "10m".to_owned()),
            ]),
            "AI_GATEWAY_POSTGRES_HOST",
        ),
    ] {
        let with_database = if expected == "AI_GATEWAY_POSTGRES_HOST" {
            database
                .iter()
                .filter(|(name, _)| name != "AI_GATEWAY_POSTGRES_HOST")
                .cloned()
                .chain(vars)
                .collect()
        } else {
            database.iter().cloned().chain(vars).collect::<Vec<_>>()
        };
        let mut child = spawn(&with_database);
        let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut seen = Vec::new();
        // A missing setting stops the binary before the gateway is built.
        let message = if expected == "AI_GATEWAY_POSTGRES_HOST" {
            "invalid configuration; not starting"
        } else {
            "startup failed"
        };
        let failed = wait_for(&mut stdout, &mut seen, message).await;
        assert_eq!(failed["level"], "ERROR", "{failed}");
        let error = failed["fields"]["error"].as_str().expect("the error");
        assert!(error.contains(expected), "{error}");
        assert!(!error.contains(&key), "{error}");
        let status = tokio::time::timeout(PROCESS_LIMIT, child.wait())
            .await
            .expect("the binary exits")
            .expect("wait for the binary");
        assert_eq!(status.code(), Some(1));
    }
}

#[tokio::test]
async fn a_change_reaches_every_process_for_its_next_request() {
    let Some(database) = Database::new(
        "CREATE DATABASE ai_gateway_processes OWNER ai_gateway_owner",
        "ai_gateway_processes",
    )
    .await
    else {
        return;
    };
    let first = Server::start(vec![message_reply(), message_reply()]).await;
    let second = Server::start(vec![message_reply(), message_reply()]).await;
    let vars = settings(&database, &first, Some(&second));
    let a = Running::start(&vars).await;
    let b = Running::start(&vars).await;

    // Both have loaded the settings and keep them in memory.
    assert_eq!(a.call().await, (200, Some("first/model-one".to_owned())));
    assert_eq!(b.call().await, (200, Some("first/model-one".to_owned())));
    assert_eq!(model_of(&first, 0), "model-one");

    // The change returns before the next request of either process starts.
    set_models(
        &database.store(),
        &rules(),
        "main",
        &["second/model-two"],
        0,
    )
    .await;
    assert_eq!(b.call().await, (200, Some("second/model-two".to_owned())));
    assert_eq!(a.call().await, (200, Some("second/model-two".to_owned())));
    assert_eq!(model_of(&second, 0), "model-two");
    assert_eq!(model_of(&second, 1), "model-two");
    assert_eq!(first.request_count(), 2);

    for log in [a.stop().await, b.stop().await] {
        assert!(
            log.iter()
                .any(|line| line["message"] == "model role uses stored models"
                    && line["fields"]["role"] == "main"),
            "{log:?}"
        );
    }
}

#[tokio::test]
async fn a_restarted_process_uses_the_last_change() {
    let Some(database) = Database::new(
        "CREATE DATABASE ai_gateway_restart OWNER ai_gateway_owner",
        "ai_gateway_restart",
    )
    .await
    else {
        return;
    };
    let first = Server::start(Vec::new()).await;
    let second = Server::start(vec![message_reply(), message_reply()]).await;
    let vars = settings(&database, &first, Some(&second));
    let store = database.store();
    let rules = rules();
    let earlier = set_models(&store, &rules, "main", &["first/model-three"], 0).await;
    set_models(&store, &rules, "main", &["second/model-two"], earlier).await;

    let running = Running::start(&vars).await;
    assert_eq!(
        running.call().await,
        (200, Some("second/model-two".to_owned()))
    );
    running.stop().await;
    let running = Running::start(&vars).await;
    assert_eq!(
        running.call().await,
        (200, Some("second/model-two".to_owned()))
    );
    running.stop().await;
    assert_eq!(model_of(&second, 1), "model-two");
    assert_eq!(first.request_count(), 0);
}

#[tokio::test]
async fn a_stored_list_the_configuration_no_longer_allows_is_ignored_with_an_error() {
    let Some(database) = Database::new(
        "CREATE DATABASE ai_gateway_ignored OWNER ai_gateway_owner",
        "ai_gateway_ignored",
    )
    .await
    else {
        return;
    };
    let first = Server::start(vec![message_reply()]).await;
    let second = Server::start(Vec::new()).await;
    let version = set_models(
        &database.store(),
        &rules(),
        "main",
        &["second/model-two"],
        0,
    )
    .await;

    // The chart removed provider `second`.
    let running = Running::start(&settings(&database, &first, None)).await;
    assert_eq!(
        running.call().await,
        (200, Some("first/model-one".to_owned()))
    );
    assert_eq!(model_of(&first, 0), "model-one");
    assert_eq!(second.request_count(), 0);
    let log = running.stop().await;
    let ignored = log
        .iter()
        .find(|line| line["message"] == IGNORED)
        .unwrap_or_else(|| panic!("no `{IGNORED}` in {log:?}"));
    assert_eq!(ignored["level"], "ERROR", "{ignored}");
    assert_eq!(ignored["fields"]["role"], "main", "{ignored}");
    assert_eq!(ignored["fields"]["version"], version, "{ignored}");
    let reason = ignored["fields"]["reason"].as_str().expect("a reason");
    assert!(reason.contains("not configured"), "{reason}");
}

#[tokio::test]
async fn settings_restored_from_an_older_backup_are_adopted_with_a_warning() {
    let Some(database) = Database::new(
        "CREATE DATABASE ai_gateway_restored OWNER ai_gateway_owner",
        "ai_gateway_restored",
    )
    .await
    else {
        return;
    };
    let first = Server::start(vec![message_reply()]).await;
    let second = Server::start(vec![message_reply(), message_reply()]).await;
    let store = database.store();
    let rules = rules();
    assert_eq!(
        set_models(&store, &rules, "main", &["second/model-two"], 0).await,
        1
    );
    let running = Running::start(&settings(&database, &first, Some(&second))).await;
    assert_eq!(
        running.call().await,
        (200, Some("second/model-two".to_owned()))
    );
    assert_eq!(
        set_models(&store, &rules, "main", &["first/model-three"], 1).await,
        2
    );
    assert_eq!(
        running.call().await,
        (200, Some("first/model-three".to_owned()))
    );

    // The backup from before the second change comes back: generation 1 again.
    let mut owner = database.connect("ai_gateway_owner").await;
    sqlx::raw_sql(RESTORE_FIRST_CHANGE)
        .execute(&mut owner)
        .await
        .expect("restore the earlier settings");
    assert_eq!(
        running.call().await,
        (200, Some("second/model-two".to_owned()))
    );
    assert_eq!(model_of(&second, 1), "model-two");
    assert_eq!(model_of(&first, 0), "model-three");

    let log = running.stop().await;
    let warned = log
        .iter()
        .find(|line| line["message"] == WENT_BACK)
        .unwrap_or_else(|| panic!("no `{WENT_BACK}` in {log:?}"));
    assert_eq!(warned["level"], "WARN", "{warned}");
    assert_eq!(warned["fields"]["loaded"], 2, "{warned}");
    assert_eq!(warned["fields"]["stored"], 1, "{warned}");
}

/// The actor of the admin requests below.
const ADMIN_ACTOR: &str = "user:0f3a-admin";

/// The INFO a written change is logged with.
const CHANGED: &str = "model role changed";

#[tokio::test]
async fn a_change_through_the_admin_port_reaches_every_process_and_outlives_a_restart() {
    let Some(database) = Database::new(
        "CREATE DATABASE ai_gateway_admin_processes OWNER ai_gateway_owner",
        "ai_gateway_admin_processes",
    )
    .await
    else {
        return;
    };
    let first = Server::start(vec![message_reply(), message_reply()]).await;
    let second = Server::start(vec![message_reply(), message_reply()]).await;
    let vars = settings(&database, &first, Some(&second));
    let a = Running::start(&vars).await;
    let b = Running::start(&vars).await;
    assert_eq!(a.call().await, (200, Some("first/model-one".to_owned())));
    assert_eq!(b.call().await, (200, Some("first/model-one".to_owned())));

    // Process A's admin port changes the role; process B's next request uses the change.
    let (status, body) = Running::request(
        &a.admin_address,
        reqwest::Method::PUT,
        "/internal/admin/v1/roles/main",
        &[("ai-gateway-actor", ADMIN_ACTOR)],
        Some(serde_json::json!({"models": ["second/model-two"], "expected_version": 0})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let changed: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
    assert_eq!(changed["version"], 1, "{changed}");
    assert_eq!(changed["source"], "database", "{changed}");
    assert_eq!(b.call().await, (200, Some("second/model-two".to_owned())));
    assert_eq!(model_of(&second, 0), "model-two");

    // The change log and process A's log name the actor.
    let mut app = database.connect("ai_gateway_app").await;
    let records: Vec<(i64, String, String)> =
        sqlx::query_as("SELECT version, action, changed_by FROM ai_gateway.role_model_changes")
            .fetch_all(&mut app)
            .await
            .expect("read the change log");
    assert_eq!(records, [(1, "set".to_owned(), ADMIN_ACTOR.to_owned())]);
    let log = a.stop().await;
    let line = log
        .iter()
        .find(|line| line["message"] == CHANGED)
        .unwrap_or_else(|| panic!("no `{CHANGED}` in {log:?}"));
    assert_eq!(line["level"], "INFO", "{line}");
    assert_eq!(line["fields"]["role"], "main", "{line}");
    assert_eq!(line["fields"]["actor"], ADMIN_ACTOR, "{line}");
    assert_eq!(line["fields"]["action"], "set", "{line}");
    assert_eq!(line["fields"]["version"], 1, "{line}");
    assert_eq!(line["fields"]["models"], "second/model-two", "{line}");
    b.stop().await;

    // A restarted process uses the change.
    let restarted = Running::start(&vars).await;
    assert_eq!(
        restarted.call().await,
        (200, Some("second/model-two".to_owned()))
    );
    restarted.stop().await;
    assert_eq!(first.request_count(), 2);
    assert_eq!(second.request_count(), 2);
}

#[tokio::test]
async fn each_port_serves_only_its_own_routes() {
    let Some(database) = Database::new(
        "CREATE DATABASE ai_gateway_admin_ports OWNER ai_gateway_owner",
        "ai_gateway_admin_ports",
    )
    .await
    else {
        return;
    };
    let first = Server::start(Vec::new()).await;
    let running = Running::start(&settings(&database, &first, None)).await;
    assert_ne!(running.address, running.admin_address);
    let actor = [("ai-gateway-actor", ADMIN_ACTOR)];
    let change = || Some(serde_json::json!({"models": ["first/model-two"], "expected_version": 0}));

    // The admin port serves the admin routes.
    let (status, body) = Running::request(
        &running.admin_address,
        reqwest::Method::GET,
        "/internal/admin/v1/roles",
        &actor,
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // The admin routes are not on the API port.
    for (method, path, body) in [
        (reqwest::Method::GET, "/internal/admin/v1/roles", None),
        (
            reqwest::Method::PUT,
            "/internal/admin/v1/roles/main",
            change(),
        ),
        (
            reqwest::Method::DELETE,
            "/internal/admin/v1/roles/main?expected_version=0",
            None,
        ),
    ] {
        let (status, text) =
            Running::request(&running.address, method.clone(), path, &actor, body).await;
        assert_eq!(status, 404, "{method} {path} on the API port: {text}");
    }

    // The forwarding routes and the health routes are not on the admin port.
    for (path, body) in [
        (
            "/internal/roles/main/v1/messages",
            serde_json::json!({"max_tokens": 16, "messages": []}),
        ),
        ("/internal/roles/main/v1/decisions", serde_json::json!({})),
    ] {
        let (status, text) = Running::request(
            &running.admin_address,
            reqwest::Method::POST,
            path,
            &[("ai-gateway-tenant", TENANT)],
            Some(body),
        )
        .await;
        assert_eq!(status, 404, "POST {path} on the admin port: {text}");
        let error: serde_json::Value = serde_json::from_str(&text).expect("a JSON error");
        assert_eq!(error["error"]["code"], "not_found", "{error}");
    }
    for path in ["/healthz", "/readyz"] {
        let (status, text) = Running::request(
            &running.admin_address,
            reqwest::Method::GET,
            path,
            &[],
            None,
        )
        .await;
        assert_eq!(status, 404, "GET {path} on the admin port: {text}");
    }
    running.stop().await;
    assert_eq!(first.request_count(), 0);

    // Nothing was changed through the API port.
    let mut app = database.connect("ai_gateway_app").await;
    let changes: i64 = sqlx::query_scalar("SELECT count(*) FROM ai_gateway.role_model_changes")
        .fetch_one(&mut app)
        .await
        .expect("count the changes");
    assert_eq!(changes, 0);
}
