//! The root CRL of the offline ceremony (services/certs/README.md, 「根 CRL」;
//! Laysi/trustiot-v2#829): `certs-ca root crl` and `ceremony::root_crl`.
//!
//! - webpki, as the gateway agent configures it for the broker's chain (rustls's
//!   `WebPkiServerVerifier` with the root CRL and unknown status allowed), refuses a server
//!   certificate under the revoked issuing CA and accepts one under another generation; a CRL
//!   signed by another key with the root's name is an error, so the refusal proves webpki
//!   checked the signature with the root's key.
//! - The DER fields the agent and webpki depend on: version 2, ecdsa-with-SHA384 encoded
//!   without parameters (webpki compares the bytes), the issuer is the root's subject byte for
//!   byte, the authority key identifier is the root's subject key identifier, a CRL number, no
//!   issuing distribution point, no delta CRL indicator, no critical extension, nextUpdate 400
//!   days after thisUpdate.
//! - Numbering, carrying entries over and dropping them when their certificates expire, on a
//!   fixed clock.
//! - Refusals: a previous root CRL of another root, a certificate to revoke that this root did
//!   not issue, an existing output file, the key from a terminal, and the other misuse.

#![allow(
    unsafe_code,
    reason = "the terminal test opens a pseudo-terminal with libc; each block says why it is sound"
)]

mod support;

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use certs::ceremony::{
    self, CeremonyError, MAX_ROOT_CRL_BYTES, ROOT_CRL_DAYS, ROOT_CRL_RETENTION_DAYS, RootCrlInputs,
    RootRevocationReason, SignedRootCrl,
};
use rcgen::{
    CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    PKCS_ECDSA_P256_SHA256, SanType,
};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier as _;
use rustls::{CertRevocationListError, CertificateError, RootCertStore};
use rustls_pki_types::{CertificateDer, CertificateRevocationListDer, ServerName, UnixTime};
use sha2::{Digest as _, Sha256};
use support::{CERTS_CA, now, temp_dir};
use time::OffsetDateTime;
use x509_parser::certificate::X509Certificate;
use x509_parser::extensions::ParsedExtension;
use x509_parser::prelude::FromDer;
use x509_parser::revocation_list::CertificateRevocationList;
use x509_parser::x509::X509Version;

const ROOT_NAME: &str = "TrustIoT test offline root";
/// The name the gateway checks in the broker's certificate (edge/agent `BROKER_NAME`).
const BROKER_NAME: &str = "mqtt-broker.trustiot.internal";
const DAY: i64 = 86_400;
/// A fixed clock for the numbering test: 2027-01-15T08:00:00Z.
const T0: i64 = 1_800_000_000;

/// The DER of ecdsa-with-SHA384's AlgorithmIdentifier as webpki expects it (RFC 5758 3.2: no
/// parameters): SEQUENCE { OID 1.2.840.10045.4.3.3 }.
const ECDSA_SHA384_ALGORITHM: [u8; 12] = [
    0x30, 0x0a, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03,
];

fn der(pem: &str, label: &str) -> Vec<u8> {
    let mut blocks = certs::pem::decode_all(pem, label).expect("PEM");
    assert_eq!(blocks.len(), 1, "one {label}");
    blocks.remove(0)
}

/// A root and the issuing CAs it signs, as the ceremony makes them.
struct Root {
    key: KeyPair,
    pem: String,
}

impl Root {
    fn new(name: &str, at: i64) -> Self {
        let key = ceremony::new_root_key().expect("a root key");
        let pem = ceremony::root_certificate(&key, name, 3650, at).expect("a root certificate");
        Self { key, pem }
    }

    fn der(&self) -> Vec<u8> {
        der(&self.pem, certs::pem::CERTIFICATE)
    }

    /// An issuing CA named `name`, valid from `at` for `days`, and its key.
    fn issuing(&self, name: &str, days: u32, at: i64) -> (String, KeyPair) {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a P-256 key");
        let request = CertificateParams::default()
            .serialize_request(&key)
            .expect("a CSR")
            .pem()
            .expect("PEM");
        let pem = ceremony::issuing_certificate(&self.key, &self.pem, &request, name, days, at)
            .expect("an issuing CA certificate");
        (pem, key)
    }

    fn crl(&self, inputs: Inputs<'_>, at: i64) -> Result<SignedRootCrl, CeremonyError> {
        ceremony::root_crl(
            &self.key,
            &RootCrlInputs {
                root_pem: &self.pem,
                previous_crl_pem: inputs.previous,
                previous_certificates_pem: &inputs.previous_certificates.concat(),
                revoke_pem: (!inputs.revoke.is_empty())
                    .then(|| inputs.revoke.concat())
                    .as_deref(),
                reason: inputs.reason,
            },
            at,
        )
    }
}

#[derive(Clone, Copy)]
struct Inputs<'a> {
    previous: Option<&'a str>,
    previous_certificates: &'a [&'a str],
    revoke: &'a [&'a str],
    reason: RootRevocationReason,
}

const FIRST: Inputs<'static> = Inputs {
    previous: None,
    previous_certificates: &[],
    revoke: &[],
    reason: RootRevocationReason::Unspecified,
};

fn not_after_of(certificate_pem: &str) -> i64 {
    let der = der(certificate_pem, certs::pem::CERTIFICATE);
    let (_, certificate) = X509Certificate::from_der(&der).expect("a certificate");
    certificate.validity().not_after.timestamp()
}

fn sha256_of(crl_pem: &str) -> String {
    certs::issue::hex(&Sha256::digest(der(crl_pem, certs::pem::CRL)))
}

fn serial_of(certificate_pem: &str) -> Vec<u8> {
    let der = der(certificate_pem, certs::pem::CERTIFICATE);
    let (_, certificate) = X509Certificate::from_der(&der).expect("a certificate");
    certificate.raw_serial().to_vec()
}

/// The entries of `crl` (PEM): serial number, revocation time and reason code.
fn entries(crl: &str) -> Vec<(Vec<u8>, i64, Option<u8>)> {
    let der = der(crl, certs::pem::CRL);
    let (_, crl) = CertificateRevocationList::from_der(&der).expect("a CRL");
    crl.iter_revoked_certificates()
        .map(|entry| {
            (
                entry.raw_serial().to_vec(),
                entry.revocation_date.timestamp(),
                entry.reason_code().map(|(_, code)| code.0),
            )
        })
        .collect()
}

/// Checks the DER fields of the root CRL `crl` (PEM) that README.md 「根 CRL」 「格式」 and the
/// gateway agent require, and returns its thisUpdate.
fn check_format(crl: &str, root: &Root, number: u64) -> i64 {
    assert!(crl.len() <= MAX_ROOT_CRL_BYTES);
    assert!(crl.starts_with("-----BEGIN X509 CRL-----\n"), "{crl}");
    let der = der(crl, certs::pem::CRL);
    let root_der = root.der();
    let (_, root_certificate) = X509Certificate::from_der(&root_der).expect("the root");
    let (rest, parsed) = CertificateRevocationList::from_der(&der).expect("a CRL");
    assert!(rest.is_empty());
    assert_eq!(parsed.version(), Some(X509Version::V2));
    // The signature AlgorithmIdentifier, inside the signed part and after it, byte for byte.
    let algorithms = der
        .windows(ECDSA_SHA384_ALGORITHM.len())
        .filter(|window| *window == ECDSA_SHA384_ALGORITHM)
        .count();
    assert_eq!(algorithms, 2, "ecdsa-with-SHA384 without parameters, twice");
    assert_eq!(
        parsed.issuer().as_raw(),
        root_certificate.subject().as_raw(),
        "the issuer is the root's subject, byte for byte"
    );
    parsed
        .verify_signature(root_certificate.public_key())
        .expect("signed by the root's key");
    let root_key_id = root_certificate
        .extensions()
        .iter()
        .find_map(|extension| match extension.parsed_extension() {
            ParsedExtension::SubjectKeyIdentifier(id) => Some(id.0.to_vec()),
            _ => None,
        })
        .expect("the root's subject key identifier");
    let oids: Vec<String> = parsed
        .extensions()
        .iter()
        .map(|extension| extension.oid.to_id_string())
        .collect();
    // authorityKeyIdentifier and cRLNumber only: no issuingDistributionPoint (2.5.29.28), no
    // deltaCRLIndicator (2.5.29.27), nothing else.
    assert_eq!(oids, ["2.5.29.35", "2.5.29.20"]);
    for extension in parsed.extensions() {
        assert!(!extension.critical, "{} is not critical", extension.oid);
        match extension.parsed_extension() {
            ParsedExtension::AuthorityKeyIdentifier(id) => {
                assert_eq!(
                    id.key_identifier.as_ref().map(|id| id.0),
                    Some(&root_key_id[..])
                );
                assert!(id.authority_cert_issuer.is_none() && id.authority_cert_serial.is_none());
            }
            ParsedExtension::CRLNumber(value) => {
                assert_eq!(u64::try_from(value).expect("a small number"), number);
            }
            other => panic!("unexpected extension {other:?}"),
        }
    }
    let this_update = parsed.last_update().timestamp();
    let next_update = parsed.next_update().expect("a nextUpdate").timestamp();
    assert_eq!(next_update - this_update, ROOT_CRL_DAYS * DAY, "400 days");
    assert_eq!(ROOT_CRL_DAYS, 400);
    this_update
}

/// A broker server certificate signed by the issuing CA `issuing` (PEM, key `issuing_key`), as
/// Laysi/trustiot-v2#557's profile has it: the fixed name, EKU serverAuth.
fn server_certificate(issuing: &str, issuing_key: &KeyPair) -> CertificateDer<'static> {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
    let issuer = Issuer::from_ca_cert_der(
        &CertificateDer::from(der(issuing, certs::pem::CERTIFICATE)),
        issuing_key,
    )
    .expect("the issuing CA as an issuer");
    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, BROKER_NAME);
    params.subject_alt_names = vec![SanType::DnsName(
        BROKER_NAME.try_into().expect("a DNS name"),
    )];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.is_ca = IsCa::ExplicitNoCa;
    params.use_authority_key_identifier_extension = true;
    params.not_before = OffsetDateTime::from_unix_timestamp(now() - 3600).expect("a time");
    params.not_after = OffsetDateTime::from_unix_timestamp(now() + DAY).expect("a time");
    params
        .signed_by(&key, &issuer)
        .expect("a server certificate")
        .der()
        .clone()
}

/// What webpki says about the broker's chain `server` + `issuing` under `root` with the root CRL
/// `crl`, configured as the gateway agent's inner TLS (edge/agent/src/tls.rs, `inner_config`).
fn webpki_verdict(
    root: &Root,
    crl: &str,
    issuing: &str,
    server: &CertificateDer<'static>,
) -> Result<(), rustls::Error> {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(root.der()))
        .expect("the root as a trust anchor");
    let verifier = WebPkiServerVerifier::builder_with_provider(
        Arc::new(roots),
        Arc::new(rustls::crypto::ring::default_provider()),
    )
    .with_crls([CertificateRevocationListDer::from(der(
        crl,
        certs::pem::CRL,
    ))])
    .allow_unknown_revocation_status()
    .build()
    .expect("webpki takes the root CRL");
    verifier
        .verify_server_cert(
            server,
            &[CertificateDer::from(der(issuing, certs::pem::CERTIFICATE))],
            &ServerName::try_from(BROKER_NAME).expect("a server name"),
            &[],
            UnixTime::since_unix_epoch(Duration::from_secs(
                u64::try_from(now()).expect("after 1970"),
            )),
        )
        .map(drop)
}

#[test]
fn webpki_refuses_the_issuing_ca_a_root_crl_revokes() {
    let at = now();
    let root = Root::new(ROOT_NAME, at);
    let (generation_1, key_1) = root.issuing("TrustIoT test issuing CA 1", 730, at);
    let (generation_2, key_2) = root.issuing("TrustIoT test issuing CA 2", 730, at);
    let server_1 = server_certificate(&generation_1, &key_1);
    let server_2 = server_certificate(&generation_2, &key_2);

    let first = root.crl(FIRST, at).expect("the first root CRL");
    assert_eq!(first.number, 1);
    assert!(first.listed.is_empty() && first.removed.is_empty());
    let this_update = check_format(&first.pem, &root, 1);
    assert_eq!(this_update, at - ceremony::BACKDATE_SECS);
    assert_eq!(first.this_update, this_update);
    assert!(entries(&first.pem).is_empty());
    webpki_verdict(&root, &first.pem, &generation_1, &server_1)
        .expect("nothing revoked yet: generation 1 is accepted");

    let second = root
        .crl(
            Inputs {
                previous: Some(&first.pem),
                revoke: &[&generation_1],
                reason: RootRevocationReason::CaCompromise,
                ..FIRST
            },
            at,
        )
        .expect("the second root CRL");
    assert_eq!(second.number, 2);
    check_format(&second.pem, &root, 2);
    assert_eq!(
        entries(&second.pem),
        [(serial_of(&generation_1), second.this_update, Some(2))],
        "generation 1, revoked at thisUpdate, cACompromise"
    );
    assert!(second.listed[0].new);
    assert_eq!(
        webpki_verdict(&root, &second.pem, &generation_1, &server_1),
        Err(rustls::Error::InvalidCertificate(CertificateError::Revoked)),
        "the revoked issuing CA's server certificate"
    );
    webpki_verdict(&root, &second.pem, &generation_2, &server_2)
        .expect("generation 2 is not revoked");
    println!(
        "root CRL 2: {} bytes PEM, generation 1 refused as revoked, generation 2 accepted",
        second.pem.len()
    );

    // The same CRL from another key with the root's name: webpki checks the CRL's signature
    // with the root's key and fails, rather than treating it as revoking anything.
    let impostor = Root::new(ROOT_NAME, at);
    let forged = ceremony::root_crl(
        &impostor.key,
        &RootCrlInputs {
            root_pem: &impostor.pem,
            previous_crl_pem: None,
            previous_certificates_pem: "",
            revoke_pem: None,
            reason: RootRevocationReason::Unspecified,
        },
        at,
    )
    .expect("a CRL of another root");
    let verdict = webpki_verdict(&root, &forged.pem, &generation_1, &server_1);
    assert!(
        matches!(
            verdict,
            Err(rustls::Error::InvalidCertRevocationList(
                CertRevocationListError::BadSignature
            ))
        ),
        "{verdict:?}"
    );
    println!("a CRL with the root's name but another key: {verdict:?}");
}

#[test]
fn entries_carry_over_until_400_days_after_their_certificates_expire() {
    let root = Root::new(ROOT_NAME, T0);
    // Generation 1 ends after 2 days, generation 2 after 730.
    let (generation_1, _) = root.issuing("TrustIoT test issuing CA 1", 2, T0);
    let (generation_2, _) = root.issuing("TrustIoT test issuing CA 2", 730, T0);
    let (generation_3, _) = root.issuing("TrustIoT test issuing CA 3", 730, T0);
    let serial_1 = serial_of(&generation_1);
    let serial_2 = serial_of(&generation_2);
    let serial_3 = serial_of(&generation_3);
    let expiry_1 = not_after_of(&generation_1);
    assert_eq!(ROOT_CRL_RETENTION_DAYS, 400);

    let first = root
        .crl(
            Inputs {
                revoke: &[&generation_1],
                reason: RootRevocationReason::Superseded,
                ..FIRST
            },
            T0,
        )
        .expect("root CRL 1");
    assert_eq!(first.number, 1);
    assert_eq!(first.this_update, T0 - ceremony::BACKDATE_SECS);
    assert_eq!(first.next_update, first.this_update + 400 * DAY);
    assert_eq!(first.previous_sha256, None);
    assert_eq!(first.sha256, sha256_of(&first.pem));
    assert_eq!(
        entries(&first.pem),
        [(serial_1.clone(), first.this_update, Some(4))]
    );
    assert_eq!(
        first.listed[0].key_id,
        ceremony::certificate_key_id(&generation_1).expect("its key id"),
        "the entry names the revoked certificate's key"
    );

    // A day later: generation 1 carries over with its time and reason; generation 2 is new.
    let second = root
        .crl(
            Inputs {
                previous: Some(&first.pem),
                previous_certificates: &[&generation_1],
                revoke: &[&generation_2],
                reason: RootRevocationReason::CaCompromise,
            },
            T0 + DAY,
        )
        .expect("root CRL 2");
    assert_eq!(second.number, 2);
    assert_eq!(
        second.previous_sha256.as_deref(),
        Some(first.sha256.as_str())
    );
    check_format(&second.pem, &root, 2);
    assert_eq!(
        entries(&second.pem),
        [
            (serial_1.clone(), first.this_update, Some(4)),
            (serial_2.clone(), second.this_update, Some(2)),
        ]
    );
    assert_eq!(
        second
            .listed
            .iter()
            .map(|entry| entry.new)
            .collect::<Vec<_>>(),
        [false, true]
    );
    assert!(second.removed.is_empty());

    // Ten days in: generation 1 has expired but stays, because a gateway whose clock runs
    // behind still trusts it (and this machine's clock may run ahead). The archive of every
    // revoked certificate may be given whole.
    let archive: &[&str] = &[&generation_1, &generation_2];
    let third = root
        .crl(
            Inputs {
                previous: Some(&second.pem),
                previous_certificates: archive,
                ..FIRST
            },
            T0 + 10 * DAY,
        )
        .expect("root CRL 3");
    assert!(expiry_1 < third.this_update, "generation 1 has expired");
    assert_eq!(third.number, 3);
    check_format(&third.pem, &root, 3);
    assert_eq!(
        entries(&third.pem),
        [
            (serial_1.clone(), first.this_update, Some(4)),
            (serial_2.clone(), second.this_update, Some(2)),
        ],
        "an expired certificate stays listed"
    );
    assert!(third.removed.is_empty());

    // The last day it stays: thisUpdate exactly 400 days after its notAfter.
    let last = expiry_1 + ROOT_CRL_RETENTION_DAYS * DAY + ceremony::BACKDATE_SECS;
    let fourth = root
        .crl(
            Inputs {
                previous: Some(&third.pem),
                previous_certificates: archive,
                ..FIRST
            },
            last,
        )
        .expect("root CRL 4");
    assert_eq!(fourth.this_update, expiry_1 + 400 * DAY);
    assert_eq!(
        entries(&fourth.pem).len(),
        2,
        "still listed on the last day"
    );
    assert!(fourth.removed.is_empty());

    // A second later it goes; generation 2 stays.
    let fifth = root
        .crl(
            Inputs {
                previous: Some(&fourth.pem),
                previous_certificates: archive,
                ..FIRST
            },
            last + 1,
        )
        .expect("root CRL 5");
    assert_eq!(fifth.number, 5);
    check_format(&fifth.pem, &root, 5);
    assert_eq!(
        entries(&fifth.pem),
        [(serial_2.clone(), second.this_update, Some(2))]
    );
    assert_eq!(fifth.removed.len(), 1);
    assert_eq!(fifth.removed[0].serial, certs::issue::hex(&serial_1));

    // The removed generation 1 in the archive is ignored from now on; a certificate added to
    // the archive before it is revoked is fine.
    let sixth_at = last + DAY;
    let sixth = root
        .crl(
            Inputs {
                previous: Some(&fifth.pem),
                previous_certificates: &[&generation_1, &generation_2, &generation_3],
                revoke: &[&generation_3],
                reason: RootRevocationReason::CessationOfOperation,
            },
            sixth_at,
        )
        .expect("root CRL 6");
    assert_eq!(sixth.number, 6);
    assert_eq!(
        entries(&sixth.pem),
        [
            (serial_2.clone(), second.this_update, Some(2)),
            (serial_3, sixth.this_update, Some(5)),
        ]
    );
    assert!(sixth.removed.is_empty());

    // An expired certificate within the 400 days may still be revoked.
    let late = root
        .crl(
            Inputs {
                revoke: &[&generation_1],
                reason: RootRevocationReason::CaCompromise,
                ..FIRST
            },
            T0 + 10 * DAY,
        )
        .expect("revoking a certificate that expired 8 days ago");
    assert_eq!(entries(&late.pem).len(), 1);

    // Refusals, each with the problem named.
    let refused =
        |inputs: Inputs<'_>, at: i64| root.crl(inputs, at).expect_err("refused").to_string();
    let other_root = Root::new("TrustIoT test other root", T0);
    let impostor = Root::new(ROOT_NAME, T0);
    let other_first = other_root.crl(FIRST, T0).expect("another root's CRL");
    let impostor_first = impostor.crl(FIRST, T0).expect("an impostor's CRL");
    let (foreign, _) = other_root.issuing("TrustIoT test issuing CA 1", 730, T0);
    let (forged, _) = impostor.issuing("TrustIoT test issuing CA 9", 730, T0);
    let leaf = {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
        let issuer = Issuer::from_ca_cert_der(&CertificateDer::from(root.der()), &root.key)
            .expect("the root as an issuer");
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, "not a CA");
        params.is_ca = IsCa::ExplicitNoCa;
        params.signed_by(&key, &issuer).expect("a leaf").pem()
    };
    let at = sixth_at;
    let cases: [(&str, Inputs<'_>, i64, &str); 11] = [
        (
            "a previous root CRL of another root",
            Inputs {
                previous: Some(&other_first.pem),
                ..FIRST
            },
            at,
            "the previous root CRL: not issued by this root",
        ),
        (
            "a previous root CRL signed by another key with the root's name",
            Inputs {
                previous: Some(&impostor_first.pem),
                ..FIRST
            },
            at,
            "the previous root CRL: not signed by this root's key",
        ),
        (
            "a previous root CRL newer than the clock",
            Inputs {
                previous: Some(&fifth.pem),
                previous_certificates: &[&generation_2],
                ..FIRST
            },
            T0,
            "lies after this machine's time",
        ),
        (
            "an entry without its certificate",
            Inputs {
                previous: Some(&fifth.pem),
                ..FIRST
            },
            at,
            "but its certificate is not among them",
        ),
        (
            "a certificate within the retention that is not listed",
            Inputs {
                previous: Some(&fifth.pem),
                previous_certificates: &[&generation_2, &generation_3],
                ..FIRST
            },
            at,
            "the previous root CRL does not list it",
        ),
        (
            "revoking a certificate of another root",
            Inputs {
                revoke: &[&foreign],
                ..FIRST
            },
            at,
            "the certificates to revoke: block 1 (CN=TrustIoT test issuing CA 1) was not issued by this root",
        ),
        (
            "revoking a certificate signed by another key with the root's name",
            Inputs {
                revoke: &[&forged],
                ..FIRST
            },
            at,
            "is not signed by this root's key",
        ),
        (
            "revoking the root",
            Inputs {
                revoke: &[&root.pem],
                ..FIRST
            },
            at,
            "is the root itself",
        ),
        (
            "revoking a certificate that is not a CA",
            Inputs {
                revoke: &[&leaf],
                ..FIRST
            },
            at,
            "is not a CA certificate",
        ),
        (
            "revoking a certificate again",
            Inputs {
                previous: Some(&fifth.pem),
                previous_certificates: &[&generation_2],
                revoke: &[&generation_2],
                ..FIRST
            },
            at,
            "is already revoked by the previous root CRL",
        ),
        (
            "revoking a certificate that expired more than 400 days ago",
            Inputs {
                revoke: &[&generation_1],
                ..FIRST
            },
            at,
            "expired more than 400 days ago",
        ),
    ];
    for (case, inputs, at, expected) in cases {
        let message = refused(inputs, at);
        assert!(message.contains(expected), "{case}: {message}");
        println!("{case}: {message}");
    }
    let mismatch = ceremony::root_crl(
        &impostor.key,
        &RootCrlInputs {
            root_pem: &root.pem,
            previous_crl_pem: None,
            previous_certificates_pem: "",
            revoke_pem: None,
            reason: RootRevocationReason::Unspecified,
        },
        at,
    )
    .expect_err("another key");
    assert!(
        matches!(mismatch, CeremonyError::RootKeyMismatch),
        "{mismatch}"
    );
    for empty in ["", "\n\n"] {
        let refused = ceremony::root_crl(
            &root.key,
            &RootCrlInputs {
                root_pem: &root.pem,
                previous_crl_pem: None,
                previous_certificates_pem: "",
                revoke_pem: Some(empty),
                reason: RootRevocationReason::CaCompromise,
            },
            at,
        )
        .expect_err("nothing to revoke");
        assert!(
            refused.to_string().contains("it holds no certificate"),
            "{refused}"
        );
        println!("an empty file of certificates to revoke: {refused}");
    }
    let expired = root
        .crl(FIRST, T0 + 3651 * DAY)
        .expect_err("an expired root");
    assert!(matches!(expired, CeremonyError::RootExpired), "{expired}");
}

/// How `certs-ca` gets its stdin.
enum Stdin<'a> {
    Text(&'a str),
    /// A pseudo-terminal, as when an operator would type the key.
    Terminal,
}

fn run(arguments: &[&str], stdin: Stdin<'_>) -> Output {
    let mut command = Command::new(CERTS_CA);
    command
        .args(arguments)
        .env_clear()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match stdin {
        Stdin::Text(text) => {
            command.stdin(Stdio::piped());
            let mut child = command.spawn().expect("start certs-ca");
            let mut pipe = child.stdin.take().expect("piped stdin");
            // certs-ca may refuse before it reads stdin; a closed pipe is fine then.
            let _ = pipe.write_all(text.as_bytes());
            drop(pipe);
            child.wait_with_output().expect("certs-ca finished")
        }
        Stdin::Terminal => {
            let (controller, terminal) = pseudo_terminal();
            command.stdin(Stdio::from(terminal));
            let output = command.output().expect("certs-ca finished");
            drop(controller);
            output
        }
    }
}

/// A new pseudo-terminal: its controlling side, and the terminal a child gets as stdin.
fn pseudo_terminal() -> (OwnedFd, File) {
    // SAFETY: posix_openpt takes flags only and returns a new descriptor or -1.
    let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    assert!(fd >= 0, "posix_openpt: {}", std::io::Error::last_os_error());
    // SAFETY: fd is a descriptor posix_openpt just returned, owned by nothing else.
    let controller = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: grantpt and unlockpt only act on the descriptor, which stays open.
    let granted = unsafe {
        libc::grantpt(controller.as_raw_fd()) == 0 && libc::unlockpt(controller.as_raw_fd()) == 0
    };
    assert!(
        granted,
        "grantpt/unlockpt: {}",
        std::io::Error::last_os_error()
    );
    let mut name = [0_u8; 128];
    // SAFETY: the buffer is writable for its whole length, which is passed along; the
    // descriptor stays open.
    let found =
        unsafe { libc::ptsname_r(controller.as_raw_fd(), name.as_mut_ptr().cast(), name.len()) };
    assert_eq!(found, 0, "ptsname_r");
    let name = CStr::from_bytes_until_nul(&name)
        .expect("a terminated name")
        .to_str()
        .expect("a UTF-8 name")
        .to_owned();
    let terminal = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(&name)
        .expect("open the terminal");
    (controller, terminal)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn path(file: &Path) -> &str {
    file.to_str().expect("a UTF-8 path")
}

/// The base64 body of a PEM block, without its armour and line breaks.
fn pem_body(pem: &str) -> String {
    pem.lines()
        .filter(|line| !line.starts_with("-----"))
        .collect()
}

#[test]
fn certs_ca_root_crl_reads_the_key_from_stdin_and_refuses_misuse() {
    let dir = temp_dir("root-crl");
    let at = now();
    let root = Root::new(ROOT_NAME, at);
    let root_key = root.key.serialize_pem();
    let key_head: String = pem_body(&root_key).chars().take(24).collect();
    let root_file = dir.join("root.pem");
    std::fs::write(&root_file, &root.pem).expect("write the root");
    let (generation_1, key_1) = root.issuing("TrustIoT test issuing CA 1", 730, at);
    let generation_1_file = dir.join("issuing-ca-generation-1.pem");
    std::fs::write(&generation_1_file, &generation_1).expect("write generation 1");
    let crl_1 = dir.join("root-crl-1.pem");
    let crl_2 = dir.join("root-crl-2.pem");

    let first = run(
        &[
            "root",
            "crl",
            "--root-certificate",
            path(&root_file),
            "--previous-crl",
            "none",
            "--previous-certificates",
            "none",
            "--crl",
            path(&crl_1),
        ],
        Stdin::Text(&root_key),
    );
    let stderr = text(&first.stderr);
    assert!(first.status.success(), "{stderr}");
    assert!(first.stdout.is_empty());
    assert!(!stderr.contains(&key_head), "{stderr}");
    assert!(
        stderr.contains("no previous root CRL: this is number 1"),
        "{stderr}"
    );
    println!("root crl, first: {}", stderr.trim());
    let first_pem = std::fs::read_to_string(&crl_1).expect("root CRL 1");
    check_format(&first_pem, &root, 1);
    assert!(
        stderr.contains(&format!(
            "wrote root CRL number 1 to {}: SHA-256 {}",
            crl_1.display(),
            sha256_of(&first_pem)
        )),
        "{stderr}"
    );

    let second = run(
        &[
            "root",
            "crl",
            "--root-certificate",
            path(&root_file),
            "--previous-crl",
            path(&crl_1),
            "--previous-certificates",
            "none",
            "--revoke",
            path(&generation_1_file),
            "--reason",
            "ca-compromise",
            "--crl",
            path(&crl_2),
        ],
        Stdin::Text(&root_key),
    );
    let stderr = text(&second.stderr);
    assert!(second.status.success(), "{stderr}");
    assert!(second.stdout.is_empty());
    assert!(!stderr.contains(&key_head), "{stderr}");
    assert!(stderr.contains("wrote root CRL number 2"), "{stderr}");
    let serial_1 = certs::issue::hex(&serial_of(&generation_1));
    let key_id_1 = ceremony::certificate_key_id(&generation_1).expect("its key id");
    assert!(
        stderr.contains(&format!(
            "lists {serial_1} CN=TrustIoT test issuing CA 1 key {key_id_1} (revoked now, \
             ca-compromise"
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "previous root CRL number 1, SHA-256 {}",
            sha256_of(&first_pem)
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains("set it only after the broker's server certificate"),
        "{stderr}"
    );
    println!("root crl, revoking generation 1: {}", stderr.trim());
    let second_pem = std::fs::read_to_string(&crl_2).expect("root CRL 2");
    check_format(&second_pem, &root, 2);
    assert_eq!(
        webpki_verdict(
            &root,
            &second_pem,
            &generation_1,
            &server_certificate(&generation_1, &key_1)
        ),
        Err(rustls::Error::InvalidCertificate(CertificateError::Revoked)),
    );

    // Misuse: refused, nothing written, no key printed.
    let other_root = Root::new("TrustIoT test other root", at);
    let other_key = other_root.key.serialize_pem();
    let other_root_file = dir.join("other-root.pem");
    std::fs::write(&other_root_file, &other_root.pem).expect("write the other root");
    let other_crl = dir.join("other-root-crl.pem");
    std::fs::write(&other_crl, other_root.crl(FIRST, at).expect("its CRL").pem)
        .expect("write the other root's CRL");
    let (foreign, _) = other_root.issuing("TrustIoT test issuing CA 7", 730, at);
    let foreign_file = dir.join("foreign.pem");
    std::fs::write(&foreign_file, foreign).expect("write a foreign issuing CA");
    let new_file = dir.join("new.pem");
    let new = path(&new_file);
    let root_arg = path(&root_file);
    let empty_file = dir.join("empty.pem");
    std::fs::write(&empty_file, "").expect("write an empty file");
    let cases: [(&str, Vec<&str>, Stdin<'_>, i32, &str); 10] = [
        (
            "a previous root CRL of another root",
            vec![
                "--root-certificate",
                root_arg,
                "--previous-crl",
                path(&other_crl),
                "--previous-certificates",
                "none",
                "--crl",
                new,
            ],
            Stdin::Text(&root_key),
            1,
            "the previous root CRL: not issued by this root",
        ),
        (
            "revoking an issuing CA of another root",
            vec![
                "--root-certificate",
                root_arg,
                "--previous-crl",
                path(&crl_2),
                "--previous-certificates",
                path(&generation_1_file),
                "--revoke",
                path(&foreign_file),
                "--reason",
                "ca-compromise",
                "--crl",
                new,
            ],
            Stdin::Text(&root_key),
            1,
            "was not issued by this root",
        ),
        (
            "an existing output file",
            vec![
                "--root-certificate",
                root_arg,
                "--previous-crl",
                "none",
                "--previous-certificates",
                "none",
                "--crl",
                path(&crl_1),
            ],
            Stdin::Text(&root_key),
            1,
            "already exists",
        ),
        (
            "the key from a terminal",
            vec![
                "--root-certificate",
                root_arg,
                "--previous-crl",
                "none",
                "--previous-certificates",
                "none",
                "--crl",
                new,
            ],
            Stdin::Terminal,
            2,
            "stdin is a terminal",
        ),
        (
            "the key of another root",
            vec![
                "--root-certificate",
                root_arg,
                "--previous-crl",
                "none",
                "--previous-certificates",
                "none",
                "--crl",
                new,
            ],
            Stdin::Text(&other_key),
            1,
            "the root key does not belong to the root certificate",
        ),
        (
            "no --previous-crl",
            vec![
                "--root-certificate",
                root_arg,
                "--previous-certificates",
                "none",
                "--crl",
                new,
            ],
            Stdin::Text(&root_key),
            2,
            "--previous-crl is required",
        ),
        (
            "no --previous-certificates",
            vec![
                "--root-certificate",
                root_arg,
                "--previous-crl",
                path(&crl_2),
                "--crl",
                new,
            ],
            Stdin::Text(&root_key),
            2,
            "--previous-certificates is required",
        ),
        (
            "an empty --revoke file",
            vec![
                "--root-certificate",
                root_arg,
                "--previous-crl",
                path(&crl_2),
                "--previous-certificates",
                path(&generation_1_file),
                "--revoke",
                path(&empty_file),
                "--reason",
                "ca-compromise",
                "--crl",
                new,
            ],
            Stdin::Text(&root_key),
            1,
            "it holds no certificate",
        ),
        (
            "--revoke without --reason",
            vec![
                "--root-certificate",
                root_arg,
                "--previous-crl",
                "none",
                "--previous-certificates",
                "none",
                "--revoke",
                path(&generation_1_file),
                "--crl",
                new,
            ],
            Stdin::Text(&root_key),
            2,
            "--revoke needs --reason",
        ),
        (
            "an unknown reason",
            vec![
                "--root-certificate",
                root_arg,
                "--previous-crl",
                "none",
                "--previous-certificates",
                "none",
                "--revoke",
                path(&generation_1_file),
                "--reason",
                "key-compromise",
                "--crl",
                new,
            ],
            Stdin::Text(&root_key),
            2,
            "--reason must be",
        ),
    ];
    let other_key_head: String = pem_body(&other_key).chars().take(24).collect();
    for (case, options, stdin, code, expected) in cases {
        let mut arguments = vec!["root", "crl"];
        arguments.extend(options);
        let refused = run(&arguments, stdin);
        let stderr = text(&refused.stderr);
        assert_eq!(refused.status.code(), Some(code), "{case}: {stderr}");
        assert!(stderr.contains(expected), "{case}: {stderr}");
        assert!(refused.stdout.is_empty(), "{case}");
        assert!(
            !stderr.contains(&key_head) && !stderr.contains(&other_key_head),
            "{case}: {stderr}"
        );
        assert!(!new_file.exists(), "{case}: nothing written");
        println!("{case}: {}", stderr.lines().next().unwrap_or_default());
    }
    assert_eq!(
        std::fs::read_to_string(&crl_1).expect("root CRL 1"),
        first_pem,
        "an existing file is never replaced"
    );
}
