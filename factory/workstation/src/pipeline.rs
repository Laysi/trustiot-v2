//! `register --gateway`: one gateway from its TPM to its registration (decision 12,
//! Laysi/trustiot-v2#616). The steps, in order; each stops the run with its own exit code
//! ([`StopKind`], or the registration's own [`crate::register::StopKind`]):
//!
//! 1. Before the gateway is touched, the `register` command checks everything local: the
//!    hardware ID, the handle, the destination, the pinned root, the workstation key and
//!    certificate, the session and the journal ([`crate::journal`]).
//! 2. **Inspect**: `trustiot-agent provision --inspect` over ssh ([`crate::gateway`]) reports the
//!    key at the handle, or that there is none, and creates nothing.
//! 3. **Key policy**: a hardware ID this workstation has a record of whose TPM now has no key is a
//!    cleared TPM, which decision 12 makes a new device: the run stops before a key is created
//!    ([`StopKind::KeyAbsent`]) unless the operator gives `--allow-new-key`.
//! 4. **Provision**: when the key is absent, `trustiot-agent provision` creates it. When the
//!    inspection found it, its report already carries a CSR signed by that key (inspecting signs
//!    one too and changes nothing on the TPM), so no second run is made.
//! 5. **Checks of the report** ([`check_report`]): the reader's obligations of
//!    crates/protocol/README.md, "出廠佈建報告". Standard output is used only when the status is 0,
//!    at most [`MAX_REPORT_BYTES`] of it, and only when it parses as a report; the report must be
//!    for this workstation's hardware ID and handle (the registration always sends the
//!    workstation's own ID, never the report's); its CSR must name exactly that ID, carry exactly
//!    the report's public key and verify with it; the endorsement key must be an uncompressed ECC
//!    P-256 SubjectPublicKeyInfo, the same in both runs, and an EK certificate, when there is
//!    one, must carry exactly that key. A TPM without an EK certificate is accepted (see
//!    [`EK_CERTIFICATE_POLICY`]). The hashes the registration is checked against are computed
//!    from the report here ([`crate::register::Gateway::new`]).
//! 6. **Register**: [`crate::register::register`] (Laysi/trustiot-v2#615), which also refuses a
//!    public key this workstation recorded under another hardware ID.
//! 7. **Install** (Laysi/trustiot-v2#617, not yet): the chain and the root certificate go back to
//!    the gateway after the registration, in [`register_gateway`] after step 6.
//!
//! A `provisioned` line in the journal's log records what the gateway reported (key created or
//! reused, EK certificate present or absent, the uploaded agent's SHA-256); a `provision_stopped`
//! line records a stop of steps 2 to 5.

use std::fmt;

use chrono::{DateTime, Utc};
use protocol::GatewayId;
use protocol::provisioning::{
    EkCertificate, GatewayKey, KeyHandle, MAX_REPORT_BYTES, ProvisioningReport,
};
use tracing::info;
use x509_parser::certification_request::X509CertificationRequest;
use x509_parser::oid_registry::OID_X509_COMMON_NAME;
use x509_parser::prelude::{FromDer as _, X509Certificate};

use crate::Error;
use crate::gateway::{self, Mode, Ran, Request, SshError, SshGateway};
use crate::journal::{Event, Journal};
use crate::register::{self, Gateway, Registered, Registrar, one_pem_block};

/// The policy for a TPM without an EK certificate, as the tool states it.
pub const EK_CERTIFICATE_POLICY: &str = "A TPM without an EK certificate is accepted: nothing \
    checks an EK certificate's signature or its chain to a TPM manufacturer, and the \
    registration sends the endorsement key only, so its presence proves nothing about the TPM \
    (decision 12). The journal records that it was absent.";

/// The DER prefix of an ECC P-256 SubjectPublicKeyInfo with an uncompressed point:
/// `SEQUENCE { SEQUENCE { id-ecPublicKey, prime256v1 }, BIT STRING (66 bytes) 00 04 ... }`.
const P256_SPKI_PREFIX: [u8; 27] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00, 0x04,
];

/// The length of such a SubjectPublicKeyInfo: the prefix and two 32-byte coordinates.
const P256_SPKI_LEN: usize = 91;

/// How a `register --gateway` run stopped before the registration, each with its own exit code
/// (factory/workstation/README.md, "結束碼").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopKind {
    /// This workstation has a record of the hardware ID, but the TPM has no key at the handle (or
    /// the run created one): a cleared TPM, which is a new device. `--allow-new-key` goes on.
    KeyAbsent,
    /// The gateway's report broke a check of step 5; nothing was registered.
    ReportRefused,
    /// `trustiot-agent provision` (or the upload before it) ended with a failure on the gateway.
    ProvisionFailed,
    /// ssh could not reach the gateway within its retries.
    Unreachable,
}

impl StopKind {
    /// The process exit code.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::KeyAbsent => 20,
            Self::ReportRefused => 21,
            Self::ProvisionFailed => 22,
            Self::Unreachable => 23,
        }
    }

    /// The name in log lines.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::KeyAbsent => "key_absent",
            Self::ReportRefused => "report_refused",
            Self::ProvisionFailed => "provision_failed",
            Self::Unreachable => "gateway_unreachable",
        }
    }
}

/// A run that stopped before the registration: the kind (and exit code) and the message.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct Stopped {
    pub kind: StopKind,
    pub message: String,
}

/// Why a report is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReportProblem {
    #[error("its standard output is larger than {MAX_REPORT_BYTES} bytes")]
    TooLarge,
    #[error("its standard output is not a provisioning report of a version this tool reads")]
    NotAReport,
    #[error("it is for another hardware ID than the one given with --hardware-id")]
    HardwareId,
    #[error("it is for another key handle than the one asked for")]
    Handle,
    #[error("it reports no key after provision, which must create one")]
    NoKey,
    #[error("its public_key is not one PEM PUBLIC KEY block")]
    PublicKey,
    #[error("its certificate_request is not one PKCS #10 request in one PEM block")]
    Request,
    #[error("its CSR's subject is not exactly one common name equal to the hardware ID")]
    Subject,
    #[error("its CSR's public key is not byte for byte the report's public_key")]
    RequestKey,
    #[error("its CSR's signature does not verify with its public key")]
    RequestSignature,
    #[error(
        "its endorsement_key is not an uncompressed ECC P-256 SubjectPublicKeyInfo in one PEM block"
    )]
    EndorsementKey,
    #[error(
        "its EK certificate is not one certificate, or does not carry exactly the endorsement key"
    )]
    EkCertificate,
    #[error(
        "its endorsement key differs from the one the inspection reported: another TPM answered"
    )]
    EndorsementKeyChanged,
    #[error("its key differs from the one the inspection reported")]
    KeyChanged,
    #[error("its CSR or endorsement key is refused for the registration: {0}")]
    Input(&'static str),
}

/// Takes `ran`'s standard output as the report of `request`, or says why not; `ran` ended with
/// status 0 (anything else is never read).
///
/// # Errors
///
/// The first check of step 5 that fails.
pub fn check_report(ran: &Ran, request: &Request<'_>) -> Result<ProvisioningReport, ReportProblem> {
    if ran.too_long || ran.stdout.len() > MAX_REPORT_BYTES {
        return Err(ReportProblem::TooLarge);
    }
    let report: ProvisioningReport =
        serde_json::from_slice(&ran.stdout).map_err(|_| ReportProblem::NotAReport)?;
    if report.hardware_id != *request.hardware_id {
        return Err(ReportProblem::HardwareId);
    }
    if report.key_handle != request.handle {
        return Err(ReportProblem::Handle);
    }
    let endorsement_key = one_pem_block(report.endorsement_key.as_bytes(), "PUBLIC KEY")
        .ok_or(ReportProblem::EndorsementKey)?;
    if endorsement_key.len() != P256_SPKI_LEN || !endorsement_key.starts_with(&P256_SPKI_PREFIX) {
        return Err(ReportProblem::EndorsementKey);
    }
    if let EkCertificate::ForThisKey { certificate } = &report.ek_certificate {
        let der = one_pem_block(certificate.as_bytes(), "CERTIFICATE")
            .ok_or(ReportProblem::EkCertificate)?;
        match X509Certificate::from_der(&der) {
            Ok(([], parsed)) if parsed.public_key().raw == endorsement_key.as_slice() => {}
            _ => return Err(ReportProblem::EkCertificate),
        }
    }
    match &report.key {
        GatewayKey::Absent if request.mode == Mode::Inspect => {}
        GatewayKey::Absent => return Err(ReportProblem::NoKey),
        GatewayKey::Present {
            public_key,
            certificate_request,
            ..
        } => check_request(request.hardware_id, public_key, certificate_request)?,
    }
    Ok(report)
}

/// The CSR of a present key: one PEM request, subject exactly `CN=<hardware ID>`, its
/// SubjectPublicKeyInfo byte for byte the report's public key, its signature verifying with it.
fn check_request(
    hardware_id: &GatewayId,
    public_key_pem: &str,
    request_pem: &str,
) -> Result<(), ReportProblem> {
    let public_key =
        one_pem_block(public_key_pem.as_bytes(), "PUBLIC KEY").ok_or(ReportProblem::PublicKey)?;
    let der = one_pem_block(request_pem.as_bytes(), "CERTIFICATE REQUEST")
        .ok_or(ReportProblem::Request)?;
    let (rest, request) =
        X509CertificationRequest::from_der(&der).map_err(|_| ReportProblem::Request)?;
    if !rest.is_empty() {
        return Err(ReportProblem::Request);
    }
    let info = &request.certification_request_info;
    let mut attributes = info.subject.iter_rdn().flat_map(|rdn| rdn.iter());
    let common_name = match (attributes.next(), attributes.next()) {
        (Some(attribute), None) if attribute.attr_type() == &OID_X509_COMMON_NAME => {
            attribute.as_str().ok()
        }
        _ => None,
    };
    if common_name != Some(hardware_id.as_str()) {
        return Err(ReportProblem::Subject);
    }
    if info.subject_pki.raw != public_key.as_slice() {
        return Err(ReportProblem::RequestKey);
    }
    request
        .verify_signature()
        .map_err(|_| ReportProblem::RequestSignature)
}

/// What one run of `register --gateway` asks for.
#[derive(Clone, Debug)]
pub struct Plan {
    /// The hardware ID from the gateway's label.
    pub hardware_id: GatewayId,
    /// The key handle.
    pub handle: KeyHandle,
    /// `--allow-new-key`: go on when a known hardware ID's TPM has no key.
    pub allow_new_key: bool,
}

/// The gateway's report, checked, and the registration input made from it.
#[derive(Clone, Debug)]
pub struct Provisioned {
    /// The report the registration is made from.
    pub report: ProvisioningReport,
    /// The registration's input: the workstation's hardware ID with the report's CSR and
    /// endorsement key.
    pub gateway: Gateway,
    /// Whether this run created the key (as the last report says: a run whose answer was lost
    /// may have created it before).
    pub key_created: bool,
    /// Whether the TPM had an EK certificate.
    pub ek_certificate: bool,
}

/// A whole run: what the gateway reported and the registration.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub provisioned: Provisioned,
    pub registered: Registered,
}

fn time(now: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(now, 0).unwrap_or_default()
}

/// Steps 2 to 7 for `plan` on the gateway behind `ssh`: see the module documentation. `now`
/// gives Unix seconds.
///
/// # Errors
///
/// [`Error::Provisioning`] for a stop of steps 2 to 5, the registration's errors for step 6, and
/// any other error for a local problem (the journal, ssh missing).
pub async fn register_gateway(
    registrar: &Registrar<'_>,
    ssh: &SshGateway,
    plan: &Plan,
    now: impl Fn() -> i64,
) -> Result<Outcome, Error> {
    let provisioned = provision(registrar.journal, ssh, plan, &now).await?;
    let registered = register::register(registrar, &provisioned.gateway, &now).await?;
    // Laysi/trustiot-v2#617: install the chain and the root certificate on the gateway here, from
    // `registered.registration`, through `ssh`.
    Ok(Outcome {
        provisioned,
        registered,
    })
}

/// Steps 2 to 5: the checked report of the gateway's key and the registration input made from it.
///
/// # Errors
///
/// [`Error::Provisioning`] for a stop, logged in `journal`; any other error for a local problem.
pub async fn provision(
    journal: &Journal,
    ssh: &SshGateway,
    plan: &Plan,
    now: &impl Fn() -> i64,
) -> Result<Provisioned, Error> {
    let id = plan.hardware_id.as_str();
    journal.prepare()?;
    let known = journal.load(id)?.is_some();
    let stop = |kind: StopKind, message: String, event: Event| {
        logged(
            journal,
            event.with("stop", kind.as_str()),
            Stopped { kind, message },
        )
    };
    let stopped_event = || Event::new(time(now()), id, "provision_stopped");

    let inspect = Request {
        hardware_id: &plan.hardware_id,
        handle: plan.handle,
        mode: Mode::Inspect,
    };
    let inspected = run(journal, ssh, &inspect, now).await?;
    // A key at the handle: the inspection's report already carries its CSR, so it is used as is.
    let report = if matches!(inspected.key, GatewayKey::Present { .. }) {
        inspected
    } else {
        if known && !plan.allow_new_key {
            return Err(stop(
                StopKind::KeyAbsent,
                key_absent_message(journal, id, plan.handle, "has no key at"),
                stopped_event().with("handle", plan.handle.to_string()),
            ));
        }
        let request = Request {
            mode: Mode::Provision,
            ..inspect
        };
        let provisioned = run(journal, ssh, &request, now).await?;
        if provisioned.endorsement_key != inspected.endorsement_key {
            return Err(refused(
                journal,
                ssh,
                id,
                ReportProblem::EndorsementKeyChanged,
                now,
            ));
        }
        provisioned
    };
    let GatewayKey::Present {
        created,
        public_key: _,
        certificate_request,
    } = &report.key
    else {
        return Err(refused(journal, ssh, id, ReportProblem::NoKey, now));
    };
    if *created && known && !plan.allow_new_key {
        // The inspection found no key either, unless the TPM was cleared in between.
        return Err(stop(
            StopKind::KeyAbsent,
            key_absent_message(journal, id, plan.handle, "had a key created at"),
            stopped_event().with("handle", plan.handle.to_string()),
        ));
    }
    let gateway = Gateway::new(
        id,
        certificate_request.as_bytes(),
        report.endorsement_key.as_bytes(),
    )
    .map_err(|error| {
        let problem = match error {
            register::InputError::CertificateRequest(reason)
            | register::InputError::EndorsementKey(reason) => ReportProblem::Input(reason),
            register::InputError::HardwareId(_) => ReportProblem::HardwareId,
        };
        refused(journal, ssh, id, problem, now)
    })?;
    let ek_certificate = matches!(report.ek_certificate, EkCertificate::ForThisKey { .. });
    let created = *created;
    let mut event = Event::new(time(now()), id, "provisioned")
        .with("handle", plan.handle.to_string())
        .with("key_created", created)
        .with(
            "ek_certificate",
            if ek_certificate {
                "for_this_key"
            } else {
                "absent"
            },
        )
        .with("public_key_sha256", gateway.public_key_sha256())
        .with("endorsement_key_sha256", gateway.endorsement_key_sha256())
        .with("allow_new_key", plan.allow_new_key);
    if let Some(sum) = ssh.uploaded_sha256() {
        event = event.with("uploaded_agent_sha256", sum);
    }
    journal.append(&event)?;
    info!(
        hardware_id = id,
        key_created = created,
        ek_certificate,
        public_key_sha256 = gateway.public_key_sha256(),
        "the gateway reported its key"
    );
    if !ek_certificate {
        tracing::warn!(
            hardware_id = id,
            "the TPM has no EK certificate; accepted (see the README, \"EK 憑證\")"
        );
    }
    Ok(Provisioned {
        report,
        gateway,
        key_created: created,
        ek_certificate,
    })
}

/// One run on the gateway, its status and report checked.
async fn run(
    journal: &Journal,
    ssh: &SshGateway,
    request: &Request<'_>,
    now: &impl Fn() -> i64,
) -> Result<ProvisioningReport, Error> {
    let id = request.hardware_id.as_str();
    let mode = match request.mode {
        Mode::Inspect => "provision --inspect",
        Mode::Provision => "provision",
    };
    let ran = match ssh.provision(request).await {
        Ok(ran) => ran,
        Err(SshError::Unreachable { attempts, last }) => {
            let event = Event::new(time(now()), id, "provision_stopped")
                .with("step", mode)
                .with("attempts", attempts)
                .with("stop", StopKind::Unreachable.as_str());
            return Err(logged(
                journal,
                event,
                Stopped {
                    kind: StopKind::Unreachable,
                    message: format!(
                        "trustiot-agent {mode} on {}: could not reach the gateway through ssh \
                         after {attempts} attempt(s) (last: {last}). Nothing was registered; \
                         check the network, the destination and the ssh configuration (keys, \
                         host key), then run the same command again: it is safe",
                        ssh.destination()
                    ),
                },
            ));
        }
        Err(SshError::Spawn(error)) => {
            return Err(Error::Failed(format!(
                "cannot run {}: {error}; the tool needs the system's OpenSSH client",
                gateway::SSH_PROGRAM
            )));
        }
        Err(SshError::Local(error)) => return Err(error),
    };
    if ran.too_long {
        // The call was ended once the limit was passed, so its status says nothing: the output
        // alone breaks the reader's limit.
        return Err(refused(journal, ssh, id, ReportProblem::TooLarge, now));
    }
    if ran.status != Some(0) {
        let event = Event::new(time(now()), id, "provision_stopped")
            .with("step", mode)
            .with("status", ran.status.map_or(-1, i64::from))
            .with("stop", StopKind::ProvisionFailed.as_str());
        return Err(logged(
            journal,
            event,
            Stopped {
                kind: StopKind::ProvisionFailed,
                message: format!(
                    "trustiot-agent {mode} on {} ended with {}; its standard output is not used. \
                     The gateway said: {}. Nothing was registered",
                    ssh.destination(),
                    gateway::describe_status(ran.status),
                    gateway::excerpt(&ran.stderr)
                ),
            },
        ));
    }
    check_report(&ran, request).map_err(|problem| refused(journal, ssh, id, problem, now))
}

/// A refused report: logged, and the stop.
fn refused(
    journal: &Journal,
    ssh: &SshGateway,
    id: &str,
    problem: ReportProblem,
    now: &impl Fn() -> i64,
) -> Error {
    let event = Event::new(time(now()), id, "provision_stopped")
        .with("stop", StopKind::ReportRefused.as_str())
        .with("problem", problem.to_string());
    logged(
        journal,
        event,
        Stopped {
            kind: StopKind::ReportRefused,
            message: format!(
                "the provisioning report from {} is refused: {problem}. Nothing was registered. \
                 Check that the destination is the gateway with this label and that its agent is \
                 the released one; report it if it happens again",
                ssh.destination()
            ),
        },
    )
}

fn key_absent_message(journal: &Journal, id: &str, handle: KeyHandle, what: &str) -> String {
    format!(
        "this workstation has a record of hardware ID {id} ({}), but the gateway's TPM {what} \
         {handle}: the TPM was cleared, or this is another gateway. A cleared TPM is a new device \
         (decision 12): check the label; to go on with a new key all the same, run the command \
         again with --allow-new-key (the platform then refuses the hardware ID registered with \
         another key, exit code 10, and the gateway needs a new hardware ID)",
        journal.record_path(id).display()
    )
}

/// Logs `event`, then gives back the stop as an error. A log line that cannot be written is
/// added to the message; it never replaces the stop.
fn logged(journal: &Journal, event: Event, mut stopped: Stopped) -> Error {
    if let Err(log) = journal.append(&event) {
        stopped.message = format!(
            "{}; also, the log line could not be written: {log}",
            stopped.message
        );
    }
    Error::Provisioning(Box::new(stopped))
}

impl fmt::Display for StopKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use rcgen::{CertificateParams, DnType, KeyPair};

    use super::*;

    fn pem(label: &str, der: &[u8]) -> String {
        use base64::Engine as _;
        let body = base64::engine::general_purpose::STANDARD.encode(der);
        format!("-----BEGIN {label}-----\n{body}\n-----END {label}-----\n")
    }

    fn id() -> GatewayId {
        GatewayId::new("tg-0001").expect("an ID")
    }

    fn handle() -> KeyHandle {
        KeyHandle::new(0x8100_0100).expect("a handle")
    }

    struct Made {
        key: KeyPair,
        ek: KeyPair,
    }

    impl Made {
        fn new() -> Self {
            Self {
                key: KeyPair::generate().expect("a key"),
                ek: KeyPair::generate().expect("an EK"),
            }
        }

        fn request(&self, names: &[(DnType, &str)]) -> String {
            let mut params = CertificateParams::new(Vec::new()).expect("params");
            for (kind, value) in names {
                params.distinguished_name.push(kind.clone(), *value);
            }
            params
                .serialize_request(&self.key)
                .expect("a CSR")
                .pem()
                .expect("PEM")
        }

        fn report(&self) -> serde_json::Value {
            serde_json::json!({
                "version": 1,
                "hardware_id": "tg-0001",
                "key_handle": "0x81000100",
                "key": {
                    "state": "present",
                    "created": false,
                    "public_key": self.key.public_key_pem(),
                    "certificate_request": self.request(&[(DnType::CommonName, "tg-0001")]),
                },
                "endorsement_key": self.ek.public_key_pem(),
                "ek_certificate": {"state": "absent"},
            })
        }
    }

    fn ran(report: &serde_json::Value) -> Ran {
        Ran {
            status: Some(0),
            stdout: serde_json::to_vec_pretty(report).expect("JSON"),
            too_long: false,
            stderr: String::new(),
            attempts: 1,
        }
    }

    fn check(report: &serde_json::Value, mode: Mode) -> Result<ProvisioningReport, ReportProblem> {
        let hardware_id = id();
        check_report(
            &ran(report),
            &Request {
                hardware_id: &hardware_id,
                handle: handle(),
                mode,
            },
        )
    }

    #[test]
    fn a_good_report_passes_and_an_absent_key_only_on_inspection() {
        let made = Made::new();
        let report = made.report();
        assert!(check(&report, Mode::Provision).is_ok());
        let mut absent = report.clone();
        absent["key"] = serde_json::json!({"state": "absent"});
        assert!(check(&absent, Mode::Inspect).is_ok());
        assert_eq!(
            check(&absent, Mode::Provision).unwrap_err(),
            ReportProblem::NoKey
        );
    }

    #[test]
    fn every_reader_obligation_is_enforced() {
        let made = Made::new();
        let other = Made::new();
        let base = made.report();
        let cases: Vec<(&str, serde_json::Value, ReportProblem)> = vec![
            (
                "other ID",
                {
                    let mut r = base.clone();
                    r["hardware_id"] = "tg-0002".into();
                    r
                },
                ReportProblem::HardwareId,
            ),
            (
                "other handle",
                {
                    let mut r = base.clone();
                    r["key_handle"] = "0x81000101".into();
                    r
                },
                ReportProblem::Handle,
            ),
            (
                "CN of another ID",
                {
                    let mut r = base.clone();
                    r["key"]["certificate_request"] =
                        made.request(&[(DnType::CommonName, "tg-0002")]).into();
                    r
                },
                ReportProblem::Subject,
            ),
            (
                "CN and O",
                {
                    let mut r = base.clone();
                    r["key"]["certificate_request"] = made
                        .request(&[
                            (DnType::CommonName, "tg-0001"),
                            (DnType::OrganizationName, "x"),
                        ])
                        .into();
                    r
                },
                ReportProblem::Subject,
            ),
            (
                "no subject",
                {
                    let mut r = base.clone();
                    r["key"]["certificate_request"] = made.request(&[]).into();
                    r
                },
                ReportProblem::Subject,
            ),
            (
                "CSR of another key",
                {
                    let mut r = base.clone();
                    r["key"]["certificate_request"] =
                        other.request(&[(DnType::CommonName, "tg-0001")]).into();
                    r
                },
                ReportProblem::RequestKey,
            ),
            (
                "public key not PEM",
                {
                    let mut r = base.clone();
                    r["key"]["public_key"] = "KEY".into();
                    r
                },
                ReportProblem::PublicKey,
            ),
            (
                "EK not P-256",
                {
                    let mut r = base.clone();
                    r["endorsement_key"] = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384)
                        .expect("a P-384 key")
                        .public_key_pem()
                        .into();
                    r
                },
                ReportProblem::EndorsementKey,
            ),
            (
                "EK certificate of another key",
                {
                    let mut r = base.clone();
                    let mut params = CertificateParams::new(Vec::new()).expect("params");
                    params.distinguished_name.push(DnType::CommonName, "EK");
                    let certificate = params.self_signed(&other.ek).expect("a certificate").pem();
                    r["ek_certificate"] =
                        serde_json::json!({"state": "for_this_key", "certificate": certificate});
                    r
                },
                ReportProblem::EkCertificate,
            ),
            (
                "help text",
                serde_json::json!("usage: trustiot-agent provision"),
                ReportProblem::NotAReport,
            ),
            (
                "version 2",
                {
                    let mut r = base.clone();
                    r["version"] = 2.into();
                    r
                },
                ReportProblem::NotAReport,
            ),
        ];
        for (name, report, problem) in cases {
            assert_eq!(
                check(&report, Mode::Provision).unwrap_err(),
                problem,
                "{name}"
            );
        }

        let mut tampered = ran(&base);
        let text = String::from_utf8(tampered.stdout.clone()).expect("UTF-8");
        // A CSR whose signature does not verify: flip one bit of its signature's last byte.
        let csr_pem = base["key"]["certificate_request"].as_str().expect("a CSR");
        let mut der = one_pem_block(csr_pem.as_bytes(), "CERTIFICATE REQUEST").expect("DER");
        let last = der.last_mut().expect("a byte");
        *last ^= 1;
        let broken = pem("CERTIFICATE REQUEST", &der);
        let replaced = text.replace(
            &serde_json::to_string(csr_pem).expect("JSON"),
            &serde_json::to_string(&broken).expect("JSON"),
        );
        assert_ne!(replaced, text);
        tampered.stdout = replaced.into_bytes();
        let hardware_id = id();
        let request = Request {
            hardware_id: &hardware_id,
            handle: handle(),
            mode: Mode::Provision,
        };
        assert_eq!(
            check_report(&tampered, &request).unwrap_err(),
            ReportProblem::RequestSignature
        );

        let mut long = ran(&base);
        long.too_long = true;
        assert_eq!(
            check_report(&long, &request).unwrap_err(),
            ReportProblem::TooLarge
        );
    }

    #[test]
    fn an_ek_certificate_for_the_ek_passes() {
        let made = Made::new();
        let mut report = made.report();
        let mut params = CertificateParams::new(Vec::new()).expect("params");
        params.distinguished_name.push(DnType::CommonName, "EK");
        let certificate = params.self_signed(&made.ek).expect("a certificate").pem();
        report["ek_certificate"] =
            serde_json::json!({"state": "for_this_key", "certificate": certificate});
        assert!(check(&report, Mode::Provision).is_ok());
    }

    #[test]
    fn stop_kinds_have_their_own_exit_codes() {
        let codes: Vec<u8> = [
            StopKind::KeyAbsent,
            StopKind::ReportRefused,
            StopKind::ProvisionFailed,
            StopKind::Unreachable,
        ]
        .iter()
        .map(|kind| kind.exit_code())
        .collect();
        assert_eq!(codes, [20, 21, 22, 23]);
    }
}
