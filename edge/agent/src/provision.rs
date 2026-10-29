//! `trustiot-agent provision`, the gateway side of the factory registration (decision 12,
//! Laysi/trustiot-v2#613): the production workstation (Laysi/trustiot-v2#616) runs it over ssh,
//! reads the JSON report it prints, and registers the gateway itself. The workstation's
//! credentials never reach the gateway and this code has no network code: it only talks to the
//! TPM and prints.
//!
//! ```text
//! trustiot-agent provision --hardware-id <id> [--handle <hex>] [--device <path>] [--inspect]
//! ```
//!
//! 1. The TPM's endorsement key and its certificate ([`tpm::endorsement_key`]), first, so a TPM
//!    whose EK certificate is not for its EK fails before the gateway key is created.
//! 2. The gateway key: [`tpm::ensure_key`] reuses the key at the handle, creates it where the
//!    handle is empty, or refuses another object there; with `--inspect`, [`tpm::inspect_key`]
//!    only looks and reports an empty handle as absent.
//! 3. A PKCS #10 CSR for `CN=<hardware ID>` signed by the key in the TPM (ecdsa-with-SHA256, made
//!    with rcgen without its crypto backends), checked here before it is reported: it parses,
//!    carries exactly the key's SubjectPublicKeyInfo and the hardware ID, and its signature
//!    verifies with the key's public key.
//! 4. One JSON object, [`protocol::provisioning::ProvisioningReport`], on standard output, only
//!    when everything above succeeded; diagnostics go to standard error. The report holds public
//!    data only: a TPM key's private part never leaves the TPM.
//!
//! Each outcome has its own exit status ([`Exit`]). The device must be the kernel's resource
//! manager (`/dev/tpmrm<N>`, [`tpm::is_resource_manager`]); anything else is refused before it is
//! opened. The hardware ID and the handle come from the arguments only, never from standard
//! input or a shell.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;

use protocol::GatewayId;
use protocol::provisioning::{
    EkCertificate, GatewayKey, KeyHandle, ProvisioningReport, ReportVersion,
};
use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
use x509_parser::certification_request::X509CertificationRequest;
use x509_parser::oid_registry::OID_SIG_ECDSA_WITH_SHA256;
use x509_parser::prelude::FromDer;

use crate::tpm::{
    self, EndorsementError, ProvisionError, TpmError, TpmKey, TpmTransport, is_resource_manager,
};

/// The subcommand's name, the first argument.
pub const SUBCOMMAND: &str = "provision";

/// The subcommand's help.
pub const USAGE: &str = "usage: trustiot-agent provision --hardware-id <id> [--handle <hex>] [--device <path>] [--inspect]

Makes sure the TPM holds the gateway key, makes a certificate signing request for it, reads the
TPM's endorsement key, and prints a JSON report of public data on standard output.

  --hardware-id <id>  the gateway's hardware ID: the CSR's common name (required)
  --handle <hex>      the persistent handle of the gateway key (default 0x81000100)
  --device <path>     the TPM resource manager device (default /dev/tpmrm0; only /dev/tpmrm<N>)
  --inspect           create nothing: report an empty handle as absent
  --help              print this help

Exit status: 0 report printed; 1 other failure; 2 invalid arguments; 3 no TPM device;
4 not a TPM resource manager device; 5 another object at the handle; 6 the EK certificate is not
for the EK or cannot be read; 7 an EK setup that is not supported (EK template or nonce in NV,
endorsement authorization). Nothing is printed on standard output unless the status is 0.";

/// How the subcommand ends: its exit status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// 0: the report was printed.
    Success,
    /// 1: anything not listed below, such as a TPM error or a CSR that does not check out.
    Failure,
    /// 2: invalid arguments: an unknown option, a hardware ID that is not a `GatewayId`, a
    /// handle that cannot hold the gateway key.
    Usage,
    /// 3: the TPM device does not exist (or has no TPM behind it).
    NoDevice,
    /// 4: the device is not `/dev/tpmrm<N>`; it was not opened.
    NotResourceManager,
    /// 5: the handle holds an object that is not the gateway key; it was left untouched.
    HandleTaken,
    /// 6: the EK certificate in NV is not for the EK, is not a certificate, or cannot be read.
    EkCertificate,
    /// 7: the TPM defines an EK template or nonce, or the endorsement hierarchy has an
    /// authorization value.
    EkUnsupported,
}

impl Exit {
    /// The process exit status.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Failure => 1,
            Self::Usage => 2,
            Self::NoDevice => 3,
            Self::NotResourceManager => 4,
            Self::HandleTaken => 5,
            Self::EkCertificate => 6,
            Self::EkUnsupported => 7,
        }
    }
}

/// The checked arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The CSR's common name.
    pub hardware_id: GatewayId,
    /// Where the gateway key is (an owner persistent handle other than the SRK's).
    pub handle: KeyHandle,
    /// The TPM device; checked by [`run`], before anything is opened.
    pub device: PathBuf,
    /// Create nothing.
    pub inspect: bool,
}

/// What the arguments ask for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Run with these options.
    Provision(Options),
    /// Print [`USAGE`].
    Help,
}

/// Parses the arguments after `provision`. Every option may be given once.
///
/// # Errors
///
/// A message naming the argument that is unknown, repeated, missing or invalid: then the
/// status is [`Exit::Usage`].
pub fn parse_args(mut args: impl Iterator<Item = OsString>) -> Result<Command, String> {
    let mut hardware_id = None;
    let mut handle = None;
    let mut device = None;
    let mut inspect = false;
    while let Some(arg) = args.next() {
        let name = arg
            .to_str()
            .ok_or_else(|| format!("unknown argument {arg:?}"))?;
        match name {
            "--hardware-id" => {
                let value = value(name, args.next())?;
                let id =
                    GatewayId::new(value).map_err(|error| format!("--hardware-id: {error}"))?;
                once(name, &mut hardware_id, id)?;
            }
            "--handle" => {
                let value = value(name, args.next())?;
                let parsed: KeyHandle = value
                    .parse()
                    .map_err(|error| format!("--handle: {error}"))?;
                if !tpm::is_key_handle(parsed.get()) {
                    return Err(format!(
                        "--handle: {parsed} cannot hold the gateway key: it must be an owner persistent handle (0x81000000 to 0x817fffff) other than the storage root key's ({:#010x})",
                        tpm::SRK_HANDLE
                    ));
                }
                once(name, &mut handle, parsed)?;
            }
            "--device" => {
                let path = PathBuf::from(args.next().ok_or("--device needs a path")?);
                once(name, &mut device, path)?;
            }
            "--inspect" => {
                if inspect {
                    return Err("--inspect is given twice".to_owned());
                }
                inspect = true;
            }
            "--help" | "-h" => return Ok(Command::Help),
            _ => return Err(format!("unknown argument {arg:?}")),
        }
    }
    let hardware_id = hardware_id.ok_or("--hardware-id is required")?;
    let handle = match handle {
        Some(handle) => handle,
        None => KeyHandle::new(tpm::DEFAULT_KEY_HANDLE).map_err(|error| error.to_string())?,
    };
    Ok(Command::Provision(Options {
        hardware_id,
        handle,
        device: device.unwrap_or_else(|| PathBuf::from(tpm::DEFAULT_DEVICE)),
        inspect,
    }))
}

/// The UTF-8 value after option `name`.
fn value(name: &str, value: Option<OsString>) -> Result<String, String> {
    value
        .ok_or_else(|| format!("{name} needs a value"))?
        .into_string()
        .map_err(|value| format!("{name}: {value:?} is not UTF-8"))
}

fn once<T>(name: &str, slot: &mut Option<T>, value: T) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("{name} is given twice"));
    }
    *slot = Some(value);
    Ok(())
}

/// Why no report was printed.
#[derive(Debug, thiserror::Error)]
pub enum Failure {
    /// The device is not the resource manager, or cannot be opened.
    #[error(transparent)]
    Device(TpmError),
    /// The gateway key could not be provisioned or inspected.
    #[error(transparent)]
    Key(#[from] ProvisionError),
    /// The endorsement key or its certificate could not be read.
    #[error(transparent)]
    Endorsement(#[from] EndorsementError),
    /// The certificate signing request could not be made, or does not check out.
    #[error("the certificate signing request {0}")]
    Request(String),
    /// The report could not be encoded.
    #[error("cannot encode the report: {0}")]
    Encode(#[source] serde_json::Error),
}

impl Failure {
    /// The exit status for this failure.
    #[must_use]
    pub fn exit(&self) -> Exit {
        match self {
            Self::Device(error)
            | Self::Key(ProvisionError::Tpm(error))
            | Self::Endorsement(EndorsementError::Tpm(error)) => tpm_exit(error),
            Self::Key(ProvisionError::UnsupportedHandle(_)) => Exit::Usage,
            Self::Key(ProvisionError::Refused { .. }) => Exit::HandleTaken,
            Self::Endorsement(
                EndorsementError::CertificateMismatch { .. }
                | EndorsementError::CertificateInvalid { .. }
                | EndorsementError::CertificateIndex(_),
            ) => Exit::EkCertificate,
            Self::Endorsement(
                EndorsementError::CustomTemplate { .. }
                | EndorsementError::EndorsementAuthorization(_),
            ) => Exit::EkUnsupported,
            Self::Endorsement(EndorsementError::Randomness)
            | Self::Request(_)
            | Self::Encode(_) => Exit::Failure,
        }
    }
}

fn tpm_exit(error: &TpmError) -> Exit {
    match error {
        TpmError::NotResourceManager(_) | TpmError::NotResourceManagerDevice { .. } => {
            Exit::NotResourceManager
        }
        TpmError::Open { source, .. } if is_no_device(source) => Exit::NoDevice,
        _ => Exit::Failure,
    }
}

/// The device node is missing, or the driver behind it has no TPM.
fn is_no_device(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
        || matches!(error.raw_os_error(), Some(libc::ENODEV | libc::ENXIO))
}

/// Runs the subcommand: builds the report with TPM connections from `connect` (each step opens
/// its own, as a separate process would) and prints it on `stdout`, or prints why not on
/// `stderr`. `connect` is not called when `options.device` is not the resource manager. On the
/// gateway, `connect` opens `options.device`; tests pass a software TPM.
pub fn run(
    options: &Options,
    connect: &mut dyn FnMut() -> TpmTransport,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Exit {
    let printed = report(options, connect).and_then(|report| {
        let mut text = serde_json::to_string_pretty(&report).map_err(Failure::Encode)?;
        text.push('\n');
        Ok(text)
    });
    match printed {
        Ok(text) => match stdout
            .write_all(text.as_bytes())
            .and_then(|()| stdout.flush())
        {
            Ok(()) => Exit::Success,
            Err(error) => {
                let _ = writeln!(
                    stderr,
                    "trustiot-agent provision: cannot write the report: {error}"
                );
                Exit::Failure
            }
        },
        Err(failure) => {
            let _ = writeln!(stderr, "trustiot-agent provision: {failure}");
            failure.exit()
        }
    }
}

/// The report, as the module documentation describes.
fn report(
    options: &Options,
    connect: &mut dyn FnMut() -> TpmTransport,
) -> Result<ProvisioningReport, Failure> {
    if !is_resource_manager(&options.device) {
        return Err(Failure::Device(TpmError::NotResourceManager(
            options.device.clone(),
        )));
    }
    let handle = options.handle.get();
    let endorsement = tpm::endorsement_key(connect())?;
    let key = if options.inspect {
        tpm::inspect_key(connect(), handle)?.map(|key| (key, false))
    } else {
        let provisioned = tpm::ensure_key(connect(), handle)?;
        Some((provisioned.key, provisioned.created))
    };
    let key = match key {
        None => GatewayKey::Absent,
        Some((key, created)) => {
            let request = certificate_request(&key, &options.hardware_id)?;
            GatewayKey::Present {
                created,
                public_key: pem("PUBLIC KEY", key.public_key_spki_der()),
                certificate_request: pem("CERTIFICATE REQUEST", &request),
            }
        }
    };
    let ek_certificate = match endorsement.certificate {
        tpm::EkCertificate::Absent => EkCertificate::Absent,
        tpm::EkCertificate::Matches(der) => EkCertificate::ForThisKey {
            certificate: pem("CERTIFICATE", &der),
        },
    };
    Ok(ProvisioningReport {
        version: ReportVersion,
        hardware_id: options.hardware_id.clone(),
        key_handle: options.handle,
        key,
        endorsement_key: pem("PUBLIC KEY", &endorsement.public_key_spki_der),
        ek_certificate,
    })
}

/// The DER prefix of every ECDSA P-256 SubjectPublicKeyInfo with an uncompressed point:
/// `SEQUENCE { SEQUENCE { id-ecPublicKey, prime256v1 }, BIT STRING { 0x04 ...`, then the 64
/// bytes of the point's coordinates.
const P256_SPKI_PREFIX: [u8; 27] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00, 0x04,
];

/// Signs a message with ECDSA P-256 over its SHA-256 digest and returns the DER signature, or
/// why it could not.
type SignFn<'a> = dyn Fn(&[u8]) -> Result<Vec<u8>, String> + 'a;

/// A key that signs as rcgen asks: the uncompressed P-256 point, and a signing function that
/// returns DER ECDSA signatures over SHA-256.
struct RequestSigner<'a> {
    point: &'a [u8],
    sign: &'a SignFn<'a>,
    /// Why the last signature failed: rcgen's error has no room for it.
    failed: std::cell::RefCell<Option<String>>,
}

impl rcgen::PublicKeyData for RequestSigner<'_> {
    fn der_bytes(&self) -> &[u8] {
        self.point
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

impl rcgen::SigningKey for RequestSigner<'_> {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        (self.sign)(message).map_err(|error| {
            *self.failed.borrow_mut() = Some(error);
            rcgen::Error::RemoteKeyError
        })
    }
}

/// A DER CSR for `CN=<hardware_id>` and the TPM key, signed in the TPM and checked.
fn certificate_request(key: &TpmKey, hardware_id: &GatewayId) -> Result<Vec<u8>, Failure> {
    let sign = |message: &[u8]| key.sign(message).map_err(|error| error.to_string());
    signed_request(key.public_key_spki_der(), &sign, hardware_id)
}

/// A DER CSR for `CN=<hardware_id>` and the P-256 key `spki`, signed with `sign`, and checked
/// with [`check_request`].
fn signed_request(
    spki: &[u8],
    sign: &SignFn<'_>,
    hardware_id: &GatewayId,
) -> Result<Vec<u8>, Failure> {
    let point = spki
        .strip_prefix(&P256_SPKI_PREFIX[..P256_SPKI_PREFIX.len() - 1])
        .filter(|point| point.len() == 65)
        .ok_or_else(|| Failure::Request("cannot be made: the key is not ECDSA P-256".to_owned()))?;
    let signer = RequestSigner {
        point,
        sign,
        failed: std::cell::RefCell::new(None),
    };
    let mut params = rcgen::CertificateParams::default();
    let mut name = rcgen::DistinguishedName::new();
    name.push(rcgen::DnType::CommonName, hardware_id.as_str());
    params.distinguished_name = name;
    let request = params.serialize_request(&signer).map_err(|error| {
        let reason = signer
            .failed
            .borrow_mut()
            .take()
            .unwrap_or_else(|| error.to_string());
        Failure::Request(format!("cannot be signed: {reason}"))
    })?;
    let der = request.der().to_vec();
    check_request(&der, spki, hardware_id).map_err(Failure::Request)?;
    Ok(der)
}

/// Fails unless `der` is one PKCS #10 request whose signature algorithm is ecdsa-with-SHA256,
/// whose subject is exactly `CN=<hardware_id>`, whose SubjectPublicKeyInfo is byte for byte
/// `spki`, and whose signature verifies with that key.
fn check_request(der: &[u8], spki: &[u8], hardware_id: &GatewayId) -> Result<(), String> {
    let (rest, request) = X509CertificationRequest::from_der(der)
        .map_err(|error| format!("does not parse: {error}"))?;
    if !rest.is_empty() {
        return Err("has bytes after it".to_owned());
    }
    if request.signature_algorithm.algorithm != OID_SIG_ECDSA_WITH_SHA256
        || request.signature_algorithm.parameters.is_some()
    {
        return Err("is not signed with ecdsa-with-SHA256".to_owned());
    }
    let info = &request.certification_request_info;
    if info.subject_pki.raw != spki {
        return Err("is not for the gateway key".to_owned());
    }
    let names: Vec<&str> = info
        .subject
        .iter_common_name()
        .map(|name| name.as_str().unwrap_or_default())
        .collect();
    if names != [hardware_id.as_str()] || info.subject.iter().count() != 1 {
        return Err(format!(
            "does not have the subject CN={hardware_id} and nothing else"
        ));
    }
    let point = spki
        .get(P256_SPKI_PREFIX.len() - 1..)
        .ok_or("cannot be checked: the key is not ECDSA P-256")?;
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, point)
        .verify(info.raw, request.signature_value.data.as_ref())
        .map_err(|_| "has a signature that does not verify with the gateway key".to_owned())
}

/// `der` as PEM with `label`: base64 in lines of 64 characters, each line ending in `\n`.
fn pem(label: &str, der: &[u8]) -> String {
    use base64::Engine as _;
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let mut text = format!("-----BEGIN {label}-----\n");
    for line in body.as_bytes().chunks(64) {
        text.push_str(std::str::from_utf8(line).unwrap_or_default());
        text.push('\n');
    }
    text.push_str(&format!("-----END {label}-----\n"));
    text
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};

    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(OsString::from))
    }

    fn gateway() -> GatewayId {
        GatewayId::new("tg-0001").unwrap()
    }

    #[test]
    fn arguments_come_from_argv_only_and_are_checked() {
        let Ok(Command::Provision(options)) = parse(&["--hardware-id", "tg-0001"]) else {
            panic!("the hardware ID alone is enough")
        };
        assert_eq!(
            options,
            Options {
                hardware_id: gateway(),
                handle: KeyHandle::new(0x8100_0100).unwrap(),
                device: PathBuf::from("/dev/tpmrm0"),
                inspect: false,
            }
        );
        let Ok(Command::Provision(options)) = parse(&[
            "--inspect",
            "--handle",
            "0x81000200",
            "--device",
            "/dev/tpmrm1",
            "--hardware-id",
            "gw.factory_7-a",
        ]) else {
            panic!("every option")
        };
        assert_eq!(options.handle.get(), 0x8100_0200);
        assert_eq!(options.device, Path::new("/dev/tpmrm1"));
        assert_eq!(options.hardware_id.as_str(), "gw.factory_7-a");
        assert!(options.inspect);
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        // The device is checked by `run`, with its own exit status.
        let Ok(Command::Provision(options)) =
            parse(&["--hardware-id", "a", "--device", "/dev/tpm0"])
        else {
            panic!("--device is any path here")
        };
        assert_eq!(options.device, Path::new("/dev/tpm0"));

        let long = "a".repeat(65);
        for refused in [
            &[][..],
            &["--inspect"],
            &["--hardware-id"],
            &["--hardware-id", ""],
            &["--hardware-id", "tg/0001"],
            &["--hardware-id", "tg 0001"],
            &["--hardware-id", "-tg"],
            &["--hardware-id", "tg-0001\n"],
            &["--hardware-id", &long],
            &["--hardware-id", "a", "--hardware-id", "b"],
            &["--hardware-id", "a", "--inspect", "--inspect"],
            &["--hardware-id", "a", "--handle"],
            &["--hardware-id", "a", "--handle", "81000100"],
            &["--hardware-id", "a", "--handle", "0x81000001"],
            &["--hardware-id", "a", "--handle", "0x81800000"],
            &["--hardware-id", "a", "--handle", "0x80000000"],
            &[
                "--hardware-id",
                "a",
                "--handle",
                "0x81000100",
                "--handle",
                "0x81000100",
            ],
            &["--hardware-id", "a", "--device"],
            &["--hardware-id", "a", "--verbose"],
            &["--hardware-id=a"],
            &["tg-0001"],
        ] {
            assert!(parse(refused).is_err(), "{refused:?}");
        }
        let error = parse(&["--hardware-id", "a", "--handle", "0x81000001"]).unwrap_err();
        assert!(error.contains("storage root key"), "{error}");
    }

    #[test]
    fn failures_have_their_exit_status() {
        let not_found = || TpmError::Open {
            path: "/dev/tpmrm0".into(),
            source: io::Error::from(io::ErrorKind::NotFound),
        };
        let cases: Vec<(Failure, Exit)> = vec![
            (Failure::Device(not_found()), Exit::NoDevice),
            (
                Failure::Key(ProvisionError::Tpm(not_found())),
                Exit::NoDevice,
            ),
            (
                Failure::Endorsement(EndorsementError::Tpm(TpmError::Open {
                    path: "/dev/tpmrm0".into(),
                    source: io::Error::from_raw_os_error(libc::ENODEV),
                })),
                Exit::NoDevice,
            ),
            (
                Failure::Device(TpmError::Open {
                    path: "/dev/tpmrm0".into(),
                    source: io::Error::from(io::ErrorKind::PermissionDenied),
                }),
                Exit::Failure,
            ),
            (
                Failure::Device(TpmError::NotResourceManager("/dev/tpm0".into())),
                Exit::NotResourceManager,
            ),
            (
                Failure::Endorsement(EndorsementError::Tpm(TpmError::NotResourceManagerDevice {
                    path: "/dev/tpmrm0".into(),
                    found: "device 10:224 is ../../devices/virtual/misc/tpm0".into(),
                })),
                Exit::NotResourceManager,
            ),
            (
                Failure::Key(ProvisionError::Refused {
                    handle: 0x8100_0100,
                    reason: TpmError::SelfTestFailed,
                }),
                Exit::HandleTaken,
            ),
            (
                Failure::Key(ProvisionError::UnsupportedHandle(0x8100_0001)),
                Exit::Usage,
            ),
            (
                Failure::Endorsement(EndorsementError::CertificateIndex("x".to_owned())),
                Exit::EkCertificate,
            ),
            (
                Failure::Endorsement(EndorsementError::CertificateMismatch {
                    endorsement_key: Vec::new(),
                    certificate: Vec::new(),
                    reason: "x".to_owned(),
                }),
                Exit::EkCertificate,
            ),
            (
                Failure::Endorsement(EndorsementError::CustomTemplate {
                    index: 0x01c0_000c,
                    what: "template",
                }),
                Exit::EkUnsupported,
            ),
            (
                Failure::Endorsement(EndorsementError::Tpm(TpmError::Timeout(
                    std::time::Duration::from_secs(1),
                ))),
                Exit::Failure,
            ),
            (Failure::Request("x".to_owned()), Exit::Failure),
        ];
        for (failure, exit) in cases {
            assert_eq!(failure.exit(), exit, "{failure}");
        }
        let codes: Vec<u8> = [
            Exit::Success,
            Exit::Failure,
            Exit::Usage,
            Exit::NoDevice,
            Exit::NotResourceManager,
            Exit::HandleTaken,
            Exit::EkCertificate,
            Exit::EkUnsupported,
        ]
        .iter()
        .map(|exit| exit.code())
        .collect();
        assert_eq!(codes, [0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn a_device_other_than_the_resource_manager_ends_the_run_before_any_connection() {
        for device in ["/dev/tpm0", "/dev/null", "/tmp/tpmrm0"] {
            let options = Options {
                hardware_id: gateway(),
                handle: KeyHandle::new(0x8100_0100).unwrap(),
                device: device.into(),
                inspect: true,
            };
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let exit = run(
                &options,
                &mut || panic!("no TPM connection may be opened"),
                &mut stdout,
                &mut stderr,
            );
            assert_eq!(exit, Exit::NotResourceManager);
            assert!(stdout.is_empty());
            let stderr = String::from_utf8(stderr).unwrap();
            assert!(
                stderr.contains(device) && stderr.contains("resource manager"),
                "{stderr}"
            );
        }
    }

    /// A software P-256 key: its SubjectPublicKeyInfo and a signing function.
    fn software_key() -> (Vec<u8>, Box<SignFn<'static>>) {
        let random = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &random).unwrap();
        let key =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &random)
                .unwrap();
        let mut spki = P256_SPKI_PREFIX[..P256_SPKI_PREFIX.len() - 1].to_vec();
        spki.extend_from_slice(key.public_key().as_ref());
        let sign = move |message: &[u8]| {
            key.sign(&SystemRandom::new(), message)
                .map(|signature| signature.as_ref().to_vec())
                .map_err(|_| "the software key failed".to_owned())
        };
        (spki, Box::new(sign))
    }

    #[test]
    fn the_request_names_the_hardware_id_carries_the_key_and_verifies() {
        let (spki, sign) = software_key();
        let der = signed_request(&spki, &sign, &gateway()).unwrap();
        // rcgen with its crypto backend, as the workstation reads it, agrees.
        let params =
            rcgen::CertificateSigningRequestParams::from_der(&der.as_slice().into()).unwrap();
        assert_eq!(
            params.public_key.algorithm(),
            &rcgen::PKCS_ECDSA_P256_SHA256
        );
        assert_eq!(
            rcgen::PublicKeyData::subject_public_key_info(&params.public_key),
            spki
        );
        let (_, parsed) = X509CertificationRequest::from_der(&der).unwrap();
        let names: Vec<&str> = parsed
            .certification_request_info
            .subject
            .iter_common_name()
            .map(|name| name.as_str().unwrap())
            .collect();
        assert_eq!(names, ["tg-0001"]);
        let text = pem("CERTIFICATE REQUEST", &der);
        assert!(text.starts_with("-----BEGIN CERTIFICATE REQUEST-----\n"));
        assert!(text.ends_with("\n-----END CERTIFICATE REQUEST-----\n"));
        assert!(text.lines().all(|line| line.len() <= 64));
    }

    #[test]
    fn a_request_signed_by_another_key_or_not_at_all_is_refused() {
        let (spki, _) = software_key();
        let (_, other_sign) = software_key();
        let error = signed_request(&spki, &other_sign, &gateway()).unwrap_err();
        assert!(error.to_string().contains("does not verify"), "{error}");
        assert_eq!(error.exit(), Exit::Failure);

        let failing = |_: &[u8]| Err("the TPM refused the command: TPM_RC_AUTH_FAIL".to_owned());
        let error = signed_request(&spki, &failing, &gateway()).unwrap_err();
        assert!(error.to_string().contains("TPM_RC_AUTH_FAIL"), "{error}");

        let error = signed_request(&spki[1..], &failing, &gateway()).unwrap_err();
        assert!(error.to_string().contains("not ECDSA P-256"), "{error}");
    }

    #[test]
    fn check_request_refuses_another_key_another_name_and_trailing_bytes() {
        let (spki, sign) = software_key();
        let der = signed_request(&spki, &sign, &gateway()).unwrap();
        check_request(&der, &spki, &gateway()).unwrap();
        let (other, _) = software_key();
        assert!(check_request(&der, &other, &gateway()).is_err());
        let other_id = GatewayId::new("tg-0002").unwrap();
        assert!(check_request(&der, &spki, &other_id).is_err());
        let mut trailing = der.clone();
        trailing.push(0);
        assert!(check_request(&trailing, &spki, &gateway()).is_err());
        // One flipped bit in the signed part.
        let mut tampered = der;
        let at = tampered.len() / 3;
        tampered[at] ^= 0x01;
        assert!(check_request(&tampered, &spki, &gateway()).is_err());
    }
}
