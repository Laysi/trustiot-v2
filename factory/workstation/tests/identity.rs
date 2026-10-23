//! The workstation's key, CSR and certificate: `init` and `install-certificate`, and the checks
//! a certificate must pass before it is kept (Laysi/trustiot-v2#614).

mod support;

use std::time::{SystemTime, UNIX_EPOCH};

use rcgen::KeyPair;
use support::{
    Environment, Profile, Purpose, Scratch, TestCa, mode, pem_body_lines, run, set_mode,
};
use time::Duration;
use workstation::Error;
use workstation::identity::{
    self, CertificateProblem, KeyOrigin, WorkstationDirectory, WorkstationKey,
};

fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after 1970")
            .as_secs(),
    )
    .expect("fits")
}

/// The workstation key in `directory`, as rcgen reads it (to issue test certificates for it).
fn key_pair(directory: &WorkstationDirectory) -> KeyPair {
    let pem = std::fs::read_to_string(directory.key()).expect("the key file");
    KeyPair::from_pem(&pem).expect("a key")
}

#[test]
fn init_creates_a_0600_key_and_a_csr_and_never_replaces_the_key() {
    let scratch = Scratch::new("init");
    let directory = WorkstationDirectory::new(scratch.join("ws"));
    assert_eq!(
        identity::init(&directory).expect("init"),
        KeyOrigin::Created
    );
    assert_eq!(mode(directory.path()), 0o700);
    assert_eq!(mode(&directory.key()), 0o600);
    assert_eq!(mode(&directory.csr()), 0o644);
    let key = std::fs::read(directory.key()).expect("the key");
    let csr = std::fs::read_to_string(directory.csr()).expect("the CSR");
    assert!(
        csr.starts_with("-----BEGIN CERTIFICATE REQUEST-----"),
        "{csr}"
    );

    assert_eq!(
        identity::init(&directory).expect("init again"),
        KeyOrigin::Existing
    );
    assert_eq!(
        std::fs::read(directory.key()).expect("the key"),
        key,
        "kept"
    );
    let again = std::fs::read_to_string(directory.csr()).expect("the CSR");
    let public = |pem: &str| {
        rcgen::CertificateSigningRequestParams::from_pem(pem)
            .expect("a CSR")
            .public_key
    };
    assert_eq!(public(&again), public(&csr), "a CSR for the same key");
}

#[test]
fn a_key_file_wider_than_0600_is_refused_by_name_without_its_contents() {
    let scratch = Scratch::new("key-mode");
    let directory = WorkstationDirectory::new(scratch.join("ws"));
    identity::init(&directory).expect("init");
    let pem = std::fs::read_to_string(directory.key()).expect("the key");
    for wider in [0o640, 0o644, 0o604, 0o700] {
        set_mode(&directory.key(), wider);
        for error in [
            WorkstationKey::load(&directory.key()).expect_err("refused"),
            identity::init(&directory).expect_err("refused"),
            identity::install_certificate(&directory, b"", now()).expect_err("refused"),
        ] {
            let message = error.to_string();
            assert!(matches!(error, Error::TooOpen { .. }), "{message}");
            assert!(
                message.contains(&directory.key().display().to_string()),
                "{message}"
            );
            for line in pem_body_lines(&pem) {
                assert!(!message.contains(&line), "{message}");
            }
        }
    }
}

#[test]
fn install_certificate_refuses_every_profile_but_the_workstation_one() {
    let scratch = Scratch::new("install");
    let directory = WorkstationDirectory::new(scratch.join("ws"));
    identity::init(&directory).expect("init");
    let key = key_pair(&directory);
    let other_key = KeyPair::generate().expect("another key");
    let ca = TestCa::new();
    let good = Profile::workstation("line-a-ws-01");
    let now = now();

    let cases: Vec<(&str, String, CertificateProblem)> = vec![
        (
            "no extended key usage",
            ca.issue(
                &key,
                &Profile {
                    purpose: Purpose::None,
                    ..good.clone()
                },
            ),
            CertificateProblem::NotWorkstationPurpose,
        ),
        (
            "clientAuth only (a gateway certificate)",
            ca.issue(
                &key,
                &Profile {
                    purpose: Purpose::ClientAuthOnly,
                    ..good.clone()
                },
            ),
            CertificateProblem::NotWorkstationPurpose,
        ),
        (
            "workstation purpose and clientAuth",
            ca.issue(
                &key,
                &Profile {
                    purpose: Purpose::WorkstationAndClientAuth,
                    ..good.clone()
                },
            ),
            CertificateProblem::TlsPurpose,
        ),
        (
            "no workstation: prefix",
            ca.issue(
                &key,
                &Profile {
                    common_name: "line-a-ws-01".to_owned(),
                    ..good.clone()
                },
            ),
            CertificateProblem::Subject,
        ),
        (
            "a gateway id as subject",
            ca.issue(
                &key,
                &Profile {
                    common_name: "gw-0001".to_owned(),
                    ..good.clone()
                },
            ),
            CertificateProblem::Subject,
        ),
        (
            "expired",
            ca.issue(
                &key,
                &Profile {
                    not_before: good.not_before - Duration::days(200),
                    not_after: good.not_before - Duration::days(1),
                    ..good.clone()
                },
            ),
            CertificateProblem::Expired(String::new()),
        ),
        (
            "not yet valid",
            ca.issue(
                &key,
                &Profile {
                    not_before: good.not_after - Duration::days(1),
                    ..good.clone()
                },
            ),
            CertificateProblem::NotYetValid(String::new()),
        ),
        (
            "another key",
            ca.issue(&other_key, &good),
            CertificateProblem::KeyMismatch,
        ),
        (
            "two certificates",
            format!("{}{}", ca.issue(&key, &good), ca.issue(&key, &good)),
            CertificateProblem::NotOneCertificate,
        ),
    ];
    for (case, pem, expected) in cases {
        let error = identity::install_certificate(&directory, pem.as_bytes(), now).expect_err(case);
        let Error::Certificate(problem) = &error else {
            panic!("{case}: {error}");
        };
        assert_eq!(
            std::mem::discriminant(problem),
            std::mem::discriminant(&expected),
            "{case}: {problem}"
        );
        assert!(
            !directory.certificate().exists(),
            "{case}: nothing is stored"
        );
    }

    let pem = ca.issue(&key, &good);
    let checked =
        identity::install_certificate(&directory, pem.as_bytes(), now).expect("the good one");
    assert_eq!(checked.subject, "workstation:line-a-ws-01");
    assert_eq!(checked.serial, good.serial_hex());
    assert_eq!(
        std::fs::read_to_string(directory.certificate()).expect("stored"),
        pem
    );
    assert_eq!(mode(&directory.certificate()), 0o644);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_binary_initialises_and_installs() {
    let scratch = Scratch::new("binary-install");
    let environment = Environment::in_scratch(&scratch);
    let dir = scratch.join("ws");
    let dir_text = dir.display().to_string();
    let init = run(&environment, &["init", "--dir", &dir_text], "").await;
    assert!(init.success, "{init:?}");
    assert!(
        init.stdout.contains("created the workstation key"),
        "{init:?}"
    );

    let directory = WorkstationDirectory::new(dir.clone());
    let key = key_pair(&directory);
    let certificate = scratch.join("issued.pem");
    std::fs::write(
        &certificate,
        TestCa::new().issue(&key, &Profile::workstation("line-a-ws-01")),
    )
    .expect("written");
    let certificate_text = certificate.display().to_string();
    let install = run(
        &environment,
        &[
            "install-certificate",
            "--certificate",
            &certificate_text,
            "--dir",
            &dir_text,
        ],
        "",
    )
    .await;
    assert!(install.success, "{install:?}");
    assert!(
        install
            .stdout
            .contains("installed workstation:line-a-ws-01"),
        "{install:?}"
    );
    assert!(directory.certificate().exists());

    // A gateway-like certificate is refused with exit code 1 and the reason.
    std::fs::write(
        &certificate,
        TestCa::new().issue(
            &key,
            &Profile {
                purpose: Purpose::ClientAuthOnly,
                ..Profile::workstation("line-a-ws-01")
            },
        ),
    )
    .expect("written");
    let refused = run(
        &environment,
        &[
            "install-certificate",
            "--certificate",
            &certificate_text,
            "--dir",
            &dir_text,
        ],
        "",
    )
    .await;
    assert_eq!(refused.code, Some(1), "{refused:?}");
    assert!(
        refused.stderr.contains("TrustIoT workstation purpose"),
        "{refused:?}"
    );

    // Usage mistakes exit with 2.
    let usage = run(&environment, &["install-certificate"], "").await;
    assert_eq!(usage.code, Some(2), "{usage:?}");
}
