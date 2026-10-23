//! The MQTT broker's certificate issuer (`migrate mqtt-broker-certificate`,
//! Laysi/trustiot-v2#535) against the fake Kubernetes API (support/secrets.rs) and a fake certs
//! service that signs the request with a test issuing CA the way services/certs does (its real
//! endpoint is tested with this issuer in services/certs/tests/broker_certificate.rs):
//!
//! - the first run fills the Secret the chart declares empty, binds it and records the
//!   generation; later runs keep it until half of its validity has passed, or until
//!   `mqttBroker.certificateGeneration` goes up, and then write a new certificate with a new key;
//! - a Secret someone else made, restored or spoilt gets a new certificate and key, and loses
//!   what neither the chart nor the issuer sets;
//! - a certs service that cannot sign (503), fails (500) or does not answer leaves an empty
//!   Secret alone and the run succeeds; with a certificate due for renewal the run fails, as it
//!   does on a refusal or an answer it cannot use, and the Secret keeps what the broker serves;
//! - a writer in between makes the run read the Secret again and write the certificate it
//!   already has;
//! - the command prints neither the key nor the certificate.

mod support;

use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use contract::certs::BROKER_SERVER_NAME;
use migrate::Report;
use migrate::config::MqttCertificateSettings;
use migrate::mqtt_certificate::{
    Current, GENERATION_ANNOTATION, ISSUE_PATH, Outcome, REQUEST_TYPE, converge_at, expected_names,
};
use migrate::mqtt_trust::check_pair;
use migrate::passwords::UID_ANNOTATION;
use rcgen::{
    CertificateParams, DnType, ExtendedKeyUsagePurpose, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
    PublicKeyData, SerialNumber, SignatureAlgorithm, date_time_ymd,
};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use support::pki::{self, Ca};
use support::secrets::{FakeSecrets, read_request};
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpListener;
use x509_parser::certification_request::X509CertificationRequest;
use x509_parser::prelude::FromDer as _;

const SECRET: &str = "release-mqtt-broker-tls";
const SERVICE: &str = "release-mqtt-broker.release-ns.svc";
const BINARY: &str = env!("CARGO_BIN_EXE_migrate");
/// The certs service's shortest broker certificate validity (`broker.validity`), as the
/// container test of the real service uses.
const VALIDITY: i64 = 3600;

/// What the fake certs service does with the next requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Signs the request for both names, as the certs service with `broker.service_name` set.
    Sign,
    /// Signs it for the fixed name only: a certs service whose `broker.service_name` differs.
    FixedNameOnly,
    /// Answers this status with a one-line error.
    Status(u16),
}

/// A certs service's broker certificate port, with a test root and issuing CA.
#[derive(Clone)]
struct FakeCerts {
    url: String,
    root: Arc<Ca>,
    issuing: Arc<Ca>,
    mode: Arc<Mutex<Mode>>,
    /// The content type of every request.
    requests: Arc<Mutex<Vec<String>>>,
}

/// The public key of a certificate signing request, for rcgen to sign.
struct RequestedKey(Vec<u8>);

impl PublicKeyData for RequestedKey {
    fn der_bytes(&self) -> &[u8] {
        &self.0
    }

    fn algorithm(&self) -> &'static SignatureAlgorithm {
        &PKCS_ECDSA_P256_SHA256
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unix_now() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970");
    i64::try_from(elapsed.as_secs()).expect("a plausible time")
}

/// `seconds` (after 1970) since the Unix epoch, to add to the epoch as rcgen takes a time.
fn since_epoch(seconds: i64) -> Duration {
    Duration::from_secs(u64::try_from(seconds).expect("after 1970"))
}

/// RFC 3339 in UTC for `seconds` since the Unix epoch (days to a civil date: H. Hinnant's
/// algorithm), as the certs service writes `not_before` and `not_after`.
fn rfc3339(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        time % 3600 / 60,
        time % 60
    )
}

impl FakeCerts {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let root = pki::root("Test root CA");
        let issuing = pki::intermediate("Test issuing CA 1", &root);
        let fake = Self {
            url: format!(
                "http://127.0.0.1:{}",
                listener.local_addr().expect("address").port()
            ),
            root: Arc::new(root),
            issuing: Arc::new(issuing),
            mode: Arc::new(Mutex::new(Mode::Sign)),
            requests: Arc::default(),
        };
        let serving = fake.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let fake = serving.clone();
                tokio::spawn(async move {
                    let Some((head, body)) = read_request(&mut stream).await else {
                        return;
                    };
                    let (status, body) = fake.answer(&head, &body);
                    let response = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        fake
    }

    fn set(&self, mode: Mode) {
        *self.mode.lock().expect("mode") = mode;
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests").clone()
    }

    fn answer(&self, head: &str, body: &[u8]) -> (u16, String) {
        let line = head.lines().next().unwrap_or_default();
        let content_type = head
            .lines()
            .skip(1)
            .find_map(|header| {
                header
                    .split_once(':')
                    .filter(|(name, _)| name.trim().eq_ignore_ascii_case("content-type"))
                    .map(|(_, value)| value.trim().to_owned())
            })
            .unwrap_or_default();
        self.requests
            .lock()
            .expect("requests")
            .push(content_type.clone());
        if !line.starts_with(&format!("POST {ISSUE_PATH} ")) {
            return (404, r#"{"error":"not found"}"#.to_owned());
        }
        let names = match *self.mode.lock().expect("mode") {
            Mode::Status(status) => {
                return (
                    status,
                    r#"{"error":"the fake certs service says no"}"#.to_owned(),
                );
            }
            Mode::Sign => expected_names(SERVICE),
            Mode::FixedNameOnly => vec![BROKER_SERVER_NAME.to_owned()],
        };
        if content_type != REQUEST_TYPE {
            return (415, r#"{"error":"not application/pkcs10"}"#.to_owned());
        }
        match self.sign(body, names) {
            Ok(answer) => (201, answer),
            Err(problem) => (422, json!({ "error": problem }).to_string()),
        }
    }

    /// Signs `request` as the certs service does: its public key only, the names it was
    /// configured with, serverAuth only, from five minutes ago for [`VALIDITY`].
    fn sign(&self, request: &[u8], names: Vec<String>) -> Result<String, String> {
        let Ok(([], request)) = X509CertificationRequest::from_der(request) else {
            return Err("not a certificate signing request".to_owned());
        };
        request
            .verify_signature()
            .map_err(|_| "the request's signature does not verify".to_owned())?;
        let key = RequestedKey(
            request
                .certification_request_info
                .subject_pki
                .subject_public_key
                .data
                .to_vec(),
        );
        let now = unix_now();
        let mut params = CertificateParams::new(names.clone()).expect("DNS names");
        params
            .distinguished_name
            .push(DnType::CommonName, BROKER_SERVER_NAME);
        let mut serial = [0_u8; 16];
        getrandom::fill(&mut serial).expect("randomness");
        serial[0] &= 0x3f;
        params.serial_number = Some(SerialNumber::from_slice(&serial));
        params.not_before = date_time_ymd(1970, 1, 1) + since_epoch(now - 300);
        params.not_after = date_time_ymd(1970, 1, 1) + since_epoch(now + VALIDITY);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let certificate = params
            .signed_by(&key, &self.issuing.issuer)
            .map_err(|error| format!("cannot sign: {error}"))?;
        let (_, issuing) = x509_parser::parse_x509_certificate(self.issuing.issuer.der())
            .expect("the issuing CA parses");
        Ok(json!({
            "serial": hex(&serial),
            "ca_key_id": hex(&Sha256::digest(issuing.public_key().raw)),
            "names": names,
            "not_before": rfc3339(now - 300),
            "not_after": rfc3339(now + VALIDITY),
            "certificate_chain": format!("{}{}", certificate.pem(), self.issuing.pem),
        })
        .to_string())
    }
}

/// A run of the issuer at `now`, and what it reported.
async fn run_at(settings: &MqttCertificateSettings, now: i64) -> (Result<Outcome, String>, Report) {
    let mut report = Report::default();
    let outcome = converge_at(settings, &mut report, now)
        .await
        .map_err(|error| error.to_string());
    (outcome, report)
}

/// The certificate a run wrote.
fn issued(outcome: Result<Outcome, String>) -> (Current, String) {
    match outcome {
        Ok(Outcome::Issued {
            certificate,
            reason,
        }) => (certificate, reason),
        other => panic!("expected a new certificate, got {other:?}"),
    }
}

/// `tls.crt` and `tls.key` of the Secret.
fn pair(secrets: &FakeSecrets, secret: &str) -> (String, String) {
    (
        secrets.value(secret, "tls.crt").expect("tls.crt"),
        secrets.value(secret, "tls.key").expect("tls.key"),
    )
}

fn patches(secrets: &FakeSecrets) -> usize {
    secrets
        .seen()
        .iter()
        .filter(|seen| seen.line.starts_with("PATCH "))
        .count()
}

#[tokio::test]
async fn the_declared_secret_is_filled_kept_and_renewed() {
    let secrets = FakeSecrets::start().await;
    secrets.declare(SECRET);
    let certs = FakeCerts::start().await;
    let settings = secrets.certificate_settings(SECRET, &certs.url, SERVICE, "1");
    let now = unix_now();

    // The first run: a certificate for both names, from the issuing CA, with its key.
    let (outcome, report) = run_at(&settings, now).await;
    let (first, reason) = issued(outcome);
    assert!(reason.contains("holds no certificate yet"), "{reason}");
    assert_eq!(first.names, expected_names(SERVICE));
    assert_eq!(
        first.names,
        [BROKER_SERVER_NAME.to_owned(), SERVICE.to_owned()]
    );
    assert_eq!(first.not_after - first.not_before, VALIDITY + 300);
    let stored = secrets.get(SECRET).expect("the Secret");
    assert_eq!(stored.annotations.get(UID_ANNOTATION), Some(&stored.uid));
    assert_eq!(
        stored
            .annotations
            .get(GENERATION_ANNOTATION)
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(
        stored.data.keys().collect::<Vec<_>>(),
        ["tls.crt", "tls.key"]
    );
    let (chain, key) = pair(&secrets, SECRET);
    check_pair(chain.as_bytes(), key.as_bytes()).expect("a usable pair");
    assert!(
        chain.ends_with(&certs.issuing.pem),
        "tls.crt is the certificate, then the issuing CA"
    );
    assert!(!chain.contains(&certs.root.pem), "tls.crt holds no root");
    assert!(key.starts_with("-----BEGIN PRIVATE KEY-----\n"), "PKCS #8");
    assert_eq!(certs.requests(), [REQUEST_TYPE]);
    assert_eq!(patches(&secrets), 1);
    assert!(
        report
            .changes
            .iter()
            .any(|change| change.contains(&first.serial)),
        "{:?}",
        report.changes
    );

    // Later runs before half of the validity: nothing changes.
    for later in [now + 60, first.renew_from() - 1] {
        let (outcome, report) = run_at(&settings, later).await;
        assert_eq!(outcome, Ok(Outcome::Kept(first.clone())));
        assert!(report.changes.is_empty());
    }
    assert_eq!(certs.requests().len(), 1);
    assert_eq!(patches(&secrets), 1);

    // From half of the validity on: a new certificate with a new key.
    let (outcome, _) = run_at(&settings, first.renew_from()).await;
    let (second, reason) = issued(outcome);
    assert!(reason.contains("half"), "{reason}");
    assert_ne!(second.serial, first.serial);
    let (second_chain, second_key) = pair(&secrets, SECRET);
    assert_ne!(second_key, key, "every renewal makes a new key");
    check_pair(second_chain.as_bytes(), second_key.as_bytes()).expect("a usable pair");

    // A higher generation renews at once; a lower one than recorded changes nothing.
    let raised = secrets.certificate_settings(SECRET, &certs.url, SERVICE, "2");
    let (outcome, _) = run_at(&raised, unix_now()).await;
    let (third, reason) = issued(outcome);
    assert!(reason.contains("certificateGeneration is 2"), "{reason}");
    assert_eq!(
        secrets
            .get(SECRET)
            .expect("the Secret")
            .annotations
            .get(GENERATION_ANNOTATION)
            .map(String::as_str),
        Some("2")
    );
    let (outcome, _) = run_at(&settings, unix_now()).await;
    assert_eq!(outcome, Ok(Outcome::Kept(third)));
    assert_eq!(
        secrets
            .get(SECRET)
            .expect("the Secret")
            .annotations
            .get(GENERATION_ANNOTATION)
            .map(String::as_str),
        Some("2"),
        "a lower generation never lowers the record"
    );

    // Another broker Service name (the release was renamed): the names no longer match.
    let renamed = secrets.certificate_settings(SECRET, &certs.url, "", "2");
    certs.set(Mode::FixedNameOnly);
    let (outcome, _) = run_at(&renamed, unix_now()).await;
    let (fourth, reason) = issued(outcome);
    assert!(reason.contains("names"), "{reason}");
    assert_eq!(fourth.names, [BROKER_SERVER_NAME]);
}

#[tokio::test]
async fn a_secret_someone_else_made_or_spoilt_gets_a_new_certificate_and_key() {
    let secrets = FakeSecrets::start().await;
    secrets.declare(SECRET);
    let certs = FakeCerts::start().await;
    let settings = secrets.certificate_settings(SECRET, &certs.url, SERVICE, "1");
    issued(run_at(&settings, unix_now()).await.0);

    // A key of another certificate: the broker could not load the pair.
    let other = pki::server(&pki::root("Someone else's CA"));
    secrets.write(SECRET, "tls.key", Some(other.key.as_bytes()));
    let (outcome, _) = run_at(&settings, unix_now()).await;
    let (_, reason) = issued(outcome);
    assert!(reason.contains("unusable"), "{reason}");
    let (chain, key) = pair(&secrets, SECRET);
    check_pair(chain.as_bytes(), key.as_bytes()).expect("a usable pair again");

    // The object was made again (restored from a backup, or copied) with its data and more: its
    // data is not trusted, and what neither the chart nor the issuer sets goes.
    secrets.recreate(SECRET);
    secrets.edit(SECRET, |secret| {
        secret
            .labels
            .insert("restored-by".to_owned(), "backup".to_owned());
        secret
            .annotations
            .insert("backup.example.com/at".to_owned(), "yesterday".to_owned());
        secret.data.insert("ca.crt".to_owned(), b"extra".to_vec());
    });
    let (outcome, report) = run_at(&settings, unix_now()).await;
    let (_, reason) = issued(outcome);
    assert!(reason.contains("not the object"), "{reason}");
    let stored = secrets.get(SECRET).expect("the Secret");
    assert_eq!(stored.annotations.get(UID_ANNOTATION), Some(&stored.uid));
    assert!(!stored.labels.contains_key("restored-by"));
    assert!(!stored.annotations.contains_key("backup.example.com/at"));
    assert_eq!(
        stored.data.keys().collect::<Vec<_>>(),
        ["tls.crt", "tls.key"]
    );
    assert_ne!(pair(&secrets, SECRET).1, key, "a new key for a new object");
    assert!(
        report
            .changes
            .iter()
            .any(|change| change.contains("ca.crt")),
        "{:?}",
        report.changes
    );

    // Being deleted, or gone: the run fails and says what to do.
    secrets.edit(SECRET, |secret| {
        secret.deletion_timestamp = Some("2026-10-24T00:00:00Z".to_owned());
    });
    let (outcome, _) = run_at(&settings, unix_now()).await;
    let error = outcome.expect_err("a Secret being deleted");
    assert!(error.contains("being deleted"), "{error}");
    secrets.delete(SECRET);
    let (outcome, _) = run_at(&settings, unix_now()).await;
    let error = outcome.expect_err("no Secret");
    assert!(error.contains("helm upgrade"), "{error}");
}

#[tokio::test]
async fn a_certs_service_that_cannot_sign_or_answers_wrong_changes_nothing() {
    let secrets = FakeSecrets::start().await;
    secrets.declare(SECRET);
    let certs = FakeCerts::start().await;
    let mut settings = secrets.certificate_settings(SECRET, &certs.url, SERVICE, "1");
    // One attempt at the certs service: these runs give up at once.
    settings.wait = Duration::from_millis(1);
    let declared = secrets.get(SECRET).expect("the Secret").resource_version;

    // Before the CA ceremony: no certificate yet, and the run succeeds.
    certs.set(Mode::Status(503));
    let (outcome, report) = run_at(&settings, unix_now()).await;
    match outcome {
        Ok(Outcome::Waiting { reason }) => assert!(reason.contains("503"), "{reason}"),
        other => panic!("expected to wait for the CA, got {other:?}"),
    }
    assert!(report.changes.is_empty());
    // Nor do a certs service that fails or one that does not answer yet, as while it starts after
    // an install: the run waits for the next schedule instead of failing.
    certs.set(Mode::Status(500));
    match run_at(&settings, unix_now()).await.0 {
        Ok(Outcome::Waiting { reason }) => assert!(reason.contains("HTTP 500"), "{reason}"),
        other => panic!("expected to wait for a failing certs service, got {other:?}"),
    }
    let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    let port = closed.local_addr().expect("its address").port();
    drop(closed);
    let mut unreachable =
        secrets.certificate_settings(SECRET, &format!("http://127.0.0.1:{port}"), SERVICE, "1");
    unreachable.wait = Duration::from_millis(1);
    match run_at(&unreachable, unix_now()).await.0 {
        Ok(Outcome::Waiting { reason }) => assert!(reason.contains("did not answer"), "{reason}"),
        other => panic!("expected to wait for a certs service that does not answer, got {other:?}"),
    }
    let stored = secrets.get(SECRET).expect("the Secret");
    assert_eq!(stored.resource_version, declared);
    assert!(stored.data.is_empty());
    assert_eq!(patches(&secrets), 0);

    certs.set(Mode::Sign);
    let (first, _) = issued(run_at(&settings, unix_now()).await.0);
    let filled = secrets.get(SECRET).expect("the Secret");

    // Renewal due, and the certs service cannot sign, refuses, fails, or answers a certificate
    // for other names: the run fails and the broker keeps serving what it has.
    for (mode, expected) in [
        (
            Mode::Status(503),
            format!("keeps serving serial {}", first.serial),
        ),
        (
            Mode::Status(422),
            "refused the request: HTTP 422".to_owned(),
        ),
        (Mode::Status(500), "HTTP 500".to_owned()),
        (Mode::FixedNameOnly, "answer is unusable".to_owned()),
    ] {
        certs.set(mode);
        let (outcome, _) = run_at(&settings, first.renew_from()).await;
        let error = outcome.expect_err("a renewal that cannot complete");
        assert!(error.contains(&expected), "{mode:?}: {error}");
        let stored = secrets.get(SECRET).expect("the Secret");
        assert_eq!(stored.resource_version, filled.resource_version, "{mode:?}");
        assert_eq!(stored.data, filled.data, "{mode:?}");
    }

    // Only the chart's never-filled Secret may wait for the certs service. An expired
    // certificate, or a Secret emptied after it was filled, is an outage the broker cannot ride
    // out: the run fails, so the failed Job shows it, and leaves the Secret as it is.
    certs.set(Mode::Status(503));
    let (outcome, report) = run_at(&settings, first.not_after).await;
    let error = outcome.expect_err("an expired certificate the certs service cannot replace");
    assert!(error.contains("the certificate has expired"), "{error}");
    assert!(
        error.contains("no certificate the broker can keep serving"),
        "{error}"
    );
    assert!(report.changes.is_empty());
    let stored = secrets.get(SECRET).expect("the Secret");
    assert_eq!(stored.resource_version, filled.resource_version);
    assert_eq!(stored.data, filled.data);
    secrets.edit(SECRET, |secret| secret.data.clear());
    let emptied = secrets.get(SECRET).expect("the Secret");
    let (outcome, _) = run_at(&settings, unix_now()).await;
    let error = outcome.expect_err("an emptied Secret the certs service cannot fill again");
    assert!(error.contains("lacks tls.crt or tls.key"), "{error}");
    assert_eq!(
        secrets.get(SECRET).expect("the Secret").resource_version,
        emptied.resource_version
    );
    assert_eq!(patches(&secrets), 1);
}

#[tokio::test]
async fn a_writer_in_between_makes_the_run_read_again() {
    let secrets = FakeSecrets::start().await;
    secrets.declare(SECRET);
    secrets.declare("busy");
    let certs = FakeCerts::start().await;

    // One conflict: read again and write the certificate the run already has.
    secrets.interfere(1);
    let settings = secrets.certificate_settings(SECRET, &certs.url, SERVICE, "1");
    issued(run_at(&settings, unix_now()).await.0);
    assert_eq!(certs.requests().len(), 1, "one key and certificate per run");
    assert_eq!(patches(&secrets), 2);
    let (chain, key) = pair(&secrets, SECRET);
    check_pair(chain.as_bytes(), key.as_bytes()).expect("a usable pair");

    // Something that keeps writing: the run gives up and says so.
    secrets.interfere(usize::MAX);
    let busy = secrets.certificate_settings("busy", &certs.url, SERVICE, "1");
    let (outcome, _) = run_at(&busy, unix_now()).await;
    let error = outcome.expect_err("a Secret that keeps changing");
    assert!(error.contains("something else writes it"), "{error}");
    assert!(secrets.get("busy").expect("the Secret").data.is_empty());
}

/// Runs the command with `settings` as the chart's environment variables; returns its exit code
/// and everything it printed.
async fn command(settings: &MqttCertificateSettings) -> (Option<i32>, String) {
    let mut command = Command::new(BINARY);
    command
        .env_clear()
        .arg("mqtt-broker-certificate")
        .env("TRUSTIOT__CERTIFICATE__API_URL", &settings.api_url)
        .env("TRUSTIOT__CERTIFICATE__NAMESPACE", &settings.namespace)
        .env("TRUSTIOT__CERTIFICATE__TOKEN_FILE", &settings.token_file)
        .env("TRUSTIOT__CERTIFICATE__CA_FILE", &settings.ca_file)
        .env("TRUSTIOT__CERTIFICATE__SECRET", &settings.secret)
        .env("TRUSTIOT__CERTIFICATE__CERTS_URL", &settings.certs_url)
        .env(
            "TRUSTIOT__CERTIFICATE__SERVICE_NAME",
            &settings.service_name,
        )
        .env("TRUSTIOT__CERTIFICATE__GENERATION", &settings.generation)
        .env("TRUSTIOT__CERTIFICATE__WAIT", "1s")
        .stdin(Stdio::null());
    // The fakes answer on this runtime's threads while the command runs.
    let output = tokio::task::spawn_blocking(move || command.output())
        .await
        .expect("the command's thread")
        .expect("run migrate");
    (
        output.status.code(),
        format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

/// The base64 lines of a PEM text: what would show up if it were printed.
fn pem_lines(pem: &str) -> Vec<&str> {
    pem.lines()
        .filter(|line| !line.starts_with("-----") && line.len() >= 16)
        .collect()
}

#[tokio::test]
async fn the_command_prints_neither_the_key_nor_the_certificate() {
    let secrets = FakeSecrets::start().await;
    secrets.declare(SECRET);
    let certs = FakeCerts::start().await;
    let settings = secrets.certificate_settings(SECRET, &certs.url, SERVICE, "1");

    // Before the CA ceremony: a warning and exit code 0.
    certs.set(Mode::Status(503));
    let (code, printed) = command(&settings).await;
    assert_eq!(code, Some(0), "{printed}");
    assert!(printed.contains("cannot sign"), "{printed}");
    assert!(secrets.get(SECRET).expect("the Secret").data.is_empty());

    certs.set(Mode::Sign);
    let (code, printed) = command(&settings).await;
    assert_eq!(code, Some(0), "{printed}");
    assert!(
        printed.contains("wrote a new server certificate"),
        "{printed}"
    );
    let (chain, key) = pair(&secrets, SECRET);
    let (code, kept) = command(&settings).await;
    assert_eq!(code, Some(0), "{kept}");
    assert!(kept.contains("is current"), "{kept}");

    // A renewal the certs service refuses: exit code 1.
    let raised = secrets.certificate_settings(SECRET, &certs.url, SERVICE, "2");
    certs.set(Mode::Status(422));
    let (code, refused) = command(&raised).await;
    assert_eq!(code, Some(1), "{refused}");
    assert!(refused.contains("HTTP 422"), "{refused}");

    for output in [&printed, &kept, &refused] {
        assert!(!output.contains("PRIVATE KEY"), "{output}");
        assert!(!output.contains("CERTIFICATE-----"), "{output}");
        for line in pem_lines(&key).into_iter().chain(pem_lines(&chain)) {
            assert!(
                !output.contains(line),
                "printed a line of the PEM:\n{output}"
            );
        }
    }
}
