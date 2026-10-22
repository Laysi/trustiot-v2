//! Gateway certificates from CSRs (Laysi/trustiot-v2#78 acceptance 1): the CA takes only the
//! public key from a CSR. A CSR that asks for another common name, subject alternative names,
//! server authentication, code signing, any key usage or a CA certificate gets the gateway
//! profile the CA decides, and the result verifies as a TLS client certificate under the root.
//! CSRs the CA cannot use (another curve or algorithm, a signature that does not verify, a
//! changed subject, oversized or trailing bytes) are refused. The ledger records what was signed,
//! only for an issuing CA that has a CRL (Laysi/trustiot-v2#534), and the service role cannot
//! change what was signed; it can only mark a revocation.
//!
//! The ledger test starts a throwaway Postgres with TLS (podman or docker); skipped without
//! either unless `CI` is set. The others need nothing.

mod support;

use std::collections::BTreeSet;
use std::time::Duration;

use certs::authority::IssuingCa;
use certs::issue::{
    self, CLOCK_SKEW_SECS, IssueError, IssuedCertificate, MAX_REQUEST_BYTES, RecordError,
    SERIAL_BYTES,
};
use protocol::GatewayId;
use rcgen::{
    BasicConstraints, CertificateParams, CustomExtension, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
    PKCS_ECDSA_P384_SHA384, PKCS_ED25519, SanType,
};
use sqlx::Row as _;
use support::{TestCa, now, verify_client};
use testkit::product::{DATABASE, product_postgres};
use x509_parser::certificate::X509Certificate;
use x509_parser::certification_request::X509CertificationRequest;
use x509_parser::extensions::{GeneralName, ParsedExtension};
use x509_parser::oid_registry::{
    OID_X509_EXT_AUTHORITY_KEY_IDENTIFIER, OID_X509_EXT_BASIC_CONSTRAINTS,
    OID_X509_EXT_EXTENDED_KEY_USAGE, OID_X509_EXT_KEY_USAGE, OID_X509_EXT_SUBJECT_KEY_IDENTIFIER,
};
use x509_parser::prelude::FromDer;

const GATEWAY: &str = "gw-78-0001";
const FORGED_NAME: &str = "gw-forged-admin";
const VALIDITY: Duration = Duration::from_secs(90 * 86_400);

fn gateway() -> GatewayId {
    GatewayId::new(GATEWAY).expect("a gateway id")
}

/// A CSR for `key` that asks for everything the CA must not grant: another common name and an
/// organization, DNS, IP and URI subject alternative names, a CA certificate without a path
/// length limit, keyCertSign and cRLSign, server authentication, code signing and any extended
/// key usage, and a CRL distribution point.
fn forged_request(key: &KeyPair) -> Vec<u8> {
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, FORGED_NAME);
    name.push(DnType::OrganizationName, "Forged");
    params.distinguished_name = name;
    params.subject_alt_names = vec![
        SanType::DnsName("broker.example.invalid".try_into().expect("a DNS name")),
        SanType::IpAddress([192, 0, 2, 1].into()),
        SanType::URI("spiffe://example.invalid/admin".try_into().expect("a URI")),
    ];
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::CodeSigning,
        ExtendedKeyUsagePurpose::Any,
    ];
    // cRLDistributionPoints (2.5.29.31): SEQUENCE OF DistributionPoint { [0] { fullName [0]
    // { uniformResourceIdentifier [6] } } } with one 32-octet URI.
    let uri = b"http://crl.example.invalid/x.crl";
    assert_eq!(uri.len(), 0x20);
    let mut distribution_points = vec![0x30, 0x28, 0x30, 0x26, 0xa0, 0x24, 0xa0, 0x22, 0x86, 0x20];
    distribution_points.extend_from_slice(uri);
    params.custom_extensions = vec![CustomExtension::from_oid_content(
        &[2, 5, 29, 31],
        distribution_points,
    )];
    params.serialize_request(key).expect("a CSR").der().to_vec()
}

/// A plain CSR for `key`.
fn plain_request(key: &KeyPair) -> Vec<u8> {
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, GATEWAY);
    params.distinguished_name = name;
    params.serialize_request(key).expect("a CSR").der().to_vec()
}

fn issue(ca: &IssuingCa, request: &[u8]) -> Result<IssuedCertificate, IssueError> {
    issue::gateway_certificate(ca, request, &gateway(), VALIDITY, now())
}

/// The OIDs of a certificate's extensions, as dotted strings.
fn extension_oids(certificate: &X509Certificate<'_>) -> BTreeSet<String> {
    certificate
        .extensions()
        .iter()
        .map(|extension| extension.oid.to_id_string())
        .collect()
}

#[test]
fn the_ca_decides_everything_but_the_public_key() {
    let ca = TestCa::new(&[1]);
    let authority = ca.authority();
    let issuing = authority.current(now()).expect("an issuing CA");
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a gateway key");
    let request = forged_request(&key);

    // Positive control: the CSR really asks for all of it.
    let (_, parsed) = X509CertificationRequest::from_der(&request).expect("the CSR parses");
    let requested_cn: Vec<String> = parsed
        .certification_request_info
        .subject
        .iter_common_name()
        .filter_map(|cn| cn.as_str().ok().map(str::to_owned))
        .collect();
    assert_eq!(requested_cn, [FORGED_NAME]);
    let requested: Vec<&ParsedExtension<'_>> = parsed
        .requested_extensions()
        .expect("the CSR requests extensions")
        .collect();
    assert!(requested.iter().any(|extension| matches!(
        extension,
        ParsedExtension::ExtendedKeyUsage(eku) if eku.server_auth && eku.code_signing && eku.any
    )));
    assert!(requested.iter().any(|extension| matches!(
        extension,
        ParsedExtension::BasicConstraints(constraints) if constraints.ca
    )));
    assert!(requested.iter().any(|extension| matches!(
        extension,
        ParsedExtension::KeyUsage(usage) if usage.key_cert_sign() && usage.crl_sign()
    )));
    assert!(requested.iter().any(|extension| matches!(
        extension,
        ParsedExtension::SubjectAlternativeName(names) if names.general_names.len() == 3
    )));
    assert!(
        requested
            .iter()
            .any(|extension| matches!(extension, ParsedExtension::CRLDistributionPoints(_)))
    );

    let signing_time = now();
    let issued = issue(issuing, &request).expect("the CSR is signed");
    let (rest, certificate) = X509Certificate::from_der(&issued.der).expect("a certificate");
    assert!(rest.is_empty());

    // The subject is CN=<gateway id> and nothing else.
    assert_eq!(certificate.subject().iter_attributes().count(), 1);
    let cn: Vec<&str> = certificate
        .subject()
        .iter_common_name()
        .filter_map(|cn| cn.as_str().ok())
        .collect();
    assert_eq!(cn, [GATEWAY]);
    assert!(!certificate.subject().to_string().contains(FORGED_NAME));
    assert!(!certificate.subject().to_string().contains("Forged"));

    // Exactly the profile's extensions: no subject alternative name, no CRL distribution point.
    let expected: BTreeSet<String> = [
        OID_X509_EXT_BASIC_CONSTRAINTS,
        OID_X509_EXT_KEY_USAGE,
        OID_X509_EXT_EXTENDED_KEY_USAGE,
        OID_X509_EXT_SUBJECT_KEY_IDENTIFIER,
        OID_X509_EXT_AUTHORITY_KEY_IDENTIFIER,
    ]
    .iter()
    .map(|oid| oid.to_id_string())
    .collect();
    assert_eq!(extension_oids(&certificate), expected);

    let constraints = certificate
        .basic_constraints()
        .expect("one basicConstraints")
        .expect("basicConstraints present");
    assert!(constraints.critical, "basicConstraints is critical");
    assert!(!constraints.value.ca, "not a CA");
    assert_eq!(constraints.value.path_len_constraint, None);

    let usage = certificate
        .key_usage()
        .expect("one keyUsage")
        .expect("keyUsage present");
    assert!(usage.critical, "keyUsage is critical");
    assert!(usage.value.digital_signature());
    assert_eq!(usage.value.flags, 1, "digitalSignature only");

    let eku = certificate
        .extended_key_usage()
        .expect("one extendedKeyUsage")
        .expect("extendedKeyUsage present");
    assert!(eku.value.client_auth);
    assert!(
        !(eku.value.any
            || eku.value.server_auth
            || eku.value.code_signing
            || eku.value.email_protection
            || eku.value.time_stamping
            || eku.value.ocsp_signing),
        "clientAuth only: {:?}",
        eku.value
    );
    assert!(eku.value.other.is_empty());

    // The key is the CSR's; the issuer is the issuing CA, identified by its key identifier.
    assert_eq!(
        certificate.public_key().raw,
        parsed.certification_request_info.subject_pki.raw
    );
    let (_, issuing_certificate) =
        X509Certificate::from_der(issuing.certificate_der()).expect("the issuing CA certificate");
    assert_eq!(
        certificate.issuer().as_raw(),
        issuing_certificate.subject().as_raw()
    );
    let issuing_ski = issuing_certificate
        .extensions()
        .iter()
        .find_map(|extension| match extension.parsed_extension() {
            ParsedExtension::SubjectKeyIdentifier(id) => Some(id.0.to_vec()),
            _ => None,
        })
        .expect("the issuing CA has a subject key identifier");
    let authority_key_id = certificate
        .extensions()
        .iter()
        .find_map(|extension| match extension.parsed_extension() {
            ParsedExtension::AuthorityKeyIdentifier(aki) => {
                aki.key_identifier.as_ref().map(|id| id.0.to_vec())
            }
            _ => None,
        })
        .expect("an authority key identifier");
    assert_eq!(authority_key_id, issuing_ski);
    certificate
        .verify_signature(Some(issuing_certificate.public_key()))
        .expect("signed by the issuing CA");

    // A positive 128-bit serial number, recorded as the certificate's.
    assert_eq!(issued.serial.len(), SERIAL_BYTES);
    assert_eq!(certificate.raw_serial(), issued.serial.as_slice());
    assert!(issued.serial[0] & 0x80 == 0 && issued.serial[0] & 0x40 != 0);

    // Valid from five minutes before signing for the configured validity.
    let not_before = certificate.validity().not_before.timestamp();
    let not_after = certificate.validity().not_after.timestamp();
    assert!(
        (signing_time - CLOCK_SKEW_SECS - 5..=signing_time - CLOCK_SKEW_SECS + 5)
            .contains(&not_before)
    );
    assert_eq!(
        not_after - not_before,
        i64::try_from(VALIDITY.as_secs()).expect("seconds") + CLOCK_SKEW_SECS
    );
    assert_eq!(
        (issued.not_before, issued.not_after),
        (not_before, not_after)
    );
    assert_eq!(issued.ca_key_id, issuing.key_id());

    // A TLS server that trusts the root accepts it as a client certificate.
    verify_client(
        &ca.root_der(),
        &ca.issuing_ders(),
        &issued.der,
        &[],
        signing_time,
    )
    .expect("a client certificate under the root");
}

#[test]
fn a_gateway_certificate_cannot_sign_or_serve() {
    let ca = TestCa::new(&[1]);
    let authority = ca.authority();
    let issuing = authority.current(now()).expect("an issuing CA");
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a gateway key");
    let issued = issue(issuing, &forged_request(&key)).expect("signed");
    let (_, certificate) = X509Certificate::from_der(&issued.der).expect("a certificate");
    let names: Vec<&GeneralName<'_>> = certificate
        .subject_alternative_name()
        .expect("at most one SAN")
        .map(|san| san.value.general_names.iter().collect())
        .unwrap_or_default();
    assert!(names.is_empty(), "no subject alternative name: {names:?}");

    // Used as a CA, it signs nothing a verifier accepts: a leaf "signed" by the gateway's key.
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "gw-78-0002");
    params.distinguished_name = name;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let gateway_issuer = rcgen::Issuer::from_ca_cert_der(
        &rustls_pki_types::CertificateDer::from(issued.der.as_slice()),
        &key,
    )
    .expect("rcgen takes any certificate as an issuer");
    let other = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("another key");
    let sub_leaf = params
        .signed_by(&other, &gateway_issuer)
        .expect("a certificate signed by the gateway key");
    let mut chain = ca.issuing_ders();
    chain.push(issued.der.clone());
    let refused = verify_client(&ca.root_der(), &chain, sub_leaf.der(), &[], now())
        .expect_err("a gateway certificate is no CA");
    println!("a leaf under the gateway certificate: {refused}");
}

#[test]
fn unusable_requests_are_refused() {
    let ca = TestCa::new(&[1]);
    let authority = ca.authority();
    let issuing = authority.current(now()).expect("an issuing CA");
    let p256 = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a P-256 key");
    let good = plain_request(&p256);
    issue(issuing, &good).expect("the control request is signed");

    // Other algorithms: ECDSA P-384 with SHA-384, and Ed25519.
    for algorithm in [&PKCS_ECDSA_P384_SHA384, &PKCS_ED25519] {
        let key = KeyPair::generate_for(algorithm).expect("a key");
        let refused = issue(issuing, &plain_request(&key)).expect_err("another algorithm");
        assert!(
            matches!(
                refused,
                IssueError::UnsupportedSignature | IssueError::UnsupportedKey
            ),
            "{refused}"
        );
    }

    // The signature does not verify: its last octet changed.
    let mut bad_signature = good.clone();
    let last = bad_signature.len() - 1;
    bad_signature[last] ^= 0x01;
    assert!(matches!(
        issue(issuing, &bad_signature),
        Err(IssueError::BadSignature)
    ));

    // The subject was changed after signing: the key did not sign this request.
    let mut changed = good.clone();
    let at = changed
        .windows(GATEWAY.len())
        .position(|window| window == GATEWAY.as_bytes())
        .expect("the common name in the CSR");
    changed[at + GATEWAY.len() - 1] = b'9';
    assert!(matches!(
        issue(issuing, &changed),
        Err(IssueError::BadSignature)
    ));

    // Trailing bytes, a certificate instead of a request, nothing, and too much.
    let mut trailing = good.clone();
    trailing.push(0);
    assert!(matches!(
        issue(issuing, &trailing),
        Err(IssueError::NotARequest)
    ));
    assert!(matches!(
        issue(issuing, issuing.certificate_der()),
        Err(IssueError::NotARequest)
    ));
    assert!(matches!(issue(issuing, &[]), Err(IssueError::NotARequest)));
    let mut oversized = good;
    oversized.resize(MAX_REQUEST_BYTES + 1, 0);
    assert!(matches!(
        issue(issuing, &oversized),
        Err(IssueError::TooLarge)
    ));
}

#[test]
fn the_newest_valid_generation_signs() {
    let ca = TestCa::new(&[1, 2]);
    let authority = ca.authority();
    assert_eq!(authority.issuing().len(), 2);
    let current = authority.current(now()).expect("an issuing CA");
    assert_eq!(current.generation(), 2);
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a gateway key");
    let issued = issue(current, &plain_request(&key)).expect("signed");
    verify_client(&ca.root_der(), &ca.issuing_ders(), &issued.der, &[], now())
        .expect("a client certificate under the root");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_ledger_records_what_was_signed_and_the_service_cannot_change_it() {
    let Some(product) = product_postgres(&["certs_app"]).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let authority = ca.authority();
    let issuing = authority.current(now()).expect("an issuing CA");
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a gateway key");
    let issued = issue(issuing, &forged_request(&key)).expect("signed");

    let mut app = product.postgres.connect("certs_app", DATABASE).await;

    // Not before the issuing CA has a CRL: a certificate nobody could check for revocation is
    // never handed out.
    let refused = issue::record(&mut app, &issued)
        .await
        .expect_err("no CRL for the issuing CA yet");
    assert!(
        matches!(&refused, RecordError::NoCrl(id) if id == issuing.key_id()),
        "{refused}"
    );
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM certs.issued_certificates WHERE serial = $1")
            .bind(&issued.serial)
            .fetch_one(&mut app)
            .await
            .expect("count the ledger");
    assert_eq!(rows, 0, "nothing recorded without a CRL");
    let crl = certs::crl::build(issuing, 1, &[], now(), Duration::from_secs(3600)).expect("a CRL");
    sqlx::query(
        "INSERT INTO certs.crls (ca_key_id, crl_number, this_update, next_update, crl)
         VALUES ($1, 1, now(), now() + interval '1 hour', $2)",
    )
    .bind(issuing.key_id())
    .bind(&crl)
    .execute(&mut app)
    .await
    .expect("the first CRL");

    issue::record(&mut app, &issued).await.expect("recorded");
    let again = issue::record(&mut app, &issued)
        .await
        .expect_err("a serial number is recorded once");
    println!("recording a serial number twice: {again}");

    let mut admin = product.superuser().await;
    let row = sqlx::query(
        "SELECT ca_key_id, profile, subject, certificate, revoked_at IS NULL AS live,
                extract(epoch FROM not_after)::bigint AS not_after
         FROM certs.issued_certificates WHERE serial = $1",
    )
    .bind(&issued.serial)
    .fetch_one(&mut admin)
    .await
    .expect("the ledger row");
    assert_eq!(row.get::<String, _>("ca_key_id"), issuing.key_id());
    assert_eq!(row.get::<String, _>("profile"), "gateway");
    assert_eq!(row.get::<String, _>("subject"), GATEWAY);
    assert_eq!(row.get::<Vec<u8>, _>("certificate"), issued.der);
    assert!(row.get::<bool, _>("live"));
    assert_eq!(row.get::<i64, _>("not_after"), issued.not_after);

    // The service role appends and marks revocations (the column grant of
    // 0002_workstation_certificates.sql); it cannot rewrite or remove what was signed.
    for statement in [
        "UPDATE certs.issued_certificates SET subject = 'gw-other' WHERE serial = $1",
        "UPDATE certs.issued_certificates SET not_after = now() + interval '9 years' WHERE serial = $1",
        "UPDATE certs.issued_certificates SET certificate = '\\x00' WHERE serial = $1",
        "UPDATE certs.issued_certificates SET profile = 'workstation' WHERE serial = $1",
        "DELETE FROM certs.issued_certificates WHERE serial = $1",
    ] {
        let refused = sqlx::query(statement)
            .bind(&issued.serial)
            .execute(&mut app)
            .await
            .expect_err("the service role cannot change the ledger");
        assert!(
            refused.to_string().contains("permission denied"),
            "{statement}: {refused}"
        );
    }
}
