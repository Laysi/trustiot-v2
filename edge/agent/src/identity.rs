//! The gateway's identity for the inner mTLS: its client certificate chain and private key.
//!
//! The gateway ID is the leaf certificate's CN (it must be a valid [`GatewayId`]), never a
//! separate setting, so the MQTT client ID, the topics and the identity the broker sees can
//! never disagree. Production keys stay in the TPM ([`KeySource::Tpm`], decision 12); a PEM
//! key file ([`KeySource::File`]) exists for development and tests. Either way the key must
//! belong to the certificate, or the agent refuses to start.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use protocol::{GatewayId, IdError, UnixMillis};
use rustls::sign::SigningKey;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::tls;
use crate::tpm::{TpmError, TpmKey, TpmTransport};

/// The TPM device of the kernel's resource manager.
pub const DEFAULT_TPM_DEVICE: &str = "/dev/tpmrm0";
/// The persistent handle of the gateway key (TPM PoC 2026-10-12).
pub const DEFAULT_TPM_HANDLE: u32 = 0x8100_0100;

/// Where the private key is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeySource {
    /// A non-exportable ECC P-256 key in the TPM, at a persistent handle. Production.
    Tpm {
        /// The TPM device.
        device: PathBuf,
        /// The persistent handle (0x81000000–0x81FFFFFF).
        handle: u32,
    },
    /// A PEM private key file (PKCS#8, SEC1 or PKCS#1). Development and tests only.
    File {
        /// The key file.
        path: PathBuf,
    },
}

/// The client certificate chain and its key, checked to belong together.
#[derive(Clone)]
pub struct Identity {
    /// The leaf certificate's CN.
    pub gateway: GatewayId,
    /// The leaf certificate first, then any intermediates, as sent in the TLS handshake.
    pub chain: Vec<CertificateDer<'static>>,
    /// The private key of the leaf certificate.
    pub key: Arc<dyn SigningKey>,
    /// When the leaf certificate becomes valid (0 for a time before 1970).
    pub not_before: UnixMillis,
    /// When the leaf certificate expires.
    pub not_after: UnixMillis,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("gateway", &self.gateway)
            .field("certificates", &self.chain.len())
            .field("not_before", &self.not_before)
            .field("not_after", &self.not_after)
            .finish_non_exhaustive()
    }
}

/// Why the identity cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// The certificate file cannot be read.
    #[error("reading the certificate file {path}: {source}")]
    ReadCertificate {
        /// The file.
        path: PathBuf,
        /// Why.
        source: rustls_pki_types::pem::Error,
    },
    /// The certificate file holds no certificate.
    #[error("the certificate file {0} holds no PEM certificate")]
    NoCertificate(PathBuf),
    /// The leaf certificate is not valid X.509.
    #[error("the certificate in {path} does not parse: {reason}")]
    BadCertificate {
        /// The file.
        path: PathBuf,
        /// Why.
        reason: String,
    },
    /// The subject has no CN or more than one.
    #[error("the certificate's subject must have exactly one CN (common name), it has {0}")]
    CommonNames(usize),
    /// The CN is not a gateway ID.
    #[error("the certificate's CN is not a valid gateway ID: {0}")]
    GatewayId(IdError),
    /// The extended key usage does not allow client authentication.
    #[error("the certificate's extended key usage does not include clientAuth")]
    NotForClients,
    /// The key usage does not allow signatures, which the TLS 1.3 client authentication needs.
    #[error("the certificate's key usage does not include digitalSignature")]
    NoDigitalSignature,
    /// The key file cannot be read.
    #[error("reading the private key file {path}: {source}")]
    ReadKey {
        /// The file.
        path: PathBuf,
        /// Why.
        source: rustls_pki_types::pem::Error,
    },
    /// The key cannot be used for signing.
    #[error("the private key in {path} is not usable: {source}")]
    BadKey {
        /// The file.
        path: PathBuf,
        /// Why.
        source: rustls::Error,
    },
    /// The TPM key cannot be used.
    #[error("the TPM key: {0}")]
    Tpm(#[from] TpmError),
    /// The key is not the certificate's.
    #[error("the private key does not belong to the certificate")]
    KeyMismatch,
}

/// What the agent needs from the leaf certificate.
struct Leaf {
    gateway: GatewayId,
    not_before: UnixMillis,
    not_after: UnixMillis,
    spki: Vec<u8>,
}

/// Loads the chain from `certificate` and the key from `key`, and checks that they belong
/// together. Opening a TPM key talks to the TPM.
///
/// # Errors
///
/// See [`IdentityError`].
pub fn load(certificate: &Path, key: &KeySource) -> Result<Identity, IdentityError> {
    load_with(certificate, key, &mut |device| {
        TpmTransport::Device(device.to_owned())
    })
}

/// [`load`], with a TPM key reached through `open_tpm(device)` instead of the device itself:
/// tests pass a software TPM.
///
/// # Errors
///
/// See [`IdentityError`].
pub fn load_with(
    certificate: &Path,
    key: &KeySource,
    open_tpm: &mut dyn FnMut(&Path) -> TpmTransport,
) -> Result<Identity, IdentityError> {
    let chain = read_chain(certificate)?;
    from_chain(certificate, chain, key, open_tpm)
}

/// The identity of `chain` (leaf first) and the key from `key`, checked as [`load`] checks the
/// chain of a file; `certificate` is only named in errors. `trustiot-agent provision install`
/// checks a chain this way before it writes the chain to `certificate`.
///
/// # Errors
///
/// See [`IdentityError`].
pub fn from_chain(
    certificate: &Path,
    chain: Vec<CertificateDer<'static>>,
    key: &KeySource,
    open_tpm: &mut dyn FnMut(&Path) -> TpmTransport,
) -> Result<Identity, IdentityError> {
    let leaf = parse_leaf(certificate, &chain)?;
    let (signing_key, key_spki): (Arc<dyn SigningKey>, Vec<u8>) = match key {
        KeySource::File { path } => {
            let der =
                PrivateKeyDer::from_pem_file(path).map_err(|source| IdentityError::ReadKey {
                    path: path.clone(),
                    source,
                })?;
            let signing_key =
                tls::provider()
                    .key_provider
                    .load_private_key(der)
                    .map_err(|source| IdentityError::BadKey {
                        path: path.clone(),
                        source,
                    })?;
            let spki = signing_key
                .public_key()
                .map(|spki| spki.as_ref().to_vec())
                .ok_or(IdentityError::KeyMismatch)?;
            (signing_key, spki)
        }
        KeySource::Tpm { device, handle } => {
            let tpm_key = TpmKey::open(open_tpm(device), *handle)?;
            let spki = tpm_key.public_key_spki_der().to_vec();
            (Arc::new(tpm_key), spki)
        }
    };
    if key_spki != leaf.spki {
        return Err(IdentityError::KeyMismatch);
    }
    Ok(Identity {
        gateway: leaf.gateway,
        chain,
        key: signing_key,
        not_before: leaf.not_before,
        not_after: leaf.not_after,
    })
}

/// `at` as an RFC 3339 UTC time to the second, such as `2026-10-22T08:30:00Z`.
pub fn utc(at: UnixMillis) -> String {
    let secs = at.get() / 1000;
    let (days, of_day) = (secs / 86_400, secs % 86_400);
    let (hour, minute, second) = (of_day / 3600, of_day % 3600 / 60, of_day % 60);
    // Howard Hinnant's civil_from_days, for days since 1970-01-01 (never negative here).
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn read_chain(path: &Path) -> Result<Vec<CertificateDer<'static>>, IdentityError> {
    let read_error = |source| IdentityError::ReadCertificate {
        path: path.to_owned(),
        source,
    };
    let chain = CertificateDer::pem_file_iter(path)
        .map_err(read_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(read_error)?;
    if chain.is_empty() {
        return Err(IdentityError::NoCertificate(path.to_owned()));
    }
    Ok(chain)
}

fn parse_leaf(path: &Path, chain: &[CertificateDer<'static>]) -> Result<Leaf, IdentityError> {
    let bad = |reason: String| IdentityError::BadCertificate {
        path: path.to_owned(),
        reason,
    };
    let der = chain
        .first()
        .ok_or_else(|| IdentityError::NoCertificate(path.to_owned()))?;
    let (rest, certificate) =
        X509Certificate::from_der(der.as_ref()).map_err(|error| bad(error.to_string()))?;
    if !rest.is_empty() {
        return Err(bad("trailing bytes after the certificate".to_owned()));
    }
    let names = certificate
        .subject()
        .iter_common_name()
        .map(|name| name.as_str().map(str::to_owned))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| bad(format!("the CN is not a string: {error}")))?;
    let [name] = names.as_slice() else {
        return Err(IdentityError::CommonNames(names.len()));
    };
    let gateway = GatewayId::new(name.as_str()).map_err(IdentityError::GatewayId)?;
    match certificate.extended_key_usage() {
        Ok(Some(usage)) if !(usage.value.client_auth || usage.value.any) => {
            return Err(IdentityError::NotForClients);
        }
        Ok(_) => {}
        Err(error) => return Err(bad(format!("extended key usage: {error}"))),
    }
    // OpenSSL (Mosquitto) refuses a client certificate whose key usage leaves out
    // digitalSignature; webpki does not look, so the agent checks it here.
    match certificate.key_usage() {
        Ok(Some(usage)) if !usage.value.digital_signature() => {
            return Err(IdentityError::NoDigitalSignature);
        }
        Ok(_) => {}
        Err(error) => return Err(bad(format!("key usage: {error}"))),
    }
    let millis = |secs: i64| UnixMillis::new(u64::try_from(secs).unwrap_or(0).saturating_mul(1000));
    let validity = certificate.validity();
    Ok(Leaf {
        gateway,
        not_before: millis(validity.not_before.timestamp()),
        not_after: millis(validity.not_after.timestamp()),
        spki: certificate.public_key().raw.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use rcgen::{CertificateParams, DnType, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose};

    use super::*;

    struct Files {
        dir: PathBuf,
    }

    impl Files {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("agent-identity-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.dir.join(name);
            std::fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for Files {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn certificate(
        common_names: &[&str],
        usage: Option<ExtendedKeyUsagePurpose>,
        key: &KeyPair,
    ) -> String {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        // rcgen names every certificate "rcgen self signed cert" unless told otherwise.
        params.distinguished_name = rcgen::DistinguishedName::new();
        for (index, name) in common_names.iter().enumerate() {
            // The map holds one value per type: a second CN goes in under the raw OID 2.5.4.3.
            let kind = if index == 0 {
                DnType::CommonName
            } else {
                DnType::CustomDnType(vec![2, 5, 4, 3])
            };
            params.distinguished_name.push(kind, *name);
        }
        if let Some(usage) = usage {
            params.extended_key_usages = vec![usage];
        }
        params.self_signed(key).unwrap().pem()
    }

    #[test]
    fn a_matching_file_key_gives_the_gateway_id_from_the_cn() {
        let files = Files::new("ok");
        let key = KeyPair::generate().unwrap();
        let cert = files.write(
            "cert.pem",
            &certificate(
                &["gw-0001"],
                Some(ExtendedKeyUsagePurpose::ClientAuth),
                &key,
            ),
        );
        let key_path = files.write("key.pem", &key.serialize_pem());
        let identity = load(&cert, &KeySource::File { path: key_path }).unwrap();
        assert_eq!(identity.gateway.as_str(), "gw-0001");
        assert_eq!(identity.chain.len(), 1);
        assert!(identity.not_after.get() > 0);
    }

    #[test]
    fn identities_that_could_mislead_the_broker_are_refused() {
        let files = Files::new("refused");
        let key = KeyPair::generate().unwrap();
        let other = KeyPair::generate().unwrap();
        let key_path = files.write("key.pem", &key.serialize_pem());
        let file_key = KeySource::File {
            path: key_path.clone(),
        };
        let cases: Vec<(String, &str)> = vec![
            (certificate(&[], None, &key), "exactly one CN"),
            (certificate(&["gw-a", "gw-b"], None, &key), "it has 2"),
            (
                certificate(&["gw/+#"], None, &key),
                "not a valid gateway ID",
            ),
            (
                certificate(&["gw-1"], Some(ExtendedKeyUsagePurpose::ServerAuth), &key),
                "clientAuth",
            ),
            (certificate(&["gw-1"], None, &other), "does not belong"),
            (
                {
                    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
                    params.distinguished_name = rcgen::DistinguishedName::new();
                    params.distinguished_name.push(DnType::CommonName, "gw-1");
                    params.key_usages = vec![KeyUsagePurpose::KeyEncipherment];
                    params.self_signed(&key).unwrap().pem()
                },
                "digitalSignature",
            ),
        ];
        for (index, (pem, expected)) in cases.into_iter().enumerate() {
            let cert = files.write(&format!("cert-{index}.pem"), &pem);
            let error = load(&cert, &file_key).unwrap_err().to_string();
            assert!(error.contains(expected), "case {index}: {error}");
        }
        let empty = files.write("empty.pem", "");
        assert!(matches!(
            load(&empty, &file_key),
            Err(IdentityError::NoCertificate(_))
        ));
        let missing = files.dir.join("missing.pem");
        assert!(matches!(
            load(&missing, &file_key),
            Err(IdentityError::ReadCertificate { .. })
        ));
        let cert = files.write("cert.pem", &certificate(&["gw-1"], None, &key));
        let no_key = KeySource::File {
            path: files.write("nokey.pem", ""),
        };
        assert!(matches!(
            load(&cert, &no_key),
            Err(IdentityError::ReadKey { .. })
        ));
    }

    #[test]
    fn times_are_written_as_rfc_3339_utc() {
        for (millis, expected) in [
            (0, "1970-01-01T00:00:00Z"),
            (951_782_400_000, "2000-02-29T00:00:00Z"),
            (1_609_459_199_000, "2020-12-31T23:59:59Z"),
            (1_792_659_600_999, "2026-10-22T09:00:00Z"),
            (4_133_980_799_000, "2100-12-31T23:59:59Z"),
            (67_090_118_400_000, "4096-01-01T00:00:00Z"),
        ] {
            assert_eq!(utc(UnixMillis::new(millis)), expected, "{millis}");
        }
    }

    #[test]
    fn the_tpm_source_reports_why_it_cannot_open() {
        let files = Files::new("tpm");
        let key = KeyPair::generate().unwrap();
        let cert = files.write("cert.pem", &certificate(&["gw-1"], None, &key));
        let error = load(
            &cert,
            &KeySource::Tpm {
                device: files.dir.join("no-such-tpm"),
                handle: DEFAULT_TPM_HANDLE,
            },
        )
        .unwrap_err();
        assert!(matches!(error, IdentityError::Tpm(_)), "{error}");
    }
}
