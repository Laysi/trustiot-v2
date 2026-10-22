//! The MQTT broker's accounts init container (`migrate mqtt-broker-accounts`,
//! Laysi/trustiot-v2#84) against the fake Kubernetes API of support/secrets.rs:
//!
//! - it fills one password per service into the empty Secret the chart declares and writes only
//!   their PBKDF2 hashes, with the release's roles and deny-by-default access, into the
//!   dynamic-security file, readable by its owner only;
//! - a restart keeps the passwords and every gateway client the broker stored; a higher
//!   generation rotates the passwords, and a service that no longer calls the broker loses its
//!   client;
//! - a release without callers never calls the API; a file the broker could not read either is
//!   left alone and stops the start; a missing Secret stops it with the way out.
//!
//! The broker accepting these hashes is tested against the real Mosquitto in mqtt_broker.rs.

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use migrate::mqtt_accounts::{DYNSEC_ADMIN_ROLE, converge, default_acl_access, pbkdf2_sha512};
use migrate::passwords::GENERATION_ANNOTATION;
use migrate::{Error, Report};
use protocol::acl::GATEWAY_ROLE;
use serde_json::{Value, json};
use support::scratch_dir;
use support::secrets::FakeSecrets;

const SECRET: &str = "trustiot-mqtt-broker-accounts";

async fn fake() -> FakeSecrets {
    let fake = FakeSecrets::start().await;
    fake.declare(SECRET);
    fake
}

fn dynsec_file() -> PathBuf {
    scratch_dir("mqtt-accounts").join("dynamic-security.json")
}

async fn start(
    fake: &FakeSecrets,
    services: &str,
    generation: &str,
    file: &Path,
) -> Result<Report, Error> {
    let mut report = Report::default();
    converge(
        &fake.mqtt_accounts_settings(SECRET, services, generation, file),
        &mut report,
    )
    .await
    .map(|()| report)
}

fn read(file: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(file).expect("the file")).expect("JSON")
}

fn client<'a>(root: &'a Value, username: &str) -> Option<&'a Value> {
    root["clients"]
        .as_array()
        .expect("clients")
        .iter()
        .find(|client| client["username"] == username)
}

/// Whether the client's `$7$` hash is the hash of `password`.
fn verifies(client: &Value, password: &str) -> bool {
    let encoded = client["encoded_password"].as_str().expect("a hash");
    let parts: Vec<&str> = encoded.split('$').collect();
    let [_, "7", iterations, salt, hash] = parts.as_slice() else {
        panic!("not a $7$ hash");
    };
    let salt = STANDARD.decode(salt).expect("Base64 salt");
    let hash = STANDARD.decode(hash).expect("Base64 hash");
    salt.len() == 64
        && pbkdf2_sha512(
            password.as_bytes(),
            &salt,
            iterations.parse().expect("iterations"),
        )
        .as_slice()
            == hash.as_slice()
}

#[tokio::test]
async fn passwords_go_to_the_secret_and_only_their_hashes_to_the_broker() {
    let fake = fake().await;
    let file = dynsec_file();
    let report = start(&fake, "ingest,certs", "1", &file)
        .await
        .expect("the first start");
    let ingest = fake.value(SECRET, "ingest").expect("ingest's password");
    let certs = fake.value(SECRET, "certs").expect("certs' password");
    for password in [&ingest, &certs] {
        assert_eq!(password.len(), 64);
        assert!(password.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
    assert_ne!(ingest, certs);
    assert!(fake.value(SECRET, "gateway-ops").is_none(), "not listed");
    assert_eq!(
        fake.get(SECRET)
            .expect("declared")
            .annotations
            .get(GENERATION_ANNOTATION)
            .map(String::as_str),
        Some("1")
    );

    let text = std::fs::read_to_string(&file).expect("the file");
    for password in [&ingest, &certs] {
        assert!(
            !text.contains(password.as_str()),
            "a plaintext password reached the file"
        );
        for line in &report.changes {
            assert!(!line.contains(password.as_str()));
        }
    }
    let mode = std::fs::metadata(&file)
        .expect("metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");
    let root = read(&file);
    assert_eq!(root["defaultACLAccess"], default_acl_access());
    assert!(root.get("anonymousGroup").is_none());
    assert!(verifies(
        client(&root, "service:ingest").expect("ingest"),
        &ingest
    ));
    assert!(verifies(
        client(&root, "service:certs").expect("certs"),
        &certs
    ));
    assert!(!verifies(
        client(&root, "service:ingest").expect("ingest"),
        &certs
    ));
    assert_eq!(
        client(&root, "service:certs").expect("certs")["roles"][1]["rolename"],
        DYNSEC_ADMIN_ROLE
    );

    // The broker registered a gateway meanwhile (Laysi/trustiot-v2#82); the pod restarts.
    let mut stored = read(&file);
    stored["clients"]
        .as_array_mut()
        .expect("clients")
        .push(json!({ "username": "gw-0001", "roles": [{ "rolename": GATEWAY_ROLE }] }));
    std::fs::write(&file, stored.to_string()).expect("the broker's save");
    start(&fake, "ingest,certs", "1", &file)
        .await
        .expect("a restart");
    assert_eq!(fake.value(SECRET, "ingest").as_ref(), Some(&ingest), "kept");
    let root = read(&file);
    assert!(client(&root, "gw-0001").is_some(), "the gateway stays");
    assert!(verifies(
        client(&root, "service:ingest").expect("ingest"),
        &ingest
    ));

    // A rotation, and certs no longer calls the broker.
    start(&fake, "ingest", "2", &file)
        .await
        .expect("a rotation");
    let rotated = fake.value(SECRET, "ingest").expect("ingest's password");
    assert_ne!(rotated, ingest);
    let root = read(&file);
    assert!(verifies(
        client(&root, "service:ingest").expect("ingest"),
        &rotated
    ));
    assert!(
        client(&root, "service:certs").is_none(),
        "certs lost its client"
    );
    assert!(client(&root, "gw-0001").is_some(), "the gateway stays");
}

#[tokio::test]
async fn a_release_without_callers_needs_no_api() {
    let fake = fake().await;
    let file = dynsec_file();
    start(&fake, "", "1", &file).await.expect("a start");
    assert!(fake.seen().is_empty(), "no request: {:?}", fake.seen());
    let root = read(&file);
    assert_eq!(root["clients"], json!([]));
    assert_eq!(root["defaultACLAccess"], default_acl_access());
}

#[tokio::test]
async fn an_unreadable_file_or_a_missing_secret_stops_the_start() {
    let fake = fake().await;
    let file = dynsec_file();
    std::fs::write(&file, "{ \"clients\": {").expect("a damaged file");
    let error = start(&fake, "ingest", "1", &file)
        .await
        .expect_err("a damaged file");
    assert!(error.to_string().contains("cannot be used"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&file).expect("the file"),
        "{ \"clients\": {",
        "left alone"
    );

    let missing = FakeSecrets::start().await;
    let error = start(&missing, "ingest", "1", &dynsec_file())
        .await
        .expect_err("no Secret");
    assert!(error.to_string().contains("helm upgrade"), "{error}");
}
