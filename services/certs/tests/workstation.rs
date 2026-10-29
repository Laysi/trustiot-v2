//! Production workstation certificates (Laysi/trustiot-v2#534; services/certs/README.md,
//! "工作站憑證"):
//!
//! - The profile: a CSR that asks for another name, a CA certificate, and client authentication,
//!   server authentication and any extended key usage gets the workstation profile the CA
//!   decides, whose only extended key usage is the TrustIoT workstation purpose. A CSR whose
//!   signature does not verify with its key (no proof of possession) is refused.
//! - The API: requests without a valid internal identity token, from users who are not platform
//!   administrators, or with a token another service exchanged, issue and revoke nothing (the
//!   ledger is compared before and after). A platform administrator issues and revokes; the
//!   ledger records the revocation and the published CRL lists it once the revocation has
//!   answered, with the next CRL number. When that new CRL fails, the revocation still answers
//!   200 and stands, an ERROR line says so, and the CRL job's next round lists it although the
//!   CRL is not due (Laysi/trustiot-v2#635).
//! - The broker's check: Mosquitto, an OpenSSL TLS server that verifies client certificates
//!   against the service's `ca.pem` and `crl.pem` as the production broker does (decision 6),
//!   accepts a gateway certificate and refuses a revoked gateway certificate and a workstation
//!   certificate. rustls's webpki check refuses the workstation certificate too.
//!
//! The API test starts a throwaway Postgres with TLS, the broker test also a Mosquitto container
//! (podman or docker); both are skipped without either unless `CI` is set. The profile tests need
//! nothing.

mod support;

use std::collections::BTreeSet;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use certs::config::CrlSchedule;
use certs::crl;
use certs::http::{CHAIN_PATH, CRL_PATH};
use certs::issue::{
    self, CLOCK_SKEW_SECS, IssueError, IssuedCertificate, SERIAL_BYTES, WORKSTATION_PURPOSE_DER,
    WorkstationName,
};
use certs::pem;
use certs::workstations::{
    ALREADY_REVOKED, CERTIFICATES_PATH, REQUEST_SIGNATURE_INVALID, REQUEST_UNSUPPORTED,
    UNKNOWN_FIELD,
};
use contract::certs::{RevocationReason, WorkstationCertificate};
use contract::{ServiceName, TenantGrant, TenantId, TenantRole};
use protocol::GatewayId;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, PKCS_ECDSA_P384_SHA384, SanType,
};
use rustls::CertificateError;
use serde_json::{Value, json};
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::TenantDb;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection as _, Row as _};
use support::{LIMIT, Reply, Service, TestCa, now, verify_client, wait_for};
use testkit::container::{Container, container_runtime, runtime_output, unavailable, unique_name};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};
use x509_parser::certificate::X509Certificate;
use x509_parser::certification_request::X509CertificationRequest;
use x509_parser::extensions::ParsedExtension;
use x509_parser::oid_registry::{
    OID_X509_EXT_AUTHORITY_KEY_IDENTIFIER, OID_X509_EXT_BASIC_CONSTRAINTS,
    OID_X509_EXT_EXTENDED_KEY_USAGE, OID_X509_EXT_KEY_USAGE, OID_X509_EXT_SUBJECT_KEY_IDENTIFIER,
};
use x509_parser::prelude::FromDer;
use x509_parser::revocation_list::CertificateRevocationList;

const WORKSTATION: &str = "line-a-ws-01";
const FORGED_NAME: &str = "gw-forged-admin";
const VALIDITY: Duration = Duration::from_secs(180 * 86_400);

/// A CRL schedule that replaces each CRL about ten seconds after the last one, every CRL valid
/// for an hour: a revocation reaches the published CRLs within seconds, and the CRL file the
/// broker test reads stays valid for as long as the test runs.
const FAST_CRLS: [(&str, &str); 4] = [
    ("TRUSTIOT__CRL__CHECK_INTERVAL", "1s"),
    ("TRUSTIOT__CRL__VALIDITY", "1h"),
    ("TRUSTIOT__CRL__RENEW_BEFORE", "3590s"),
    ("TRUSTIOT__CRL__ALERT_BEFORE", "3500s"),
];

/// A P-256 key and a CSR for it that asks for everything the CA must not grant a workstation:
/// another common name (a gateway id) and an organization, DNS and IP subject alternative names,
/// a CA certificate with keyCertSign, and client authentication, server authentication and any
/// extended key usage.
fn forged_request() -> (KeyPair, Vec<u8>) {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a workstation key");
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, FORGED_NAME);
    name.push(DnType::OrganizationName, "Forged");
    params.distinguished_name = name;
    params.subject_alt_names = vec![
        SanType::DnsName("broker.example.invalid".try_into().expect("a DNS name")),
        SanType::IpAddress([192, 0, 2, 1].into()),
    ];
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
    ];
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ClientAuth,
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::Any,
    ];
    let request = params
        .serialize_request(&key)
        .expect("a CSR")
        .der()
        .to_vec();
    (key, request)
}

/// A plain CSR with `common_name` for a new key of `algorithm`.
fn plain_request(
    common_name: &str,
    algorithm: &'static rcgen::SignatureAlgorithm,
) -> (KeyPair, Vec<u8>) {
    let key = KeyPair::generate_for(algorithm).expect("a key");
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, common_name);
    params.distinguished_name = name;
    let request = params
        .serialize_request(&key)
        .expect("a CSR")
        .der()
        .to_vec();
    (key, request)
}

fn workstation_name() -> WorkstationName {
    WorkstationName::parse(WORKSTATION).expect("a workstation name")
}

/// The OIDs of a certificate's extensions, as dotted strings.
fn extension_oids(certificate: &X509Certificate<'_>) -> BTreeSet<String> {
    certificate
        .extensions()
        .iter()
        .map(|extension| extension.oid.to_id_string())
        .collect()
}

/// Checks that `der` is a workstation certificate for [`WORKSTATION`]: the subject, the
/// extensions and their values of the profile, nothing from the CSR but the public key.
fn assert_workstation_profile(der: &[u8], request: &[u8]) {
    let (rest, certificate) = X509Certificate::from_der(der).expect("a certificate");
    assert!(rest.is_empty());

    assert_eq!(certificate.subject().iter_attributes().count(), 1);
    let cn: Vec<&str> = certificate
        .subject()
        .iter_common_name()
        .filter_map(|cn| cn.as_str().ok())
        .collect();
    assert_eq!(cn, [format!("workstation:{WORKSTATION}").as_str()]);
    assert!(!certificate.subject().to_string().contains(FORGED_NAME));

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

    let usage = certificate
        .key_usage()
        .expect("one keyUsage")
        .expect("keyUsage present");
    assert!(usage.critical, "keyUsage is critical");
    assert_eq!(usage.value.flags, 1, "digitalSignature only");

    // The workstation purpose only: never client or server authentication, never any purpose.
    let eku = certificate
        .extended_key_usage()
        .expect("one extendedKeyUsage")
        .expect("extendedKeyUsage present");
    assert!(
        !(eku.value.any
            || eku.value.server_auth
            || eku.value.client_auth
            || eku.value.code_signing
            || eku.value.email_protection
            || eku.value.time_stamping
            || eku.value.ocsp_signing),
        "the workstation purpose only: {:?}",
        eku.value
    );
    let other: Vec<&[u8]> = eku.value.other.iter().map(|oid| oid.as_bytes()).collect();
    assert_eq!(other, [WORKSTATION_PURPOSE_DER.as_slice()]);
    let raw = certificate
        .extensions()
        .iter()
        .find(|extension| extension.oid == OID_X509_EXT_EXTENDED_KEY_USAGE)
        .expect("the extendedKeyUsage extension")
        .value;
    let mut expected_value = vec![0x30, 22, 0x06, 20];
    expected_value.extend_from_slice(&WORKSTATION_PURPOSE_DER);
    assert_eq!(raw, expected_value.as_slice());

    // The key is the CSR's.
    let (_, parsed) = X509CertificationRequest::from_der(request).expect("the CSR parses");
    assert_eq!(
        certificate.public_key().raw,
        parsed.certification_request_info.subject_pki.raw
    );
}

#[test]
fn the_ca_decides_the_workstation_profile_and_a_tls_server_refuses_it() {
    let ca = TestCa::new(&[1]);
    let authority = ca.authority();
    let issuing = authority.current(now()).expect("an issuing CA");
    let (_, request) = forged_request();

    // Positive control: the CSR really asks for client authentication, a CA certificate and
    // another name.
    let (_, parsed) = X509CertificationRequest::from_der(&request).expect("the CSR parses");
    let requested: Vec<&ParsedExtension<'_>> = parsed
        .requested_extensions()
        .expect("the CSR requests extensions")
        .collect();
    assert!(requested.iter().any(|extension| matches!(
        extension,
        ParsedExtension::ExtendedKeyUsage(eku) if eku.client_auth && eku.server_auth && eku.any
    )));
    assert!(requested.iter().any(|extension| matches!(
        extension,
        ParsedExtension::BasicConstraints(constraints) if constraints.ca
    )));
    assert!(requested.iter().any(|extension| matches!(
        extension,
        ParsedExtension::SubjectAlternativeName(names) if names.general_names.len() == 2
    )));

    let signing_time = now();
    let issued = issue::workstation_certificate(
        issuing,
        &request,
        &workstation_name(),
        VALIDITY,
        signing_time,
    )
    .expect("the CSR is signed");
    assert_eq!(issued.profile.as_str(), "workstation");
    assert_eq!(issued.subject, format!("workstation:{WORKSTATION}"));
    assert_eq!(issued.ca_key_id, issuing.key_id());
    assert_workstation_profile(&issued.der, &request);

    let (_, certificate) = X509Certificate::from_der(&issued.der).expect("a certificate");
    let (_, issuing_certificate) =
        X509Certificate::from_der(issuing.certificate_der()).expect("the issuing CA certificate");
    certificate
        .verify_signature(Some(issuing_certificate.public_key()))
        .expect("signed by the issuing CA");
    assert_eq!(issued.serial.len(), SERIAL_BYTES);
    assert_eq!(certificate.raw_serial(), issued.serial.as_slice());
    let not_before = certificate.validity().not_before.timestamp();
    let not_after = certificate.validity().not_after.timestamp();
    assert_eq!(
        not_after - not_before,
        i64::try_from(VALIDITY.as_secs()).expect("seconds") + CLOCK_SKEW_SECS
    );
    assert_eq!(
        (issued.not_before, issued.not_after),
        (not_before, not_after)
    );

    // A TLS server that authenticates clients under this root: a gateway certificate from the
    // same issuing CA passes, the workstation certificate is refused for its purpose.
    let (_, gateway_request) = plain_request("gw-534-0001", &PKCS_ECDSA_P256_SHA256);
    let gateway = issue::gateway_certificate(
        issuing,
        &gateway_request,
        &GatewayId::new("gw-534-0001").expect("a gateway id"),
        VALIDITY,
        signing_time,
    )
    .expect("signed");
    verify_client(
        &ca.root_der(),
        &ca.issuing_ders(),
        &gateway.der,
        &[],
        signing_time,
    )
    .expect("the gateway certificate is a client certificate");
    let refused = verify_client(
        &ca.root_der(),
        &ca.issuing_ders(),
        &issued.der,
        &[],
        signing_time,
    )
    .expect_err("a workstation certificate is no client certificate");
    println!("rustls on the workstation certificate: {refused}");
    assert!(
        matches!(
            refused,
            rustls::Error::InvalidCertificate(
                CertificateError::InvalidPurpose | CertificateError::InvalidPurposeContext { .. }
            )
        ),
        "refused for its purpose: {refused:?}"
    );
}

#[test]
fn a_request_without_proof_of_possession_is_refused() {
    let ca = TestCa::new(&[1]);
    let authority = ca.authority();
    let issuing = authority.current(now()).expect("an issuing CA");
    let name = workstation_name();
    let issue =
        |request: &[u8]| issue::workstation_certificate(issuing, request, &name, VALIDITY, now());
    let (_, good) = plain_request(WORKSTATION, &PKCS_ECDSA_P256_SHA256);
    issue(&good).expect("the control request is signed");

    // The signature does not verify: its last octet changed.
    let mut bad_signature = good.clone();
    let last = bad_signature.len() - 1;
    bad_signature[last] ^= 0x01;
    assert!(matches!(
        issue(&bad_signature),
        Err(IssueError::BadSignature)
    ));

    // The subject was changed after signing: the key did not sign this request.
    let mut changed = good.clone();
    let at = changed
        .windows(WORKSTATION.len())
        .position(|window| window == WORKSTATION.as_bytes())
        .expect("the common name in the CSR");
    changed[at + WORKSTATION.len() - 1] = b'9';
    assert!(matches!(issue(&changed), Err(IssueError::BadSignature)));

    // Another curve.
    let (_, p384) = plain_request(WORKSTATION, &PKCS_ECDSA_P384_SHA384);
    let refused = issue(&p384).expect_err("another curve");
    assert!(
        matches!(
            refused,
            IssueError::UnsupportedSignature | IssueError::UnsupportedKey
        ),
        "{refused}"
    );
}

/// The published CRL bundle, once every issuing CA has a CRL.
async fn published_crls(service: &Service) -> String {
    wait_for("the published CRLs", || async {
        let (status, body) = service.get(CRL_PATH).await;
        (status == 200).then_some(body)
    })
    .await
}

/// Whether a CRL of `bundle` (PEM) lists `serial` (hex), with its reason code.
fn crl_entry(bundle: &str, serial: &str) -> Option<Option<u8>> {
    for der in pem::decode_all(bundle, pem::CRL).expect("PEM CRLs") {
        let (_, crl) = CertificateRevocationList::from_der(&der).expect("a CRL");
        for revoked in crl.iter_revoked_certificates() {
            if issue::hex(revoked.raw_serial()) == serial {
                return Some(revoked.reason_code().map(|(_, code)| code.0));
            }
        }
    }
    None
}

/// The ledger rows of the certificates `ca_key_id` signed: serial (hex), profile, subject,
/// revocation reason (or -1 while not revoked).
async fn ledger(product: &ProductPostgres, ca_key_id: &str) -> Vec<(String, String, String, i16)> {
    let rows = sqlx::query(
        "SELECT encode(serial, 'hex') AS serial, profile, subject,
                CASE WHEN revoked_at IS NULL THEN -1 ELSE coalesce(revocation_reason, 0) END
                    AS reason
         FROM certs.issued_certificates WHERE ca_key_id = $1 ORDER BY serial",
    )
    .bind(ca_key_id)
    .fetch_all(&mut product.superuser().await)
    .await
    .expect("read the ledger");
    rows.iter()
        .map(|row| {
            (
                row.get("serial"),
                row.get("profile"),
                row.get("subject"),
                row.get::<i32, _>("reason")
                    .try_into()
                    .expect("a reason code"),
            )
        })
        .collect()
}

/// The error code and message key of an M1 error reply, with the field errors as
/// `(field, message_key)`.
fn error_of(reply: &Reply) -> (String, String, Vec<(String, String)>) {
    let body = reply.json();
    let fields = body["field_errors"]
        .as_array()
        .map(|errors| {
            errors
                .iter()
                .map(|error| {
                    (
                        error["field"].as_str().unwrap_or_default().to_owned(),
                        error["message_key"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    (
        body["code"].as_str().unwrap_or_default().to_owned(),
        body["message_key"].as_str().unwrap_or_default().to_owned(),
        fields,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn only_platform_administrators_issue_and_revoke_and_the_crl_lists_a_revocation() {
    let Some(product) = product_postgres(&["certs_app"]).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let authority = ca.authority();
    let issuing = authority.current(now()).expect("an issuing CA");
    let ca_key_id = issuing.key_id().to_owned();
    let service = Service::start(&product, &ca, &FAST_CRLS).await;
    published_crls(&service).await;

    // Positive control: a platform administrator issues one, from a CSR that asks for another
    // name, a CA certificate and client authentication.
    let admin = service.admin_token();
    let (_, request) = forged_request();
    let issue_body = json!({
        "workstation": WORKSTATION,
        "certificate_request": pem::encode(pem::CERTIFICATE_REQUEST, &request),
    });
    let issued = service
        .post_json(CERTIFICATES_PATH, Some(&admin), &issue_body)
        .await;
    assert_eq!(issued.status, 201, "{}", issued.body);
    assert_eq!(issued.cache_control.as_deref(), Some("no-store"));
    let control: WorkstationCertificate =
        serde_json::from_str(&issued.body).expect("a workstation certificate");
    println!("issued: {}", issued.body.replace('\n', " "));
    assert_eq!(control.workstation, WORKSTATION);
    assert_eq!(control.subject, format!("workstation:{WORKSTATION}"));
    assert_eq!(control.ca_key_id, ca_key_id);
    assert_eq!(control.revoked_at, None);
    let der = pem::decode_all(&control.certificate, pem::CERTIFICATE)
        .expect("a PEM certificate")
        .remove(0);
    assert_workstation_profile(&der, &request);
    let (_, certificate) = X509Certificate::from_der(&der).expect("a certificate");
    assert_eq!(issue::hex(certificate.raw_serial()), control.serial);
    let after_control = ledger(&product, &ca_key_id).await;
    assert_eq!(
        after_control,
        [(
            control.serial.clone(),
            "workstation".to_owned(),
            control.subject.clone(),
            -1
        )]
    );
    // The service's stdout reaches the test through a reader thread, so a line can arrive after
    // its response does.
    let issue_lines = wait_for("the issuance log line", || async {
        let lines = service.stdout.messages("issued a workstation certificate");
        (!lines.is_empty()).then_some(lines)
    })
    .await;
    assert_eq!(issue_lines.len(), 1);
    assert_eq!(issue_lines[0]["fields"]["serial"], json!(control.serial));

    // Requests that must issue and revoke nothing: no token, not a token, a token of another
    // key, for another service, expired, from users who are not platform administrators, and a
    // platform administrator's token another service exchanged (it calls on their behalf).
    let stranger = TestIssuer::new();
    let mut admin_claims = TestIssuer::claims(ServiceName::Certs, None);
    admin_claims.platform_admin = true;
    let mut elsewhere = TestIssuer::claims(ServiceName::Assets, None);
    elsewhere.platform_admin = true;
    let mut expired = admin_claims.clone();
    expired.iat -= 600;
    expired.exp = expired.iat + 60;
    let tenant_admin = TenantGrant {
        id: TenantId::generate(),
        role: TenantRole::Admin,
    };
    let refusals: Vec<(&str, Option<String>, u16, &str)> = vec![
        ("no internal identity", None, 401, "unauthenticated"),
        (
            "not a token",
            Some("not-a-token".to_owned()),
            401,
            "unauthenticated",
        ),
        (
            "signed by another key",
            Some(stranger.token(&admin_claims)),
            401,
            "unauthenticated",
        ),
        (
            "addressed to another service",
            Some(service.issuer.token(&elsewhere)),
            401,
            "unauthenticated",
        ),
        (
            "expired",
            Some(service.issuer.token(&expired)),
            401,
            "unauthenticated",
        ),
        (
            "a tenant administrator",
            Some(service.token(false, Some(tenant_admin), None)),
            403,
            "permission_denied",
        ),
        (
            "a user without a tenant",
            Some(service.token(false, None, None)),
            403,
            "permission_denied",
        ),
        (
            "a platform administrator's token the assets service exchanged",
            Some(service.token(true, None, Some(ServiceName::Assets))),
            403,
            "permission_denied",
        ),
    ];
    let revoke_path = format!("{CERTIFICATES_PATH}/{}/revoke", control.serial);
    let revoke_body = json!({"reason": "key_compromise"});
    for (who, token, status, code) in &refusals {
        let (_, request) = forged_request();
        let body = json!({
            "workstation": "ws-refused",
            "certificate_request": pem::encode(pem::CERTIFICATE_REQUEST, &request),
        });
        for (action, reply) in [
            (
                "issue",
                service
                    .post_json(CERTIFICATES_PATH, token.as_deref(), &body)
                    .await,
            ),
            (
                "revoke",
                service
                    .post_json(&revoke_path, token.as_deref(), &revoke_body)
                    .await,
            ),
        ] {
            println!("{action} as {who}: {} {}", reply.status, reply.body);
            assert_eq!(reply.status, *status, "{action} as {who}: {}", reply.body);
            assert_eq!(error_of(&reply).0, *code, "{action} as {who}");
            assert_eq!(
                reply.cache_control.as_deref(),
                Some("no-store"),
                "{action} as {who}"
            );
        }
    }
    assert_eq!(
        ledger(&product, &ca_key_id).await,
        after_control,
        "the refused requests issued and revoked nothing"
    );

    // A platform administrator's requests the API refuses: no proof of possession, another
    // curve, a field the CA decides, another content type. Nothing is issued.
    let mut no_possession = request.clone();
    let last = no_possession.len() - 1;
    no_possession[last] ^= 0x01;
    let (_, p384) = plain_request(WORKSTATION, &PKCS_ECDSA_P384_SHA384);
    for (what, body, field, key) in [
        (
            "a CSR whose signature does not verify",
            json!({
                "workstation": WORKSTATION,
                "certificate_request": pem::encode(pem::CERTIFICATE_REQUEST, &no_possession),
            }),
            "/certificate_request",
            REQUEST_SIGNATURE_INVALID,
        ),
        (
            "a P-384 CSR",
            json!({
                "workstation": WORKSTATION,
                "certificate_request": pem::encode(pem::CERTIFICATE_REQUEST, &p384),
            }),
            "/certificate_request",
            REQUEST_UNSUPPORTED,
        ),
        (
            "a subject of its own",
            json!({
                "workstation": WORKSTATION,
                "certificate_request": pem::encode(pem::CERTIFICATE_REQUEST, &request),
                "subject": FORGED_NAME,
            }),
            "/subject",
            UNKNOWN_FIELD,
        ),
    ] {
        let reply = service
            .post_json(CERTIFICATES_PATH, Some(&admin), &body)
            .await;
        println!("{what}: {} {}", reply.status, reply.body);
        assert_eq!(reply.status, 422, "{what}: {}", reply.body);
        let (code, _, fields) = error_of(&reply);
        assert_eq!(code, "validation_failed", "{what}");
        assert_eq!(
            fields,
            [(field.to_owned(), key.as_str().to_owned())],
            "{what}"
        );
    }
    let wrong_type = service
        .post(
            CERTIFICATES_PATH,
            Some(&admin),
            Some("text/plain"),
            issue_body.to_string(),
        )
        .await;
    assert_eq!(wrong_type.status, 400, "{}", wrong_type.body);
    assert_eq!(
        ledger(&product, &ca_key_id).await,
        after_control,
        "the refused requests issued nothing"
    );

    // Only workstation certificates are revoked here: a gateway's serial and unknown serials
    // are not found.
    let (_, gateway_request) = plain_request("gw-534-0002", &PKCS_ECDSA_P256_SHA256);
    let gateway = issue::gateway_certificate(
        issuing,
        &gateway_request,
        &GatewayId::new("gw-534-0002").expect("a gateway id"),
        VALIDITY,
        now(),
    )
    .expect("signed");
    {
        let mut app = product.postgres.connect("certs_app", DATABASE).await;
        issue::record(&mut app, &gateway).await.expect("recorded");
    }
    let gateway_serial = issue::hex(&gateway.serial);
    for serial in [gateway_serial.as_str(), "00ff", "not-hex", "0"] {
        let reply = service
            .post_json(
                &format!("{CERTIFICATES_PATH}/{serial}/revoke"),
                Some(&admin),
                &revoke_body,
            )
            .await;
        assert_eq!(reply.status, 404, "revoke {serial}: {}", reply.body);
    }

    // The platform administrator revokes the control certificate.
    let revoked = service
        .post_json(&revoke_path, Some(&admin), &revoke_body)
        .await;
    assert_eq!(revoked.status, 200, "{}", revoked.body);
    println!("revoked: {}", revoked.body.replace('\n', " "));
    let revoked: WorkstationCertificate =
        serde_json::from_str(&revoked.body).expect("a workstation certificate");
    assert_eq!(revoked.serial, control.serial);
    assert!(revoked.revoked_at.is_some());
    assert_eq!(
        revoked.revocation_reason,
        Some(RevocationReason::KeyCompromise)
    );
    let lines = wait_for("the revocation log line", || async {
        let lines = service.stdout.messages("revoked a workstation certificate");
        (!lines.is_empty()).then_some(lines)
    })
    .await;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["fields"]["serial"], json!(control.serial));
    let mut expected = vec![
        (
            control.serial.clone(),
            "workstation".to_owned(),
            control.subject.clone(),
            1,
        ),
        (
            gateway_serial.clone(),
            "gateway".to_owned(),
            "gw-534-0002".to_owned(),
            -1,
        ),
    ];
    expected.sort();
    assert_eq!(ledger(&product, &ca_key_id).await, expected);

    let again = service
        .post_json(&revoke_path, Some(&admin), &json!({}))
        .await;
    assert_eq!(again.status, 409, "{}", again.body);
    assert_eq!(error_of(&again).1, ALREADY_REVOKED.as_str());

    // The published CRLs list it already, with the reason: the revocation gave the issuing CA a
    // new CRL before it answered (Laysi/trustiot-v2#635).
    let (status, bundle) = service.get(CRL_PATH).await;
    assert_eq!(status, 200, "{bundle}");
    assert_eq!(
        crl_entry(&bundle, &control.serial),
        Some(Some(1)),
        "keyCompromise"
    );
    println!("the published CRL lists {} (keyCompromise)", control.serial);
}

/// The first replica: rounds an hour apart, so after its first round (at startup) its CRL job
/// stays out of the way and every new CRL of the test's CA comes from a revocation. The rest of
/// the schedule is the default (7-day CRLs renewed below 5 days left).
const ROUNDS_HOURLY: [(&str, &str); 1] = [("TRUSTIOT__CRL__CHECK_INTERVAL", "1h")];

/// The replica after the restart: rounds every second.
const ROUNDS_EVERY_SECOND: [(&str, &str); 1] = [("TRUSTIOT__CRL__CHECK_INTERVAL", "1s")];

/// The stored CRL of `ca_key_id`: its number and the seconds it has left.
async fn stored_crl(product: &ProductPostgres, ca_key_id: &str) -> (i64, i64) {
    let row = sqlx::query(
        "SELECT crl_number, extract(epoch FROM next_update - now())::bigint AS remaining
         FROM certs.crls WHERE ca_key_id = $1",
    )
    .bind(ca_key_id)
    .fetch_one(&mut product.superuser().await)
    .await
    .expect("read the CRL");
    (row.get("crl_number"), row.get("remaining"))
}

/// Issues a workstation certificate named `name` through the API as a platform administrator.
async fn issue_workstation(service: &Service, name: &str) -> WorkstationCertificate {
    let (_, request) = plain_request(name, &PKCS_ECDSA_P256_SHA256);
    let reply = service
        .post_json(
            CERTIFICATES_PATH,
            Some(&service.admin_token()),
            &json!({
                "workstation": name,
                "certificate_request": pem::encode(pem::CERTIFICATE_REQUEST, &request),
            }),
        )
        .await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    serde_json::from_str(&reply.body).expect("a workstation certificate")
}

/// Revokes `serial` through the API as a platform administrator (key compromise).
async fn revoke_workstation(service: &Service, serial: &str) -> Reply {
    service
        .post_json(
            &format!("{CERTIFICATES_PATH}/{serial}/revoke"),
            Some(&service.admin_token()),
            &json!({"reason": "key_compromise"}),
        )
        .await
}

/// The log line of the revocation of `serial`.
async fn revocation_line(service: &Service, serial: &str) -> Value {
    wait_for("the revocation log line", || async {
        service
            .stdout
            .messages("revoked a workstation certificate")
            .into_iter()
            .find(|line| line["fields"]["serial"] == serial)
    })
    .await
}

/// The `issued a new CRL` lines of `ca_key_id` so far.
fn crl_lines(service: &Service, ca_key_id: &str) -> Vec<Value> {
    service
        .stdout
        .messages("issued a new CRL")
        .into_iter()
        .filter(|line| line["fields"]["ca_key_id"] == ca_key_id)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revocation_is_in_the_published_crl_at_once_or_after_a_failure_within_one_round() {
    let Some(product) = product_postgres(&["certs_app"]).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let authority = ca.authority();
    let ca_key_id = authority
        .current(now())
        .expect("an issuing CA")
        .key_id()
        .to_owned();
    // The CA's first CRL, made here as the CRL job makes it: the tests of this binary share one
    // database and its job lock, so the first replica's single round at startup may be another
    // service's turn.
    let work = TenantDb::connect_lazy_with_background(
        product.postgres.options("certs_app", DATABASE),
        PgPoolOptions::new().max_connections(1),
        ServiceIdentity::new(ServiceName::Certs),
    )
    .expect("the certs service role's pool")
    .1;
    crl::refresh(&work, &authority, &CrlSchedule::default(), now())
        .await
        .expect("the first CRL");
    let service = Service::start(&product, &ca, &ROUNDS_HOURLY).await;
    published_crls(&service).await;
    let listed = issue_workstation(&service, "line-a-ws-listed").await;
    let failed = issue_workstation(&service, "line-a-ws-failed").await;

    // 1. The new CRL at once: when the revocation answers, the published CRL lists it, with the
    //    next number. Nothing else makes a CRL of this CA meanwhile: it is days from
    //    renew_before and the replica's next round is an hour away.
    let (before, _) = stored_crl(&product, &ca_key_id).await;
    let reply = revoke_workstation(&service, &listed.serial).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let (status, bundle) = service.get(CRL_PATH).await;
    assert_eq!(status, 200, "{bundle}");
    assert_eq!(
        crl_entry(&bundle, &listed.serial),
        Some(Some(1)),
        "listed when the revocation answered (keyCompromise)"
    );
    let (after, _) = stored_crl(&product, &ca_key_id).await;
    assert_eq!(after, before + 1, "the next CRL number");
    let line = revocation_line(&service, &listed.serial).await;
    assert!(
        line["fields"]["user_id"].is_string(),
        "the audit line: {line}"
    );
    let issued = crl_lines(&service, &ca_key_id)
        .into_iter()
        .find(|line| line["fields"]["crl_number"] == after)
        .unwrap_or_else(|| panic!("no log line for CRL {after}:\n{}", service.output()));
    assert_eq!(issued["fields"]["cause"], "revocation", "{issued}");
    println!(
        "revoked {}: CRL {before} -> {after} before the response, listing it",
        listed.serial
    );

    // 2. The new CRL fails: another session holds the CA's CRL row, so the attempt gives up when
    //    its lock wait (a third of the 10 s limit) runs out. The revocation still answers 200 and
    //    is in the ledger; an ERROR line names the failure; the CRL stays as it was.
    let mut blocker = product.superuser().await;
    let mut blocking = blocker.begin().await.expect("a transaction");
    sqlx::query("SELECT 1 FROM certs.crls WHERE ca_key_id = $1 FOR UPDATE")
        .bind(&ca_key_id)
        .execute(&mut *blocking)
        .await
        .expect("lock the CA's CRL row");
    let reply = revoke_workstation(&service, &failed.serial).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let revoked: WorkstationCertificate =
        serde_json::from_str(&reply.body).expect("a workstation certificate");
    assert!(revoked.revoked_at.is_some());
    assert_eq!(
        revoked.revocation_reason,
        Some(RevocationReason::KeyCompromise)
    );
    let ledger_rows = ledger(&product, &ca_key_id).await;
    assert!(
        ledger_rows
            .iter()
            .any(|row| row.0 == failed.serial && row.3 == 1),
        "revoked in the ledger: {ledger_rows:?}"
    );
    let line = revocation_line(&service, &failed.serial).await;
    assert!(
        line["fields"]["user_id"].is_string(),
        "the audit line: {line}"
    );
    let error = wait_for("the ERROR line of the failed CRL", || async {
        service
            .stdout
            .messages("could not issue a new CRL after a revocation")
            .into_iter()
            .find(|line| line["level"] == "ERROR" && line["fields"]["serial"] == failed.serial)
    })
    .await;
    println!("the failed new CRL: {error}");
    assert_eq!(stored_crl(&product, &ca_key_id).await.0, after, "unchanged");
    let (status, bundle) = service.get(CRL_PATH).await;
    assert_eq!(status, 200, "{bundle}");
    assert_eq!(crl_entry(&bundle, &failed.serial), None, "not listed yet");
    blocking.rollback().await.expect("release the CA's CRL row");

    // 3. The replica stops before its next round; another starts (rounds every second). Its CRL
    //    job lists the revocation in its next round, although the CRL is days from
    //    renew_before: the revocation is newer than the CRL's thisUpdate.
    drop(service);
    let restarted = Service::start(&product, &ca, &ROUNDS_EVERY_SECOND).await;
    let reason = wait_for("the job's CRL listing the revocation", || async {
        let (status, bundle) = restarted.get(CRL_PATH).await;
        if status != 200 {
            return None;
        }
        crl_entry(&bundle, &failed.serial)
    })
    .await;
    assert_eq!(reason, Some(1), "keyCompromise");
    let caught_up = crl_lines(&restarted, &ca_key_id)
        .into_iter()
        .find(|line| line["fields"]["cause"] == "unlisted_revocation")
        .unwrap_or_else(|| panic!("no CRL for an unlisted revocation:\n{}", restarted.output()));
    println!("the restarted replica's job caught up: {caught_up}");
    let (number, remaining) = stored_crl(&product, &ca_key_id).await;
    assert!(number > after, "a new number: {after} then {number}");
    let renew_before =
        i64::try_from(CrlSchedule::default().renew_before.as_secs()).expect("seconds");
    assert!(
        remaining > renew_before,
        "not due by renew_before: {remaining} s left"
    );
}

// ---- the broker's check ----

/// eclipse-mosquitto:2.1.2-alpine, pinned to the multi-arch index digest, as in
/// `crates/protocol/tests/broker.rs` and the local dev environment; Renovate updates every copy
/// in one pull request.
const MOSQUITTO_IMAGE: &str = "docker.io/library/eclipse-mosquitto:2.1.2-alpine@sha256:38c0da4f2ef84284d47b3b3eeea1cb3bdeabe81ee10caf0cd5c5ff61ee3ea408";

/// Where the test puts the broker's files inside the container.
const BROKER_DIR: &str = "/run/trustiot";

/// Waits until the test has put the files in place, then runs the broker as its own user.
const START_SCRIPT: &str = r#"set -eu
while [ ! -f /run/trustiot/ready ]; do sleep 1; done
chown -R mosquitto:mosquitto /run/trustiot
exec mosquitto -c /run/trustiot/mosquitto.conf
"#;

/// One TLS listener on the container's loopback interface that requires a client certificate
/// under `ca.pem` (the certs service's chain), checks it against `crl.pem` (the service's CRL
/// bundle) and takes its common name as the username: the broker's check of decision 6
/// (Laysi/trustiot-v2#84 runs it in the cluster).
const MOSQUITTO_CONF: &str = "listener 8883 127.0.0.1
cafile /run/trustiot/ca.pem
certfile /run/trustiot/server.pem
keyfile /run/trustiot/server-key.pem
crlfile /run/trustiot/crl.pem
require_certificate true
use_identity_as_username true
log_dest stdout
log_type all
";

/// A throwaway CA and the broker's server certificate for 127.0.0.1 under it, PEM: (CA
/// certificate, server certificate, server key). Made at run time; nothing outside the
/// container trusts them.
fn broker_server_certificate() -> (String, String, String) {
    let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a CA key");
    let mut ca_params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "certs test broker CA");
    ca_params.distinguished_name = name;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_certificate = ca_params.self_signed(&ca_key).expect("the broker CA");
    let issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a server key");
    let mut params = CertificateParams::new(vec!["localhost".to_owned()]).expect("params");
    params
        .subject_alt_names
        .push(SanType::IpAddress([127, 0, 0, 1].into()));
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let certificate = params
        .signed_by(&key, &issuer)
        .expect("the server certificate");
    (ca_certificate.pem(), certificate.pem(), key.serialize_pem())
}

/// The Mosquitto container.
struct Broker {
    container: Container,
}

impl Broker {
    fn start(runtime: &'static str) -> Self {
        let container = Container {
            runtime,
            name: unique_name("certs-mosquitto"),
        };
        // The container's id; `container` removes it by name when dropped.
        let _id = runtime_output(
            runtime,
            &[
                "run",
                "--detach",
                "--name",
                &container.name,
                "--memory",
                "128m",
                "--entrypoint",
                "/bin/sh",
                MOSQUITTO_IMAGE,
                "-c",
                START_SCRIPT,
            ],
        );
        Self { container }
    }

    /// Writes `contents` to `path` inside the container, through standard input.
    fn push(&self, path: &str, contents: &[u8]) {
        let mut child = Command::new(self.container.runtime)
            .args([
                "exec",
                "-i",
                &self.container.name,
                "sh",
                "-c",
                r#"mkdir -p "$(dirname "$0")" && cat > "$0""#,
                path,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start exec");
        {
            let mut stdin = child.stdin.take().expect("piped stdin");
            stdin.write_all(contents).expect("write the file");
        }
        let output = child.wait_with_output().expect("exec");
        assert!(
            output.status.success(),
            "push {path}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn logs(&self) -> String {
        let output = Command::new(self.container.runtime)
            .args(["logs", &self.container.name])
            .output()
            .expect("read the broker's logs");
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    /// Waits until the broker logs that it runs.
    async fn wait_running(&self) {
        let deadline = Instant::now() + LIMIT;
        loop {
            let logs = self.logs();
            if logs.contains(" running") {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Mosquitto did not log that it runs within {LIMIT:?}:\n{logs}"
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    /// Connects with the certificate and key in `name.pem` and `name-key.pem` and publishes one
    /// message; whether the broker accepted the connection (CONNACK 0) and the client's output.
    async fn connect_as(&self, name: &str) -> (bool, String) {
        let certificate = format!("{BROKER_DIR}/clients/{name}.pem");
        let key = format!("{BROKER_DIR}/clients/{name}-key.pem");
        let server_ca = format!("{BROKER_DIR}/server-ca.pem");
        let client_id = format!("certs-test-{name}");
        // The server certificate is checked against its throwaway CA; --insecure only skips
        // matching its name, which is not what this test is about.
        let run = tokio::process::Command::new(self.container.runtime)
            .args([
                "exec",
                &self.container.name,
                "mosquitto_pub",
                "-d",
                "-h",
                "127.0.0.1",
                "-p",
                "8883",
                "--cafile",
                &server_ca,
                "--insecure",
                "--cert",
                &certificate,
                "--key",
                &key,
                "-i",
                &client_id,
                "-t",
                "certs/test",
                "-m",
                "hello",
            ])
            .stdin(Stdio::null())
            .output();
        let output = tokio::time::timeout(LIMIT, run)
            .await
            .unwrap_or_else(|_| panic!("mosquitto_pub as {name} did not finish within {LIMIT:?}"))
            .expect("run mosquitto_pub");
        let text = format!(
            "status {}\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let accepted = output.status.success() && text.contains("received CONNACK (0)");
        (accepted, text)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_broker_accepts_a_gateway_certificate_and_refuses_a_workstation_certificate() {
    let Some(product) = product_postgres(&["certs_app"]).await else {
        return;
    };
    let Some(runtime) = container_runtime() else {
        unavailable("podman or docker");
        return;
    };
    let ca = TestCa::new(&[1]);
    let authority = ca.authority();
    let issuing = authority.current(now()).expect("an issuing CA");
    let service = Service::start(&product, &ca, &FAST_CRLS).await;
    published_crls(&service).await;

    // Two gateway certificates, one of them revoked, and a workstation certificate from the
    // API for a CSR that asks for client authentication.
    let gateway = |id: &str| -> (KeyPair, IssuedCertificate) {
        let (key, request) = plain_request(id, &PKCS_ECDSA_P256_SHA256);
        let issued = issue::gateway_certificate(
            issuing,
            &request,
            &GatewayId::new(id).expect("a gateway id"),
            VALIDITY,
            now(),
        )
        .expect("signed");
        (key, issued)
    };
    let (accepted_key, accepted) = gateway("gw-534-accepted");
    let (revoked_key, revoked) = gateway("gw-534-revoked");
    {
        let mut app = product.postgres.connect("certs_app", DATABASE).await;
        issue::record(&mut app, &accepted).await.expect("recorded");
        issue::record(&mut app, &revoked).await.expect("recorded");
        // Gateway revocation is Laysi/trustiot-v2#82; the test marks the row as the service
        // role would.
        sqlx::query(
            "UPDATE certs.issued_certificates SET revoked_at = now(), revocation_reason = 1
             WHERE serial = $1",
        )
        .bind(&revoked.serial)
        .execute(&mut app)
        .await
        .expect("mark the gateway certificate revoked");
    }
    let (workstation_key, request) = forged_request();
    let reply = service
        .post_json(
            CERTIFICATES_PATH,
            Some(&service.admin_token()),
            &json!({
                "workstation": WORKSTATION,
                "certificate_request": pem::encode(pem::CERTIFICATE_REQUEST, &request),
            }),
        )
        .await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let workstation: WorkstationCertificate =
        serde_json::from_str(&reply.body).expect("a workstation certificate");

    // The broker reads the files the service publishes, once they list the revoked gateway.
    let revoked_serial = issue::hex(&revoked.serial);
    let crls = wait_for("the revoked gateway in the published CRLs", || async {
        let (status, bundle) = service.get(CRL_PATH).await;
        (status == 200 && crl_entry(&bundle, &revoked_serial).is_some()).then_some(bundle)
    })
    .await;
    assert_eq!(crl_entry(&crls, &workstation.serial), None);
    let (status, chain) = service.get(CHAIN_PATH).await;
    assert_eq!(status, 200, "{chain}");

    let broker = Broker::start(runtime);
    let (server_ca, server_certificate, server_key) = broker_server_certificate();
    broker.push(&format!("{BROKER_DIR}/ca.pem"), chain.as_bytes());
    broker.push(&format!("{BROKER_DIR}/crl.pem"), crls.as_bytes());
    broker.push(&format!("{BROKER_DIR}/server-ca.pem"), server_ca.as_bytes());
    broker.push(
        &format!("{BROKER_DIR}/server.pem"),
        server_certificate.as_bytes(),
    );
    broker.push(
        &format!("{BROKER_DIR}/server-key.pem"),
        server_key.as_bytes(),
    );
    for (name, key, certificate) in [
        (
            "gateway-accepted",
            &accepted_key,
            pem::encode(pem::CERTIFICATE, &accepted.der),
        ),
        (
            "gateway-revoked",
            &revoked_key,
            pem::encode(pem::CERTIFICATE, &revoked.der),
        ),
        (
            "workstation",
            &workstation_key,
            workstation.certificate.clone(),
        ),
    ] {
        broker.push(
            &format!("{BROKER_DIR}/clients/{name}.pem"),
            certificate.as_bytes(),
        );
        broker.push(
            &format!("{BROKER_DIR}/clients/{name}-key.pem"),
            key.serialize_pem().as_bytes(),
        );
    }
    broker.push(
        &format!("{BROKER_DIR}/mosquitto.conf"),
        MOSQUITTO_CONF.as_bytes(),
    );
    broker.push(&format!("{BROKER_DIR}/ready"), b"");
    broker.wait_running().await;

    let mut outcomes = Vec::new();
    for name in ["gateway-accepted", "gateway-revoked", "workstation"] {
        let (accepted, output) = broker.connect_as(name).await;
        println!("== {name}: accepted {accepted}\n{output}");
        outcomes.push((name, accepted, output));
    }
    println!("== broker log\n{}", broker.logs());
    let accepted_by_name: Vec<(&str, bool)> = outcomes
        .iter()
        .map(|(name, accepted, _)| (*name, *accepted))
        .collect();
    assert_eq!(
        accepted_by_name,
        [
            ("gateway-accepted", true),
            ("gateway-revoked", false),
            ("workstation", false),
        ]
    );
    for (name, _, output) in &outcomes[1..] {
        assert!(
            !output.contains("received CONNACK"),
            "{name} got no CONNACK at all:\n{output}"
        );
    }
}
