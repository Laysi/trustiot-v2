//! The real binary under service-kit, against a provider stand-in over TLS that trusts only a
//! test CA (`SSL_CERT_FILE`): it reads the key from the variable its configuration names, uses
//! it, and never writes it to its logs (at trace level, every crate included) or its responses.
//! It does not start without its key, or when a role's limits do not fit in the request timeout.

mod support;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use support::upstream::TestCa;
use support::{Server, Step, TENANT, fixture, http_response, json_reply, key_text};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};

const GATEWAY: &str = env!("CARGO_BIN_EXE_ai-gateway");

/// How long the binary may take to start, answer or exit on a loaded machine (CLAUDE.md, "Test
/// time limits"): reaching it means it is stuck.
const PROCESS_LIMIT: Duration = Duration::from_secs(120);

const KEY_VARIABLE: &str = "GATEWAY_TEST_ANTHROPIC_KEY";

/// The test CA's certificate in a file of this test's own, for `SSL_CERT_FILE`.
fn ca_file(ca: &TestCa, name: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("ai-gateway-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("create the test directory");
    let path = directory.join("ca.pem");
    std::fs::write(&path, &ca.pem).expect("write the CA certificate");
    path
}

/// The binary with only these settings in its environment, logging everything.
fn spawn(vars: &[(&str, String)]) -> Child {
    let mut command = Command::new(GATEWAY);
    command
        .env_clear()
        .env("AI_GATEWAY__HTTP__LISTEN_ADDR", "127.0.0.1:0")
        .env("AI_GATEWAY__HTTP__HEALTH_LISTEN_ADDR", "127.0.0.1:0")
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

#[tokio::test]
async fn the_key_is_used_but_never_logged_or_returned() {
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
    let mut child = spawn(&[
        ("SSL_CERT_FILE", ca_file(&ca, "key").display().to_string()),
        (KEY_VARIABLE, key.clone()),
        (
            "AI_GATEWAY__PROVIDERS__ANTHROPIC__BASE_URL",
            format!("https://127.0.0.1:{}", server.addr.port()),
        ),
        ("AI_GATEWAY__HTTP__REQUEST_TIMEOUT", "10m".to_owned()),
    ]);
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
                .body(r#"{"max_tokens":16,"messages":[{"role":"user","content":"Hi"}]}"#)
                .send(),
        )
        .await
        .expect("the gateway answers in time")
        .expect("send the request");
        assert_eq!(response.status().as_u16(), expected);
        let headers = format!("{:?}", response.headers());
        let body = response.text().await.expect("the body");
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
    for body in &bodies {
        assert!(!body.contains(&key), "the key is in a response: {body}");
    }
}

#[tokio::test]
async fn the_binary_does_not_start_without_its_key_or_with_limits_that_cannot_be_kept() {
    let ca = TestCa::generate();
    let certificates = ca_file(&ca, "startup").display().to_string();
    let key = key_text();
    for (vars, expected) in [
        (
            vec![
                ("SSL_CERT_FILE", certificates.clone()),
                ("AI_GATEWAY__HTTP__REQUEST_TIMEOUT", "10m".to_owned()),
            ],
            KEY_VARIABLE,
        ),
        (
            // service-kit's default request timeout (30 s) is shorter than a role's default
            // response limit (300 s).
            vec![
                ("SSL_CERT_FILE", certificates.clone()),
                (KEY_VARIABLE, key.clone()),
            ],
            "AI_GATEWAY__HTTP__REQUEST_TIMEOUT",
        ),
    ] {
        let mut child = spawn(&vars);
        let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut seen = Vec::new();
        let failed = wait_for(&mut stdout, &mut seen, "startup failed").await;
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
