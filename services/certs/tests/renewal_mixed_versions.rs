//! The daily count of keyless renewal refusals with two builds of the service on one database
//! (Laysi/trustiot-v2#871; services/certs/README.md, "換發", "事件" and "規則與取捨"): the build
//! right before Laysi/trustiot-v2#868 and this tree's, both as real child processes on the
//! product's schema, which has `0008_unverified_renewal_refusals.sql`.
//!
//! - `the_previous_version_and_this_one_share_the_refusal_counts`, in four steps:
//!   1. this build counts refusals that need no key in `renewal_unverified_refusals`;
//!   2. rolled back to the previous build, which starts on the schema with 0008 applied, counts
//!      and reports every reason from `renewal_refusals` as before and leaves the new table's
//!      row alone, even though that row's day has ended;
//!   3. rolled forward again, this build reports that row as one event with its count and
//!      reasons;
//!   4. both builds at once each write their own table, so a gateway named by refusals without
//!      its key gets one row (one event) from each build for the window, not more.
//!
//! A day does not pass in a test: a window is moved back in time with an UPDATE as the
//! administrator, so it is due when the next report runs. The window rows themselves, the
//! service's counting and its reports are the real ones.
//!
//! Ignored by default: it needs the executable of the previous build, named by
//! `TRUSTIOT_PREVIOUS_CERTS` (the commit before Laysi/trustiot-v2#868, built from its own source
//! tree, never from this one). The commands are in
//! `docs/test-reports/2026-11-01-871-renewal-refusals-mixed-versions.md`. Without podman or
//! docker it is skipped unless `CI` is set.

mod support;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use certs::pem;
use certs::registrations::REGISTRATIONS_PATH;
use certs::workstations::CERTIFICATES_PATH;
use contract::certs::{
    WORKSTATION_CERTIFICATE_HEADER, WORKSTATION_NONCE_HEADER, WORKSTATION_SIGNATURE_HEADER,
    WORKSTATION_TIMESTAMP_HEADER, WorkstationCertificate, workstation_signing_input,
};
use contract::events::certs::GatewayRenewalRefused;
use protocol::renewal::{Challenge, RENEWAL_PATH};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256,
    PKCS_ECDSA_P384_SHA384, PublicKeyData as _, SigningKey as _,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use support::{CERTS, Service, TestCa, now, wait_for};
use testkit::product::{ProductPostgres, product_postgres};

/// The executable of the previous build.
const PREVIOUS: &str = "TRUSTIOT_PREVIOUS_CERTS";

/// The roles the tests of `tests/renewal.rs` log in with.
const LOGINS: &[&str] = &[
    "assets_app",
    "assets_platform",
    "certs_app",
    "certs_platform",
];

/// The CRL schedule of `tests/registration.rs`, so the registration can sign.
const FAST_CRLS: [(&str, &str); 4] = [
    ("TRUSTIOT__CRL__CHECK_INTERVAL", "1s"),
    ("TRUSTIOT__CRL__VALIDITY", "1h"),
    ("TRUSTIOT__CRL__RENEW_BEFORE", "3590s"),
    ("TRUSTIOT__CRL__ALERT_BEFORE", "3500s"),
];

/// The windows of a hardware ID in both tables: whether it is the table of the reasons that need
/// no key, the window's length in seconds, the count and the reasons.
const WINDOWS: &str = "SELECT false, extract(epoch FROM window_end - window_start)::bigint,
    refusals, reasons::text
FROM certs.renewal_refusals WHERE hardware_id = $1
UNION ALL
SELECT true, extract(epoch FROM window_end - window_start)::bigint, refusals, reasons::text
FROM certs.renewal_unverified_refusals WHERE hardware_id = $1
ORDER BY 1, 2, 4";

/// A row of the table of the reasons that need no key, with the version of the row (`xmin`
/// changes when anything rewrites or recreates it). `$1` hardware ID.
const UNVERIFIED_ROW: &str = "SELECT window_start::text, window_end::text, refusals,
    reasons::text, xmin::text
FROM certs.renewal_unverified_refusals WHERE hardware_id = $1 ORDER BY window_start";

/// Moves the windows of the reasons that need no key back two days, so they ended well over a
/// report margin ago whatever time of day it is. `$1` hardware ID.
const END_UNVERIFIED_DAY: &str = "UPDATE certs.renewal_unverified_refusals
SET window_start = window_start - interval '2 days', window_end = window_end - interval '2 days'
WHERE hardware_id = $1";

/// Moves the windows of the other reasons back three hours (they are an hour here). `$1`
/// hardware ID.
const END_HOURS: &str = "UPDATE certs.renewal_refusals
SET window_start = window_start - interval '3 hours', window_end = window_end - interval '3 hours'
WHERE hardware_id = $1";

/// The refusal events of a hardware ID.
const EVENTS: &str = "SELECT envelope::text FROM certs.outbox_event
WHERE event_type = 'certs.gateway_renewal_refused' AND aggregate_type = 'gateway_registration'
    AND aggregate_id = $1 AND tenant_id IS NULL";

/// One row of [`WINDOWS`].
type Window = (bool, i64, i64, String);

/// One row of [`UNVERIFIED_ROW`].
type Row = (String, String, i64, String, String);

/// A refusal event's window length in seconds, count and reasons.
type Event = (i64, u64, BTreeMap<String, u64>);

/// A new P-256 key and a CSR for it.
fn csr() -> (KeyPair, Vec<u8>) {
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

/// A production workstation with its certificate, which registers gateways.
struct Factory<'a> {
    service: &'a Service,
    key: KeyPair,
    certificate: Vec<u8>,
}

impl<'a> Factory<'a> {
    async fn new(service: &'a Service) -> Self {
        let (key, request) = csr();
        let issued = service
            .post_json(
                CERTIFICATES_PATH,
                Some(&service.admin_token()),
                &json!({
                    "workstation": "mixed-ws",
                    "certificate_request": pem::encode(pem::CERTIFICATE_REQUEST, &request),
                }),
            )
            .await;
        assert_eq!(issued.status, 201, "{}", issued.body);
        let issued: WorkstationCertificate = serde_json::from_str(&issued.body).expect("issued");
        Self {
            service,
            key,
            certificate: pem::decode_all(&issued.certificate, pem::CERTIFICATE)
                .expect("PEM")
                .remove(0),
        }
    }

    /// Registers `hardware_id` with a new TPM key, which it returns.
    async fn register(&self, hardware_id: &str) -> KeyPair {
        let (key, request) = csr();
        let endorsement = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).expect("an EK");
        let body = json!({
            "hardware_id": hardware_id,
            "certificate_request": pem::encode(pem::CERTIFICATE_REQUEST, &request),
            "endorsement_key": pem::encode(pem::PUBLIC_KEY, &endorsement.subject_public_key_info()),
        })
        .to_string()
        .into_bytes();
        let timestamp = now();
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).expect("random");
        let nonce = URL_SAFE_NO_PAD.encode(nonce);
        let digest: [u8; 32] = Sha256::digest(&body).into();
        let input =
            workstation_signing_input("POST", REGISTRATIONS_PATH, timestamp, &nonce, &digest);
        let signature = self.key.sign(&input).expect("sign the request");
        let response = reqwest::Client::new()
            .post(format!("{}{REGISTRATIONS_PATH}", self.service.base_url))
            .timeout(Duration::from_secs(30))
            .header("content-type", "application/json")
            .header(
                contract::INTERNAL_IDENTITY_HEADER,
                self.service.admin_token(),
            )
            .header(
                WORKSTATION_CERTIFICATE_HEADER,
                STANDARD.encode(&self.certificate),
            )
            .header(WORKSTATION_TIMESTAMP_HEADER, timestamp.to_string())
            .header(WORKSTATION_NONCE_HEADER, nonce)
            .header(WORKSTATION_SIGNATURE_HEADER, STANDARD.encode(signature))
            .body(body)
            .send()
            .await
            .expect("register a gateway");
        let status = response.status().as_u16();
        let text = response.text().await.expect("read the body");
        assert_eq!(status, 201, "register {hardware_id}: {text}");
        key
    }
}

/// Posts `body` to the renewal endpoint of `service`: the status and the message key.
async fn send(service: &Service, body: &Value) -> (u16, String) {
    let response = reqwest::Client::new()
        .post(format!("{}{RENEWAL_PATH}", service.base_url))
        .timeout(Duration::from_secs(30))
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .expect("call the renewal endpoint");
    let status = response.status().as_u16();
    let text = response.text().await.expect("read the body");
    let body: Value = serde_json::from_str(&text).unwrap_or_else(|_| panic!("a JSON body: {text}"));
    (
        status,
        body["message_key"].as_str().unwrap_or_default().to_owned(),
    )
}

/// A renew request for a challenge that was never handed out (`challenge_unknown`, 409).
fn made_up_renewal(hardware_id: &str, n: u8) -> Value {
    json!({"step": "renew", "hardware_id": hardware_id,
        "nonce": URL_SAFE_NO_PAD.encode([n; 32]), "signature": "MEUCIQ=="})
}

/// A challenge for `hardware_id`.
async fn challenge(service: &Service, hardware_id: &str) -> Challenge {
    let response = reqwest::Client::new()
        .post(format!("{}{RENEWAL_PATH}", service.base_url))
        .timeout(Duration::from_secs(30))
        .header("content-type", "application/json")
        .body(json!({"step": "challenge", "hardware_id": hardware_id}).to_string())
        .send()
        .await
        .expect("ask for a challenge");
    assert_eq!(response.status().as_u16(), 200, "a challenge");
    serde_json::from_slice(&response.bytes().await.expect("read the body")).expect("a challenge")
}

/// The renew request for `challenge`, signed by `key`.
fn renew_request(key: &KeyPair, challenge: &Challenge) -> Value {
    let signature = key.sign(&challenge.signing_input()).expect("sign");
    json!({
        "step": "renew",
        "hardware_id": challenge.hardware_id,
        "nonce": challenge.nonce,
        "signature": STANDARD.encode(signature),
    })
}

/// `count` refusals `challenge_unknown` for `hardware_id`, which need no key.
async fn refuse_unknown(service: &Service, hardware_id: &str, count: u8) {
    for n in 0..count {
        let (status, key) = send(service, &made_up_renewal(hardware_id, n)).await;
        assert_eq!(status, 409, "{hardware_id}: {key}");
    }
}

/// A redemption signed by a key that is not the registered one: `signature_invalid`, 401.
async fn refuse_signature(service: &Service, hardware_id: &str) {
    let other = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
    let challenge = challenge(service, hardware_id).await;
    let (status, key) = send(service, &renew_request(&other, &challenge)).await;
    assert_eq!(status, 401, "{hardware_id}: {key}");
}

/// A redemption signed by the registered key of a gateway that is not bound: `not_bound`, 403,
/// a reason that only counts after the signature held.
async fn refuse_not_bound(service: &Service, hardware_id: &str, key: &KeyPair) {
    let challenge = challenge(service, hardware_id).await;
    let (status, message) = send(service, &renew_request(key, &challenge)).await;
    assert_eq!(status, 403, "{hardware_id}: {message}");
}

/// Starts the executable at `binary` with `env` and waits until it decides renewals instead of
/// answering 503 for the lag of its consumers, which is not counted.
async fn start(
    binary: &str,
    product: &ProductPostgres,
    ca: &TestCa,
    env: &[(&str, &str)],
) -> Service {
    let service = Service::spawn_binary(binary, product, ca, env).await;
    service.wait_readiness(200).await;
    wait_for("the service deciding renewals", || async {
        let (status, _) = send(&service, &made_up_renewal("mixed-ghost", 0)).await;
        (status != 503).then_some(())
    })
    .await;
    service
}

async fn windows(product: &ProductPostgres, hardware_id: &str) -> Vec<Window> {
    sqlx::query_as(WINDOWS)
        .bind(hardware_id)
        .fetch_all(&mut product.superuser().await)
        .await
        .expect("read the refusal windows")
}

async fn unverified_rows(product: &ProductPostgres, hardware_id: &str) -> Vec<Row> {
    sqlx::query_as(UNVERIFIED_ROW)
        .bind(hardware_id)
        .fetch_all(&mut product.superuser().await)
        .await
        .expect("read the rows of the reasons that need no key")
}

async fn events(product: &ProductPostgres, hardware_id: &str) -> Vec<Event> {
    let envelopes: Vec<String> = sqlx::query_scalar(EVENTS)
        .bind(hardware_id)
        .fetch_all(&mut product.superuser().await)
        .await
        .expect("read the refusal events");
    let mut found: Vec<Event> = envelopes
        .iter()
        .map(|envelope| {
            let envelope: Value = serde_json::from_str(envelope).expect("an envelope");
            let event: GatewayRenewalRefused =
                serde_json::from_value(envelope["payload"].clone()).expect("a refusal event");
            (
                (event.window_end - event.window_start).num_seconds(),
                event.refusals,
                event.reasons,
            )
        })
        .collect();
    // The shorter window first.
    found.sort();
    found
}

/// Waits until `hardware_id` has `count` refusal events.
async fn wait_for_events(product: &ProductPostgres, hardware_id: &str, count: usize) -> Vec<Event> {
    wait_for(
        &format!("{count} refusal events of {hardware_id}"),
        || async {
            let found = events(product, hardware_id).await;
            (found.len() >= count).then_some(found)
        },
    )
    .await
}

fn reasons(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
    pairs
        .iter()
        .map(|(reason, count)| ((*reason).to_owned(), *count))
        .collect()
}

/// A count or report that failed, which the service logs at WARN and retries.
fn failed_counts(service: &Service) -> usize {
    ["could not count", "could not report"]
        .iter()
        .map(|message| service.stdout.messages(message).len())
        .sum()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the executable of the previous build in TRUSTIOT_PREVIOUS_CERTS"]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario on one database: each step builds on the rows the one before left"
)]
async fn the_previous_version_and_this_one_share_the_refusal_counts() {
    let previous = std::env::var(PREVIOUS)
        .unwrap_or_else(|_| panic!("{PREVIOUS} names the previous build's certs executable"));
    assert!(Path::new(&previous).is_file(), "{PREVIOUS}={previous}");
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let mut env = FAST_CRLS.to_vec();
    env.extend([
        // Limits no step here reaches.
        ("TRUSTIOT__RENEWAL__GATEWAY_BURST", "100"),
        ("TRUSTIOT__RENEWAL__GLOBAL_BURST", "10000"),
        ("TRUSTIOT__RENEWAL__GLOBAL_INTERVAL", "1ms"),
        ("TRUSTIOT__RENEWAL__MAX_CONCURRENT", "3"),
        // An hour for the reasons the previous build counts in `renewal_refusals`, so its
        // windows differ from the day of this build's table.
        ("TRUSTIOT__RENEWAL__REFUSAL_WINDOW", "1h"),
        ("TRUSTIOT__RENEWAL__REFUSAL_FLUSH_INTERVAL", "1s"),
    ]);

    // Step 1: this build counts the reasons that need no key per day, in its own table.
    let current = start(CERTS, &product, &ca, &env).await;
    let factory = Factory::new(&current).await;
    let _key_a = factory.register("mixed-a").await;
    let key_b = factory.register("mixed-b").await;
    let _key_c = factory.register("mixed-c").await;
    refuse_unknown(&current, "mixed-a", 2).await;
    refuse_signature(&current, "mixed-a").await;
    let counted = windows(&product, "mixed-a").await;
    assert_eq!(
        counted,
        [(
            true,
            86_400,
            3,
            r#"{"challenge_unknown": 2, "signature_invalid": 1}"#.to_owned()
        )],
        "this build: one day window in the new table, nothing in renewal_refusals"
    );
    println!("evidence: step 1, this build counted mixed-a: {counted:?}");
    drop(current);

    // The day of that row has ended while the previous build runs.
    let left = unverified_rows(&product, "mixed-a").await;
    sqlx::query(END_UNVERIFIED_DAY)
        .bind("mixed-a")
        .execute(&mut product.superuser().await)
        .await
        .expect("end the day");
    let due = unverified_rows(&product, "mixed-a").await;
    assert_ne!(left[0].0, due[0].0, "the window moved");

    // Step 2: the previous build starts on the schema with 0008 applied, counts and reports
    // every reason from `renewal_refusals`, and does not touch the new table.
    let old = start(&previous, &product, &ca, &env).await;
    refuse_unknown(&old, "mixed-a", 3).await;
    refuse_not_bound(&old, "mixed-b", &key_b).await;
    let counted = windows(&product, "mixed-a").await;
    assert_eq!(
        counted,
        [
            (false, 3600, 3, r#"{"challenge_unknown": 3}"#.to_owned()),
            (
                true,
                86_400,
                3,
                r#"{"challenge_unknown": 2, "signature_invalid": 1}"#.to_owned()
            ),
        ],
        "the previous build counts a keyless reason in renewal_refusals"
    );
    assert_eq!(
        windows(&product, "mixed-b").await,
        [(false, 3600, 1, r#"{"not_bound": 1}"#.to_owned())]
    );
    println!("evidence: step 2, the previous build counted mixed-a: {counted:?}");
    for hardware_id in ["mixed-a", "mixed-b"] {
        sqlx::query(END_HOURS)
            .bind(hardware_id)
            .execute(&mut product.superuser().await)
            .await
            .expect("end the hours");
    }
    let reported_a = wait_for_events(&product, "mixed-a", 1).await;
    let reported_b = wait_for_events(&product, "mixed-b", 1).await;
    assert_eq!(
        reported_a,
        [(3600, 3, reasons(&[("challenge_unknown", 3)]))]
    );
    assert_eq!(reported_b, [(3600, 1, reasons(&[("not_bound", 1)]))]);
    // Its reports have run, and the new table's row, whose window ended a day ago, is as it was.
    assert_eq!(
        unverified_rows(&product, "mixed-a").await,
        due,
        "the previous build leaves the new table alone"
    );
    assert_eq!(
        failed_counts(&old),
        0,
        "no failed count or report:\n{}",
        old.output()
    );
    println!(
        "evidence: step 2, the previous build reported mixed-a {reported_a:?} and mixed-b \
         {reported_b:?}; the new table's row is unchanged: {due:?}"
    );
    drop(old);

    // Step 3: rolled forward, this build reports the row the rollback left.
    let current = start(CERTS, &product, &ca, &env).await;
    let reported = wait_for_events(&product, "mixed-a", 2).await;
    assert_eq!(
        reported,
        [
            (3600, 3, reasons(&[("challenge_unknown", 3)])),
            (
                86_400,
                3,
                reasons(&[("challenge_unknown", 2), ("signature_invalid", 1)])
            ),
        ],
        "one event for the row: its count and reasons"
    );
    assert!(unverified_rows(&product, "mixed-a").await.is_empty());
    assert_eq!(events(&product, "mixed-b").await.len(), 1);
    println!("evidence: step 3, this build reported the row left over: {reported:?}");

    // Step 4: both builds at once, 10 refusals without a key from each for one gateway: each
    // writes its own table.
    let old = start(&previous, &product, &ca, &env).await;
    refuse_unknown(&current, "mixed-c", 10).await;
    refuse_unknown(&old, "mixed-c", 10).await;
    let counted = windows(&product, "mixed-c").await;
    // A clock that crosses an hour or midnight UTC during the run splits a row in two.
    let sum = |unverified: bool| -> (usize, i64) {
        let rows: Vec<&Window> = counted.iter().filter(|row| row.0 == unverified).collect();
        for (_, seconds, _, reasons) in &rows {
            assert_eq!(
                *seconds,
                if unverified { 86_400 } else { 3600 },
                "{reasons}"
            );
            assert!(
                reasons.starts_with(r#"{"challenge_unknown": "#),
                "{reasons}"
            );
        }
        (rows.len(), rows.iter().map(|row| row.2).sum())
    };
    let (new_rows, new_total) = sum(true);
    let (old_rows, old_total) = sum(false);
    assert!((1..=2).contains(&new_rows), "{counted:?}");
    assert!((1..=2).contains(&old_rows), "{counted:?}");
    assert_eq!(
        (new_total, old_total),
        (10, 10),
        "each build counted its own 10, in its own table: {counted:?}"
    );
    println!("evidence: step 4, both builds counted mixed-c: {counted:?}");
    for statement in [END_UNVERIFIED_DAY, END_HOURS] {
        sqlx::query(statement)
            .bind("mixed-c")
            .execute(&mut product.superuser().await)
            .await
            .expect("end the windows");
    }
    let reported = wait_for_events(&product, "mixed-c", new_rows + old_rows).await;
    assert!(windows(&product, "mixed-c").await.is_empty());
    let total_of = |seconds: i64| -> (usize, u64) {
        let found: Vec<&Event> = reported.iter().filter(|event| event.0 == seconds).collect();
        (found.len(), found.iter().map(|event| event.1).sum())
    };
    assert_eq!(total_of(86_400), (new_rows, 10), "{reported:?}");
    assert_eq!(total_of(3600), (old_rows, 10), "{reported:?}");
    assert_eq!(reported.len(), new_rows + old_rows, "{reported:?}");
    for service in [&current, &old] {
        assert_eq!(failed_counts(service), 0, "{}", service.output());
    }
    println!(
        "evidence: step 4, 20 refusals from two builds, {} events: {reported:?}",
        reported.len()
    );
}
