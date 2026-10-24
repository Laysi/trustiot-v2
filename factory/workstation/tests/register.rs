//! `register` against a stand-in for the certs service's registration endpoint
//! (Laysi/trustiot-v2#615): retries with a new signature each time, the refusals that stop at
//! once with their own exit codes, the checks of the answer, the records and the log, and that
//! none of the session cookie, the workstation key or a signature header ever shows.

mod support;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use contract::certs::{
    WORKSTATION_CERTIFICATE_HEADER, WORKSTATION_NONCE_HEADER, WORKSTATION_SIGNATURE_HEADER,
    WORKSTATION_TIMESTAMP_HEADER,
};
use http::StatusCode;
use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P384_SHA384};
use sha2::{Digest as _, Sha256};
use support::registry::{self, Action, Registry, Tamper};
use support::{
    Environment, FakeSecrets, Profile, Run, Scratch, TestCa, mode, pem_body_lines, run,
    run_with_file_size_limit,
};
use workstation::Error;
use workstation::config::Origin;
use workstation::http::{Client, ExtraHeaders};
use workstation::identity::{self, WorkstationDirectory};
use workstation::journal::{Journal, State};
use workstation::register::{self as registration, Backoff, Gateway, Registrar, StopKind};
use workstation::secret::SessionCookie;
use workstation::session::{Session, SessionStore};

fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after 1970")
            .as_secs(),
    )
    .expect("fits")
}

/// A gateway's CSR and endorsement key, as files.
struct GatewayFiles {
    csr: PathBuf,
    ek: PathBuf,
}

impl GatewayFiles {
    /// A new gateway key's CSR and a P-384 endorsement key, written under `scratch`.
    fn new(scratch: &Scratch, name: &str) -> Self {
        let key = KeyPair::generate().expect("a gateway key");
        let request = CertificateParams::new(Vec::new())
            .expect("params")
            .serialize_request(&key)
            .expect("a CSR")
            .pem()
            .expect("PEM");
        let endorsement = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384)
            .expect("an endorsement key")
            .public_key_pem();
        let csr = scratch.join(&format!("{name}.csr.pem"));
        let ek = scratch.join(&format!("{name}.ek.pem"));
        std::fs::write(&csr, request).expect("the CSR file");
        std::fs::write(&ek, endorsement).expect("the endorsement key file");
        Self { csr, ek }
    }

    fn read(&self) -> (String, String) {
        (
            std::fs::read_to_string(&self.csr).expect("the CSR"),
            std::fs::read_to_string(&self.ek).expect("the endorsement key"),
        )
    }
}

/// A workstation with a key, a certificate and a platform administrator's session, and the
/// stand-in it registers with.
struct Fixture {
    scratch: Scratch,
    environment: Environment,
    secrets: FakeSecrets,
    /// A value the stand-in puts in a `Set-Cookie` of every answer.
    planted: String,
    directory: WorkstationDirectory,
    registry: Registry,
    /// The SHA-256 of the stand-in's root certificate, as `--root-sha256` takes it.
    root_sha256: [u8; 32],
}

impl Fixture {
    async fn start(name: &str, script: Vec<Action>) -> Self {
        let scratch = Scratch::new(name);
        let environment = Environment::in_scratch(&scratch);
        let secrets = FakeSecrets::new();
        let planted = FakeSecrets::new().cookie;
        let directory = WorkstationDirectory::new(scratch.join("ws"));
        identity::init(&directory).expect("init");
        let key = KeyPair::from_pem(&std::fs::read_to_string(directory.key()).expect("the key"))
            .expect("a key");
        let certificate = TestCa::new().issue(&key, &Profile::workstation("line-a-ws-01"));
        identity::install_certificate(&directory, certificate.as_bytes(), now())
            .expect("installed");
        let registry = Registry::start(
            &secrets.cookie,
            registry::pem_der(&certificate),
            &planted,
            script,
        )
        .await;
        let store = SessionStore::new(
            environment
                .runtime
                .as_ref()
                .expect("a runtime directory")
                .join("trustiot-workstation"),
        );
        store
            .save(&Session {
                origin: Origin::parse(&registry.url).expect("a loopback origin"),
                cookie: SessionCookie::parse(&secrets.cookie).expect("a cookie"),
            })
            .expect("the session saved");
        let root_sha256 = Sha256::digest(registry::pem_der(registry.root_pem())).into();
        Self {
            scratch,
            environment,
            secrets,
            planted,
            directory,
            registry,
            root_sha256,
        }
    }

    fn journal(&self) -> Journal {
        Journal::new(self.directory.path())
    }

    async fn register(&self, hardware_id: &str, gateway: &GatewayFiles, extra: &[&str]) -> Run {
        let arguments = self.arguments(hardware_id, gateway, extra);
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        run(&self.environment, &arguments, "").await
    }

    /// The `register` command line; `--root-sha256` is the stand-in's root unless `extra` gives
    /// one.
    fn arguments(&self, hardware_id: &str, gateway: &GatewayFiles, extra: &[&str]) -> Vec<String> {
        let path = |path: &Path| path.to_str().expect("a UTF-8 path").to_owned();
        let mut arguments = vec![
            "register".to_owned(),
            "--hardware-id".to_owned(),
            hardware_id.to_owned(),
            "--csr".to_owned(),
            path(&gateway.csr),
            "--ek".to_owned(),
            path(&gateway.ek),
            "--dir".to_owned(),
            path(self.directory.path()),
        ];
        if !extra.contains(&"--root-sha256") {
            let pin: String = self
                .root_sha256
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            arguments.extend(["--root-sha256".to_owned(), pin]);
        }
        arguments.extend(extra.iter().map(|argument| (*argument).to_owned()));
        arguments
    }

    /// The log's events in order: `(hardware_id, event, the whole line)`.
    fn events(&self) -> Vec<(String, String, serde_json::Value)> {
        let text = std::fs::read_to_string(self.journal().log_path()).unwrap_or_default();
        text.lines()
            .map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).expect("a JSON line");
                (
                    value["hardware_id"].as_str().expect("an ID").to_owned(),
                    value["event"].as_str().expect("an event").to_owned(),
                    value,
                )
            })
            .collect()
    }

    fn event_names(&self, hardware_id: &str) -> Vec<String> {
        self.events()
            .into_iter()
            .filter(|(id, _, _)| id == hardware_id)
            .map(|(_, event, _)| event)
            .collect()
    }

    /// Every file under the records' directory (temporary ones too) and the log.
    fn journal_texts(&self) -> Vec<(PathBuf, String)> {
        let journal = self.journal();
        let mut texts = Vec::new();
        if let Ok(entries) = std::fs::read_dir(journal.records_directory()) {
            for entry in entries {
                let path = entry.expect("an entry").path();
                let bytes = std::fs::read(&path).expect("readable");
                let text = String::from_utf8_lossy(&bytes);
                texts.push((path, text.into_owned()));
            }
        }
        if let Ok(text) = std::fs::read_to_string(journal.log_path()) {
            texts.push((journal.log_path().to_owned(), text));
        }
        texts
    }

    /// Neither `runs`' output nor any file of the journal shows the session cookie, the cookie
    /// the stand-in planted, a line of the workstation key, or a signature or nonce header sent.
    fn assert_nothing_secret_shows(&self, runs: &[&Run]) {
        let mut forbidden: Vec<(&str, String)> = vec![
            ("the session cookie", self.secrets.cookie.clone()),
            ("a Set-Cookie value of an answer", self.planted.clone()),
        ];
        let key = std::fs::read_to_string(self.directory.key()).expect("the key");
        forbidden.extend(
            pem_body_lines(&key)
                .into_iter()
                .map(|line| ("a line of the workstation key", line)),
        );
        let seen = self.registry.seen();
        assert!(!seen.is_empty(), "requests were sent");
        for request in &seen {
            forbidden.push((
                "a signature header",
                request.header(WORKSTATION_SIGNATURE_HEADER).to_owned(),
            ));
            forbidden.push((
                "a nonce header",
                request.header(WORKSTATION_NONCE_HEADER).to_owned(),
            ));
        }
        let mut texts: Vec<(String, String)> = runs
            .iter()
            .map(|run| ("the output".to_owned(), run.output()))
            .collect();
        texts.extend(
            self.journal_texts()
                .into_iter()
                .map(|(path, text)| (path.display().to_string(), text)),
        );
        for (place, text) in &texts {
            for (what, value) in &forbidden {
                assert!(!value.is_empty(), "{what} is set");
                assert!(!text.contains(value.as_str()), "{what} shows in {place}");
            }
        }
    }
}

/// Every request was signed afresh: verified by the stand-in, a nonce and a signature never
/// used before, the same workstation certificate and the same body.
fn assert_signed_afresh(registry: &Registry) {
    let seen = registry.seen();
    assert!(
        seen.iter().all(|request| request.verified),
        "every request verified"
    );
    let nonces: HashSet<&str> = seen
        .iter()
        .map(|request| request.header(WORKSTATION_NONCE_HEADER))
        .collect();
    assert_eq!(nonces.len(), seen.len(), "a new nonce on every attempt");
    let signatures: HashSet<&str> = seen
        .iter()
        .map(|request| request.header(WORKSTATION_SIGNATURE_HEADER))
        .collect();
    assert_eq!(
        signatures.len(),
        seen.len(),
        "a new signature on every attempt"
    );
    for request in &seen {
        assert!(!request.header(WORKSTATION_TIMESTAMP_HEADER).is_empty());
        assert_eq!(
            request.header(WORKSTATION_CERTIFICATE_HEADER),
            seen[0].header(WORKSTATION_CERTIFICATE_HEADER)
        );
        assert_eq!(request.body, seen[0].body, "the same body every time");
    }
}

fn answered(registry: &Registry) -> Vec<u16> {
    registry
        .seen()
        .iter()
        .map(|request| request.answered)
        .collect()
}

#[tokio::test]
async fn a_lost_answer_is_sent_again_and_the_repeat_counts_as_success() {
    let fixture = Fixture::start("lost", vec![Action::DropAfterCommit]).await;
    let gateway = GatewayFiles::new(&fixture.scratch, "gw");
    let result = fixture.register("gw-lost-0001", &gateway, &[]).await;
    assert_eq!(result.code, Some(0), "{}", result.output());
    assert!(
        result
            .stdout
            .contains("the platform already had the registration of hardware ID gw-lost-0001"),
        "{}",
        result.stdout
    );
    assert!(
        result.stdout.contains("after 2 attempt(s)"),
        "{}",
        result.stdout
    );
    assert_eq!(answered(&fixture.registry), vec![0, 200]);
    assert_eq!(fixture.registry.registrations(), 1);
    assert_signed_afresh(&fixture.registry);

    let record = fixture
        .journal()
        .load("gw-lost-0001")
        .expect("readable")
        .expect("a record");
    assert_eq!(record.state, State::Registered);
    let pinned: String = fixture
        .root_sha256
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        record.root_sha256,
        Some(pinned),
        "the pinned root is recorded"
    );
    let registration = record.registration.expect("the registration");
    assert_eq!(registration.hardware_id, "gw-lost-0001");
    assert!(
        registration
            .certificate
            .starts_with("-----BEGIN CERTIFICATE-----")
    );
    assert_eq!(mode(&fixture.journal().record_path("gw-lost-0001")), 0o600);
    assert_eq!(mode(fixture.journal().log_path()), 0o600);
    assert_eq!(
        fixture.event_names("gw-lost-0001"),
        ["started", "attempt_failed", "registered"]
    );
    fixture.assert_nothing_secret_shows(&[&result]);
}

#[tokio::test]
async fn a_503_is_tried_again_and_the_next_answer_registers() {
    let fixture = Fixture::start(
        "unavailable",
        vec![Action::error(503, "unavailable", "error.unavailable")],
    )
    .await;
    let gateway = GatewayFiles::new(&fixture.scratch, "gw");
    let result = fixture.register("gw-busy-0001", &gateway, &[]).await;
    assert_eq!(result.code, Some(0), "{}", result.output());
    assert!(
        result
            .stdout
            .contains("registered hardware ID gw-busy-0001"),
        "{}",
        result.stdout
    );
    assert_eq!(answered(&fixture.registry), vec![503, 201]);
    assert_signed_afresh(&fixture.registry);
    let events = fixture.events();
    let failed = events
        .iter()
        .find(|(_, event, _)| event == "attempt_failed")
        .expect("the failed attempt is logged");
    assert_eq!(failed.2["problem"], "status 503");
    fixture.assert_nothing_secret_shows(&[&result]);
}

#[tokio::test]
async fn each_refusal_stops_at_once_with_its_own_exit_code() {
    let cases: Vec<(&str, Action, StopKind, &str)> = vec![
        (
            "public key mismatch",
            Action::error(409, "conflict", "gateway_registration.public_key_mismatch"),
            StopKind::KeyMismatch,
            "registered with another TPM key",
        ),
        (
            "endorsement key mismatch",
            Action::error(
                409,
                "conflict",
                "gateway_registration.endorsement_key_mismatch",
            ),
            StopKind::KeyMismatch,
            "registered with another TPM key",
        ),
        (
            "public key in use",
            Action::error(409, "conflict", "gateway_registration.public_key_in_use"),
            StopKind::KeyInUse,
            "registered under another hardware ID",
        ),
        (
            "endorsement key in use",
            Action::error(
                409,
                "conflict",
                "gateway_registration.endorsement_key_in_use",
            ),
            StopKind::KeyInUse,
            "registered under another hardware ID",
        ),
        (
            "decommissioned",
            Action::error(409, "conflict", "gateway_registration.decommissioned"),
            StopKind::Decommissioned,
            "decommissioned",
        ),
        (
            "session ended",
            Action::error(401, "unauthenticated", "error.unauthenticated"),
            StopKind::LoginRequired,
            "log in again",
        ),
        (
            "stale timestamp",
            Action::refused("stale"),
            StopKind::ClockSkew,
            "clock",
        ),
        (
            "revoked workstation certificate",
            Action::refused("revoked"),
            StopKind::WorkstationRefused,
            "workstation certificate",
        ),
        (
            "rewritten signature header",
            Action::refused("signature_invalid"),
            StopKind::SignatureRejected,
            "do not enroll again",
        ),
        (
            "not a platform administrator",
            Action::error(403, "permission_denied", "error.permission_denied"),
            StopKind::NotPlatformAdministrator,
            "platform administrator",
        ),
        (
            "invalid field",
            Action::Error {
                status: 422,
                code: "validation_failed",
                message_key: "error.validation_failed",
                reason: None,
                fields: vec![("/hardware_id", "validation.gateway_id")],
            },
            StopKind::Invalid,
            "refuses these fields",
        ),
    ];
    let codes: HashSet<u8> = cases.iter().map(|case| case.2.exit_code()).collect();
    assert_eq!(codes.len(), 9, "each kind its own exit code");
    for (name, action, kind, advice) in cases {
        let fixture = Fixture::start("refused", vec![action]).await;
        let gateway = GatewayFiles::new(&fixture.scratch, "gw");
        let result = fixture.register("gw-refused-01", &gateway, &[]).await;
        assert_eq!(
            result.code,
            Some(i32::from(kind.exit_code())),
            "{name}: {}",
            result.output()
        );
        assert!(result.stderr.contains(advice), "{name}: {}", result.stderr);
        assert_eq!(fixture.registry.seen().len(), 1, "{name}: sent once");
        let record = fixture
            .journal()
            .load("gw-refused-01")
            .expect("readable")
            .expect("a record");
        assert_eq!(record.state, State::Pending, "{name}");
        assert!(record.registration.is_none(), "{name}");
        let events = fixture.events();
        let (_, last, line) = events.last().expect("events");
        assert_eq!(last, "stopped", "{name}");
        assert_eq!(line["stop"], kind.as_str(), "{name}");
        if kind == StopKind::LoginRequired {
            assert_eq!(fixture.registry.logouts(), 1, "{name}: the session revoked");
            assert!(
                !fixture.environment.session_file().exists(),
                "{name}: the session file removed"
            );
        }
        if kind == StopKind::ClockSkew {
            assert_eq!(line["reason"], "stale", "{name}");
            assert!(result.stderr.contains("Date header"), "{}", result.stderr);
        }
        fixture.assert_nothing_secret_shows(&[&result]);
    }
}

#[tokio::test]
async fn an_answer_that_is_not_this_gateways_registration_is_not_recorded() {
    for tamper in [
        Tamper::HardwareId,
        Tamper::PublicKeySha256,
        Tamper::EndorsementKeySha256,
        Tamper::CommonName,
        Tamper::CertificateKey,
        Tamper::ForeignIssuer,
        Tamper::ChainLeaf,
        Tamper::ChainWithoutIssuer,
    ] {
        let fixture = Fixture::start("tampered", vec![Action::Tamper(tamper)]).await;
        let gateway = GatewayFiles::new(&fixture.scratch, "gw");
        let result = fixture.register("gw-checked-01", &gateway, &[]).await;
        assert_eq!(
            result.code,
            Some(i32::from(StopKind::ResponseMismatch.exit_code())),
            "{tamper:?}: {}",
            result.output()
        );
        assert!(
            result.stderr.contains("Nothing was recorded as registered"),
            "{tamper:?}: {}",
            result.stderr
        );
        assert_eq!(fixture.registry.seen().len(), 1, "{tamper:?}: not retried");
        let record = fixture
            .journal()
            .load("gw-checked-01")
            .expect("readable")
            .expect("a record");
        assert_eq!(record.state, State::Pending, "{tamper:?}");
        assert!(record.registration.is_none(), "{tamper:?}");
        let (_, last, line) = fixture.events().pop().expect("events");
        assert_eq!(last, "stopped");
        assert_eq!(line["stop"], "response_mismatch", "{tamper:?}");
    }
}

#[tokio::test]
async fn a_root_certificate_other_than_the_given_fingerprint_is_not_recorded() {
    let fixture = Fixture::start("root", Vec::new()).await;
    let gateway = GatewayFiles::new(&fixture.scratch, "gw");
    let other = "ab".repeat(32);
    let result = fixture
        .register("gw-root-0001", &gateway, &["--root-sha256", &other])
        .await;
    assert_eq!(
        result.code,
        Some(i32::from(StopKind::ResponseMismatch.exit_code())),
        "{}",
        result.output()
    );
    assert!(result.stderr.contains("--root-sha256"), "{}", result.stderr);
    let record = fixture
        .journal()
        .load("gw-root-0001")
        .expect("readable")
        .expect("a record");
    assert_eq!(record.state, State::Pending);

    // The fingerprint as `openssl x509 -fingerprint -sha256` prints it.
    let digest = Sha256::digest(registry::pem_der(fixture.registry.root_pem()));
    let fingerprint = digest
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":");

    // A whole other hierarchy, consistent in itself, with the gateway's name and key: only the
    // pinned root tells it apart.
    fixture
        .registry
        .push(vec![Action::Tamper(Tamper::ForeignRoot)]);
    let result = fixture
        .register("gw-root-0001", &gateway, &["--root-sha256", &fingerprint])
        .await;
    assert_eq!(
        result.code,
        Some(i32::from(StopKind::ResponseMismatch.exit_code())),
        "{}",
        result.output()
    );
    assert!(result.stderr.contains("--root-sha256"), "{}", result.stderr);

    let result = fixture
        .register("gw-root-0001", &gateway, &["--root-sha256", &fingerprint])
        .await;
    assert_eq!(result.code, Some(0), "{}", result.output());
    assert_eq!(answered(&fixture.registry), vec![201, 200, 200]);
    let record = fixture
        .journal()
        .load("gw-root-0001")
        .expect("readable")
        .expect("a record");
    assert_eq!(record.state, State::Registered);
}

#[tokio::test]
async fn a_rerun_keeps_the_record_and_gets_the_same_registration() {
    let fixture = Fixture::start("rerun", Vec::new()).await;
    let gateway = GatewayFiles::new(&fixture.scratch, "gw");
    let first = fixture.register("gw-rerun-001", &gateway, &[]).await;
    assert_eq!(first.code, Some(0), "{}", first.output());
    let journal = fixture.journal();
    let before = journal
        .load("gw-rerun-001")
        .expect("readable")
        .expect("a record");
    let second = fixture.register("gw-rerun-001", &gateway, &[]).await;
    assert_eq!(second.code, Some(0), "{}", second.output());
    assert!(
        second.stdout.contains("already had the registration"),
        "{}",
        second.stdout
    );
    assert_eq!(answered(&fixture.registry), vec![201, 200]);
    let after = journal
        .load("gw-rerun-001")
        .expect("readable")
        .expect("a record");
    assert_eq!(after.state, State::Registered);
    assert_eq!(
        after.registration.expect("registered").serial,
        before.registration.expect("registered").serial
    );
    let started: Vec<bool> = fixture
        .events()
        .iter()
        .filter(|(_, event, _)| event == "started")
        .map(|(_, _, line)| line["recorded_before"].as_bool().expect("a flag"))
        .collect();
    assert_eq!(started, [false, true]);
    assert_eq!(
        fixture.event_names("gw-rerun-001"),
        ["started", "registered", "started", "registered"]
    );
    fixture.assert_nothing_secret_shows(&[&first, &second]);
}

#[tokio::test]
async fn a_gateway_key_recorded_under_another_hardware_id_is_not_sent() {
    let fixture = Fixture::start("duplicate", Vec::new()).await;
    let gateway = GatewayFiles::new(&fixture.scratch, "gw");
    let first = fixture.register("gw-label-001", &gateway, &[]).await;
    assert_eq!(first.code, Some(0), "{}", first.output());
    let second = fixture.register("gw-label-002", &gateway, &[]).await;
    assert_eq!(
        second.code,
        Some(i32::from(StopKind::KeyInUse.exit_code())),
        "{}",
        second.output()
    );
    assert!(
        second.stderr.contains("gw-label-001") && second.stderr.contains("nothing was sent"),
        "{}",
        second.stderr
    );
    assert_eq!(fixture.registry.seen().len(), 1, "the second was not sent");
    assert!(
        fixture
            .journal()
            .load("gw-label-002")
            .expect("readable")
            .is_none(),
        "no record for the second ID"
    );
    let (id, last, line) = fixture.events().pop().expect("events");
    assert_eq!((id.as_str(), last.as_str()), ("gw-label-002", "stopped"));
    assert_eq!(line["recorded_under"], "gw-label-001");
}

#[tokio::test]
async fn retries_stop_at_the_attempt_limit_and_a_rerun_completes() {
    let fixture = Fixture::start(
        "limit",
        vec![
            Action::error(503, "unavailable", "error.unavailable"),
            Action::Hold,
        ],
    )
    .await;
    let gateway = GatewayFiles::new(&fixture.scratch, "gw");
    let result = fixture
        .register(
            "gw-limit-001",
            &gateway,
            &["--attempts", "2", "--timeout", "1"],
        )
        .await;
    assert_eq!(
        result.code,
        Some(i32::from(StopKind::GaveUp.exit_code())),
        "{}",
        result.output()
    );
    assert!(
        result
            .stderr
            .contains("run the same register command again"),
        "{}",
        result.stderr
    );
    assert_eq!(answered(&fixture.registry), vec![503, 0]);
    let (_, last, line) = fixture.events().pop().expect("events");
    assert_eq!(last, "gave_up");
    assert_eq!(line["attempts"], 2);
    assert_eq!(line["problem"], "timeout");

    let again = fixture.register("gw-limit-001", &gateway, &[]).await;
    assert_eq!(again.code, Some(0), "{}", again.output());
    assert_eq!(answered(&fixture.registry), vec![503, 0, 201]);
    assert_signed_afresh(&fixture.registry);
    fixture.assert_nothing_secret_shows(&[&result, &again]);
}

#[tokio::test]
async fn a_write_cut_off_by_a_crash_leaves_the_previous_record_whole() {
    let fixture = Fixture::start("crash", Vec::new()).await;
    let gateway = GatewayFiles::new(&fixture.scratch, "gw");
    let journal = fixture.journal();
    // 2 blocks are 1 or 2 KiB: enough for the pending record and the first log line, not for the
    // registered record with its certificates, so the process dies in the middle of writing it.
    let arguments = fixture.arguments("gw-crash-0001", &gateway, &[]);
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let crashed = run_with_file_size_limit(&fixture.environment, 2, &arguments).await;
    assert!(!crashed.success, "{crashed:?}");
    assert_eq!(crashed.code, None, "killed by SIGXFSZ: {crashed:?}");
    assert_eq!(
        fixture.registry.registrations(),
        1,
        "the platform registered it"
    );

    let record = journal
        .load("gw-crash-0001")
        .expect("the record is whole")
        .expect("a record");
    assert_eq!(record.state, State::Pending);
    let names: Vec<String> = std::fs::read_dir(journal.records_directory())
        .expect("the records directory")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let half: Vec<&String> = names.iter().filter(|name| name.starts_with('.')).collect();
    assert_eq!(half.len(), 1, "the cut-off temporary file: {names:?}");
    assert!(
        names.contains(&"gw-crash-0001.json".to_owned()),
        "{names:?}"
    );
    let partial = std::fs::read(journal.records_directory().join(half[0])).expect("readable");
    assert!(!partial.is_empty(), "the write was cut off, not skipped");
    assert!(
        serde_json::from_slice::<serde_json::Value>(&partial).is_err(),
        "the temporary file is a half record"
    );
    assert_eq!(journal.records().expect("the records").len(), 1);

    let again = fixture.register("gw-crash-0001", &gateway, &[]).await;
    assert_eq!(again.code, Some(0), "{}", again.output());
    assert_eq!(answered(&fixture.registry), vec![201, 200]);
    let record = journal
        .load("gw-crash-0001")
        .expect("readable")
        .expect("a record");
    assert_eq!(record.state, State::Registered);
    fixture.assert_nothing_secret_shows(&[&crashed, &again]);
}

/// The library function Laysi/trustiot-v2#616 calls with the CSR and the endorsement key it got
/// from the gateway's TPM.
#[tokio::test]
async fn the_library_function_retries_until_settled_and_reports_the_attempts() {
    let fixture = Fixture::start(
        "library",
        vec![
            Action::error(503, "unavailable", "error.unavailable"),
            Action::error(502, "unavailable", "error.unavailable"),
            Action::DropAfterCommit,
        ],
    )
    .await;
    let gateway_files = GatewayFiles::new(&fixture.scratch, "gw");
    let (request, endorsement_key) = gateway_files.read();
    let gateway = Gateway::new(
        "gw-lib-00001",
        request.as_bytes(),
        endorsement_key.as_bytes(),
    )
    .expect("a gateway");
    let workstation =
        registration::Workstation::load(&fixture.directory, now()).expect("the workstation");
    let client = Client::new(
        Origin::parse(&fixture.registry.url).expect("an origin"),
        None,
        ExtraHeaders::none(),
        Duration::from_secs(60),
    )
    .expect("a client");
    let cookie = SessionCookie::parse(&fixture.secrets.cookie).expect("a cookie");
    let journal = fixture.journal();
    let registrar = Registrar {
        client: &client,
        cookie: &cookie,
        workstation: &workstation,
        journal: &journal,
        backoff: Backoff {
            initial: Duration::from_millis(1),
            cap: Duration::from_millis(4),
            max_attempts: 5,
            max_total: Duration::from_secs(600),
        },
        root_sha256: fixture.root_sha256,
    };
    let registered = registration::register(&registrar, &gateway, now)
        .await
        .expect("registered");
    assert_eq!(registered.status, StatusCode::OK);
    assert_eq!(registered.attempts, 4);
    assert_eq!(registered.registration.hardware_id, "gw-lib-00001");
    assert_eq!(answered(&fixture.registry), vec![503, 502, 0, 200]);
    assert_signed_afresh(&fixture.registry);

    // A 409 is not retried.
    fixture.registry.push(vec![Action::error(
        409,
        "conflict",
        "gateway_registration.decommissioned",
    )]);
    let error = registration::register(&registrar, &gateway, now)
        .await
        .expect_err("stopped");
    let Error::Registration(stopped) = error else {
        panic!("a registration stop: {error}");
    };
    assert_eq!(stopped.kind, StopKind::Decommissioned);
    assert_eq!(fixture.registry.seen().len(), 5);
    // A stop after a registration does not take back the record.
    let record = journal
        .load("gw-lib-00001")
        .expect("readable")
        .expect("a record");
    assert_eq!(record.state, State::Registered);
}
