//! The MQTT broker's server certificate (Laysi/trustiot-v2#557; services/certs/README.md,
//! "broker 伺服器憑證"):
//!
//! - The profile: a CSR that asks for another name, a CA certificate and every extended key usage
//!   gets the broker profile the CA decides: `CN=mqtt-broker.trustiot.internal`, those DNS names
//!   (and the Service name when configured), server authentication only. rustls's webpki check
//!   accepts it as the server certificate for each of its names under the root, refuses it for
//!   another name and under another root, refuses a certificate for the same name from another CA,
//!   and refuses it as a client certificate; a gateway certificate is no server certificate, and
//!   no gateway may have the broker's name.
//! - The endpoint: the broker port signs a CSR, records it in the ledger with profile `broker`
//!   and answers the chain; it refuses other media types, oversized and broken CSRs and other
//!   keys, serves nothing else, and the API port does not serve it. Before the issuing CA has its
//!   first CRL, it signs nothing and records nothing.
//! - Mosquitto (OpenSSL), set up as the production broker with the issued certificate and a key
//!   the test generated (as the broker does, Laysi/trustiot-v2#535): `mosquitto_pub` that trusts
//!   the root and connects to `mqtt-broker.trustiot.internal` with a gateway certificate is
//!   accepted; another name or another trust anchor is refused; the broker's certificate used as
//!   a client certificate is refused.
//!
//! The endpoint tests start a throwaway Postgres with TLS, the Mosquitto test also a Mosquitto
//! container (podman or docker); all are skipped without either unless `CI` is set. The profile
//! test needs nothing.

mod support;

use std::collections::BTreeSet;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use certs::authority::CaState;
use certs::broker::{self, BROKER_CERTIFICATE_PATH, REQUEST_TYPE};
use certs::http::{CHAIN_PATH, CRL_PATH};
use certs::issue::{
    self, BROKER_SERVER_NAME, BrokerNames, CLOCK_SKEW_SECS, IssueError, MAX_REQUEST_BYTES,
};
use certs::pem;
use certs::workstations::CERTIFICATES_PATH;
use contract::certs::BrokerCertificate;
use protocol::GatewayId;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, PKCS_ECDSA_P384_SHA384, SanType,
};
use rustls::CertificateError;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier as _;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use sqlx::Row as _;
use support::{LIMIT, Service, TestCa, now, verify_client, wait_for};
use testkit::container::{Container, container_runtime, runtime_output, unavailable, unique_name};
use testkit::product::{DATABASE, ProductPostgres, product_postgres};
use x509_parser::certificate::X509Certificate;
use x509_parser::extensions::GeneralName;
use x509_parser::oid_registry::{
    OID_X509_EXT_AUTHORITY_KEY_IDENTIFIER, OID_X509_EXT_BASIC_CONSTRAINTS,
    OID_X509_EXT_EXTENDED_KEY_USAGE, OID_X509_EXT_KEY_USAGE, OID_X509_EXT_SUBJECT_ALT_NAME,
    OID_X509_EXT_SUBJECT_KEY_IDENTIFIER,
};
use x509_parser::prelude::FromDer;

/// The Service name the chart passes for a release `trustiot` in the namespace `trustiot`.
const SERVICE_NAME: &str = "trustiot-mqtt-broker.trustiot.svc";

/// The service's default `broker.validity`.
const DEFAULT_VALIDITY: Duration = Duration::from_secs(30 * 86_400);

const FORGED_NAME: &str = "gw-forged-broker";

/// A CRL schedule that gives each issuing CA its first CRL within a second, valid for an hour.
const FAST_CRLS: [(&str, &str); 4] = [
    ("TRUSTIOT__CRL__CHECK_INTERVAL", "1s"),
    ("TRUSTIOT__CRL__VALIDITY", "1h"),
    ("TRUSTIOT__CRL__RENEW_BEFORE", "3590s"),
    ("TRUSTIOT__CRL__ALERT_BEFORE", "3500s"),
];

/// A P-256 key and a CSR for it that asks for everything the CA must not grant the broker:
/// another common name and an organization, other DNS and IP names, a CA certificate with
/// keyCertSign, and client authentication and any extended key usage besides server
/// authentication.
fn forged_request() -> (KeyPair, Vec<u8>) {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a broker key");
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, FORGED_NAME);
    name.push(DnType::OrganizationName, "Forged");
    params.distinguished_name = name;
    params.subject_alt_names = vec![
        SanType::DnsName("broker.example.invalid".try_into().expect("a DNS name")),
        SanType::DnsName("*.trustiot.internal".try_into().expect("a DNS name")),
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

/// A throwaway CA that is not ours and a server certificate under it for `names` (server
/// authentication only), DER: (CA certificate, server certificate, server key).
fn foreign_server_certificate(names: &[&str]) -> (Vec<u8>, Vec<u8>, KeyPair) {
    let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a CA key");
    let mut ca_params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "another CA");
    ca_params.distinguished_name = name;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_certificate = ca_params.self_signed(&ca_key).expect("another CA");
    let issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a server key");
    let mut params = CertificateParams::new(
        names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>(),
    )
    .expect("params");
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let certificate = params
        .signed_by(&key, &issuer)
        .expect("the server certificate");
    (
        ca_certificate.der().to_vec(),
        certificate.der().to_vec(),
        key,
    )
}

/// Verifies `leaf` (DER) as a TLS server certificate for `name` the way a rustls client would:
/// chained to the trust anchor `root` through `intermediates`, for server authentication, at
/// `at` (Unix seconds).
///
/// # Errors
///
/// rustls's reason.
fn verify_server(
    root: &[u8],
    intermediates: &[Vec<u8>],
    leaf: &[u8],
    name: &str,
    at: i64,
) -> Result<(), rustls::Error> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(root.to_vec()))
        .expect("the root as a trust anchor");
    let verifier = WebPkiServerVerifier::builder_with_provider(
        Arc::new(roots),
        Arc::new(rustls::crypto::ring::default_provider()),
    )
    .build()
    .expect("a server verifier");
    let intermediates: Vec<CertificateDer<'_>> = intermediates
        .iter()
        .map(|der| CertificateDer::from(der.as_slice()))
        .collect();
    let name = ServerName::try_from(name.to_owned()).expect("a server name");
    verifier
        .verify_server_cert(
            &CertificateDer::from(leaf),
            &intermediates,
            &name,
            &[],
            UnixTime::since_unix_epoch(Duration::from_secs(
                u64::try_from(at).expect("a time after 1970"),
            )),
        )
        .map(drop)
}

/// The OIDs of a certificate's extensions, as dotted strings.
fn extension_oids(certificate: &X509Certificate<'_>) -> BTreeSet<String> {
    certificate
        .extensions()
        .iter()
        .map(|extension| extension.oid.to_id_string())
        .collect()
}

/// Checks that `der` is the broker's server certificate for `names`: the subject, the extensions
/// and their values of the profile, nothing from the CSR `request` but the public key.
fn assert_broker_profile(der: &[u8], request: &[u8], names: &[&str]) {
    let (rest, certificate) = X509Certificate::from_der(der).expect("a certificate");
    assert!(rest.is_empty());

    assert_eq!(certificate.subject().iter_attributes().count(), 1);
    let cn: Vec<&str> = certificate
        .subject()
        .iter_common_name()
        .filter_map(|cn| cn.as_str().ok())
        .collect();
    assert_eq!(cn, [BROKER_SERVER_NAME]);
    assert!(!certificate.subject().to_string().contains(FORGED_NAME));

    let expected: BTreeSet<String> = [
        OID_X509_EXT_BASIC_CONSTRAINTS,
        OID_X509_EXT_KEY_USAGE,
        OID_X509_EXT_EXTENDED_KEY_USAGE,
        OID_X509_EXT_SUBJECT_ALT_NAME,
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

    // Server authentication only: never client authentication, never any purpose.
    let eku = certificate
        .extended_key_usage()
        .expect("one extendedKeyUsage")
        .expect("extendedKeyUsage present");
    assert!(eku.value.server_auth, "server authentication");
    assert!(
        !(eku.value.any
            || eku.value.client_auth
            || eku.value.code_signing
            || eku.value.email_protection
            || eku.value.time_stamping
            || eku.value.ocsp_signing)
            && eku.value.other.is_empty(),
        "server authentication only: {:?}",
        eku.value
    );

    // The configured DNS names, in order, and nothing from the CSR.
    let alternative = certificate
        .subject_alternative_name()
        .expect("one subjectAltName")
        .expect("subjectAltName present");
    assert!(!alternative.critical, "the subject is not empty");
    let dns: Vec<&str> = alternative
        .value
        .general_names
        .iter()
        .map(|name| match name {
            GeneralName::DNSName(name) => *name,
            other => panic!("only DNS names: {other:?}"),
        })
        .collect();
    assert_eq!(dns, names);

    // The key is the CSR's.
    let (_, parsed) =
        x509_parser::certification_request::X509CertificationRequest::from_der(request)
            .expect("the CSR parses");
    assert_eq!(
        certificate.public_key().raw,
        parsed.certification_request_info.subject_pki.raw
    );
}

#[test]
fn the_ca_decides_the_broker_profile_and_tls_checks_its_name_and_purpose() {
    let ca = TestCa::new(&[1]);
    let authority = ca.authority();
    let issuing = authority.current(now()).expect("an issuing CA");
    let (_, request) = forged_request();
    let names = BrokerNames::new(Some(SERVICE_NAME)).expect("names");
    let signing_time = now();
    let issued =
        issue::broker_certificate(issuing, &request, &names, DEFAULT_VALIDITY, signing_time)
            .expect("the CSR is signed");
    assert_eq!(issued.profile.as_str(), "broker");
    assert_eq!(issued.subject, BROKER_SERVER_NAME);
    assert_eq!(issued.names, [BROKER_SERVER_NAME, SERVICE_NAME]);
    assert_eq!(issued.ca_key_id, issuing.key_id());
    assert_eq!(issued.issuer_der, issuing.certificate_der());
    assert_broker_profile(&issued.der, &request, &[BROKER_SERVER_NAME, SERVICE_NAME]);
    let (_, certificate) = X509Certificate::from_der(&issued.der).expect("a certificate");
    assert_eq!(
        certificate.validity().not_after.timestamp()
            - certificate.validity().not_before.timestamp(),
        i64::try_from(DEFAULT_VALIDITY.as_secs()).expect("seconds") + CLOCK_SKEW_SECS
    );

    // Without a Service name the fixed name is the only one.
    let alone = issue::broker_certificate(
        issuing,
        &request,
        &BrokerNames::new(None).expect("names"),
        DEFAULT_VALIDITY,
        signing_time,
    )
    .expect("signed");
    assert_broker_profile(&alone.der, &request, &[BROKER_SERVER_NAME]);

    // A client that trusts the root accepts it for each of its names, with the issuing CA
    // certificate the broker sends after it.
    let root = ca.root_der();
    let chain = [issued.issuer_der.clone()];
    for name in [BROKER_SERVER_NAME, SERVICE_NAME] {
        verify_server(&root, &chain, &issued.der, name, signing_time)
            .unwrap_or_else(|error| panic!("a server certificate for {name}: {error}"));
    }
    // Another name is refused.
    for name in [
        "wrong-name.trustiot.internal",
        "trustiot.internal",
        "mqtt-broker.trustiot.internal.example",
        FORGED_NAME,
        "broker.example.invalid",
        "192.0.2.1",
    ] {
        let refused = verify_server(&root, &chain, &issued.der, name, signing_time)
            .expect_err("another name");
        println!("rustls for {name}: {refused}");
        assert!(
            matches!(
                refused,
                rustls::Error::InvalidCertificate(
                    CertificateError::NotValidForName
                        | CertificateError::NotValidForNameContext { .. }
                )
            ),
            "refused for its name: {refused:?}"
        );
    }
    // A client that trusts another root refuses it, and a client that trusts ours refuses a
    // certificate for the same name from another CA.
    let (foreign_root, foreign_leaf, _) = foreign_server_certificate(&[BROKER_SERVER_NAME]);
    verify_server(
        &foreign_root,
        &[],
        &foreign_leaf,
        BROKER_SERVER_NAME,
        signing_time,
    )
    .expect("positive control: the foreign certificate is valid under its own root");
    for (anchor, leaf, intermediates) in [
        (&foreign_root, &issued.der, chain.as_slice()),
        (&root, &foreign_leaf, &[][..]),
        (&root, &foreign_leaf, chain.as_slice()),
    ] {
        let refused = verify_server(
            anchor,
            intermediates,
            leaf,
            BROKER_SERVER_NAME,
            signing_time,
        )
        .expect_err("another CA");
        println!("rustls across CAs: {refused}");
        assert!(
            matches!(
                refused,
                rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer)
            ),
            "refused for its issuer: {refused:?}"
        );
    }

    // It is no client certificate: a TLS server that authenticates gateways under this root (as
    // the broker does) refuses it for its purpose, while it accepts a gateway certificate.
    let (_, gateway_request) = plain_request("gw-557-0001", &PKCS_ECDSA_P256_SHA256);
    let gateway = issue::gateway_certificate(
        issuing,
        &gateway_request,
        &GatewayId::new("gw-557-0001").expect("a gateway id"),
        DEFAULT_VALIDITY,
        signing_time,
    )
    .expect("signed");
    verify_client(&root, &ca.issuing_ders(), &gateway.der, &[], signing_time)
        .expect("the gateway certificate is a client certificate");
    let refused = verify_client(&root, &ca.issuing_ders(), &issued.der, &[], signing_time)
        .expect_err("the broker certificate is no client certificate");
    println!("rustls on the broker certificate as a client certificate: {refused}");
    assert!(
        matches!(
            refused,
            rustls::Error::InvalidCertificate(
                CertificateError::InvalidPurpose | CertificateError::InvalidPurposeContext { .. }
            )
        ),
        "refused for its purpose: {refused:?}"
    );
    // And the other way round: a gateway certificate is no server certificate, and no gateway
    // can be named after the broker.
    let refused = verify_server(&root, &chain, &gateway.der, "gw-557-0001", signing_time)
        .expect_err("a gateway certificate is no server certificate");
    println!("rustls on a gateway certificate as a server certificate: {refused}");
    assert!(
        matches!(
            refused,
            rustls::Error::InvalidCertificate(
                CertificateError::InvalidPurpose | CertificateError::InvalidPurposeContext { .. }
            )
        ),
        "refused for its purpose: {refused:?}"
    );
    for id in [BROKER_SERVER_NAME, "MQTT-Broker.TrustIoT.internal"] {
        let refused = issue::gateway_certificate(
            issuing,
            &gateway_request,
            &GatewayId::new(id).expect("a gateway id"),
            DEFAULT_VALIDITY,
            signing_time,
        )
        .expect_err("the broker's name is reserved");
        assert!(
            matches!(refused, IssueError::ReservedGatewayId),
            "{refused}"
        );
    }
}

#[test]
fn a_broker_request_without_proof_of_possession_or_with_another_key_is_refused() {
    let ca = TestCa::new(&[1]);
    let authority = ca.authority();
    let issuing = authority.current(now()).expect("an issuing CA");
    let names = BrokerNames::new(None).expect("names");
    let sign = |request: &[u8]| {
        issue::broker_certificate(issuing, request, &names, DEFAULT_VALIDITY, now())
    };

    // The CSR's signature is over its content; flip a byte of the requested name.
    let (_, mut tampered) = plain_request("tamper-me", &PKCS_ECDSA_P256_SHA256);
    let at = tampered
        .windows(9)
        .position(|window| window == b"tamper-me")
        .expect("the name in the CSR");
    tampered[at] ^= 0x01;
    assert!(matches!(sign(&tampered), Err(IssueError::BadSignature)));

    let (_, other_curve) = plain_request("p384", &PKCS_ECDSA_P384_SHA384);
    assert!(matches!(
        sign(&other_curve),
        Err(IssueError::UnsupportedSignature | IssueError::UnsupportedKey)
    ));
    assert!(matches!(sign(b"not a CSR"), Err(IssueError::NotARequest)));
}

/// The ledger rows with profile `broker`: serial (hex), subject, DER.
async fn broker_rows(product: &ProductPostgres, ca_key_id: &str) -> Vec<(String, String, Vec<u8>)> {
    let rows = sqlx::query(
        "SELECT encode(serial, 'hex') AS serial, subject, certificate
         FROM certs.issued_certificates WHERE ca_key_id = $1 AND profile = 'broker'
         ORDER BY serial",
    )
    .bind(ca_key_id)
    .fetch_all(&mut product.superuser().await)
    .await
    .expect("read the ledger");
    rows.iter()
        .map(|row| {
            (
                row.get("serial"),
                row.get("subject"),
                row.get("certificate"),
            )
        })
        .collect()
}

/// Every ledger row of `ca_key_id`, any profile.
async fn ledger_count(product: &ProductPostgres, ca_key_id: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM certs.issued_certificates WHERE ca_key_id = $1")
        .bind(ca_key_id)
        .fetch_one(&mut product.superuser().await)
        .await
        .expect("count the ledger")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_broker_port_issues_the_broker_certificate_and_nothing_else() {
    let Some(product) = product_postgres(&["certs_app"]).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let authority = ca.authority();
    let issuing = authority.current(now()).expect("an issuing CA");
    let mut env = FAST_CRLS.to_vec();
    env.push(("TRUSTIOT__BROKER__SERVICE_NAME", SERVICE_NAME));
    let service = Service::start(&product, &ca, &env).await;
    let serving = service
        .stdout
        .messages("serving the MQTT broker's certificate issuance");
    assert_eq!(
        serving[0]["fields"]["names"],
        format!("{BROKER_SERVER_NAME},{SERVICE_NAME}")
    );
    wait_for("the published CRLs", || async {
        let (status, body) = service.get(CRL_PATH).await;
        (status == 200).then_some(body)
    })
    .await;

    // A CSR that asks for everything gets the profile the CA decides.
    let (_, request) = forged_request();
    let reply = service
        .post_broker(BROKER_CERTIFICATE_PATH, Some(REQUEST_TYPE), request.clone())
        .await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(reply.cache_control.as_deref(), Some("no-store"));
    let issued: BrokerCertificate = serde_json::from_str(&reply.body).expect("a certificate");
    assert_eq!(issued.ca_key_id, issuing.key_id());
    assert_eq!(issued.names, [BROKER_SERVER_NAME, SERVICE_NAME]);
    assert_eq!(
        (issued.not_after - issued.not_before).num_seconds(),
        i64::try_from(DEFAULT_VALIDITY.as_secs()).expect("seconds") + CLOCK_SKEW_SECS
    );
    let chain = pem::decode_all(&issued.certificate_chain, pem::CERTIFICATE).expect("PEM");
    assert_eq!(chain.len(), 2, "the certificate, then its issuing CA");
    assert_eq!(chain[1], issuing.certificate_der());
    assert_broker_profile(&chain[0], &request, &[BROKER_SERVER_NAME, SERVICE_NAME]);
    let (_, leaf) = X509Certificate::from_der(&chain[0]).expect("a certificate");
    assert_eq!(issue::hex(leaf.raw_serial()), issued.serial);
    verify_server(
        &ca.root_der(),
        &chain[1..],
        &chain[0],
        BROKER_SERVER_NAME,
        now(),
    )
    .expect("a server certificate under the root");

    // Recorded with the broker profile before it was handed out, and logged without the CSR or
    // the certificate.
    assert_eq!(
        broker_rows(&product, issuing.key_id()).await,
        [(
            issued.serial.clone(),
            BROKER_SERVER_NAME.to_owned(),
            chain[0].clone()
        )]
    );
    let logged = wait_for("the issue's log line", || async {
        service
            .stdout
            .messages("issued the MQTT broker's server certificate")
            .into_iter()
            .next()
    })
    .await;
    println!("{logged}");
    assert_eq!(logged["level"], "INFO");
    assert_eq!(logged["fields"]["serial"], issued.serial);
    assert_eq!(logged["fields"]["ca_key_id"], issued.ca_key_id);
    let leaf_base64 = issued
        .certificate_chain
        .lines()
        .nth(1)
        .expect("a PEM line")
        .to_owned();
    let output = service.output();
    assert!(!output.contains(&leaf_base64), "the log has no certificate");
    assert!(!output.contains("BEGIN"), "the log has no PEM");

    // Refusals sign and record nothing.
    let before = ledger_count(&product, issuing.key_id()).await;
    let (_, other_curve) = plain_request("p384", &PKCS_ECDSA_P384_SHA384);
    let (_, plain) = plain_request("plain", &PKCS_ECDSA_P256_SHA256);
    let oversized = vec![0x30; MAX_REQUEST_BYTES + 1];
    let pem_request = pem::encode(pem::CERTIFICATE_REQUEST, &plain).into_bytes();
    for (what, content_type, body, status) in [
        ("no Content-Type", None, plain.clone(), 415),
        ("JSON", Some("application/json"), plain.clone(), 415),
        (
            "PEM",
            Some("application/x-pem-file"),
            pem_request.clone(),
            415,
        ),
        ("PEM as pkcs10", Some(REQUEST_TYPE), pem_request, 422),
        ("not a CSR", Some(REQUEST_TYPE), b"not a CSR".to_vec(), 422),
        ("P-384", Some(REQUEST_TYPE), other_curve, 422),
        ("over 4096 bytes", Some(REQUEST_TYPE), oversized, 413),
    ] {
        let reply = service
            .post_broker(BROKER_CERTIFICATE_PATH, content_type, body)
            .await;
        println!("{what}: {} {}", reply.status, reply.body);
        assert_eq!(reply.status, status, "{what}: {}", reply.body);
        assert_eq!(reply.cache_control.as_deref(), Some("no-store"), "{what}");
        assert!(!reply.body.contains("BEGIN"), "{what}: {}", reply.body);
    }
    assert_eq!(ledger_count(&product, issuing.key_id()).await, before);

    // The API port does not issue it, and the broker port serves nothing else.
    let reply = service
        .post(
            BROKER_CERTIFICATE_PATH,
            None,
            Some(REQUEST_TYPE),
            plain.clone(),
        )
        .await;
    assert_eq!(reply.status, 404, "the API port: {}", reply.body);
    for path in [
        CHAIN_PATH,
        CRL_PATH,
        "/",
        "/healthz",
        "/readyz",
        BROKER_CERTIFICATE_PATH,
    ] {
        let (status, body) = service.get_broker(path).await;
        let expected = if path == BROKER_CERTIFICATE_PATH {
            405
        } else {
            404
        };
        assert_eq!(status, expected, "GET {path} on the broker port: {body}");
    }
    let reply = service
        .post_broker(CERTIFICATES_PATH, Some("application/json"), "{}")
        .await;
    assert_eq!(reply.status, 404, "the workstation API on the broker port");
    assert_eq!(ledger_count(&product, issuing.key_id()).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_broker_certificate_before_the_issuing_ca_has_its_first_crl() {
    let Some(product) = product_postgres(&["certs_app"]).await else {
        return;
    };
    // The endpoint in this process, without the CRL job: the issuing CA has no CRL until the
    // test writes one.
    let ca = TestCa::new(&[1]);
    let authority = ca.authority();
    let key_id = authority
        .current(now())
        .expect("an issuing CA")
        .key_id()
        .to_owned();
    let pool = sqlx::PgPool::connect_with(product.postgres.options("certs_app", DATABASE))
        .await
        .expect("the service role's pool");
    let routes = broker::routes(
        pool.clone(),
        CaState::of(authority),
        BrokerNames::new(None).expect("names"),
        DEFAULT_VALIDITY,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let url = format!(
        "http://{}{BROKER_CERTIFICATE_PATH}",
        listener.local_addr().expect("the address")
    );
    let server = tokio::spawn(async move { axum::serve(listener, routes).await });
    let post = |request: Vec<u8>| {
        let url = url.clone();
        async move {
            let response = reqwest::Client::new()
                .post(url)
                .timeout(Duration::from_secs(30))
                .header("content-type", REQUEST_TYPE)
                .body(request)
                .send()
                .await
                .expect("call the endpoint");
            let status = response.status().as_u16();
            (status, response.text().await.expect("read the body"))
        }
    };

    let (_, request) = plain_request("broker", &PKCS_ECDSA_P256_SHA256);
    let (status, body) = post(request.clone()).await;
    println!("before the first CRL: {status} {body}");
    assert_eq!(status, 503, "{body}");
    assert!(!body.contains("BEGIN"), "{body}");
    assert_eq!(ledger_count(&product, &key_id).await, 0, "nothing recorded");

    // Positive control: once the CRL job's first CRL is there, the same request is signed.
    let crl = certs::crl::build(
        ca.authority().current(now()).expect("an issuing CA"),
        1,
        &[],
        now(),
        Duration::from_secs(3600),
    )
    .expect("a CRL");
    sqlx::query(
        "INSERT INTO certs.crls (ca_key_id, crl_number, this_update, next_update, crl)
         VALUES ($1, 1, now(), now() + interval '1 hour', $2)",
    )
    .bind(&key_id)
    .bind(&crl)
    .execute(&pool)
    .await
    .expect("the first CRL");
    let (status, body) = post(request).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(ledger_count(&product, &key_id).await, 1);
    server.abort();
}

/// eclipse-mosquitto:2.1.2-alpine, pinned to the multi-arch index digest, as in
/// `crates/protocol/tests/broker.rs` and the local dev environment; Renovate updates every copy
/// in one pull request.
const MOSQUITTO_IMAGE: &str = "docker.io/library/eclipse-mosquitto:2.1.2-alpine@sha256:860aeb5d63ce509508f6dcfb2bc8a1c1dd2bc60555b562b45ce90e52d8dabb0f";

/// Where the test puts the broker's files inside the container.
const BROKER_DIR: &str = "/run/trustiot";

/// A name that resolves to the broker in the container but is not in its certificate.
const WRONG_NAME: &str = "wrong-name.trustiot.internal";

/// Waits until the test has put the files in place, then runs the broker as its own user.
const START_SCRIPT: &str = r#"set -eu
while [ ! -f /run/trustiot/ready ]; do sleep 1; done
chown -R mosquitto:mosquitto /run/trustiot
exec mosquitto -c /run/trustiot/mosquitto.conf
"#;

/// The production broker's TLS listener (Laysi/trustiot-v2#84): the server certificate with its
/// chain and the key the broker generated; client certificates under `ca.pem` (the certs
/// service's chain), checked against `crl.pem`, the common name as the username.
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

/// The Mosquitto container, where both names resolve to its loopback interface.
struct Broker {
    container: Container,
}

impl Broker {
    fn start(runtime: &'static str) -> Self {
        let container = Container {
            runtime,
            name: unique_name("certs-broker-tls"),
        };
        let fixed = format!("{BROKER_SERVER_NAME}:127.0.0.1");
        let wrong = format!("{WRONG_NAME}:127.0.0.1");
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
                "--add-host",
                &fixed,
                "--add-host",
                &wrong,
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

    /// Connects to `host` with the client certificate and key `client` (`clients/<client>.pem`
    /// and `-key.pem`), trusting the CA certificates in `trust` (`trust/<trust>.pem`), and
    /// publishes one message; whether the broker accepted the connection (CONNACK 0) and the
    /// client's output. The client checks the server certificate's chain and its name against
    /// `host` (no `--insecure`).
    async fn connect(&self, host: &str, trust: &str, client: &str) -> (bool, String) {
        let certificate = format!("{BROKER_DIR}/clients/{client}.pem");
        let key = format!("{BROKER_DIR}/clients/{client}-key.pem");
        let cafile = format!("{BROKER_DIR}/trust/{trust}.pem");
        let run = tokio::process::Command::new(self.container.runtime)
            .args([
                "exec",
                &self.container.name,
                "mosquitto_pub",
                "-d",
                "-h",
                host,
                "-p",
                "8883",
                "--cafile",
                &cafile,
                "--cert",
                &certificate,
                "--key",
                &key,
                "-i",
                &format!("certs-test-{client}"),
                "-t",
                "certs/test",
                "-m",
                "hello",
            ])
            .stdin(Stdio::null())
            .output();
        let output = tokio::time::timeout(LIMIT, run)
            .await
            .unwrap_or_else(|_| panic!("mosquitto_pub to {host} did not finish within {LIMIT:?}"))
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
async fn mosquitto_serves_the_broker_certificate_and_refuses_it_from_a_client() {
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
    let crls = wait_for("the published CRLs", || async {
        let (status, body) = service.get(CRL_PATH).await;
        (status == 200).then_some(body)
    })
    .await;
    let (status, chain) = service.get(CHAIN_PATH).await;
    assert_eq!(status, 200, "{chain}");

    // The broker generates its key and asks the broker port for its certificate.
    let (broker_key, request) = plain_request("broker", &PKCS_ECDSA_P256_SHA256);
    let reply = service
        .post_broker(BROKER_CERTIFICATE_PATH, Some(REQUEST_TYPE), request)
        .await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let issued: BrokerCertificate = serde_json::from_str(&reply.body).expect("a certificate");

    // A gateway certificate from the same issuing CA, recorded as registration would.
    let (gateway_key, gateway_request) = plain_request("gw-557-accepted", &PKCS_ECDSA_P256_SHA256);
    let gateway = issue::gateway_certificate(
        issuing,
        &gateway_request,
        &GatewayId::new("gw-557-accepted").expect("a gateway id"),
        DEFAULT_VALIDITY,
        now(),
    )
    .expect("signed");
    {
        let mut app = product.postgres.connect("certs_app", DATABASE).await;
        issue::record(&mut app, &gateway).await.expect("recorded");
    }
    let (foreign_root, _, _) = foreign_server_certificate(&["another.example.invalid"]);

    let broker = Broker::start(runtime);
    broker.push(&format!("{BROKER_DIR}/ca.pem"), chain.as_bytes());
    broker.push(&format!("{BROKER_DIR}/crl.pem"), crls.as_bytes());
    broker.push(
        &format!("{BROKER_DIR}/server.pem"),
        issued.certificate_chain.as_bytes(),
    );
    broker.push(
        &format!("{BROKER_DIR}/server-key.pem"),
        broker_key.serialize_pem().as_bytes(),
    );
    // What a client trusts: the root alone (README.md, "broker 伺服器憑證"), or another CA.
    broker.push(
        &format!("{BROKER_DIR}/trust/root.pem"),
        pem::encode(pem::CERTIFICATE, &ca.root_der()).as_bytes(),
    );
    broker.push(
        &format!("{BROKER_DIR}/trust/other-ca.pem"),
        pem::encode(pem::CERTIFICATE, &foreign_root).as_bytes(),
    );
    for (name, key, certificate) in [
        (
            "gateway",
            &gateway_key,
            pem::encode(pem::CERTIFICATE, &gateway.der),
        ),
        // The broker's own certificate and key, presented as a client's.
        ("broker", &broker_key, issued.certificate_chain.clone()),
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

    let cases = [
        (
            "the broker's name, the root",
            BROKER_SERVER_NAME,
            "root",
            "gateway",
        ),
        ("another name", WRONG_NAME, "root", "gateway"),
        ("another CA", BROKER_SERVER_NAME, "other-ca", "gateway"),
        (
            "the broker certificate as a client's",
            BROKER_SERVER_NAME,
            "root",
            "broker",
        ),
    ];
    let mut outcomes = Vec::new();
    for (what, host, trust, client) in cases {
        let (accepted, output) = broker.connect(host, trust, client).await;
        println!("== {what}: accepted {accepted}\n{output}");
        outcomes.push((what, accepted, output));
    }
    let logs = broker.logs();
    println!("== broker log\n{logs}");
    let accepted: Vec<(&str, bool)> = outcomes
        .iter()
        .map(|(what, accepted, _)| (*what, *accepted))
        .collect();
    assert_eq!(
        accepted,
        [
            ("the broker's name, the root", true),
            ("another name", false),
            ("another CA", false),
            ("the broker certificate as a client's", false),
        ]
    );
    for (what, _, output) in &outcomes[1..] {
        assert!(
            !output.contains("received CONNACK"),
            "{what} got no CONNACK at all:\n{output}"
        );
    }
    // Who refused, and why: the client refused the server certificate for another name and under
    // another CA (OpenSSL's "certificate verify failed"); the broker refused the broker certificate
    // as a client certificate with the TLS alert unsupported_certificate, which OpenSSL sends for
    // a certificate whose extended key usage does not allow the purpose.
    for (index, expected) in [
        (1, "certificate verify failed"),
        (2, "certificate verify failed"),
        (3, "alert unsupported certificate"),
    ] {
        let (what, _, output) = &outcomes[index];
        assert!(output.contains(expected), "{what}: {expected}\n{output}");
    }
}
