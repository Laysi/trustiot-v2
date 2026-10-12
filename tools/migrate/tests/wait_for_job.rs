//! `migrate wait-for-job` against a fake Kubernetes API over HTTPS with a test CA: it waits
//! through a missing and a running Job, returns when the Job is complete, fails on a failed Job
//! or a refused token, and trusts no CA but the configured one. No containers needed.

mod support;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use migrate::config::KubeSettings;
use migrate::kube::{self as kube, WaitError, look_once, wait_for_job};
use support::postgres::TestCa;
use support::{random_hex, scratch_dir};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

const JOB_PATH: &str = "/apis/batch/v1/namespaces/release-ns/jobs/trustiot-migrate-r2";
const RUNNING: &str = r#"{"kind":"Job","status":{"active":1}}"#;
const COMPLETE: &str =
    r#"{"kind":"Job","status":{"conditions":[{"type":"Complete","status":"True"}],"succeeded":1}}"#;
const FAILED: &str =
    r#"{"kind":"Job","status":{"conditions":[{"type":"Failed","status":"True"}]}}"#;

/// Longer than several polls (every 2 s) on a loaded machine; the test fails only if a wait that
/// should end did not.
const ENDS_WITHIN: Duration = Duration::from_secs(60);
/// How long a wait that must not end is watched.
const KEEPS_WAITING_FOR: Duration = Duration::from_secs(6);

/// One request the fake API saw: the request line and the Authorization header.
type Seen = Arc<Mutex<Vec<(String, String)>>>;

/// Serves `answers` in order (the last one repeats) over HTTPS with a certificate for 127.0.0.1
/// signed by `ca`. Returns its URL and what it saw.
async fn fake_api(ca: &TestCa, answers: Vec<(u16, &'static str)>) -> (String, Seen) {
    let (certificate, key) = ca.server_certificate(&["127.0.0.1"]);
    let certificates =
        vec![CertificateDer::from_pem_slice(certificate.as_bytes()).expect("certificate")];
    let key = PrivateKeyDer::from_pem_slice(key.as_bytes()).expect("key");
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("protocol versions")
    .with_no_client_auth()
    .with_single_cert(certificates, key)
    .expect("server config");
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!(
        "https://127.0.0.1:{}",
        listener.local_addr().expect("address").port()
    );
    let seen: Seen = Arc::default();
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        let mut index = 0;
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut tls) = acceptor.accept(stream).await else {
                continue;
            };
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match tls.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&buffer[..read]),
                }
            }
            let text = String::from_utf8_lossy(&request).into_owned();
            let line = text.lines().next().unwrap_or_default().to_owned();
            let authorization = text
                .lines()
                .find_map(|header| {
                    header
                        .split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                        .map(|(_, value)| value.trim().to_owned())
                })
                .unwrap_or_default();
            log.lock().expect("log").push((line, authorization));
            let (status, body) = answers[index.min(answers.len() - 1)];
            index += 1;
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = tls.write_all(response.as_bytes()).await;
            let _ = tls.shutdown().await;
        }
    });
    (url, seen)
}

/// Settings that trust `ca` and read the token from a file.
fn settings(api_url: String, ca: &TestCa, token: &str) -> KubeSettings {
    let dir: PathBuf = scratch_dir("kube");
    let ca_file = dir.join("ca.crt");
    std::fs::write(&ca_file, &ca.pem).expect("write the CA");
    let token_file = dir.join("token");
    std::fs::write(&token_file, format!("{token}\n")).expect("write the token");
    KubeSettings {
        job: "trustiot-migrate-r2".to_owned(),
        namespace: "release-ns".to_owned(),
        api_url,
        token_file,
        ca_file,
    }
}

#[tokio::test]
async fn waits_through_missing_and_running_until_complete() {
    let ca = TestCa::new("fake Kubernetes API");
    let (url, seen) = fake_api(&ca, vec![(404, "{}"), (200, RUNNING), (200, COMPLETE)]).await;
    let token = random_hex();
    tokio::time::timeout(ENDS_WITHIN, wait_for_job(&settings(url, &ca, &token)))
        .await
        .expect("the wait ends once the Job is complete")
        .expect("a complete Job");
    let seen = seen.lock().expect("log").clone();
    assert_eq!(seen.len(), 3, "{seen:?}");
    for (line, authorization) in seen {
        assert_eq!(line, format!("GET {JOB_PATH} HTTP/1.1"));
        assert_eq!(
            authorization,
            format!("Bearer {token}"),
            "the token, without the newline"
        );
    }
}

/// A pod of an older release whose Job an upgrade deleted gets 404 for good: the wait goes on,
/// and what it logs names the Job, the likely cause and the way out.
#[tokio::test]
async fn a_missing_job_names_the_cause_and_the_way_out() {
    let ca = TestCa::new("fake Kubernetes API");
    let (url, seen) = fake_api(&ca, vec![(404, r#"{"kind":"Status","code":404}"#)]).await;
    let settings = settings(url, &ca, "t");
    let found = tokio::time::timeout(ENDS_WITHIN, look_once(&settings))
        .await
        .expect("one look ends")
        .expect("a missing Job is not an error");
    let kube::Seen::Missing(message) = found else {
        panic!("a 404 must be reported as a missing Job, not {found:?}");
    };
    for part in [
        "trustiot-migrate-r2",
        "another release",
        "helm rollback",
        "migrate.revision",
        // The runbook's path, without naming it as a file this test reads
        // (scripts/ci/changes.test.ts).
        "runbooks/migrations.md",
    ] {
        assert!(message.contains(part), "{part:?} missing from: {message}");
    }
    assert_eq!(seen.lock().expect("log").len(), 1);
    let waited = tokio::time::timeout(KEEPS_WAITING_FOR, wait_for_job(&settings)).await;
    assert!(
        waited.is_err(),
        "a missing Job is waited out, never an error"
    );
}

#[tokio::test]
async fn a_failed_job_or_a_refused_token_ends_the_wait_with_an_error() {
    let ca = TestCa::new("fake Kubernetes API");
    let (url, _) = fake_api(&ca, vec![(200, RUNNING), (200, FAILED)]).await;
    let error = tokio::time::timeout(ENDS_WITHIN, wait_for_job(&settings(url, &ca, "t")))
        .await
        .expect("the wait ends")
        .expect_err("a failed Job");
    assert!(matches!(error, WaitError::JobFailed(_)), "{error}");

    let (url, _) = fake_api(&ca, vec![(403, "{}")]).await;
    let error = tokio::time::timeout(ENDS_WITHIN, wait_for_job(&settings(url, &ca, "t")))
        .await
        .expect("the wait ends")
        .expect_err("a refused token");
    assert!(
        matches!(error, WaitError::Forbidden { status: 403, .. }),
        "{error}"
    );
}

#[tokio::test]
async fn an_api_signed_by_another_ca_is_never_believed() {
    let ca = TestCa::new("fake Kubernetes API");
    let other = TestCa::new("the CA the wait trusts");
    let (url, seen) = fake_api(&ca, vec![(200, COMPLETE)]).await;
    let waited =
        tokio::time::timeout(KEEPS_WAITING_FOR, wait_for_job(&settings(url, &other, "t"))).await;
    assert!(
        waited.is_err(),
        "a Job reported by an untrusted server must not end the wait"
    );
    assert!(
        seen.lock().expect("log").is_empty(),
        "no request may pass an untrusted TLS handshake"
    );
}
