//! The offline root ceremony with the real `certs-ca` binary (services/certs/README.md, "CA 階層
//! 與私鑰保管"): `root create` writes the root certificate and sends the root key only to stdout
//! (a pipe, as into age); `root sign` reads the root key only from stdin (a pipe, as from age)
//! and signs the issuing CA certificate for the CSR the init container made; the service accepts
//! the result; `root check` confirms that a backup copy of the root key is the root's. Misuse is
//! refused without printing a key: an existing output file, a key that does not belong to the
//! root, a P-256 key, the root's name, a bad validity, a missing option.
//!
//! Then the first install with the real service (README, 「發簽發 CA 憑證」 steps 2 to 5): before
//! the ceremony the service is ready but cannot sign, exports `certs.ca.signing` 0 and logs the
//! waiting key at every check; once the certificate is mounted it signs without a restart. That
//! test starts a throwaway Postgres with TLS (podman or docker); skipped without either unless
//! `CI` is set.

#![allow(
    clippy::string_slice,
    reason = "prefixes of ASCII test data (base64 key bodies)"
)]

mod support;

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use ca_keys::{CaKey, CaKeys, Request};
use certs::authority::Authority;
use certs::ceremony;
use certs::http::{CHAIN_PATH, CRL_PATH};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256, PKCS_ECDSA_P384_SHA384,
};
use rustls_pki_types::CertificateDer;
use support::{CERTS_CA, FakeCollector, Service, TestCa, now, points, temp_dir, wait_for};
use testkit::product::product_postgres;
use time::OffsetDateTime;
use x509_parser::certificate::X509Certificate;
use x509_parser::prelude::FromDer;

const ROOT_NAME: &str = "TrustIoT test offline root";
const ISSUING_NAME: &str = "TrustIoT test issuing CA 1";

fn run(arguments: &[&str], stdin: Option<&str>) -> Output {
    let mut command = Command::new(CERTS_CA);
    command
        .args(arguments)
        .env_clear()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = command.spawn().expect("start certs-ca");
    if let Some(text) = stdin {
        let mut pipe = child.stdin.take().expect("piped stdin");
        pipe.write_all(text.as_bytes()).expect("write stdin");
    }
    child.wait_with_output().expect("certs-ca finished")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The base64 body of a PEM block, without its armour and line breaks.
fn pem_body(pem: &str) -> String {
    pem.lines()
        .filter(|line| !line.starts_with("-----"))
        .collect()
}

fn path(file: &Path) -> &str {
    file.to_str().expect("a UTF-8 path")
}

fn certificate(pem: &str) -> Vec<u8> {
    certs::pem::decode_all(pem, certs::pem::CERTIFICATE)
        .expect("a PEM certificate")
        .remove(0)
}

#[test]
fn the_offline_ceremony_makes_a_root_and_signs_an_issuing_ca() {
    let dir = temp_dir("ceremony");
    let root_file = dir.join("root.pem");
    let issuing_file = dir.join("issuing-ca-generation-1.pem");

    // root create: the key goes to stdout only.
    let created = run(
        &[
            "root",
            "create",
            "--common-name",
            ROOT_NAME,
            "--days",
            "3650",
            "--certificate",
            path(&root_file),
        ],
        None,
    );
    let stderr = text(&created.stderr);
    assert!(created.status.success(), "{stderr}");
    let root_key = text(&created.stdout);
    assert!(
        root_key.starts_with("-----BEGIN PRIVATE KEY-----"),
        "a PKCS#8 PEM key on stdout"
    );
    let key_body = pem_body(&root_key);
    assert!(!stderr.contains(&key_body[..24]), "stderr: {stderr}");
    println!("root create: {}", stderr.trim());
    let root_pem = std::fs::read_to_string(&root_file).expect("the root certificate");
    assert!(!root_pem.contains(&key_body[..24]));
    let root_der = certificate(&root_pem);
    let (_, root) = X509Certificate::from_der(&root_der).expect("a certificate");
    root.verify_signature(None).expect("self-signed");
    let constraints = root
        .basic_constraints()
        .expect("one basicConstraints")
        .expect("basicConstraints");
    assert!(constraints.critical && constraints.value.ca);
    assert_eq!(constraints.value.path_len_constraint, Some(1));
    let usage = root.key_usage().expect("one keyUsage").expect("keyUsage");
    assert!(usage.critical && usage.value.key_cert_sign() && usage.value.crl_sign());
    let key = KeyPair::from_pem(&root_key).expect("the root key");
    assert_eq!(key.algorithm(), &PKCS_ECDSA_P384_SHA384);

    // It never replaces a certificate, and then prints no key.
    let again = run(
        &[
            "root",
            "create",
            "--common-name",
            ROOT_NAME,
            "--certificate",
            path(&root_file),
        ],
        None,
    );
    assert_eq!(again.status.code(), Some(1), "{}", text(&again.stderr));
    assert!(
        again.stdout.is_empty(),
        "no key when the certificate cannot be written"
    );
    assert_eq!(
        std::fs::read_to_string(&root_file).expect("the root certificate"),
        root_pem
    );

    // The init container's key set and the CSR it publishes.
    let keys = CaKeys::default()
        .plan(
            Request {
                generation: 1,
                remove_before: 0,
            },
            now(),
            CaKey::generate,
        )
        .expect("a key set")
        .keys;
    let request_file = dir.join(ca_keys::request_file(1));
    std::fs::write(&request_file, keys.keys()[0].request_pem()).expect("write the CSR");

    // root sign: the key comes from stdin only.
    let sign_arguments = |common_name: &'static str, days: &'static str, output: &Path| {
        vec![
            "root".to_owned(),
            "sign".to_owned(),
            "--root-certificate".to_owned(),
            path(&root_file).to_owned(),
            "--request".to_owned(),
            path(&request_file).to_owned(),
            "--common-name".to_owned(),
            common_name.to_owned(),
            "--days".to_owned(),
            days.to_owned(),
            "--certificate".to_owned(),
            path(output).to_owned(),
        ]
    };
    let sign = |arguments: Vec<String>, stdin: &str| {
        let borrowed: Vec<&str> = arguments.iter().map(String::as_str).collect();
        run(&borrowed, Some(stdin))
    };
    let signed = sign(
        sign_arguments(ISSUING_NAME, "730", &issuing_file),
        &root_key,
    );
    let stderr = text(&signed.stderr);
    assert!(signed.status.success(), "{stderr}");
    assert!(signed.stdout.is_empty());
    assert!(!stderr.contains(&key_body[..24]));
    // The key id the operator compares with the one the certs service reports as waiting.
    assert!(
        stderr.contains(&format!("for key {}", keys.keys()[0].id())),
        "{stderr}"
    );
    println!("root sign: {}", stderr.trim());
    let issuing_pem = std::fs::read_to_string(&issuing_file).expect("the issuing CA certificate");
    let issuing_der = certificate(&issuing_pem);
    let (_, issuing) = X509Certificate::from_der(&issuing_der).expect("a certificate");
    issuing
        .verify_signature(Some(root.public_key()))
        .expect("signed by the root");
    let constraints = issuing
        .basic_constraints()
        .expect("one basicConstraints")
        .expect("basicConstraints");
    assert!(constraints.critical && constraints.value.ca);
    assert_eq!(constraints.value.path_len_constraint, Some(0));
    assert!(issuing.validity().not_after <= root.validity().not_after);
    assert_eq!(
        issuing.public_key().raw,
        keys.keys()[0].subject_public_key_info().as_slice()
    );

    // The service accepts the result: generation 1 can sign.
    let authority =
        Authority::from_parts(keys, &root_pem, &issuing_pem, now()).expect("the authority");
    let current = authority.current(now()).expect("an issuing CA");
    assert_eq!(current.generation(), 1);
    assert!(current.subject().contains(ISSUING_NAME));

    // Misuse.
    let other_root = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384)
        .expect("a key")
        .serialize_pem();
    let p256 = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .expect("a key")
        .serialize_pem();

    // root check: the check of a backup copy of the root key. It prints the root's key id and
    // never a key; another key, a P-256 key and no key are refused.
    let check = |stdin: &str| {
        run(
            &["root", "check", "--root-certificate", path(&root_file)],
            Some(stdin),
        )
    };
    let checked = check(&root_key);
    let stderr = text(&checked.stderr);
    assert!(checked.status.success(), "{stderr}");
    assert!(checked.stdout.is_empty());
    assert!(!stderr.contains(&key_body[..24]), "{stderr}");
    assert!(
        stderr.contains(&ca_keys::key_id(root.public_key().raw)),
        "{stderr}"
    );
    println!("root check: {}", stderr.trim());
    for (case, stdin) in [
        ("another key", other_root.as_str()),
        ("a P-256 key", p256.as_str()),
        ("not a key", "hello"),
    ] {
        let refused = check(stdin);
        let stderr = text(&refused.stderr);
        assert_eq!(
            refused.status.code(),
            Some(1),
            "root check, {case}: {stderr}"
        );
        assert!(refused.stdout.is_empty(), "root check, {case}");
        for secret in [&key_body, &pem_body(&other_root), &pem_body(&p256)] {
            assert!(
                !stderr.contains(&secret[..24]),
                "root check, {case}: {stderr}"
            );
        }
        println!("root check, {case}: {}", stderr.trim());
    }

    let cases: [(&str, Vec<String>, &str, i32); 6] = [
        (
            "a key that is not the root's",
            sign_arguments(ISSUING_NAME, "730", &dir.join("a.pem")),
            other_root.as_str(),
            1,
        ),
        (
            "a P-256 key",
            sign_arguments(ISSUING_NAME, "730", &dir.join("b.pem")),
            p256.as_str(),
            1,
        ),
        (
            "not a key",
            sign_arguments(ISSUING_NAME, "730", &dir.join("c.pem")),
            "hello",
            1,
        ),
        (
            "the root's name",
            sign_arguments(ROOT_NAME, "730", &dir.join("d.pem")),
            root_key.as_str(),
            1,
        ),
        (
            "zero days",
            sign_arguments(ISSUING_NAME, "0", &dir.join("e.pem")),
            root_key.as_str(),
            1,
        ),
        (
            "an existing output file",
            sign_arguments(ISSUING_NAME, "730", &issuing_file),
            root_key.as_str(),
            1,
        ),
    ];
    for (case, arguments, stdin, code) in cases {
        let refused = sign(arguments, stdin);
        let stderr = text(&refused.stderr);
        assert_eq!(refused.status.code(), Some(code), "{case}: {stderr}");
        assert!(refused.stdout.is_empty(), "{case}");
        for secret in [&key_body, &pem_body(&other_root), &pem_body(&p256)] {
            assert!(!stderr.contains(&secret[..24]), "{case}: {stderr}");
        }
        println!("{case}: {}", stderr.lines().next().unwrap_or_default());
    }
    assert_eq!(
        std::fs::read_to_string(&issuing_file).expect("the issuing CA certificate"),
        issuing_pem
    );
    let usage = run(&["root", "sign", "--common-name", ISSUING_NAME], Some(""));
    assert_eq!(usage.status.code(), Some(2), "{}", text(&usage.stderr));
    let unknown = run(&["root", "destroy"], None);
    assert_eq!(unknown.status.code(), Some(2));
}

/// The service refuses an issuing CA certificate that is valid after the root expires, whoever
/// signed it: past the root's notAfter no client can verify what that CA signs, so it must not
/// keep signing (`certs-ca root sign` never makes one, see above). Up to the root's notAfter is
/// accepted.
#[test]
fn an_issuing_ca_that_outlives_the_root_is_refused() {
    let root_key = ceremony::new_root_key().expect("a root key");
    let root_pem = ceremony::root_certificate(&root_key, ROOT_NAME, 30, now()).expect("a root");
    let root_der = certificate(&root_pem);
    let (_, root) = X509Certificate::from_der(&root_der).expect("the root");
    let root_not_after = root.validity().not_after.timestamp();
    let keys = CaKeys::default()
        .plan(
            Request {
                generation: 1,
                remove_before: 0,
            },
            now(),
            CaKey::generate,
        )
        .expect("plan")
        .keys;
    let keys_json = keys.to_json();
    let request = certs::pem::decode_all(keys.keys()[0].request_pem(), "CERTIFICATE REQUEST")
        .expect("the request")
        .remove(0);
    let public_key = certs::issue::requested_key(&request).expect("a P-256 request");
    let issuer = Issuer::from_ca_cert_der(&CertificateDer::from(root_der.as_slice()), &root_key)
        .expect("the root as an issuer");
    // The ceremony's profile, with the notAfter the test chooses.
    let sign = |not_after: i64| {
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, ISSUING_NAME);
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.use_authority_key_identifier_extension = true;
        params.not_before = OffsetDateTime::from_unix_timestamp(now() - 3600).expect("a notBefore");
        params.not_after = OffsetDateTime::from_unix_timestamp(not_after).expect("a notAfter");
        params
            .signed_by(&public_key, &issuer)
            .expect("an issuing CA certificate")
            .pem()
    };

    let within = sign(root_not_after);
    let parsed = CaKeys::parse(&keys_json).expect("the key set");
    let authority = Authority::from_parts(parsed, &root_pem, &within, now())
        .expect("an issuing CA that ends with the root");
    assert_eq!(
        authority.current(now()).map(|ca| ca.not_after()),
        Some(root_not_after)
    );

    let beyond = sign(root_not_after + 1);
    let parsed = CaKeys::parse(&keys_json).expect("the key set");
    let error = Authority::from_parts(parsed, &root_pem, &beyond, now())
        .expect_err("an issuing CA that outlives the root");
    assert!(
        error.to_string().contains("valid after the root expires"),
        "{error}"
    );
    println!("outlives the root: {error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_install_is_ready_and_waits_for_the_ceremony() {
    let Some(product) = product_postgres(&["certs_app"]).await else {
        return;
    };
    // Step 2: the root certificate and the init container's key, no issuing CA certificate yet.
    let mut ca = TestCa::new(&[1]);
    let certificates = std::mem::take(&mut ca.issuing_pems);
    ca.write_all();
    let key_id = ca.keys().keys()[0].id().to_owned();
    let collector = FakeCollector::start();
    let endpoint = collector.url();
    // `start` returns once /readyz answers 200: readiness does not wait for the ceremony.
    let service = Service::start(
        &product,
        &ca,
        &[
            ("TRUSTIOT__CA__RELOAD_INTERVAL", "1s"),
            ("TRUSTIOT__CRL__CHECK_INTERVAL", "1s"),
            ("TRUSTIOT__TELEMETRY__OTLP_ENDPOINT", endpoint.as_str()),
            ("OTEL_METRIC_EXPORT_INTERVAL", "300"),
        ],
    )
    .await;
    println!("ready before the ceremony");

    // Step 3: the waiting key, at load and again at every check, as the runbook's grep finds it.
    let waiting = wait_for("two WARN lines naming the waiting key", || async {
        let lines = service
            .stdout
            .messages("no issuing CA certificate is valid now, so the service cannot sign");
        (lines.len() >= 2).then_some(lines)
    })
    .await;
    for line in &waiting {
        assert_eq!(line["level"], "WARN", "{line}");
        let message = line["message"].as_str().unwrap_or_default();
        assert!(message.contains("keys wait for a certificate"), "{line}");
        let field = line["fields"]["waiting"].as_str().unwrap_or_default();
        assert_eq!(
            field,
            format!("{key_id} (generation 1, request issuing-ca-generation-1.csr.pem)"),
            "{line}"
        );
    }
    println!("waiting key, {} lines: {}", waiting.len(), waiting[0]);
    wait_for("certs.ca.signing = 0", || async {
        points(&collector)
            .into_iter()
            .find(|point| point.metric == "certs.ca.signing" && point.value == 0.0)
    })
    .await;
    // Nothing to publish yet: no issuing CA, so no CRL; the chain is the root alone.
    assert_eq!(service.get(CRL_PATH).await, (200, String::new()));
    let (status, chain) = service.get(CHAIN_PATH).await;
    assert_eq!(status, 200, "{chain}");
    assert_eq!(
        certs::pem::decode_all(&chain, certs::pem::CERTIFICATE).expect("PEM certificates"),
        [ca.root_der()]
    );

    // Step 5: the certificate the ceremony signed is mounted; the service reloads it and signs.
    std::fs::write(&ca.certificates_file, certificates.concat()).expect("mount the certificate");
    let loaded = wait_for("the issuing CA loaded as current", || async {
        service
            .stdout
            .messages("loaded the issuing CA")
            .into_iter()
            .find(|line| line["fields"]["current"] == key_id.as_str())
    })
    .await;
    println!("after the ceremony: {loaded}");
    wait_for("certs.ca.signing = 1", || async {
        points(&collector)
            .into_iter()
            .find(|point| point.metric == "certs.ca.signing" && point.value == 1.0)
    })
    .await;
    let crls = wait_for("the new issuing CA's first CRL published", || async {
        let (status, body) = service.get(CRL_PATH).await;
        (status == 200 && !body.is_empty()).then_some(body)
    })
    .await;
    let crls = certs::pem::decode_all(&crls, certs::pem::CRL).expect("PEM CRLs");
    assert_eq!(crls.len(), 1, "one CRL for the one issuing CA");
    assert!(
        service.stdout.messages("issuing CA keys wait").is_empty(),
        "no key waits once its certificate is set"
    );
}
