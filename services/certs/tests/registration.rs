//! Factory registration (Laysi/trustiot-v2#79; services/certs/README.md, "出廠登錄"):
//!
//! - The workstation identity (no container): a request signed with a workstation certificate's
//!   key holds; one signed with a gateway certificate, a certificate of another CA, another key,
//!   over another body or path, too old, or with a missing or malformed header does not, and an
//!   expired workstation certificate does not either; nor does a certificate of the issuing CA
//!   that has the workstation profile but no keyUsage digitalSignature, or is a CA.
//! - The API (a throwaway Postgres with TLS and the real binary): a new hardware ID is
//!   registered with its initial gateway certificate and one `certs.gateway_registered` event; a
//!   repeat returns the same record and certificate and writes nothing (acceptance 1, 5), also
//!   when two arrive at once; a changed public key or endorsement key is refused (acceptance 2);
//!   a decommissioned hardware ID is refused and never freed (acceptance 3); a request without
//!   the workstation's signature or without a platform administrator registers nothing
//!   (acceptance 4); a gateway certificate cannot act as a workstation (acceptance 6); a
//!   replayed request and a revoked workstation certificate are refused.
//! - The certificate chain (Laysi/trustiot-v2#610, the real binary): the 201 and the 200 carry
//!   the certificate, then the issuing CA certificate whose key verifies it, whose key id is
//!   `ca_key_id` and which the root verifies; with two issuing CAs loaded, a repeat carries the
//!   one that signed it, not the one that signs now; after rotation step 3 removed it, a repeat
//!   still answers 200 with the first certificate and a chain of the certificate alone, and
//!   logs a WARN with the key id only.
//!
//! The API test is skipped without podman or docker unless `CI` is set. Run with `--nocapture`
//! to see the evidence lines the test report quotes.

mod support;

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ca_keys::{CaKey, Request};
use certs::authority::Authority;
use certs::issue::{self, WORKSTATION_PURPOSE_DER, WORKSTATION_SUBJECT_PREFIX, WorkstationName};
use certs::pem;
use certs::registrations::{
    DECOMMISSIONED, ENDORSEMENT_KEY_IN_USE, ENDORSEMENT_KEY_MISMATCH, ENDORSEMENT_KEY_UNSUPPORTED,
    PUBLIC_KEY_IN_USE, PUBLIC_KEY_MISMATCH, REGISTRATIONS_PATH, WORKSTATION_REFUSED,
    WorkstationRefusal, verify_workstation,
};
use certs::workstations::CERTIFICATES_PATH;
use contract::certs::{
    GatewayRegistration, WORKSTATION_CERTIFICATE_HEADER, WORKSTATION_NONCE_HEADER,
    WORKSTATION_SIGNATURE_HEADER, WORKSTATION_TIMESTAMP_HEADER, WorkstationCertificate,
    workstation_signing_input,
};
use contract::{ServiceName, TenantGrant, TenantId, TenantRole};
use protocol::GatewayId;
use rcgen::{
    BasicConstraints, CertificateParams, CustomExtension, DistinguishedName, DnType, IsCa, Issuer,
    KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, PKCS_ECDSA_P384_SHA384, PublicKeyData as _,
    SigningKey as _,
};
use rustls_pki_types::CertificateDer;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row as _;
use support::{Reply, Service, TestCa, now, wait_for};
use testkit::identity::TestIssuer;
use testkit::product::{ProductPostgres, product_postgres};
use x509_parser::certificate::X509Certificate;
use x509_parser::prelude::FromDer;

const WORKSTATION: &str = "line-b-ws-02";
const VALIDITY: Duration = Duration::from_secs(90 * 86_400);

/// The CRL schedule of `tests/workstation.rs`: the first CRL is out within seconds, so an
/// issuing CA can sign.
const FAST_CRLS: [(&str, &str); 4] = [
    ("TRUSTIOT__CRL__CHECK_INTERVAL", "1s"),
    ("TRUSTIOT__CRL__VALIDITY", "1h"),
    ("TRUSTIOT__CRL__RENEW_BEFORE", "3590s"),
    ("TRUSTIOT__CRL__ALERT_BEFORE", "3500s"),
];

/// A new P-256 key and a CSR for it, with a common name the CA ignores.
fn request() -> (KeyPair, Vec<u8>) {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "ignored");
    params.distinguished_name = name;
    let request = params
        .serialize_request(&key)
        .expect("a CSR")
        .der()
        .to_vec();
    (key, request)
}

/// A stand-in for a TPM's endorsement key: a P-384 public key, PEM.
fn endorsement_key() -> String {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).expect("an endorsement key");
    pem::encode(pem::PUBLIC_KEY, &key.subject_public_key_info())
}

/// A fresh nonce: 16 random bytes, base64url without padding.
fn nonce() -> String {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).expect("random");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The workstation headers of a request with `body` to `path`, signed at `timestamp` by `key`
/// with `certificate` (DER).
fn signed_headers(
    key: &KeyPair,
    certificate: &[u8],
    path: &str,
    body: &[u8],
    timestamp: i64,
    nonce: &str,
) -> Vec<(&'static str, String)> {
    let digest: [u8; 32] = Sha256::digest(body).into();
    let input = workstation_signing_input("POST", path, timestamp, nonce, &digest);
    let signature = key.sign(&input).expect("sign the request");
    vec![
        (WORKSTATION_CERTIFICATE_HEADER, STANDARD.encode(certificate)),
        (WORKSTATION_TIMESTAMP_HEADER, timestamp.to_string()),
        (WORKSTATION_NONCE_HEADER, nonce.to_owned()),
        (WORKSTATION_SIGNATURE_HEADER, STANDARD.encode(signature)),
    ]
}

fn header_map(headers: &[(&'static str, String)]) -> axum::http::HeaderMap {
    let mut map = axum::http::HeaderMap::new();
    for (name, value) in headers {
        map.append(*name, value.parse().expect("a header value"));
    }
    map
}

/// A workstation certificate for a new key, signed by the current issuing CA of `authority`.
fn workstation(authority: &Authority, name: &str) -> (KeyPair, Vec<u8>) {
    let (key, request) = request();
    let ca = authority.current(now()).expect("an issuing CA");
    let name = WorkstationName::parse(name).expect("a workstation name");
    let issued =
        issue::workstation_certificate(ca, &request, &name, VALIDITY, now()).expect("signed");
    (key, issued.der)
}

/// A gateway certificate for a new key, signed by the current issuing CA of `authority`.
fn gateway(authority: &Authority, id: &str) -> (KeyPair, Vec<u8>) {
    let (key, request) = request();
    let ca = authority.current(now()).expect("an issuing CA");
    let id = GatewayId::new(id).expect("a gateway id");
    let issued = issue::gateway_certificate(ca, &request, &id, VALIDITY, now()).expect("signed");
    (key, issued.der)
}

#[test]
fn only_a_workstation_certificate_signs_for_a_workstation() {
    let ca = TestCa::new(&[1]);
    let authority = ca.authority();
    let body = br#"{"hardware_id":"gw-1"}"#;
    let at = now();
    let check = |headers: &[(&'static str, String)], body: &[u8], path: &str, at: i64| {
        verify_workstation(&header_map(headers), "POST", path, body, &authority, at)
    };

    let (key, certificate) = workstation(&authority, WORKSTATION);
    let headers = signed_headers(&key, &certificate, REGISTRATIONS_PATH, body, at, &nonce());
    let signed = check(&headers, body, REGISTRATIONS_PATH, at).expect("a workstation");
    assert_eq!(signed.name.as_str(), WORKSTATION);
    let (_, parsed) = X509Certificate::from_der(&certificate).expect("a certificate");
    assert_eq!(signed.serial, parsed.raw_serial());
    println!("a workstation certificate's signature holds: {WORKSTATION}");

    // Acceptance 6: a gateway certificate of the same CA, correctly signed with its own key.
    let (gateway_key, gateway_certificate) = gateway(&authority, "gw-impostor");
    let headers = signed_headers(
        &gateway_key,
        &gateway_certificate,
        REGISTRATIONS_PATH,
        body,
        at,
        &nonce(),
    );
    assert_eq!(
        check(&headers, body, REGISTRATIONS_PATH, at).unwrap_err(),
        WorkstationRefusal::NotWorkstation
    );
    println!("a gateway certificate signing as a workstation: not_workstation");

    // A workstation certificate of another CA (another root and issuing CA).
    let other = TestCa::new(&[1]).authority();
    let (other_key, other_certificate) = workstation(&other, WORKSTATION);
    let headers = signed_headers(
        &other_key,
        &other_certificate,
        REGISTRATIONS_PATH,
        body,
        at,
        &nonce(),
    );
    assert_eq!(
        check(&headers, body, REGISTRATIONS_PATH, at).unwrap_err(),
        WorkstationRefusal::Untrusted
    );

    // Our certificate with a signature by another key, over another body, for another path.
    let (stranger, _) = request();
    let headers = signed_headers(
        &stranger,
        &certificate,
        REGISTRATIONS_PATH,
        body,
        at,
        &nonce(),
    );
    assert_eq!(
        check(&headers, body, REGISTRATIONS_PATH, at).unwrap_err(),
        WorkstationRefusal::SignatureInvalid
    );
    let headers = signed_headers(&key, &certificate, REGISTRATIONS_PATH, body, at, &nonce());
    assert_eq!(
        check(
            &headers,
            br#"{"hardware_id":"gw-2"}"#,
            REGISTRATIONS_PATH,
            at
        )
        .unwrap_err(),
        WorkstationRefusal::SignatureInvalid
    );
    assert_eq!(
        check(&headers, body, CERTIFICATES_PATH, at).unwrap_err(),
        WorkstationRefusal::SignatureInvalid
    );

    // Signed too long ago, or in the future, by more than the allowed skew; and an expired
    // certificate.
    for skew in [-301, 301] {
        let headers = signed_headers(
            &key,
            &certificate,
            REGISTRATIONS_PATH,
            body,
            at + skew,
            &nonce(),
        );
        assert_eq!(
            check(&headers, body, REGISTRATIONS_PATH, at).unwrap_err(),
            WorkstationRefusal::Stale,
            "{skew}"
        );
    }
    let later = at + i64::try_from(VALIDITY.as_secs()).expect("seconds") + 60;
    let headers = signed_headers(
        &key,
        &certificate,
        REGISTRATIONS_PATH,
        body,
        later,
        &nonce(),
    );
    assert_eq!(
        check(&headers, body, REGISTRATIONS_PATH, later).unwrap_err(),
        WorkstationRefusal::Expired
    );

    // Missing, repeated and malformed headers.
    let good = signed_headers(&key, &certificate, REGISTRATIONS_PATH, body, at, &nonce());
    for index in 0..good.len() {
        let mut missing = good.clone();
        missing.remove(index);
        assert_eq!(
            check(&missing, body, REGISTRATIONS_PATH, at).unwrap_err(),
            WorkstationRefusal::Missing,
            "{}",
            good[index].0
        );
        let mut repeated = good.clone();
        repeated.push(good[index].clone());
        assert_eq!(
            check(&repeated, body, REGISTRATIONS_PATH, at).unwrap_err(),
            WorkstationRefusal::Malformed,
            "{}",
            good[index].0
        );
    }
    for (name, value) in [
        (WORKSTATION_CERTIFICATE_HEADER, "not base64!".to_owned()),
        (WORKSTATION_CERTIFICATE_HEADER, STANDARD.encode(b"not DER")),
        (WORKSTATION_TIMESTAMP_HEADER, "-5".to_owned()),
        (WORKSTATION_NONCE_HEADER, "short".to_owned()),
        (WORKSTATION_SIGNATURE_HEADER, "@@".to_owned()),
    ] {
        let headers: Vec<(&'static str, String)> = good
            .iter()
            .map(|(header, current)| {
                let value = if *header == name { &value } else { current };
                (*header, value.clone())
            })
            .collect();
        assert_eq!(
            check(&headers, body, REGISTRATIONS_PATH, at).unwrap_err(),
            WorkstationRefusal::Malformed,
            "{name}: {value}"
        );
    }
}

/// A workstation certificate for a new key that `issue::workstation_certificate` would sign
/// (common name `workstation:<WORKSTATION>`, keyUsage digitalSignature, not a CA, the workstation
/// purpose as the only extended key usage), signed by the first issuing CA of `ca`, with
/// `adjust` applied to its parameters first.
fn workstation_variant(
    ca: &TestCa,
    adjust: impl FnOnce(&mut CertificateParams),
) -> (KeyPair, Vec<u8>) {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
    let signing_key = ca.keys().into_keys().remove(0).into_signing_key();
    let issuing = ca.issuing_ders().remove(0);
    let issuer = Issuer::from_ca_cert_der(&CertificateDer::from(issuing.as_slice()), signing_key)
        .expect("the issuing CA as an issuer");
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    let common_name = format!("{WORKSTATION_SUBJECT_PREFIX}{WORKSTATION}");
    name.push(DnType::CommonName, common_name.as_str());
    params.distinguished_name = name;
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    // extendedKeyUsage (2.5.29.37): SEQUENCE { OBJECT IDENTIFIER <the workstation purpose> }.
    let mut purposes = vec![0x30, 0x16, 0x06, 0x14];
    purposes.extend_from_slice(&WORKSTATION_PURPOSE_DER);
    params.custom_extensions = vec![CustomExtension::from_oid_content(&[2, 5, 29, 37], purposes)];
    params.use_authority_key_identifier_extension = true;
    let start = now() - 60;
    params.not_before = time::OffsetDateTime::from_unix_timestamp(start).expect("a time");
    params.not_after = time::OffsetDateTime::from_unix_timestamp(start + 86_400).expect("a time");
    adjust(&mut params);
    let certificate = params.signed_by(&key, &issuer).expect("signed");
    (key, certificate.der().to_vec())
}

/// One change to an otherwise valid workstation certificate's parameters.
type Adjust = fn(&mut CertificateParams);

#[test]
fn a_workstation_certificate_needs_digital_signature_and_no_ca() {
    let ca = TestCa::new(&[1]);
    let authority = ca.authority();
    let body = br#"{"hardware_id":"gw-1"}"#;
    let at = now();
    let check = |key: &KeyPair, certificate: &[u8]| {
        let headers = signed_headers(key, certificate, REGISTRATIONS_PATH, body, at, &nonce());
        verify_workstation(
            &header_map(&headers),
            "POST",
            REGISTRATIONS_PATH,
            body,
            &authority,
            at,
        )
    };

    // The control: built here rather than by the CA, the profile holds, so each refusal below
    // is the one change it makes.
    let (key, certificate) = workstation_variant(&ca, |_| {});
    let signed = check(&key, &certificate).expect("the workstation profile");
    assert_eq!(signed.name.as_str(), WORKSTATION);

    let cases: [(&str, Adjust); 3] = [
        ("keyUsage without digitalSignature", |params| {
            params.key_usages = vec![KeyUsagePurpose::KeyEncipherment];
        }),
        ("no keyUsage at all", |params| {
            params.key_usages = Vec::new();
        }),
        ("basicConstraints CA:TRUE", |params| {
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        }),
    ];
    for (case, adjust) in cases {
        let (key, certificate) = workstation_variant(&ca, adjust);
        assert_eq!(
            check(&key, &certificate).unwrap_err(),
            WorkstationRefusal::NotWorkstation,
            "{case}"
        );
        println!("a workstation certificate with {case}: not_workstation");
    }
}

/// `POST` a registration of `body` with `token` and `headers`.
async fn post(
    service: &Service,
    token: Option<&str>,
    headers: &[(&'static str, String)],
    body: &[u8],
) -> Reply {
    let mut request = reqwest::Client::new()
        .post(format!("{}{REGISTRATIONS_PATH}", service.base_url))
        .timeout(Duration::from_secs(30))
        .header("content-type", "application/json")
        .body(body.to_vec());
    if let Some(token) = token {
        request = request.header(contract::INTERNAL_IDENTITY_HEADER, token);
    }
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let response = request.send().await.expect("call the certs service");
    let status = response.status().as_u16();
    let cache_control = response
        .headers()
        .get("cache-control")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    Reply {
        status,
        body: response.text().await.expect("read the body"),
        cache_control,
    }
}

/// A production workstation: its key and certificate (DER) from the platform administrators'
/// API, and the administrator signed in on it.
struct Workstation<'a> {
    service: &'a Service,
    key: KeyPair,
    certificate: Vec<u8>,
    serial: String,
}

impl Workstation<'_> {
    /// Registers `body` (JSON) as a platform administrator, signed by this workstation now.
    async fn register(&self, body: &Value) -> Reply {
        let body = body.to_string().into_bytes();
        let headers = self.headers(&body);
        post(
            self.service,
            Some(&self.service.admin_token()),
            &headers,
            &body,
        )
        .await
    }

    fn headers(&self, body: &[u8]) -> Vec<(&'static str, String)> {
        signed_headers(
            &self.key,
            &self.certificate,
            REGISTRATIONS_PATH,
            body,
            now(),
            &nonce(),
        )
    }
}

/// A new gateway: its TPM key and the registration body for `hardware_id`.
fn new_gateway(hardware_id: &str) -> (KeyPair, Value) {
    let (key, request) = request();
    let body = json!({
        "hardware_id": hardware_id,
        "certificate_request": pem::encode(pem::CERTIFICATE_REQUEST, &request),
        "endorsement_key": endorsement_key(),
    });
    (key, body)
}

/// A production workstation named `name`: a new key and its certificate from the platform
/// administrators' API, signed by the issuing CA that signs now.
async fn new_workstation<'a>(service: &'a Service, name: &str) -> (Workstation<'a>, String) {
    let (key, csr) = request();
    let issued = service
        .post_json(
            CERTIFICATES_PATH,
            Some(&service.admin_token()),
            &json!({
                "workstation": name,
                "certificate_request": pem::encode(pem::CERTIFICATE_REQUEST, &csr),
            }),
        )
        .await;
    assert_eq!(issued.status, 201, "{}", issued.body);
    let issued: WorkstationCertificate = serde_json::from_str(&issued.body).expect("issued");
    let workstation = Workstation {
        service,
        key,
        certificate: pem::decode_all(&issued.certificate, pem::CERTIFICATE)
            .expect("PEM")
            .remove(0),
        serial: issued.serial,
    };
    (workstation, issued.ca_key_id)
}

/// The two certificates of `registration.certificate_chain`, DER, once checked: the first is the
/// response's `certificate`; the second's public key verifies the first's signature, its key id
/// is the response's `ca_key_id`, and `root` (DER) verifies its signature.
fn checked_chain(registration: &GatewayRegistration, root: &[u8]) -> [Vec<u8>; 2] {
    let chain = pem::decode_all(&registration.certificate_chain, pem::CERTIFICATE)
        .expect("a PEM certificate chain");
    let [leaf_der, issuing_der]: [Vec<u8>; 2] = chain
        .try_into()
        .unwrap_or_else(|chain: Vec<Vec<u8>>| panic!("{} certificates, not 2", chain.len()));
    let certificate = pem::decode_all(&registration.certificate, pem::CERTIFICATE)
        .expect("PEM")
        .remove(0);
    assert_eq!(
        leaf_der, certificate,
        "the chain starts with the certificate"
    );
    let (_, leaf) = X509Certificate::from_der(&leaf_der).expect("the gateway certificate");
    let (_, issuing) = X509Certificate::from_der(&issuing_der).expect("the issuing CA");
    let (_, root) = X509Certificate::from_der(root).expect("the root");
    leaf.verify_signature(Some(issuing.public_key()))
        .expect("the issuing CA's public key verifies the gateway certificate");
    issuing
        .verify_signature(Some(root.public_key()))
        .expect("the root verifies the issuing CA certificate");
    // Negative control: the check tells keys apart.
    assert!(
        leaf.verify_signature(Some(root.public_key())).is_err(),
        "the root did not sign the gateway certificate itself"
    );
    assert_eq!(
        ca_keys::key_id(issuing.public_key().raw),
        registration.ca_key_id,
        "the chain's issuing CA is the one that signed the certificate"
    );
    [leaf_der, issuing_der]
}

/// The rows of a table count, as the superuser sees them.
async fn count(product: &ProductPostgres, query: &'static str) -> i64 {
    sqlx::query_scalar(query)
        .fetch_one(&mut product.superuser().await)
        .await
        .expect("count")
}

// The tests of this binary share one database (`testkit::product`) and may run at once, so the
// counts cover the hardware IDs of the first scenario (`gw-*`) only; the rotation scenario
// registers `rot-*`.
const REGISTRATIONS: &str =
    "SELECT count(*) FROM certs.gateway_registrations WHERE hardware_id LIKE 'gw-%'";
const GATEWAY_CERTIFICATES: &str = "SELECT count(*) FROM certs.issued_certificates
    WHERE profile = 'gateway' AND subject LIKE 'gw-%'";
const EVENTS: &str = "SELECT count(*) FROM certs.outbox_event
    WHERE event_type = 'certs.gateway_registered' AND aggregate_id LIKE 'gw-%'";

/// What the database holds for the `gw-*` hardware IDs: registrations, gateway certificates,
/// registration events.
async fn totals(product: &ProductPostgres) -> (i64, i64, i64) {
    (
        count(product, REGISTRATIONS).await,
        count(product, GATEWAY_CERTIFICATES).await,
        count(product, EVENTS).await,
    )
}

/// Sessions waiting for a workstation certificate's lock (`issue::LOCK_WORKSTATION`'s class).
const WAITING_REVOCATIONS: &str = "SELECT count(*) FROM pg_locks
    WHERE locktype = 'advisory' AND classid = 1953983347 AND NOT granted";

/// A request that must register nothing: what it is, its token, its workstation headers, the
/// expected status and `details.reason` (empty: none).
type RefusalCase = (
    &'static str,
    Option<String>,
    Vec<(&'static str, String)>,
    u16,
    &'static str,
);

fn message_key(reply: &Reply) -> String {
    reply.json()["message_key"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario on one database: each step builds on the registrations before it"
)]
async fn registration_is_idempotent_and_needs_a_workstation_and_an_administrator() {
    let Some(product) = product_postgres(&["certs_app", "certs_platform"]).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let service = Service::start(&product, &ca, &FAST_CRLS).await;
    wait_for("the published CRLs", || async {
        let (status, body) = service.get(certs::http::CRL_PATH).await;
        (status == 200).then_some(body)
    })
    .await;

    // The workstation gets its certificate from a platform administrator (Laysi/trustiot-v2#534).
    let (key, csr) = request();
    let issued = service
        .post_json(
            CERTIFICATES_PATH,
            Some(&service.admin_token()),
            &json!({
                "workstation": WORKSTATION,
                "certificate_request": pem::encode(pem::CERTIFICATE_REQUEST, &csr),
            }),
        )
        .await;
    assert_eq!(issued.status, 201, "{}", issued.body);
    let issued: WorkstationCertificate = serde_json::from_str(&issued.body).expect("issued");
    let workstation = Workstation {
        service: &service,
        key,
        certificate: pem::decode_all(&issued.certificate, pem::CERTIFICATE)
            .expect("PEM")
            .remove(0),
        serial: issued.serial.clone(),
    };

    // A new hardware ID: 201 with the initial certificate, one event.
    let (gateway_key, body) = new_gateway("gw-0001");
    let first = workstation.register(&body).await;
    assert_eq!(first.status, 201, "{}", first.body);
    assert_eq!(first.cache_control.as_deref(), Some("no-store"));
    let registered: GatewayRegistration = serde_json::from_str(&first.body).expect("registered");
    println!("registered: {}", first.body.replace('\n', " "));
    assert_eq!(registered.hardware_id, "gw-0001");
    assert_eq!(registered.workstation, WORKSTATION);
    // The trust anchor the gateway keeps for the broker's server certificate.
    assert_eq!(
        pem::decode_all(&registered.root_certificate, pem::CERTIFICATE).expect("PEM"),
        [ca.root_der()]
    );
    let gateway_certificate = pem::decode_all(&registered.certificate, pem::CERTIFICATE)
        .expect("PEM")
        .remove(0);
    let (_, parsed) = X509Certificate::from_der(&gateway_certificate).expect("a certificate");
    let common_names: Vec<&str> = parsed
        .subject()
        .iter_common_name()
        .filter_map(|name| name.as_str().ok())
        .collect();
    assert_eq!(common_names, ["gw-0001"]);
    assert_eq!(
        parsed.public_key().raw,
        gateway_key.subject_public_key_info().as_slice(),
        "the certificate is for the TPM key of the CSR"
    );
    support::verify_client(
        &ca.root_der(),
        &ca.issuing_ders(),
        &gateway_certificate,
        &[],
        now(),
    )
    .expect("the initial certificate is a client certificate under the root");
    // Laysi/trustiot-v2#610: the chain the gateway presents, the certificate and then the issuing
    // CA that signed it.
    let [_, issuing] = checked_chain(&registered, &ca.root_der());
    assert_eq!(issuing, ca.issuing_ders()[0]);
    println!(
        "201 certificate_chain: the certificate, then the issuing CA {}; signatures verify up to \
         the root",
        registered.ca_key_id
    );
    assert_eq!(totals(&product).await, (1, 1, 1));

    // The event: platform-wide, the administrator as the actor, the registration's facts.
    let row = sqlx::query(
        "SELECT tenant_id IS NULL AS platform, aggregate_type, aggregate_id,
                envelope::text AS envelope
         FROM certs.outbox_event
         WHERE event_type = 'certs.gateway_registered' AND aggregate_id LIKE 'gw-%'",
    )
    .fetch_one(&mut product.superuser().await)
    .await
    .expect("the event");
    assert!(row.get::<bool, _>("platform"));
    assert_eq!(
        row.get::<String, _>("aggregate_type"),
        "gateway_registration"
    );
    assert_eq!(row.get::<String, _>("aggregate_id"), "gw-0001");
    let envelope: Value =
        serde_json::from_str(&row.get::<String, _>("envelope")).expect("the envelope as JSON");
    println!("event: {envelope}");
    assert_eq!(envelope["producer"], json!("certs"));
    assert_eq!(envelope["tenant_id"], Value::Null);
    assert_eq!(envelope["actor"]["kind"], json!("user"));
    assert_eq!(
        envelope["payload"],
        json!({
            "hardware_id": "gw-0001",
            "public_key_sha256": registered.public_key_sha256,
            "endorsement_key_sha256": registered.endorsement_key_sha256,
            "certificate_serial": registered.serial,
            "workstation": WORKSTATION,
        })
    );

    // Acceptance 1: the same registration again (a new nonce, as a retry has) returns the same
    // record and certificate, and signs and writes nothing.
    let again = workstation.register(&body).await;
    assert_eq!(again.status, 200, "{}", again.body);
    let repeated: GatewayRegistration = serde_json::from_str(&again.body).expect("repeated");
    assert_eq!(repeated, registered);
    checked_chain(&repeated, &ca.root_der());
    assert_eq!(totals(&product).await, (1, 1, 1));
    println!("repeated: 200, the same record and certificate; totals (1, 1, 1)");

    // A replay of the first request's exact headers and body is refused.
    let replay_body = body.to_string().into_bytes();
    let replay_headers = workstation.headers(&replay_body);
    let once = post(
        &service,
        Some(&service.admin_token()),
        &replay_headers,
        &replay_body,
    )
    .await;
    assert_eq!(once.status, 200, "{}", once.body);
    let replayed = post(
        &service,
        Some(&service.admin_token()),
        &replay_headers,
        &replay_body,
    )
    .await;
    assert_eq!(replayed.status, 403, "{}", replayed.body);
    assert_eq!(message_key(&replayed), WORKSTATION_REFUSED.as_str());
    assert_eq!(replayed.json()["details"]["reason"], json!("replayed"));

    // Acceptance 2: the same hardware ID with another TPM key, or another endorsement key.
    let (_, other_key) = new_gateway("gw-0001");
    let mut changed_key = body.clone();
    changed_key["certificate_request"] = other_key["certificate_request"].clone();
    let refused = workstation.register(&changed_key).await;
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(message_key(&refused), PUBLIC_KEY_MISMATCH.as_str());
    let mut changed_endorsement = body.clone();
    changed_endorsement["endorsement_key"] = json!(endorsement_key());
    let refused = workstation.register(&changed_endorsement).await;
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(message_key(&refused), ENDORSEMENT_KEY_MISMATCH.as_str());
    // A new hardware ID with a registered TPM key or endorsement key.
    let mut stolen_key = other_key.clone();
    stolen_key["hardware_id"] = json!("gw-0002");
    stolen_key["certificate_request"] = body["certificate_request"].clone();
    let refused = workstation.register(&stolen_key).await;
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(message_key(&refused), PUBLIC_KEY_IN_USE.as_str());
    let mut shared_endorsement = other_key.clone();
    shared_endorsement["hardware_id"] = json!("gw-0002");
    shared_endorsement["endorsement_key"] = body["endorsement_key"].clone();
    let refused = workstation.register(&shared_endorsement).await;
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(message_key(&refused), ENDORSEMENT_KEY_IN_USE.as_str());
    let mut rsa_1024 = other_key.clone();
    rsa_1024["hardware_id"] = json!("gw-0002");
    rsa_1024["endorsement_key"] = json!(pem::encode(pem::PUBLIC_KEY, &rsa_public_key(1024)));
    let refused = workstation.register(&rsa_1024).await;
    assert_eq!(refused.status, 422, "{}", refused.body);
    assert_eq!(
        refused.json()["field_errors"][0]["message_key"],
        json!(ENDORSEMENT_KEY_UNSUPPORTED.as_str())
    );
    assert_eq!(totals(&product).await, (1, 1, 1));
    println!(
        "changed public key, changed endorsement key, keys of another gateway: 409; totals (1, 1, 1)"
    );

    // Acceptance 4: without the workstation's signature, or without a platform administrator,
    // nothing is registered.
    let (_, fresh) = new_gateway("gw-0003");
    let fresh_bytes = fresh.to_string().into_bytes();
    let tenant_admin = TenantGrant {
        id: TenantId::generate(),
        role: TenantRole::Admin,
    };
    let cases: Vec<RefusalCase> = vec![
        (
            "a platform administrator without the workstation headers",
            Some(service.admin_token()),
            Vec::new(),
            403,
            "missing",
        ),
        (
            "the workstation's signature without a token",
            None,
            workstation.headers(&fresh_bytes),
            401,
            "",
        ),
        (
            "the workstation's signature with a token of another issuer",
            Some(TestIssuer::new().token(&{
                let mut claims = TestIssuer::claims(ServiceName::Certs, None);
                claims.platform_admin = true;
                claims
            })),
            workstation.headers(&fresh_bytes),
            401,
            "",
        ),
        (
            "the workstation's signature with a tenant administrator",
            Some(service.token(false, Some(tenant_admin), None)),
            workstation.headers(&fresh_bytes),
            403,
            "",
        ),
        (
            "the workstation's signature with an exchanged administrator token",
            Some(service.token(true, None, Some(ServiceName::Assets))),
            workstation.headers(&fresh_bytes),
            403,
            "",
        ),
        (
            "the signature of another key with the workstation certificate",
            Some(service.admin_token()),
            signed_headers(
                &request().0,
                &workstation.certificate,
                REGISTRATIONS_PATH,
                &fresh_bytes,
                now(),
                &nonce(),
            ),
            403,
            "signature_invalid",
        ),
        (
            "a signature over another body",
            Some(service.admin_token()),
            workstation.headers(br#"{"hardware_id":"gw-0003"}"#),
            403,
            "signature_invalid",
        ),
        // Acceptance 6: the registered gateway's own certificate and TPM key.
        (
            "a gateway certificate in place of a workstation certificate",
            Some(service.admin_token()),
            signed_headers(
                &gateway_key,
                &gateway_certificate,
                REGISTRATIONS_PATH,
                &fresh_bytes,
                now(),
                &nonce(),
            ),
            403,
            "not_workstation",
        ),
    ];
    for (what, token, headers, status, reason) in &cases {
        let reply = post(&service, token.as_deref(), headers, &fresh_bytes).await;
        assert_eq!(reply.status, *status, "{what}: {}", reply.body);
        assert_eq!(reply.cache_control.as_deref(), Some("no-store"), "{what}");
        if reason.is_empty() {
            assert!(
                reply.json()["details"]["reason"].is_null(),
                "{what}: {}",
                reply.body
            );
        } else {
            assert_eq!(
                reply.json()["details"]["reason"],
                json!(reason),
                "{what}: {}",
                reply.body
            );
        }
        println!("{what}: {} {}", reply.status, reply.body.replace('\n', " "));
    }
    assert_eq!(totals(&product).await, (1, 1, 1));

    // A body that is not a JSON object is a 400 before the workstation is checked.
    for body in [&b"[1]"[..], b"not json"] {
        let reply = post(&service, Some(&service.admin_token()), &[], body).await;
        assert_eq!(reply.status, 400, "{}", reply.body);
        assert_eq!(message_key(&reply), "gateway_registration.body_invalid");
    }

    // Two registrations of a new hardware ID at once: one registers, the other gets the same
    // record; one certificate, one event.
    let (_, concurrent) = new_gateway("gw-0004");
    let (left, right) = tokio::join!(
        workstation.register(&concurrent),
        workstation.register(&concurrent)
    );
    let mut statuses = [left.status, right.status];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 201], "{} / {}", left.body, right.body);
    assert_eq!(left.json()["serial"], right.json()["serial"]);
    assert_eq!(totals(&product).await, (2, 2, 2));
    println!("two at once: {statuses:?}, one certificate and one event");

    // Acceptance 3: a decommissioned hardware ID stays taken. The decommissioning API is
    // Laysi/trustiot-v2#115; here the superuser sets the column, the one change the table allows.
    let mut superuser = product.superuser().await;
    sqlx::query(
        "UPDATE certs.gateway_registrations SET decommissioned_at = now()
         WHERE hardware_id = 'gw-0004'",
    )
    .execute(&mut superuser)
    .await
    .expect("decommission");
    let refused = workstation.register(&concurrent).await;
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(message_key(&refused), DECOMMISSIONED.as_str());
    let (_, reuse) = new_gateway("gw-0004");
    let refused = workstation.register(&reuse).await;
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert_eq!(message_key(&refused), DECOMMISSIONED.as_str());
    // Not even the superuser frees it, rewrites it or takes the decommissioning back.
    for statement in [
        "DELETE FROM certs.gateway_registrations WHERE hardware_id = 'gw-0004'",
        "TRUNCATE certs.gateway_registrations CASCADE",
        "UPDATE certs.gateway_registrations SET decommissioned_at = NULL
         WHERE hardware_id = 'gw-0004'",
        "UPDATE certs.gateway_registrations SET workstation = 'other'
         WHERE hardware_id = 'gw-0001'",
    ] {
        let error = sqlx::query(statement)
            .execute(&mut superuser)
            .await
            .expect_err(statement);
        let code = error
            .as_database_error()
            .and_then(|problem| problem.code().map(|code| code.into_owned()));
        assert_eq!(code.as_deref(), Some("42501"), "{statement}: {error}");
        println!("refused as superuser: {statement}: {error}");
    }
    // The service role reaches no registration at all.
    let mut app = product
        .postgres
        .connect("certs_app", testkit::product::DATABASE)
        .await;
    let error = sqlx::query("SELECT count(*) FROM certs.gateway_registrations")
        .execute(&mut app)
        .await
        .expect_err("certs_app reads no registration");
    println!("refused as certs_app: {error}");
    assert_eq!(totals(&product).await, (2, 2, 2));

    // A revocation waits for a registration that already read the workstation's ledger row as
    // valid: here a transaction holds the registration's lock on the certificate, and the
    // revocation commits only after it ends. Then the revoked certificate registers nothing.
    let (_, parsed) = X509Certificate::from_der(&workstation.certificate).expect("a certificate");
    let serial = parsed.raw_serial().to_vec();
    let mut registering = product.superuser().await;
    sqlx::raw_sql("BEGIN")
        .execute(&mut registering)
        .await
        .expect("begin");
    sqlx::query(issue::LOCK_WORKSTATION_SHARED)
        .bind(&serial)
        .execute(&mut registering)
        .await
        .expect("take the registration's lock");
    let product_ref = &product;
    let revoke_path = format!("{CERTIFICATES_PATH}/{}/revoke", workstation.serial);
    let admin = service.admin_token();
    let revoke_body = json!({"reason": "key_compromise"});
    let (revoked, ()) = tokio::join!(
        service.post_json(&revoke_path, Some(&admin), &revoke_body),
        async {
            wait_for(
                "the revocation waiting for the registration's lock",
                || async move {
                    let waiting: i64 = sqlx::query_scalar(WAITING_REVOCATIONS)
                        .fetch_one(&mut product_ref.superuser().await)
                        .await
                        .expect("read pg_locks");
                    (waiting == 1).then_some(())
                },
            )
            .await;
            let revoked_yet: bool = sqlx::query_scalar(
                "SELECT revoked_at IS NOT NULL FROM certs.issued_certificates WHERE serial = $1",
            )
            .bind(&serial)
            .fetch_one(&mut product_ref.superuser().await)
            .await
            .expect("the ledger row");
            assert!(!revoked_yet, "the revocation waited for the registration");
            println!("a revocation waits for a registration in progress: not revoked yet");
            sqlx::raw_sql("COMMIT")
                .execute(&mut registering)
                .await
                .expect("commit");
        }
    );
    assert_eq!(revoked.status, 200, "{}", revoked.body);
    let (_, after) = new_gateway("gw-0005");
    let refused = workstation.register(&after).await;
    assert_eq!(refused.status, 403, "{}", refused.body);
    assert_eq!(refused.json()["details"]["reason"], json!("revoked"));
    assert_eq!(totals(&product).await, (2, 2, 2));
    println!("a revoked workstation: 403 revoked; totals (2, 2, 2)");

    // The service's stdout reaches the test through a reader thread, so a line can arrive after
    // its response does.
    let lines = wait_for("both registration log lines", || async {
        let lines = service.stdout.messages("registered a gateway");
        (lines.len() >= 2).then_some(lines)
    })
    .await;
    assert_eq!(lines.len(), 2);
    for line in &lines {
        for secret in ["certificate_request", "BEGIN"] {
            assert!(!line.to_string().contains(secret), "{line}");
        }
    }
}

/// An RSA public key of `bits` bits, DER SubjectPublicKeyInfo (exponent 65537); the modulus is
/// not a real product of primes, which the service never checks.
fn rsa_public_key(bits: usize) -> Vec<u8> {
    fn der(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let len = content.len();
        if len < 0x80 {
            out.push(u8::try_from(len).expect("short"));
        } else {
            let bytes: Vec<u8> = len
                .to_be_bytes()
                .into_iter()
                .skip_while(|&byte| byte == 0)
                .collect();
            out.push(0x80 | u8::try_from(bytes.len()).expect("length octets"));
            out.extend(bytes);
        }
        out.extend_from_slice(content);
        out
    }
    let mut modulus = vec![0x00, 0xc0];
    modulus.extend(std::iter::repeat_n(0x5a, bits / 8 - 1));
    let key = der(
        0x30,
        &[der(0x02, &modulus), der(0x02, &[0x01, 0x00, 0x01])].concat(),
    );
    let mut bit_string = vec![0x00];
    bit_string.extend(key);
    // rsaEncryption with NULL parameters.
    let algorithm = der(
        0x30,
        &[
            der(
                0x06,
                &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01],
            ),
            vec![0x05, 0x00],
        ]
        .concat(),
    );
    der(0x30, &[algorithm, der(0x03, &bit_string)].concat())
}

#[test]
fn endorsement_keys_are_the_ones_a_tpm_makes() {
    use certs::registrations::parse_registration;

    let (_, request) = request();
    let body = |key: &str| {
        json!({
            "hardware_id": "gw-1",
            "certificate_request": pem::encode(pem::CERTIFICATE_REQUEST, &request),
            "endorsement_key": key,
        })
        .to_string()
        .into_bytes()
    };
    for bits in [2048, 3072, 4096] {
        let key = pem::encode(pem::PUBLIC_KEY, &rsa_public_key(bits));
        assert!(parse_registration(&body(&key)).is_ok(), "RSA {bits}");
    }
    for algorithm in [&PKCS_ECDSA_P256_SHA256, &PKCS_ECDSA_P384_SHA384] {
        let key = KeyPair::generate_for(algorithm).expect("a key");
        let key = pem::encode(pem::PUBLIC_KEY, &key.subject_public_key_info());
        assert!(parse_registration(&body(&key)).is_ok(), "{algorithm:?}");
    }
    for bits in [1024, 2040, 8192] {
        let key = pem::encode(pem::PUBLIC_KEY, &rsa_public_key(bits));
        let refused = parse_registration(&body(&key)).expect_err("too small or too large");
        assert_eq!(
            refused.field_errors()[0].message_key.as_str(),
            ENDORSEMENT_KEY_UNSUPPORTED.as_str(),
            "RSA {bits}"
        );
    }
    let ed25519 = KeyPair::generate_for(&rcgen::PKCS_ED25519).expect("an Ed25519 key");
    let refused = parse_registration(&body(&pem::encode(
        pem::PUBLIC_KEY,
        &ed25519.subject_public_key_info(),
    )))
    .expect_err("Ed25519");
    assert_eq!(
        refused.field_errors()[0].message_key.as_str(),
        ENDORSEMENT_KEY_UNSUPPORTED.as_str()
    );
    // A certificate in place of a public key, and two keys.
    let p256 = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
    let one = pem::encode(pem::PUBLIC_KEY, &p256.subject_public_key_info());
    for wrong in [
        pem::encode(pem::CERTIFICATE, &p256.subject_public_key_info()),
        format!("{one}{one}"),
    ] {
        let refused = parse_registration(&body(&wrong)).expect_err("not one public key");
        assert_eq!(
            refused.field_errors()[0].message_key.as_str(),
            "gateway_registration.endorsement_key_invalid"
        );
    }
}

/// Laysi/trustiot-v2#610 across an issuing CA rotation (services/certs/README.md, "輪替"): the
/// chain carries the issuing CA that signed the certificate, by its key id, not the one that
/// signs now; once rotation step 3 removed that CA, a repeat still answers 200 with the first
/// certificate, and its chain is the certificate alone with a WARN naming the key id.
#[tokio::test(flavor = "multi_thread")]
async fn the_chain_carries_the_issuing_ca_that_signed_the_certificate() {
    let Some(product) = product_postgres(&["certs_app", "certs_platform"]).await else {
        return;
    };
    // Generation 1 has its certificate; generation 2's key waits for its ceremony.
    let mut ca = TestCa::new(&[1, 2]);
    let both = ca.issuing_pems.clone();
    let ders = ca.issuing_ders();
    let ids: Vec<String> = ca.keys().key_ids().map(str::to_owned).collect();
    ca.issuing_pems.truncate(1);
    ca.write_all();
    let mut env = FAST_CRLS.to_vec();
    env.push(("TRUSTIOT__CA__RELOAD_INTERVAL", "1s"));
    let service = Service::start(&product, &ca, &env).await;
    wait_for("the published CRL of generation 1", || async {
        let (status, body) = service.get(certs::http::CRL_PATH).await;
        (status == 200 && !body.is_empty()).then_some(body)
    })
    .await;

    // A gateway registered while generation 1 signs.
    let (first_station, signer) = new_workstation(&service, "line-c-ws-01").await;
    assert_eq!(signer, ids[0]);
    let (_, old_body) = new_gateway("rot-0101");
    let reply = first_station.register(&old_body).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let old: GatewayRegistration = serde_json::from_str(&reply.body).expect("registered");
    assert_eq!(old.ca_key_id, ids[0]);
    let [_, issuing] = checked_chain(&old, &ca.root_der());
    assert_eq!(issuing, ders[0]);

    // Rotation step 2: generation 2's certificate joins generation 1's, and once it has its
    // first CRL it signs. Two issuing CAs are loaded.
    ca.issuing_pems = both;
    ca.write_all();
    wait_for("the published CRLs of both generations", || async {
        let (status, body) = service.get(certs::http::CRL_PATH).await;
        (status == 200)
            .then(|| pem::decode_all(&body, pem::CRL).ok())
            .flatten()
            .filter(|crls| crls.len() == 2)
    })
    .await;
    let (second_station, signer) = new_workstation(&service, "line-c-ws-02").await;
    assert_eq!(signer, ids[1], "generation 2 signs now");
    let (_, new_body) = new_gateway("rot-0102");
    let reply = second_station.register(&new_body).await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let new: GatewayRegistration = serde_json::from_str(&reply.body).expect("registered");
    assert_eq!(new.ca_key_id, ids[1]);
    let [_, issuing] = checked_chain(&new, &ca.root_der());
    assert_eq!(issuing, ders[1]);
    println!(
        "two issuing CAs loaded: the new registration's chain carries generation 2 ({})",
        ids[1]
    );
    // The repeat of the first registration: its chain carries generation 1, which signed it,
    // although generation 2 signs now.
    let reply = second_station.register(&old_body).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let repeated: GatewayRegistration = serde_json::from_str(&reply.body).expect("repeated");
    assert_eq!(repeated, old);
    let [_, issuing] = checked_chain(&repeated, &ca.root_der());
    assert_eq!(issuing, ders[0]);
    println!(
        "two issuing CAs loaded: the repeat's chain carries generation 1 ({}), its ca_key_id",
        ids[0]
    );

    // Rotation step 3: generation 1's key and certificate are removed.
    let plan = ca
        .keys()
        .plan(
            Request {
                generation: 2,
                remove_before: 2,
            },
            now(),
            CaKey::generate,
        )
        .expect("plan");
    assert_eq!(plan.keys.key_ids().collect::<Vec<_>>(), [ids[1].as_str()]);
    ca.keys_json = plan.keys.to_json().to_string();
    ca.issuing_pems.remove(0);
    ca.write_all();
    wait_for("generation 2 loaded alone", || async {
        service
            .stdout
            .messages("loaded the issuing CA")
            .into_iter()
            .find(|line| {
                let issuing = line["fields"]["issuing"].to_string();
                issuing.contains(&ids[1]) && !issuing.contains(&ids[0])
            })
    })
    .await;
    let reply = second_station.register(&old_body).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let repeated: GatewayRegistration = serde_json::from_str(&reply.body).expect("repeated");
    assert_eq!(
        repeated,
        GatewayRegistration {
            certificate_chain: old.certificate.clone(),
            ..old.clone()
        },
        "the first registration and certificate; the chain is the certificate alone"
    );
    assert_eq!(
        pem::decode_all(&repeated.certificate_chain, pem::CERTIFICATE).expect("PEM"),
        pem::decode_all(&old.certificate, pem::CERTIFICATE).expect("PEM")
    );
    let warning = wait_for("the WARN for the unloaded issuing CA", || async {
        service
            .stdout
            .messages("the issuing CA of a repeated gateway registration is no longer loaded")
            .into_iter()
            .next()
    })
    .await;
    assert_eq!(warning["level"], "WARN", "{warning}");
    assert_eq!(warning["fields"]["ca_key_id"], ids[0].as_str(), "{warning}");
    let fields: Vec<&String> = warning["fields"]
        .as_object()
        .map(|fields| fields.keys().filter(|key| *key != "message").collect())
        .unwrap_or_default();
    assert_eq!(
        fields,
        ["ca_key_id"],
        "the WARN names the key id only: {warning}"
    );
    println!("generation 1 removed: the repeat answers 200, chain = the certificate; {warning}");
    // The other registration keeps its two-certificate chain.
    let reply = second_station.register(&new_body).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let repeated: GatewayRegistration = serde_json::from_str(&reply.body).expect("repeated");
    assert_eq!(repeated, new);
}
