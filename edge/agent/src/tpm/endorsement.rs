//! The TPM's endorsement key (EK) for the factory registration (decision 12,
//! Laysi/trustiot-v2#612): [`endorsement_key`] computes the ECC NIST P-256 EK and compares it
//! with the EK certificate the TPM's manufacturer may have stored in NV, for the `provision`
//! subcommand (Laysi/trustiot-v2#613), which puts both into its report.
//!
//! - **The EK.** TPM2_CreatePrimary in the endorsement hierarchy with the default ECC P-256 EK
//!   template of the TCG EK Credential Profile (template L-2, `marshal::ek_template`; what
//!   `tpm2_createek -G ecc` uses), then TPM2_ReadPublic of the new object and TPM2_FlushContext.
//!   A primary key is a function of the hierarchy's seed and the template, so every run and every
//!   connection get the same EK. Nothing is persisted, and no RSA operation is ever sent: the
//!   gateway's SLB9670 times out on RSA (docs/requirements/field-constraints.md).
//! - **Checks.** The TPM's answer must be that template exactly before `unique` (the same
//!   attributes, `PolicyA`, AES-128-CFB, P-256), its name the SHA-256 of that public area,
//!   TPM2_ReadPublic must give the same area and name, and the point must lie on P-256.
//! - **Only the default template.** When the TPM defines an EK template (`0x01C0000C`) or EK nonce
//!   (`0x01C0000B`) in NV, the profile builds the EK from those instead; that is refused with
//!   [`EndorsementError::CustomTemplate`] before anything is created, rather than reporting an EK
//!   the manufacturer's certificate would not be for.
//! - **The EK certificate** at NV index [`EK_CERTIFICATE_INDEX`] (`0x01C0000A`) is read when it is
//!   defined and written, with the owner's empty authorization when the index has `ownerRead`
//!   (the TCG profile's attributes; the owner hierarchy is not subject to dictionary-attack
//!   lockout) or else its own empty authorization when it has `authRead` and `noDA` (an index
//!   with `authRead` only and a non-empty authorization would count the failed read towards the
//!   lockout, so it is refused with [`EndorsementError::CertificateIndex`] before any read), in
//!   chunks of [`NV_READ_CHUNK`] bytes. It must be one DER X.509 certificate and nothing after
//!   it, and its `SubjectPublicKeyInfo` must be byte for byte the EK's (`id-ecPublicKey`, `prime256v1`, the
//!   uncompressed point): then [`EkCertificate::Matches`]; a certificate for any other key, or
//!   the same key encoded differently, is refused with [`EndorsementError::CertificateMismatch`],
//!   and one that does not parse with [`EndorsementError::CertificateInvalid`]. No index, or an
//!   index never written, is [`EkCertificate::Absent`]. The chain to the manufacturer's root is
//!   not checked: the certs service records and compares the EK only
//!   (services/certs/README.md, 「規則與取捨」: 「EK 只記錄、比對，不驗證」). A match is therefore
//!   no evidence that the TPM is genuine; see [`EkCertificate::Matches`].
//! - **Endorsement authorization.** Only the empty authorization (the TPM's default) is
//!   supported. When the hierarchy has one, TPM2_CreatePrimary fails authorization and
//!   [`EndorsementError::EndorsementAuthorization`] is returned; hierarchy authorizations are not
//!   subject to the dictionary-attack counter, so the TPM is left exactly as it was.
//!
//! The EK is a transient object between TPM2_CreatePrimary and TPM2_FlushContext. It is flushed
//! whatever the later checks find; if the process dies in between, the kernel's resource manager
//! (`/dev/tpmrm0`) flushes it when the device is closed, so the caller must use the resource
//! manager (Laysi/trustiot-v2#613 refuses other devices). The same holds when
//! TPM2_CreatePrimary's response is malformed or does not arrive in time: the handle is then
//! unknown, so nothing is flushed and the resource manager removes the EK when the device is
//! closed. Every command goes through the worker with resends on the TPM's retry warnings, at
//! most [`super::PROVISION_RESPONSE_TIMEOUT`] each.

use ring::agreement;
use ring::rand::SystemRandom;
use x509_parser::prelude::{FromDer, X509Certificate};

use super::marshal::{self, NvPublic, nv_attribute};
use super::provision::{Hex, flush, is_authorization_failure};
use super::worker::Worker;
use super::{ResponseCode, TpmError, TpmTransport, spawn_provisioning_worker};

/// The NV index of the ECC NIST P-256 EK certificate (TCG EK Credential Profile, section
/// 2.2.1.5).
pub const EK_CERTIFICATE_INDEX: u32 = 0x01c0_000a;
/// The NV index of the ECC NIST P-256 EK nonce.
const EK_NONCE_INDEX: u32 = 0x01c0_000b;
/// The NV index of the ECC NIST P-256 EK template.
const EK_TEMPLATE_INDEX: u32 = 0x01c0_000c;

/// The largest EK certificate read; a larger index is refused. Manufacturers' EK certificates
/// are around 1 KiB.
pub const MAX_EK_CERTIFICATE_BYTES: usize = 4096;
/// How many bytes one TPM2_NV_Read asks for: at most the TPM's `TPM_PT_NV_BUFFER_MAX`, which the
/// PC Client platform TPM profile requires to be at least 512 (the SLB9670 has 768, swtpm 1024).
pub const NV_READ_CHUNK: u16 = 512;

/// `TPM_RC_HANDLE` for the first handle: TPM2_NV_ReadPublic of an index that is not defined.
const INDEX_UNDEFINED: u32 = 0x18b;

/// The endorsement key and what was found about its certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endorsement {
    /// The EK's DER `SubjectPublicKeyInfo` (ECC NIST P-256, uncompressed point).
    pub public_key_spki_der: Vec<u8>,
    /// The EK certificate, when the TPM has one.
    pub certificate: EkCertificate,
}

/// The EK certificate at [`EK_CERTIFICATE_INDEX`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EkCertificate {
    /// The index is not defined, or was never written: there is no EK certificate.
    Absent,
    /// The certificate (DER), whose `SubjectPublicKeyInfo` is byte for byte the EK's.
    ///
    /// That is all it means. Neither the certificate's signature nor its chain to a TPM
    /// manufacturer is checked, so it does not show that the TPM is genuine: whoever holds the
    /// platform authorization can rewrite the index (a certificate for the EK's key signed by
    /// anyone, even a self-signed one, matches too), and a counterfeit TPM, or a man in the middle on the bus, can report
    /// another device's EK together with that device's real certificate. The certs service
    /// records and compares the EK without verifying it (services/certs/README.md, 「規則與取捨」:
    /// 「EK 只記錄、比對，不驗證」).
    Matches(Vec<u8>),
}

/// Why [`endorsement_key`] did not end with the EK.
#[derive(Debug, thiserror::Error)]
pub enum EndorsementError {
    /// The endorsement hierarchy has an authorization value; only the empty one is supported.
    #[error(
        "the endorsement hierarchy has an authorization value (TPM2_CreatePrimary: {0}); only the empty authorization, the TPM's default, is supported"
    )]
    EndorsementAuthorization(ResponseCode),
    /// The TPM defines an EK template or nonce in NV, so its EK is not the default template's.
    #[error(
        "the TPM defines an EK {what} at NV index {index:#010x}; only the default ECC P-256 EK template is supported"
    )]
    CustomTemplate {
        /// The NV index.
        index: u32,
        /// "template" or "nonce".
        what: &'static str,
    },
    /// The EK certificate's NV index exists but cannot be read as one.
    #[error("the EK certificate index {EK_CERTIFICATE_INDEX:#010x} cannot be read: {0}")]
    CertificateIndex(String),
    /// The EK certificate is not one DER X.509 certificate.
    #[error(
        "the EK certificate at {EK_CERTIFICATE_INDEX:#010x} is not a valid X.509 certificate: {reason}"
    )]
    CertificateInvalid {
        /// The bytes read from the index.
        certificate: Vec<u8>,
        /// What is wrong with them.
        reason: String,
    },
    /// The EK certificate is for another public key than the EK.
    #[error(
        "the EK certificate at {EK_CERTIFICATE_INDEX:#010x} is not for this TPM's endorsement key: {reason}"
    )]
    CertificateMismatch {
        /// The EK's DER `SubjectPublicKeyInfo`.
        endorsement_key: Vec<u8>,
        /// The certificate (DER).
        certificate: Vec<u8>,
        /// How the keys differ.
        reason: String,
    },
    /// The system's random number generator failed, so the EK's point could not be checked.
    #[error("cannot check the endorsement key's point: the system random number generator failed")]
    Randomness,
    /// The TPM could not be reached, refused a command, or answered something unexpected.
    #[error(transparent)]
    Tpm(#[from] TpmError),
}

/// Computes the TPM's ECC P-256 endorsement key and compares it with the EK certificate in NV,
/// as the module documentation describes. Blocks for the TPM commands, each at most
/// [`super::PROVISION_RESPONSE_TIMEOUT`].
///
/// # Errors
///
/// [`EndorsementError::CustomTemplate`] when the TPM defines an EK template or nonce;
/// [`EndorsementError::EndorsementAuthorization`] when the endorsement hierarchy has an
/// authorization value; [`EndorsementError::CertificateMismatch`],
/// [`EndorsementError::CertificateInvalid`] or [`EndorsementError::CertificateIndex`] when the EK
/// certificate is not this EK's or cannot be read; [`EndorsementError::Tpm`] when the device
/// cannot be opened, or the TPM refuses a command or answers something unexpected; it is
/// [`TpmError::NotResourceManager`], before the device is opened, for a device that is not
/// `/dev/tpmrm<N>`, and [`TpmError::NotResourceManagerDevice`], before any command, when the
/// kernel does not list the device as a resource manager device.
pub fn endorsement_key(transport: TpmTransport) -> Result<Endorsement, EndorsementError> {
    let worker = spawn_provisioning_worker(transport)?;
    for (index, what) in [(EK_TEMPLATE_INDEX, "template"), (EK_NONCE_INDEX, "nonce")] {
        if nv_public(&worker, index)?.is_some() {
            return Err(EndorsementError::CustomTemplate { index, what });
        }
    }
    let public_key_spki_der = create_ek(&worker)?;
    let certificate = match read_certificate(&worker)? {
        None => {
            tracing::info!(index = %Hex(EK_CERTIFICATE_INDEX), "the TPM has no EK certificate");
            EkCertificate::Absent
        }
        Some(certificate) => {
            let checked = check_certificate(&public_key_spki_der, certificate)?;
            tracing::info!(index = %Hex(EK_CERTIFICATE_INDEX), "the EK certificate is for the EK");
            checked
        }
    };
    Ok(Endorsement {
        public_key_spki_der,
        certificate,
    })
}

/// The NV index's public area, or `None` when it is not defined.
fn nv_public(worker: &Worker, index: u32) -> Result<Option<NvPublic>, TpmError> {
    let response = worker.transact(marshal::nv_read_public_command(index))?;
    let public = match marshal::parse_nv_read_public(&response) {
        Ok(public) => public,
        Err(TpmError::Response(ResponseCode(INDEX_UNDEFINED))) => return Ok(None),
        Err(error) => return Err(error),
    };
    if public.index != index {
        return Err(TpmError::Malformed(format!(
            "TPM2_NV_ReadPublic of {} described {}",
            Hex(index),
            Hex(public.index)
        )));
    }
    Ok(Some(public))
}

/// Creates the EK, checks it, flushes it, and returns its `SubjectPublicKeyInfo`.
fn create_ek(worker: &Worker) -> Result<Vec<u8>, EndorsementError> {
    let template = marshal::ek_template();
    let response = worker.transact(marshal::create_primary_command(
        marshal::TPM_RH_ENDORSEMENT,
        &template,
    ))?;
    let primary = match marshal::parse_create_primary(&response) {
        Ok(primary) => primary,
        Err(TpmError::Response(code)) if is_authorization_failure(code) => {
            return Err(EndorsementError::EndorsementAuthorization(code));
        }
        Err(error) => return Err(error.into()),
    };
    let handle = primary.loaded.handle;
    let checked = check_ek(worker, &primary);
    // Not persisted: flushed whatever the checks found.
    flush(worker, handle);
    let point = checked?;
    on_curve(&point.point)?;
    Ok(point.spki_der())
}

/// The created primary is the template's EK, and TPM2_ReadPublic agrees.
fn check_ek(worker: &Worker, primary: &marshal::Primary) -> Result<marshal::PublicKey, TpmError> {
    let public = marshal::parse_ek_area(&primary.area)?;
    if primary.loaded.name != marshal::sha256_name(&primary.area) {
        return Err(TpmError::Malformed(
            "TPM2_CreatePrimary returned a name that is not the SHA-256 of the EK's public area"
                .to_owned(),
        ));
    }
    let response = worker.transact(marshal::read_public_command(primary.loaded.handle))?;
    let read = marshal::parse_read_public_area(&response)?;
    if read.area != primary.area || read.name != primary.loaded.name {
        return Err(TpmError::Malformed(
            "TPM2_ReadPublic of the EK does not give what TPM2_CreatePrimary returned".to_owned(),
        ));
    }
    Ok(public)
}

/// Fails unless `point` (uncompressed) lies on P-256: ring checks a peer's public key before an
/// ECDH agreement, so one agreement with a throwaway key is the check.
fn on_curve(point: &[u8]) -> Result<(), EndorsementError> {
    let random = SystemRandom::new();
    let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &random)
        .map_err(|_| EndorsementError::Randomness)?;
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, point);
    agreement::agree_ephemeral(private, &peer, |_| ()).map_err(|_| {
        EndorsementError::Tpm(TpmError::Malformed(
            "the endorsement key's point is not on NIST P-256".to_owned(),
        ))
    })
}

/// The bytes of the EK certificate index, or `None` when there is no certificate.
fn read_certificate(worker: &Worker) -> Result<Option<Vec<u8>>, EndorsementError> {
    let Some(public) = nv_public(worker, EK_CERTIFICATE_INDEX)? else {
        return Ok(None);
    };
    let unreadable = |reason: String| Err(EndorsementError::CertificateIndex(reason));
    let attributes = public.attributes;
    if attributes & nv_attribute::TYPE_MASK != 0 {
        return unreadable(format!(
            "it is not an ordinary index (attributes {attributes:#010x})"
        ));
    }
    if attributes & nv_attribute::WRITTEN == 0 {
        tracing::info!(index = %Hex(EK_CERTIFICATE_INDEX), "the EK certificate index was never written");
        return Ok(None);
    }
    let size = usize::from(public.data_size);
    if size == 0 || size > MAX_EK_CERTIFICATE_BYTES {
        return unreadable(format!(
            "it holds {size} bytes; an EK certificate has 1 to {MAX_EK_CERTIFICATE_BYTES}"
        ));
    }
    let auth = if attributes & nv_attribute::OWNERREAD != 0 {
        marshal::TPM_RH_OWNER
    } else if attributes & nv_attribute::AUTHREAD != 0 {
        // The empty password fails when the index has an authorization value; without noDA
        // that failure would count towards the TPM's dictionary-attack lockout.
        if attributes & nv_attribute::NO_DA == 0 {
            return unreadable(format!(
                "only authRead is set, without noDA (attributes {attributes:#010x}); reading it could count towards the dictionary-attack lockout"
            ));
        }
        EK_CERTIFICATE_INDEX
    } else {
        return unreadable(format!(
            "neither ownerRead nor authRead is set (attributes {attributes:#010x})"
        ));
    };
    let mut certificate = Vec::with_capacity(size);
    while certificate.len() < size {
        let left = size - certificate.len();
        let chunk = u16::try_from(left).map_or(NV_READ_CHUNK, |left| left.min(NV_READ_CHUNK));
        // Below MAX_EK_CERTIFICATE_BYTES, so it fits.
        let offset = u16::try_from(certificate.len())
            .map_err(|_| TpmError::Malformed("the EK certificate is too large".to_owned()))?;
        let response = worker.transact(marshal::nv_read_command(
            auth,
            EK_CERTIFICATE_INDEX,
            chunk,
            offset,
        ))?;
        certificate.extend_from_slice(&marshal::parse_nv_read(&response, chunk)?);
    }
    Ok(Some(certificate))
}

/// [`EkCertificate::Matches`] when `certificate` is one DER X.509 certificate whose
/// `SubjectPublicKeyInfo` is exactly `endorsement_key`; a refusal otherwise.
fn check_certificate(
    endorsement_key: &[u8],
    certificate: Vec<u8>,
) -> Result<EkCertificate, EndorsementError> {
    let certificate_key = match X509Certificate::from_der(&certificate) {
        Ok((rest, _)) if !rest.is_empty() => {
            Err(format!("{} bytes follow the certificate", rest.len()))
        }
        Ok((_, parsed)) => Ok(parsed.public_key().raw.to_vec()),
        Err(error) => Err(error.to_string()),
    };
    let certificate_key = match certificate_key {
        Ok(key) => key,
        Err(reason) => {
            return Err(EndorsementError::CertificateInvalid {
                certificate,
                reason,
            });
        }
    };
    if certificate_key != endorsement_key {
        let reason = format!(
            "its SubjectPublicKeyInfo ({} bytes, SHA-256 {}) is not the EK's ({} bytes, SHA-256 {})",
            certificate_key.len(),
            sha256_hex(&certificate_key),
            endorsement_key.len(),
            sha256_hex(endorsement_key)
        );
        return Err(EndorsementError::CertificateMismatch {
            endorsement_key: endorsement_key.to_vec(),
            certificate,
            reason,
        });
    }
    Ok(EkCertificate::Matches(certificate))
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    //! The certificate comparison with certificates made by rcgen. The whole flow against the
    //! exchange recorded from swtpm is in `flow`; the real TPM tests are
    //! `tests/tpm_endorsement.rs`.

    use super::*;

    /// A P-256 key's SubjectPublicKeyInfo.
    fn p256_key() -> Vec<u8> {
        rcgen::PublicKeyData::subject_public_key_info(&rcgen::KeyPair::generate().unwrap())
    }

    /// A certificate whose subject key bytes are `point`, for the curve P-256, signed by a
    /// throwaway CA.
    fn certificate_with_point(point: &[u8]) -> Vec<u8> {
        struct Public(Vec<u8>);
        impl rcgen::PublicKeyData for Public {
            fn der_bytes(&self) -> &[u8] {
                &self.0
            }
            fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
                &rcgen::PKCS_ECDSA_P256_SHA256
            }
        }
        let issuer = rcgen::Issuer::new(
            rcgen::CertificateParams::new(vec!["ek-ca.test".to_owned()]).unwrap(),
            rcgen::KeyPair::generate().unwrap(),
        );
        rcgen::CertificateParams::new(vec!["ek.test".to_owned()])
            .unwrap()
            .signed_by(&Public(point.to_vec()), &issuer)
            .unwrap()
            .der()
            .to_vec()
    }

    /// The point after the fixed 26-byte prefix of a P-256 SubjectPublicKeyInfo.
    fn point_of(spki: &[u8]) -> &[u8] {
        &spki[26..]
    }

    #[test]
    fn a_certificate_for_the_ek_matches() {
        let ek = p256_key();
        let certificate = certificate_with_point(point_of(&ek));
        assert_eq!(
            check_certificate(&ek, certificate.clone()).unwrap(),
            EkCertificate::Matches(certificate)
        );
    }

    #[test]
    fn a_self_signed_certificate_for_the_ek_key_still_matches_because_only_the_key_is_compared() {
        // Matches says nothing about who signed the certificate: anyone who can write the NV
        // index can store one like this.
        let key = rcgen::KeyPair::generate().unwrap();
        let ek = rcgen::PublicKeyData::subject_public_key_info(&key);
        let self_signed = rcgen::CertificateParams::new(vec!["not-a-manufacturer.test".to_owned()])
            .unwrap()
            .self_signed(&key)
            .unwrap()
            .der()
            .to_vec();
        let (_, parsed) = X509Certificate::from_der(&self_signed).unwrap();
        assert_eq!(parsed.issuer(), parsed.subject());
        assert_eq!(
            check_certificate(&ek, self_signed.clone()).unwrap(),
            EkCertificate::Matches(self_signed)
        );
    }

    #[test]
    fn a_certificate_for_another_key_or_another_encoding_is_refused() {
        let ek = p256_key();
        let other = certificate_with_point(point_of(&p256_key()));
        // The same key with its point compressed: a different SubjectPublicKeyInfo.
        let point = point_of(&ek);
        let mut compressed = vec![0x02 | (point[64] & 1)];
        compressed.extend_from_slice(&point[1..33]);
        let recoded = certificate_with_point(&compressed);
        for certificate in [other, recoded] {
            match check_certificate(&ek, certificate.clone()) {
                Err(EndorsementError::CertificateMismatch {
                    endorsement_key,
                    certificate: returned,
                    reason,
                }) => {
                    assert_eq!(endorsement_key, ek);
                    assert_eq!(returned, certificate);
                    assert!(reason.contains(&sha256_hex(&ek)), "{reason}");
                }
                other => panic!("expected a mismatch, got {other:?}"),
            }
        }
    }

    #[test]
    fn bytes_that_are_not_exactly_one_certificate_are_refused() {
        let ek = p256_key();
        let certificate = certificate_with_point(point_of(&ek));
        let mut trailing = certificate.clone();
        trailing.push(0);
        let mut padded = certificate.clone();
        padded.extend_from_slice(&[0xff; 16]);
        let cases = [
            (trailing, "1 bytes follow"),
            (padded, "16 bytes follow"),
            (certificate[..certificate.len() - 1].to_vec(), ""),
            (Vec::new(), ""),
            (ek.clone(), ""),
            (vec![0x30, 0x80, 0x00, 0x00], ""),
        ];
        for (bytes, expected) in cases {
            match check_certificate(&ek, bytes.clone()) {
                Err(EndorsementError::CertificateInvalid {
                    certificate,
                    reason,
                }) => {
                    assert_eq!(certificate, bytes);
                    assert!(reason.contains(expected), "{expected:?} in {reason}");
                }
                other => panic!("expected an invalid certificate, got {other:?}"),
            }
        }
    }

    #[test]
    fn certificate_garbage_is_refused_without_panicking() {
        let ek = p256_key();
        let certificate = certificate_with_point(point_of(&ek));
        let mut state: u64 = 0x1234_5678_9abc_def1;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..5_000 {
            // A certificate with a few bytes changed, or random bytes behind a SEQUENCE header.
            let mut bytes = certificate.clone();
            if next() % 2 == 0 {
                for _ in 0..1 + next() % 4 {
                    let at = usize::try_from(next()).unwrap() % bytes.len();
                    bytes[at] = next().to_be_bytes()[0];
                }
            } else {
                let length = usize::try_from(next() % 600).unwrap();
                bytes = vec![0x30, 0x82];
                bytes.extend((0..length).map(|_| next().to_be_bytes()[0]));
            }
            // Whatever it gives, a match needs the EK's key in a parseable certificate.
            if let Ok(EkCertificate::Matches(matched)) = check_certificate(&ek, bytes.clone()) {
                let (rest, parsed) = X509Certificate::from_der(&matched).unwrap();
                assert!(rest.is_empty());
                assert_eq!(parsed.public_key().raw, &ek[..]);
            }
        }
    }
}

#[cfg(test)]
mod flow {
    //! [`endorsement_key`] against a scripted TPM that answers with the exchange recorded from
    //! swtpm (`super::super::fixtures`), checking every command byte for byte, and with those
    //! answers changed the ways a faulty or hostile TPM could change them.

    use std::collections::VecDeque;
    use std::io;
    use std::sync::{Arc, Mutex};

    use super::super::fixtures as recorded;
    use super::super::marshal::tests::hex;
    use super::*;
    use crate::tpm::TpmChannel;

    /// Expects exactly these commands, in order, and gives these answers.
    struct Scripted {
        script: VecDeque<(Vec<u8>, Vec<u8>)>,
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl TpmChannel for Scripted {
        fn transact(&mut self, command: &[u8]) -> io::Result<Vec<u8>> {
            self.sent.lock().unwrap().push(command.to_vec());
            let Some((expected, response)) = self.script.pop_front() else {
                return Err(io::Error::other("the script has no more answers"));
            };
            assert_eq!(
                command,
                &expected[..],
                "unexpected command; expected {expected:02x?}"
            );
            Ok(response)
        }
    }

    /// A command and the answer to it.
    type Step = (Vec<u8>, Vec<u8>);

    /// Runs the flow against `steps`; every step must be used.
    fn run(steps: Vec<Step>) -> Result<Endorsement, EndorsementError> {
        let count = steps.len();
        let (result, sent) = run_partly(steps);
        assert_eq!(sent.len(), count, "commands sent: {sent:02x?}");
        result
    }

    /// Runs the flow against `steps` and returns the commands it sent.
    fn run_partly(steps: Vec<Step>) -> (Result<Endorsement, EndorsementError>, Vec<Vec<u8>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let scripted = Scripted {
            script: steps.into(),
            sent: Arc::clone(&sent),
        };
        let result = endorsement_key(TpmTransport::Channel(Box::new(scripted)));
        let sent = sent.lock().unwrap().clone();
        (result, sent)
    }

    fn nv_read_public(index: u32) -> Vec<u8> {
        marshal::nv_read_public_command(index)
    }

    fn undefined(index: u32) -> Step {
        (
            nv_read_public(index),
            hex(recorded::NV_READ_PUBLIC_UNDEFINED_RESPONSE),
        )
    }

    fn create_primary(response: Vec<u8>) -> Step {
        (hex(recorded::EK_CREATE_PRIMARY_COMMAND), response)
    }

    fn read_public(response: Vec<u8>) -> Step {
        (hex(recorded::EK_READ_PUBLIC_COMMAND), response)
    }

    fn flush() -> Step {
        (
            hex(recorded::FLUSH_PRIMARY_COMMAND),
            hex(recorded::FLUSH_CONTEXT_RESPONSE),
        )
    }

    /// No EK template or nonce, then the recorded EK, read back and flushed.
    fn ek_steps() -> Vec<Step> {
        vec![
            undefined(EK_TEMPLATE_INDEX),
            undefined(EK_NONCE_INDEX),
            create_primary(hex(recorded::EK_CREATE_PRIMARY_RESPONSE)),
            read_public(hex(recorded::EK_READ_PUBLIC_RESPONSE)),
            flush(),
        ]
    }

    /// The recorded certificate index's attributes.
    const RECORDED_ATTRIBUTES: u32 = 0x6207_2001;

    /// The recorded certificate index's TPM2_NV_ReadPublic response with other attributes and
    /// size.
    fn certificate_index(attributes: u32, size: u16) -> Step {
        let mut response = hex(recorded::NV_READ_PUBLIC_CERTIFICATE_RESPONSE);
        // nvIndex at 12, nameAlg at 16, attributes at 18, an empty authPolicy at 22, dataSize
        // at 24.
        assert_eq!(response[12..16], EK_CERTIFICATE_INDEX.to_be_bytes());
        assert_eq!(response[18..22], RECORDED_ATTRIBUTES.to_be_bytes());
        assert_eq!(response[22..24], [0, 0]);
        response[18..22].copy_from_slice(&attributes.to_be_bytes());
        response[24..26].copy_from_slice(&size.to_be_bytes());
        (nv_read_public(EK_CERTIFICATE_INDEX), response)
    }

    /// A TPM2_NV_Read response with `data` and an empty password session.
    fn nv_read_response(data: &[u8]) -> Vec<u8> {
        let mut response = vec![0x80, 0x02, 0, 0, 0, 0, 0, 0, 0, 0];
        let length = u16::try_from(data.len()).unwrap();
        response.extend_from_slice(&(u32::from(length) + 2).to_be_bytes());
        response.extend_from_slice(&length.to_be_bytes());
        response.extend_from_slice(data);
        response.extend_from_slice(&[0, 0, 1, 0, 0]);
        let size = u32::try_from(response.len()).unwrap();
        response[2..6].copy_from_slice(&size.to_be_bytes());
        response
    }

    /// TPM2_NV_Reads of `data` from the certificate index with `auth`, chunked as the agent
    /// asks for them.
    fn reads(auth: u32, data: &[u8]) -> Vec<Step> {
        data.chunks(usize::from(NV_READ_CHUNK))
            .enumerate()
            .map(|(number, chunk)| {
                let offset = u16::try_from(number * usize::from(NV_READ_CHUNK)).unwrap();
                let size = u16::try_from(chunk.len()).unwrap();
                (
                    marshal::nv_read_command(auth, EK_CERTIFICATE_INDEX, size, offset),
                    nv_read_response(chunk),
                )
            })
            .collect()
    }

    fn ek() -> Vec<u8> {
        hex(recorded::EK_TPM2_TOOLS_SPKI)
    }

    #[test]
    fn the_nv_read_response_helper_gives_the_recorded_responses() {
        let certificate = hex(recorded::EK_CERTIFICATE);
        let steps = reads(marshal::TPM_RH_OWNER, &certificate);
        assert_eq!(
            steps,
            [
                (
                    hex(recorded::NV_READ_FIRST_COMMAND),
                    hex(recorded::NV_READ_FIRST_RESPONSE)
                ),
                (
                    hex(recorded::NV_READ_SECOND_COMMAND),
                    hex(recorded::NV_READ_SECOND_RESPONSE)
                ),
            ]
        );
    }

    #[test]
    fn without_a_certificate_the_ek_is_tpm2_tools_ek_and_absent_is_reported() {
        let mut steps = ek_steps();
        steps.push(undefined(EK_CERTIFICATE_INDEX));
        assert_eq!(
            run(steps).unwrap(),
            Endorsement {
                public_key_spki_der: ek(),
                certificate: EkCertificate::Absent,
            }
        );
    }

    #[test]
    fn the_recorded_certificate_is_read_in_two_chunks_and_matches() {
        let mut steps = ek_steps();
        // A retry warning on TPM2_CreatePrimary is resent.
        steps.insert(2, create_primary(hex(recorded::RETRY_RESPONSE)));
        steps.extend([
            (
                nv_read_public(EK_CERTIFICATE_INDEX),
                hex(recorded::NV_READ_PUBLIC_CERTIFICATE_RESPONSE),
            ),
            (
                hex(recorded::NV_READ_FIRST_COMMAND),
                hex(recorded::NV_READ_FIRST_RESPONSE),
            ),
            (
                hex(recorded::NV_READ_SECOND_COMMAND),
                hex(recorded::NV_READ_SECOND_RESPONSE),
            ),
        ]);
        assert_eq!(
            run(steps).unwrap(),
            Endorsement {
                public_key_spki_der: ek(),
                certificate: EkCertificate::Matches(hex(recorded::EK_CERTIFICATE)),
            }
        );
    }

    #[test]
    fn an_index_with_only_auth_read_is_read_with_its_own_authorization() {
        let certificate = hex(recorded::EK_CERTIFICATE);
        let size = u16::try_from(certificate.len()).unwrap();
        let mut steps = ek_steps();
        steps.push(certificate_index(
            RECORDED_ATTRIBUTES & !nv_attribute::OWNERREAD,
            size,
        ));
        steps.extend(reads(EK_CERTIFICATE_INDEX, &certificate));
        assert_eq!(
            run(steps).unwrap().certificate,
            EkCertificate::Matches(certificate)
        );
    }

    #[test]
    fn an_index_of_the_largest_size_is_read_in_eight_chunks_and_judged() {
        let mut data = hex(recorded::EK_CERTIFICATE);
        data.resize(MAX_EK_CERTIFICATE_BYTES, 0);
        let mut steps = ek_steps();
        steps.push(certificate_index(
            RECORDED_ATTRIBUTES,
            u16::try_from(data.len()).unwrap(),
        ));
        let chunks = reads(marshal::TPM_RH_OWNER, &data);
        assert_eq!(chunks.len(), 8);
        steps.extend(chunks);
        // The certificate followed by zeros is not exactly one certificate.
        match run(steps) {
            Err(EndorsementError::CertificateInvalid {
                certificate,
                reason,
            }) => {
                assert_eq!(certificate, data);
                assert!(reason.contains("3298 bytes follow"), "{reason}");
            }
            other => panic!("expected an invalid certificate, got {other:?}"),
        }
    }

    /// A certificate for the P-256 SubjectPublicKeyInfo `spki`, signed by a throwaway CA.
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
        let issuer = rcgen::Issuer::new(
            rcgen::CertificateParams::new(vec!["ek-ca.test".to_owned()]).unwrap(),
            rcgen::KeyPair::generate().unwrap(),
        );
        rcgen::CertificateParams::new(vec!["ek.test".to_owned()])
            .unwrap()
            .signed_by(&Public(spki[26..].to_vec()), &issuer)
            .unwrap()
            .der()
            .to_vec()
    }

    #[test]
    fn a_certificate_for_another_key_is_refused() {
        // The recorded gateway key, which is not the EK.
        let other = marshal::parse_read_public(&hex(recorded::READ_PUBLIC_KEY_RESPONSE))
            .unwrap()
            .spki_der();
        let certificate = certificate_for(&other);
        let mut steps = ek_steps();
        steps.push(certificate_index(
            RECORDED_ATTRIBUTES,
            u16::try_from(certificate.len()).unwrap(),
        ));
        steps.extend(reads(marshal::TPM_RH_OWNER, &certificate));
        match run(steps) {
            Err(EndorsementError::CertificateMismatch {
                endorsement_key,
                certificate: returned,
                ..
            }) => {
                assert_eq!(endorsement_key, ek());
                assert_eq!(returned, certificate);
            }
            other => panic!("expected a mismatch, got {other:?}"),
        }
    }

    #[test]
    fn an_ek_template_or_nonce_is_refused_before_anything_is_created() {
        let result = run(vec![(
            nv_read_public(EK_TEMPLATE_INDEX),
            hex(recorded::NV_READ_PUBLIC_TEMPLATE_RESPONSE),
        )]);
        assert!(matches!(
            result,
            Err(EndorsementError::CustomTemplate {
                index: 0x01c0_000c,
                what: "template"
            })
        ));

        let mut nonce = hex(recorded::NV_READ_PUBLIC_TEMPLATE_RESPONSE);
        nonce[12..16].copy_from_slice(&EK_NONCE_INDEX.to_be_bytes());
        let result = run(vec![
            undefined(EK_TEMPLATE_INDEX),
            (nv_read_public(EK_NONCE_INDEX), nonce),
        ]);
        assert!(matches!(
            result,
            Err(EndorsementError::CustomTemplate {
                index: 0x01c0_000b,
                what: "nonce"
            })
        ));
    }

    #[test]
    fn an_nv_answer_about_another_index_is_refused() {
        let result = run(vec![(
            nv_read_public(EK_TEMPLATE_INDEX),
            hex(recorded::NV_READ_PUBLIC_CERTIFICATE_RESPONSE),
        )]);
        assert!(matches!(
            result,
            Err(EndorsementError::Tpm(TpmError::Malformed(detail))) if detail.contains("described")
        ));
    }

    #[test]
    fn an_endorsement_authorization_ends_after_create_primary() {
        let result = run(vec![
            undefined(EK_TEMPLATE_INDEX),
            undefined(EK_NONCE_INDEX),
            create_primary(hex(recorded::EK_CREATE_PRIMARY_BAD_AUTH_RESPONSE)),
        ]);
        assert!(matches!(
            result,
            Err(EndorsementError::EndorsementAuthorization(code)) if code.code() == 0x9a2
        ));
    }

    /// Offsets in the recorded responses: the EK's public area in TPM2_CreatePrimary's
    /// response and in TPM2_ReadPublic's, and its name.
    const CREATE_PRIMARY_AREA: usize = 20;
    const READ_PUBLIC_AREA: usize = 12;
    const AREA_BYTES: usize = 122;
    const NAME_BYTES: usize = 34;
    /// TPM2_CreatePrimary's response ends with the name, then the 5-byte session.
    const SESSION_BYTES: usize = 5;

    fn create_primary_name(response: &mut [u8]) -> &mut [u8] {
        let end = response.len() - SESSION_BYTES;
        &mut response[end - NAME_BYTES..end]
    }

    fn read_public_name(response: &mut [u8]) -> &mut [u8] {
        let start = READ_PUBLIC_AREA + AREA_BYTES + 2;
        &mut response[start..start + NAME_BYTES]
    }

    #[test]
    fn the_recorded_offsets_are_right() {
        let mut primary = hex(recorded::EK_CREATE_PRIMARY_RESPONSE);
        let mut read = hex(recorded::EK_READ_PUBLIC_RESPONSE);
        let area = primary[CREATE_PRIMARY_AREA..CREATE_PRIMARY_AREA + AREA_BYTES].to_vec();
        assert_eq!(area, read[READ_PUBLIC_AREA..READ_PUBLIC_AREA + AREA_BYTES]);
        assert_eq!(
            create_primary_name(&mut primary),
            marshal::sha256_name(&area)
        );
        assert_eq!(read_public_name(&mut read), marshal::sha256_name(&area));
    }

    /// Runs `steps`, which end with the EK's flush; the result must be a TPM error naming
    /// `detail`, after exactly the commands `codes`.
    fn assert_refused_and_flushed(steps: Vec<Step>, codes: &[u32], detail: &str) {
        let count = steps.len();
        let (result, sent) = run_partly(steps);
        assert_eq!(sent.len(), count);
        let sent_codes: Vec<u32> = sent
            .iter()
            .map(|command| u32::from_be_bytes(command[6..10].try_into().unwrap()))
            .collect();
        assert_eq!(sent_codes, codes);
        let message = match result {
            Err(EndorsementError::Tpm(error)) => error.to_string(),
            other => panic!("expected a TPM error, got {other:?}"),
        };
        assert!(message.contains(detail), "{detail:?} in {message}");
    }

    /// NV_ReadPublic twice, CreatePrimary, FlushContext.
    const UNREAD_EK: [u32; 4] = [0x169, 0x169, 0x131, 0x165];
    /// NV_ReadPublic twice, CreatePrimary, ReadPublic, FlushContext.
    const READ_EK: [u32; 5] = [0x169, 0x169, 0x131, 0x173, 0x165];

    #[test]
    fn an_ek_with_other_attributes_is_flushed_and_refused() {
        let mut primary = hex(recorded::EK_CREATE_PRIMARY_RESPONSE);
        // objectAttributes (0x000300b2) with userWithAuth as well.
        assert_eq!(primary[CREATE_PRIMARY_AREA + 7], 0xb2);
        primary[CREATE_PRIMARY_AREA + 7] |= 0x40;
        let mut steps = ek_steps();
        steps.truncate(2);
        steps.extend([create_primary(primary), flush()]);
        assert_refused_and_flushed(steps, &UNREAD_EK, "EK template");
    }

    #[test]
    fn an_ek_whose_name_is_not_its_area_is_flushed_and_refused() {
        let mut primary = hex(recorded::EK_CREATE_PRIMARY_RESPONSE);
        create_primary_name(&mut primary)[NAME_BYTES - 1] ^= 0x01;
        let mut steps = ek_steps();
        steps.truncate(2);
        steps.extend([create_primary(primary), flush()]);
        assert_refused_and_flushed(steps, &UNREAD_EK, "not the SHA-256");
    }

    #[test]
    fn an_ek_that_read_public_disagrees_with_is_flushed_and_refused() {
        // The last byte of the area, and the first byte of the name.
        for change in [
            READ_PUBLIC_AREA + AREA_BYTES - 1,
            READ_PUBLIC_AREA + AREA_BYTES + 2,
        ] {
            let mut read = hex(recorded::EK_READ_PUBLIC_RESPONSE);
            read[change] ^= 0x01;
            let mut steps = ek_steps();
            steps[3] = read_public(read);
            assert_refused_and_flushed(steps, &READ_EK, "does not give");
        }
    }

    #[test]
    fn an_ek_point_off_the_curve_is_flushed_and_refused() {
        let mut primary = hex(recorded::EK_CREATE_PRIMARY_RESPONSE);
        let mut read = hex(recorded::EK_READ_PUBLIC_RESPONSE);
        // The last byte of y, with both names made to match the changed area.
        primary[CREATE_PRIMARY_AREA + AREA_BYTES - 1] ^= 0x01;
        read[READ_PUBLIC_AREA + AREA_BYTES - 1] ^= 0x01;
        let area = primary[CREATE_PRIMARY_AREA..CREATE_PRIMARY_AREA + AREA_BYTES].to_vec();
        let name = marshal::sha256_name(&area);
        create_primary_name(&mut primary).copy_from_slice(&name);
        read_public_name(&mut read).copy_from_slice(&name);
        let mut steps = ek_steps();
        steps[2] = create_primary(primary);
        steps[3] = read_public(read);
        assert_refused_and_flushed(steps, &READ_EK, "not on NIST P-256");
    }

    #[test]
    fn a_certificate_index_that_cannot_hold_a_certificate_is_refused() {
        for (attributes, size, detail) in [
            // A counter index.
            (RECORDED_ATTRIBUTES | 0x10, 798, "not an ordinary index"),
            (
                RECORDED_ATTRIBUTES & !(nv_attribute::OWNERREAD | nv_attribute::AUTHREAD),
                798,
                "neither ownerRead nor authRead",
            ),
            (RECORDED_ATTRIBUTES, 0, "holds 0 bytes"),
            (RECORDED_ATTRIBUTES, 4097, "holds 4097 bytes"),
            (RECORDED_ATTRIBUTES, u16::MAX, "holds 65535 bytes"),
        ] {
            let mut steps = ek_steps();
            steps.push(certificate_index(attributes, size));
            match run(steps) {
                Err(EndorsementError::CertificateIndex(reason)) => {
                    assert!(reason.contains(detail), "{detail:?} in {reason}");
                }
                other => panic!("expected an unreadable index, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_auth_read_index_without_no_da_is_refused_before_any_read() {
        let mut steps = ek_steps();
        steps.push(certificate_index(
            RECORDED_ATTRIBUTES & !nv_attribute::OWNERREAD & !nv_attribute::NO_DA,
            798,
        ));
        // run() requires every step to be used and nothing more: no TPM2_NV_Read is sent.
        match run(steps) {
            Err(EndorsementError::CertificateIndex(reason)) => {
                assert!(reason.contains("without noDA"), "{reason}");
            }
            other => panic!("expected an unreadable index, got {other:?}"),
        }
    }

    #[test]
    fn a_certificate_index_never_written_is_absent() {
        let mut steps = ek_steps();
        steps.push(certificate_index(
            RECORDED_ATTRIBUTES & !nv_attribute::WRITTEN,
            798,
        ));
        assert_eq!(run(steps).unwrap().certificate, EkCertificate::Absent);
    }

    #[test]
    fn a_short_nv_read_is_refused() {
        let mut steps = ek_steps();
        steps.extend([
            (
                nv_read_public(EK_CERTIFICATE_INDEX),
                hex(recorded::NV_READ_PUBLIC_CERTIFICATE_RESPONSE),
            ),
            (
                hex(recorded::NV_READ_FIRST_COMMAND),
                hex(recorded::NV_READ_SECOND_RESPONSE),
            ),
        ]);
        assert!(matches!(
            run(steps),
            Err(EndorsementError::Tpm(TpmError::Malformed(detail))) if detail.contains("asked for")
        ));
    }
}
