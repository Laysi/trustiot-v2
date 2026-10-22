//! The real binary under service-kit: it serves the fixture site with the health endpoints in
//! front, and it does not start without a usable site.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use web::headers::CSP;

const WEB: &str = env!("CARGO_BIN_EXE_web");

/// How long the binary may take to start or to exit on a loaded machine (CLAUDE.md, "Test time
/// limits"): it reads a few files and binds two ports, so reaching the bound means it is stuck.
const PROCESS_LIMIT: Duration = Duration::from_secs(120);

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/site")
}

/// Starts the binary on free local ports with only the given settings in its environment.
fn spawn(root: &Path) -> Child {
    Command::new(WEB)
        .env_clear()
        .env("TRUSTIOT__ROOT_DIR", root)
        .env("TRUSTIOT__HTTP__LISTEN_ADDR", "127.0.0.1:0")
        .env("TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR", "127.0.0.1:0")
        .env("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "0s")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("start the web binary")
}

/// Reads JSON log lines until one has `message`; returns it and every line read so far.
async fn wait_for(stdout: &mut BufReader<ChildStdout>, message: &str) -> serde_json::Value {
    let mut seen = Vec::new();
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

#[tokio::test]
async fn the_binary_serves_deep_links_and_health() {
    let mut child = spawn(&fixture());
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let loaded = wait_for(&mut stdout, "loaded the site").await;
    assert_eq!(loaded["fields"]["files"], 5, "{loaded}");
    let listening = wait_for(&mut stdout, "listening").await;
    let address = listening["fields"]["address"]
        .as_str()
        .expect("the API address")
        .to_owned();

    let client = reqwest::Client::new();
    let index = std::fs::read_to_string(fixture().join("index.html")).expect("read index.html");
    let deep = client
        .get(format!("http://{address}/devices/42?tab=data"))
        .send()
        .await
        .expect("GET a deep link");
    assert_eq!(deep.status(), 200);
    assert_eq!(deep.headers()["content-security-policy"], CSP);
    assert_eq!(deep.headers()["content-type"], "text/html; charset=utf-8");
    assert_eq!(deep.text().await.expect("the body"), index);

    // HEAD through hyper keeps GET's length.
    let head = client
        .head(format!("http://{address}/static/B7f3kQ2x.js"))
        .send()
        .await
        .expect("HEAD an asset");
    assert_eq!(head.status(), 200);
    let length = std::fs::metadata(fixture().join("static/B7f3kQ2x.js"))
        .expect("the asset")
        .len()
        .to_string();
    assert_eq!(head.headers()["content-length"], length.as_str());

    // service-kit's health endpoints are on the API port too, in front of the site.
    let health = client
        .get(format!("http://{address}/healthz"))
        .send()
        .await
        .expect("GET /healthz");
    assert_eq!(health.status(), 200);
    assert!(health.headers().get("content-security-policy").is_none());

    let post = client
        .post(format!("http://{address}/"))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("POST /");
    assert_eq!(post.status(), 405);
    assert_eq!(post.headers()["allow"], "GET, HEAD");
}

#[tokio::test]
async fn the_binary_does_not_start_without_index_html() {
    let empty = std::env::temp_dir().join(format!("trustiot-web-empty-{}", std::process::id()));
    std::fs::create_dir_all(&empty).expect("create an empty site");
    let mut child = spawn(&empty);
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let failed = wait_for(&mut stdout, "startup failed").await;
    assert_eq!(failed["level"], "ERROR", "{failed}");
    let error = failed["fields"]["error"].as_str().expect("the error");
    assert!(error.contains("no index.html"), "{error}");
    let status = tokio::time::timeout(PROCESS_LIMIT, child.wait())
        .await
        .expect("the binary exits")
        .expect("wait for the binary");
    assert_eq!(status.code(), Some(1));
    std::fs::remove_dir_all(&empty).expect("remove the empty site");
}
