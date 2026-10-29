//! `trustiot-agent provision` (`agent::provision::run`, what the executable runs) against a real
//! TPM 2.0 implementation: swtpm (libtpms) in a throwaway container (`testkit::swtpm`), set up
//! and inspected with tpm2-tools (Laysi/trustiot-v2#613).
//!
//! - on a TPM with the standard SRK, the report's CSR verifies (read back as the workstation
//!   reads it), names the hardware ID and carries the report's public key, which is the key
//!   tpm2-tools reads at the handle; the report's EK is the one `endorsement_key` and
//!   `tpm2_createek -G ecc` compute;
//! - neither the report nor standard error nor the logs contain anything private;
//! - reruns, on the same connection and a new one, report the same public key with `created`
//!   false and add no persistent handle;
//! - `--inspect` on an empty TPM reports the key absent and leaves the TPM exactly as it was;
//! - an EK certificate for another key ends with status 6 before the key is created, another
//!   object at the handle with status 5 and untouched, both with nothing on standard output; an
//!   EK certificate for the EK is reported `for_this_key`.
//!
//! Every TPM connection the run opens is its own `testkit::swtpm::Session`, which flushes what
//! it loaded when it is closed, as the kernel's resource manager does for a process. The
//! executable itself (arguments, no TPM device, devices other than `/dev/tpmrm<N>`) is tested in
//! `tests/binary.rs`. Without podman or docker the tests skip, unless `CI` is set, where they
//! fail.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

use std::io;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent::provision::{Command, Exit, parse_args, run};
use agent::tpm::{DEFAULT_KEY_HANDLE, TpmChannel, TpmTransport, endorsement_key};
use base64::Engine as _;
use protocol::provisioning::{EkCertificate, GatewayKey, ProvisioningReport};
use testkit::swtpm::{SRK_HANDLE, Session, Swtpm};
use tracing_subscriber::fmt::MakeWriter;
use x509_parser::certification_request::X509CertificationRequest;
use x509_parser::prelude::FromDer;

const KEY: u32 = DEFAULT_KEY_HANDLE;
const HARDWARE_ID: &str = "tg-0001";

/// How long a test waits for the TPM thread to close its session: milliseconds unless the code
/// is wrong.
const WAIT: Duration = Duration::from_secs(60);

/// One TPM connection of the run: a session that tells the test when it is closed.
struct Connection {
    session: Session,
    _closed: mpsc::Sender<()>,
}

impl TpmChannel for Connection {
    fn transact(&mut self, command: &[u8]) -> io::Result<Vec<u8>> {
        self.session.transact(command)
    }
}

/// Collects the run's log lines, as the executable writes them to standard error.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Logs {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// What a run printed.
struct Ran {
    exit: Exit,
    stdout: String,
    /// The run's standard error and its log lines (debug level and up).
    stderr: String,
}

impl Ran {
    fn report(&self) -> ProvisioningReport {
        assert_eq!(self.exit, Exit::Success, "stderr:\n{}", self.stderr);
        serde_json::from_str(&self.stdout).unwrap()
    }

    /// Fails unless the run ended with `exit` and printed nothing on standard output.
    fn failed(&self, exit: Exit) {
        assert_eq!(self.exit, exit, "stderr:\n{}", self.stderr);
        assert!(self.stdout.is_empty(), "{}", self.stdout);
    }
}

/// Runs `trustiot-agent provision <args>` with every TPM connection a new session of `swtpm`,
/// and waits until every session is closed.
fn provision(swtpm: &Swtpm, args: &[&str]) -> Ran {
    let Ok(Command::Provision(options)) = parse_args(args.iter().map(Into::into)) else {
        panic!("invalid arguments {args:?}")
    };
    let logs = Logs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    let mut closed = Vec::new();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit = tracing::subscriber::with_default(subscriber, || {
        let mut connect = || {
            let (sender, receiver) = mpsc::channel();
            closed.push(receiver);
            TpmTransport::Channel(Box::new(Connection {
                session: swtpm.session(),
                _closed: sender,
            }))
        };
        run(&options, &mut connect, &mut stdout, &mut stderr)
    });
    for receiver in closed {
        match receiver.recv_timeout(WAIT) {
            Err(RecvTimeoutError::Disconnected) => {}
            other => panic!("the TPM thread did not close its session within {WAIT:?}: {other:?}"),
        }
    }
    let mut stderr = String::from_utf8(stderr).unwrap();
    stderr.push_str(&String::from_utf8(logs.0.lock().unwrap().clone()).unwrap());
    Ran {
        exit,
        stdout: String::from_utf8(stdout).unwrap(),
        stderr,
    }
}

/// The DER of the one PEM block labelled `label` in `text`.
fn pem_der(text: &str, label: &str) -> Vec<u8> {
    let begin = format!("-----BEGIN {label}-----\n");
    let end = format!("-----END {label}-----\n");
    let body = text
        .strip_prefix(&begin)
        .and_then(|rest| rest.strip_suffix(&end))
        .unwrap_or_else(|| panic!("not one {label} block: {text}"));
    assert_eq!(text.matches("-----BEGIN").count(), 1, "{text}");
    let body: String = body
        .lines()
        .inspect(|line| assert!(line.len() <= 64, "{line}"))
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .unwrap()
}

fn hex(text: &str) -> Vec<u8> {
    text.trim()
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

/// The SubjectPublicKeyInfo of the object at `handle`, as tpm2-tools computes it.
fn tools_public_key(swtpm: &Swtpm, handle: u32) -> Vec<u8> {
    hex(&swtpm.tools(&format!(
        "tpm2_readpublic -Q -c {handle:#010x} -f der -o spki.der\nod -An -v -tx1 spki.der | tr -d ' \\n'"
    )))
}

/// The SubjectPublicKeyInfo of `tpm2_createek -G ecc`'s EK, as tpm2-tools computes it.
fn tools_ek(swtpm: &Swtpm) -> Vec<u8> {
    hex(&swtpm.tools(
        "tpm2_createek -Q -G ecc -c ek.ctx\n\
         tpm2_readpublic -Q -c ek.ctx -f der -o ek.der\n\
         tpm2_flushcontext -t\n\
         od -An -v -tx1 ek.der | tr -d ' \\n'",
    ))
}

/// The public key and the CSR of a report whose key is present, checked as the workstation
/// checks them: the CSR verifies with its own key (rcgen with its x509-parser and ring
/// features), which is the reported public key, and its subject is `CN=<hardware ID>` alone.
fn checked_key(report: &ProvisioningReport) -> (Vec<u8>, bool) {
    let GatewayKey::Present {
        created,
        public_key,
        certificate_request,
    } = &report.key
    else {
        panic!("the key is absent: {report:?}")
    };
    let public_key = pem_der(public_key, "PUBLIC KEY");
    let request = pem_der(certificate_request, "CERTIFICATE REQUEST");
    let params =
        rcgen::CertificateSigningRequestParams::from_der(&request.as_slice().into()).unwrap();
    assert_eq!(
        params.public_key.algorithm(),
        &rcgen::PKCS_ECDSA_P256_SHA256
    );
    assert_eq!(
        rcgen::PublicKeyData::subject_public_key_info(&params.public_key),
        public_key
    );
    let (rest, parsed) = X509CertificationRequest::from_der(&request).unwrap();
    assert!(rest.is_empty());
    let subject = &parsed.certification_request_info.subject;
    assert_eq!(subject.to_string(), format!("CN={HARDWARE_ID}"));
    assert_eq!(
        parsed.certification_request_info.subject_pki.raw,
        public_key
    );
    (public_key, *created)
}

/// The report with every PEM block replaced by its label, so a word search does not hit
/// base64 by chance; fails unless every PEM block is a public key, a CSR or a certificate.
fn without_pem_bodies(stdout: &str) -> String {
    fn strip(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) if text.starts_with("-----BEGIN ") => {
                let label = text
                    .lines()
                    .next()
                    .unwrap()
                    .trim_start_matches("-----BEGIN ")
                    .trim_end_matches("-----")
                    .to_owned();
                assert!(
                    ["PUBLIC KEY", "CERTIFICATE REQUEST", "CERTIFICATE"].contains(&label.as_str()),
                    "a PEM block labelled {label}"
                );
                let _ = pem_der(text, &label);
                *text = format!("<{label}>");
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(strip),
            serde_json::Value::Object(fields) => fields.values_mut().for_each(strip),
            _ => {}
        }
    }
    if stdout.is_empty() {
        return String::new();
    }
    let mut report: serde_json::Value = serde_json::from_str(stdout).unwrap();
    strip(&mut report);
    report.to_string()
}

/// Fails if the run printed anything that looks private (a private key, a secret, a password)
/// in the report (outside its public PEM blocks), on standard error or in the logs.
fn nothing_private(ran: &Ran) {
    for (name, text) in [
        ("stdout", without_pem_bodies(&ran.stdout)),
        ("stderr", ran.stderr.clone()),
    ] {
        let lower = text.to_lowercase();
        for word in ["private", "secret", "password"] {
            assert!(!lower.contains(word), "{name} contains {word:?}:\n{text}");
        }
    }
}

#[test]
fn the_report_has_a_verifying_csr_for_the_key_and_the_tpms_ek_and_reruns_reuse_the_key() {
    let Some(swtpm) = Swtpm::start() else {
        return;
    };
    swtpm.create_standard_srk();
    assert_eq!(swtpm.persistent_handles(), [SRK_HANDLE]);

    let first = provision(&swtpm, &["--hardware-id", HARDWARE_ID]);
    let report = first.report();
    nothing_private(&first);
    assert_eq!(report.hardware_id.as_str(), HARDWARE_ID);
    assert_eq!(report.key_handle.get(), KEY);
    let (public_key, created) = checked_key(&report);
    assert!(created);
    assert_eq!(public_key, tools_public_key(&swtpm, KEY));
    assert_eq!(swtpm.persistent_handles(), [SRK_HANDLE, KEY]);
    assert!(swtpm.transient_handles().is_empty());

    // The EK is endorsement_key's (Laysi/trustiot-v2#612) and tpm2-tools'.
    let endorsement_key_der = pem_der(&report.endorsement_key, "PUBLIC KEY");
    let (sender, _receiver) = mpsc::channel();
    let computed = endorsement_key(TpmTransport::Channel(Box::new(Connection {
        session: swtpm.session(),
        _closed: sender,
    })))
    .unwrap();
    assert_eq!(endorsement_key_der, computed.public_key_spki_der);
    assert_eq!(endorsement_key_der, tools_ek(&swtpm));
    assert_eq!(report.ek_certificate, EkCertificate::Absent);

    // Again, on the same connection and on a new one: the same key, not created, nothing added.
    let before = swtpm.snapshot();
    for new_connection in [false, true] {
        if new_connection {
            swtpm.disconnect();
        }
        let again = provision(&swtpm, &["--hardware-id", HARDWARE_ID]);
        nothing_private(&again);
        let again = again.report();
        let (again_key, created) = checked_key(&again);
        assert!(!created);
        assert_eq!(again_key, public_key);
        assert_eq!(again.endorsement_key, report.endorsement_key);
        assert_eq!(swtpm.snapshot(), before, "a rerun changed the TPM");
    }

    // --inspect reports the same key, also not created.
    let inspected = provision(&swtpm, &["--hardware-id", HARDWARE_ID, "--inspect"]);
    nothing_private(&inspected);
    let (inspected_key, created) = checked_key(&inspected.report());
    assert!(!created);
    assert_eq!(inspected_key, public_key);
    assert_eq!(swtpm.snapshot(), before, "--inspect changed the TPM");
}

#[test]
fn inspect_on_an_empty_tpm_reports_the_key_absent_and_changes_nothing() {
    let Some(swtpm) = Swtpm::start() else {
        return;
    };
    let before = swtpm.snapshot();
    let inspected = provision(&swtpm, &["--hardware-id", HARDWARE_ID, "--inspect"]);
    nothing_private(&inspected);
    let report = inspected.report();
    assert_eq!(report.key, GatewayKey::Absent);
    assert_eq!(
        pem_der(&report.endorsement_key, "PUBLIC KEY"),
        tools_ek(&swtpm)
    );
    assert!(swtpm.persistent_handles().is_empty());
    assert!(swtpm.transient_handles().is_empty());
    assert_eq!(swtpm.snapshot(), before, "--inspect changed the TPM");
}

/// A certificate made by rcgen for the P-256 key `spki`, signed by a throwaway CA.
fn certificate_for(spki: &[u8]) -> Vec<u8> {
    struct Public(Vec<u8>);
    impl rcgen::PublicKeyData for Public {
        fn der_bytes(&self) -> &[u8] {
            &self.0
        }
        fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
            &rcgen::PKCS_ECDSA_P256_SHA256
        }
    }
    let issuer_key = rcgen::KeyPair::generate().unwrap();
    let issuer_params = rcgen::CertificateParams::new(vec!["ek-ca.test".to_owned()]).unwrap();
    let issuer = rcgen::Issuer::new(issuer_params, issuer_key);
    let params = rcgen::CertificateParams::new(vec!["ek.manufacturer.test".to_owned()]).unwrap();
    // The point after the fixed 26-byte P-256 SubjectPublicKeyInfo prefix.
    params
        .signed_by(&Public(spki[26..].to_vec()), &issuer)
        .unwrap()
        .der()
        .to_vec()
}

/// Defines the EK certificate index in the platform hierarchy with the TCG profile's attributes,
/// as a manufacturer does, and writes `certificate` to it.
fn put_ek_certificate(swtpm: &Swtpm, certificate: &[u8]) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(certificate);
    let _ = swtpm.tools(&format!(
        "printf '%s' '{encoded}' | base64 -d > cert.der\n\
         tpm2_nvdefine 0x01c0000a -C p -a 'ppwrite|writedefine|ppread|ownerread|authread|no_da|platformcreate' -s {}\n\
         tpm2_nvwrite 0x01c0000a -C p -i cert.der",
        certificate.len()
    ));
}

#[test]
fn ek_certificates_and_other_objects_at_the_handle_end_with_their_status_and_no_report() {
    let Some(swtpm) = Swtpm::start() else {
        return;
    };
    swtpm.create_standard_srk();
    let ek = tools_ek(&swtpm);

    // A certificate for another key: status 6, before the gateway key is created.
    let other = rcgen::PublicKeyData::subject_public_key_info(&rcgen::KeyPair::generate().unwrap());
    put_ek_certificate(&swtpm, &certificate_for(&other));
    let before = swtpm.snapshot();
    let mismatch = provision(&swtpm, &["--hardware-id", HARDWARE_ID]);
    mismatch.failed(Exit::EkCertificate);
    nothing_private(&mismatch);
    assert!(
        mismatch
            .stderr
            .contains("is not for this TPM's endorsement key"),
        "{}",
        mismatch.stderr
    );
    assert_eq!(swtpm.snapshot(), before, "the TPM changed");
    assert_eq!(swtpm.persistent_handles(), [SRK_HANDLE]);

    // A certificate for the EK: reported, byte for byte, as for this key.
    let _ = swtpm.tools("tpm2_nvundefine 0x01c0000a -C p");
    let certificate = certificate_for(&ek);
    put_ek_certificate(&swtpm, &certificate);

    // A storage key at the handle: status 5, left as it was.
    let _ = swtpm.tools(&format!(
        "tpm2_createprimary -Q -C o -g sha256 -G ecc256:aes128cfb -c other.ctx\n\
         tpm2_evictcontrol -Q -C o -c other.ctx {KEY:#010x}\n\
         tpm2_flushcontext -t"
    ));
    let before = swtpm.snapshot();
    for args in [
        &["--hardware-id", HARDWARE_ID][..],
        &["--hardware-id", HARDWARE_ID, "--inspect"],
    ] {
        let taken = provision(&swtpm, args);
        taken.failed(Exit::HandleTaken);
        nothing_private(&taken);
        assert!(
            taken
                .stderr
                .contains("is not the gateway key and was left untouched"),
            "{}",
            taken.stderr
        );
        assert_eq!(swtpm.snapshot(), before, "the TPM changed: {args:?}");
    }

    let _ = swtpm.tools(&format!("tpm2_evictcontrol -Q -C o -c {KEY:#010x}"));
    let provisioned = provision(&swtpm, &["--hardware-id", HARDWARE_ID]);
    nothing_private(&provisioned);
    let report = provisioned.report();
    let (_, created) = checked_key(&report);
    assert!(created);
    let EkCertificate::ForThisKey { certificate: pem } = &report.ek_certificate else {
        panic!("the EK certificate is not reported: {report:?}")
    };
    assert_eq!(pem_der(pem, "CERTIFICATE"), certificate);
    let text = without_pem_bodies(&provisioned.stdout).to_lowercase();
    assert!(text.contains("\"for_this_key\""), "{text}");
    for word in ["verif", "trust", "valid", "genuine"] {
        assert!(!text.contains(word), "the report says {word:?}: {text}");
    }
}
