//! Internal identity verification end to end, from a real `example-service` process: the keys
//! come from the file the platform setting `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE` names, which the
//! service reads again while it runs; requests carry tokens in `x-internal-identity`, and
//! `/v1/whoami` uses the `UserIdentity` extractor. Rejected requests answer 401 with the M1 body
//! `unauthenticated` (503 `unavailable` before the service has keys), and neither the token nor
//! the key set reaches the logs.
//!
//! The signing keys are throwaway test keys derived in code from a one-byte seed; signing real
//! tokens is the identity service's job (Laysi/trustiot-v2#61).
#![cfg(feature = "contract")]

mod support;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};
use support::{Response, Service, request};

const USER: &str = "01928f6e-7a3c-7d4e-8f00-0123456789ab";
const TENANT: &str = "01928f6e-7a3c-7d4e-8f00-00000000000a";

/// A throwaway Ed25519 key pair for tests.
struct TestKey(SigningKey);

impl TestKey {
    fn new(seed: u8) -> Self {
        Self(SigningKey::from_bytes(&[seed; 32]))
    }

    fn jwk(&self, kid: &str) -> Value {
        let x = URL_SAFE_NO_PAD.encode(self.0.verifying_key().as_bytes());
        json!({"kty": "OKP", "crv": "Ed25519", "kid": kid, "x": x})
    }

    /// A token in the internal identity format.
    fn sign(&self, kid: &str, claims: &Value) -> String {
        let header = json!({"alg": "EdDSA", "typ": "internal-identity+jwt", "kid": kid});
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let signature = self.0.sign(signed.as_bytes());
        format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
    }
}

fn jwks(entries: &[(&TestKey, &str)]) -> String {
    let keys: Vec<Value> = entries.iter().map(|(key, kid)| key.jwk(kid)).collect();
    json!({ "keys": keys }).to_string()
}

fn unix_now() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970");
    i64::try_from(elapsed.as_secs()).expect("seconds fit in i64")
}

/// Claims issued at `iat` for 60 seconds, as the 2026-10-10 PoC issued them.
fn claims(iat: i64, audience: &str) -> Value {
    json!({
        "iss": "identity",
        "sub": USER,
        "aud": audience,
        "iat": iat,
        "exp": iat + 60,
        "tenant": {"id": TENANT, "role": "editor"},
        "platform_admin": false
    })
}

fn whoami(address: SocketAddr, headers: &[(&str, &str)]) -> Response {
    request(address, "GET", "/v1/whoami", headers, None).expect("GET /v1/whoami")
}

/// Waits until the service has logged at least `count` rejections with `reason`, and returns
/// them.
fn wait_for_rejections(service: &Service, reason: &str, count: usize) -> Option<Vec<Value>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let lines: Vec<Value> = service
            .stdout
            .json_lines()
            .into_iter()
            .filter(|line| {
                line["message"] == "rejected the request's internal identity"
                    && line["fields"]["reason"] == reason
            })
            .collect();
        if lines.len() >= count {
            return Some(lines);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_unauthenticated(response: &Response, case: &str) {
    assert_eq!(response.status, 401, "{case}: {response:?}");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "{case}"
    );
    assert_eq!(
        response.json(),
        json!({"code": "unauthenticated", "message_key": "error.unauthenticated"}),
        "{case}"
    );
}

/// Acceptance: forged signature, expired, wrong audience, and missing header are each rejected
/// with the M1 code `unauthenticated`; a valid token reaches the handler.
#[test]
fn only_requests_with_a_valid_token_reach_the_handler() {
    let key = TestKey::new(11);
    let keys = jwks(&[(&key, "2026-10-a")]);
    let keys_file = support::keys_file(&keys);
    let mut service = Service::start(&[(support::KEYS_FILE_VARIABLE, &keys_file)]);
    let now = unix_now();

    let valid = key.sign("2026-10-a", &claims(now, "assets"));
    let response = whoami(service.address, &[("x-internal-identity", &valid)]);
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(
        response.json(),
        json!({
            "user_id": USER,
            "tenant": {"id": TENANT, "role": "editor"},
            "platform_admin": false
        })
    );

    let forged = TestKey::new(12).sign("2026-10-a", &claims(now, "assets"));
    let expired = key.sign("2026-10-a", &claims(now - 3600, "assets"));
    let other_audience = key.sign("2026-10-a", &claims(now, "certs"));
    let bearer = format!("Bearer {valid}");
    // (case, request headers, logged reason)
    type Case<'a> = (&'a str, Vec<(&'a str, &'a str)>, &'a str);
    let cases: [Case<'_>; 6] = [
        (
            "forged signature",
            vec![("x-internal-identity", &forged)],
            "bad_signature",
        ),
        (
            "expired",
            vec![("x-internal-identity", &expired)],
            "expired",
        ),
        (
            "audience is another service",
            vec![("x-internal-identity", &other_audience)],
            "wrong_audience",
        ),
        ("missing header", vec![], "missing"),
        (
            "token in another header",
            vec![("authorization", &bearer)],
            "missing",
        ),
        (
            "header sent twice",
            vec![
                ("x-internal-identity", &valid),
                ("x-internal-identity", &valid),
            ],
            "repeated",
        ),
    ];
    println!("valid token: {} {}", response.status, response.body);
    let mut logged_so_far: HashMap<&str, usize> = HashMap::new();
    for (case, headers, reason) in &cases {
        let response = whoami(service.address, headers);
        assert_unauthenticated(&response, case);
        let expected = logged_so_far.entry(reason).or_default();
        *expected += 1;
        let lines = wait_for_rejections(&service, reason, *expected)
            .unwrap_or_else(|| panic!("{case}: the rejection is logged with reason {reason}"));
        assert!(
            lines.iter().all(|line| line["level"] == "WARN"),
            "{case}: {lines:?}"
        );
        println!(
            "{case}: {} {} logged WARN reason={reason}",
            response.status, response.body
        );
    }

    service.signal("TERM");
    let status = service
        .wait_exit(Duration::from_secs(10))
        .expect("the service exits");
    assert!(
        status.success(),
        "{status}\nstdout:\n{}\nstderr:\n{}",
        service.stdout.text(),
        service.stderr.text()
    );

    // Neither tokens nor key material reach the logs.
    let logs = format!("{}\n{}", service.stdout.text(), service.stderr.text());
    for (what, text) in [
        ("valid token", valid.as_str()),
        ("forged token", forged.as_str()),
        ("expired token", expired.as_str()),
        ("signature", valid.rsplit('.').next().expect("signature")),
        ("public key", key.jwk("2026-10-a")["x"].as_str().expect("x")),
    ] {
        assert!(!logs.contains(text), "the {what} appears in the logs");
    }
}

/// How long the tests wait for the service to read a changed keys file: many reload intervals
/// (5 s) on a loaded machine.
const RELOAD_WAIT: Duration = Duration::from_secs(60);

/// Waits until the service has loaded a set with exactly `kids`, as its INFO line says.
fn wait_for_loaded(service: &Service, kids: &[&str]) {
    let expected = format!("{kids:?}");
    service
        .stdout
        .wait_for_json(RELOAD_WAIT, |line| {
            line["message"]
                == "verifying internal identity tokens with these public keys of the identity service"
                && line["level"] == "INFO"
                && line["fields"]["keys"] == expected.as_str()
        })
        .unwrap_or_else(|| {
            panic!(
                "the service did not log loading the keys {expected} within {RELOAD_WAIT:?}\n{}",
                service.stdout.text()
            )
        });
}

/// Waits until `/readyz` on the health port answers `status`.
fn wait_for_readiness(service: &Service, status: u16) {
    let deadline = Instant::now() + RELOAD_WAIT;
    loop {
        let answered = support::get(service.health_address, "/readyz")
            .map(|response| response.status)
            .ok();
        if answered == Some(status) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "/readyz did not answer {status} within {RELOAD_WAIT:?} (last: {answered:?})"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Acceptance (Laysi/trustiot-v2#404): a rotation reaches a running service through its keys
/// file, without a restart. Before the new public key arrives a token under it is refused with
/// `unknown_key` (the WARN line names the key id); while both are published either verifies;
/// once the old key is gone its tokens are refused.
#[test]
fn a_rotation_reaches_a_running_service_through_its_keys_file() {
    let old = TestKey::new(21);
    let new = TestKey::new(22);
    let file = support::keys_file(&jwks(&[(&old, "2026-10-a")]));
    let service = Service::start(&[(support::KEYS_FILE_VARIABLE, &file)]);
    wait_for_loaded(&service, &["2026-10-a"]);
    let now = unix_now();
    let old_token = old.sign("2026-10-a", &claims(now, "assets"));
    let new_token = new.sign("2026-11-a", &claims(now, "assets"));
    let status = |token: &str| whoami(service.address, &[("x-internal-identity", token)]).status;

    assert_eq!(status(&old_token), 200);
    assert_unauthenticated(
        &whoami(service.address, &[("x-internal-identity", &new_token)]),
        "a key not published yet",
    );
    let lines = wait_for_rejections(&service, "unknown_key", 1).expect("unknown_key is logged");
    assert_eq!(lines[0]["fields"]["kid"], "2026-11-a", "{lines:?}");
    println!("before the new key: old 200, new 401 unknown_key (kid logged)");

    support::replace_keys_file(&file, &jwks(&[(&old, "2026-10-a"), (&new, "2026-11-a")]));
    wait_for_loaded(&service, &["2026-10-a", "2026-11-a"]);
    assert_eq!(status(&old_token), 200, "an old token during the overlap");
    assert_eq!(status(&new_token), 200, "a new token during the overlap");
    println!("both keys published: old 200, new 200, no restart");

    support::replace_keys_file(&file, &jwks(&[(&new, "2026-11-a")]));
    wait_for_loaded(&service, &["2026-11-a"]);
    assert_eq!(status(&new_token), 200);
    assert_eq!(status(&old_token), 401, "the retired key");
    println!("old key retired: old 401, new 200");
}

/// A keys file that is missing (a fresh install), empty or unusable keeps the service unready,
/// with a WARN line that says why without the file's content; a usable file later makes it
/// ready. Once it has keys, a broken file keeps the last usable set.
#[test]
fn a_missing_or_unusable_keys_file_keeps_the_service_unready() {
    let key = TestKey::new(31);
    let mut with_private_part = key.jwk("2026-10-a");
    with_private_part["d"] = json!("presence-is-enough");
    for (case, text) in [
        ("missing", None),
        ("empty", Some(String::new())),
        (
            "private key",
            Some(json!({ "keys": [with_private_part.clone()] }).to_string()),
        ),
        ("not JSON", Some("not-a-jwk-set".to_owned())),
        ("no keys", Some(json!({ "keys": [] }).to_string())),
    ] {
        let file = support::keys_file("");
        match &text {
            Some(text) => support::replace_keys_file(&file, text),
            None => std::fs::remove_file(&file).expect("remove the keys file"),
        }
        let mut service = Service::start(&[(support::KEYS_FILE_VARIABLE, &file)]);
        let warning = service
            .stdout
            .wait_for_message(
                "no usable public keys of the identity service yet; not ready",
                RELOAD_WAIT,
            )
            .unwrap_or_else(|| panic!("{case}: the problem is logged"));
        assert_eq!(warning["level"], "WARN", "{case}");
        wait_for_readiness(&service, 503);
        let token = key.sign("2026-10-a", &claims(unix_now(), "assets"));
        let response = whoami(service.address, &[("x-internal-identity", &token)]);
        assert_eq!(response.status, 503, "{case}: {response:?}");
        assert_eq!(
            response.json(),
            json!({"code": "unavailable", "message_key": "error.unavailable"}),
            "{case}"
        );

        support::replace_keys_file(&file, &jwks(&[(&key, "2026-10-a")]));
        wait_for_loaded(&service, &["2026-10-a"]);
        wait_for_readiness(&service, 200);
        let token = key.sign("2026-10-a", &claims(unix_now(), "assets"));
        assert_eq!(
            whoami(service.address, &[("x-internal-identity", &token)]).status,
            200,
            "{case}"
        );

        // Broken again: the last usable set stays, and readiness with it.
        support::replace_keys_file(&file, "not-a-jwk-set");
        service
            .stdout
            .wait_for_message(
                "the identity service's public keys file is not usable; keeping the last usable keys",
                RELOAD_WAIT,
            )
            .unwrap_or_else(|| panic!("{case}: keeping the last set is logged"));
        wait_for_readiness(&service, 200);
        assert_eq!(
            whoami(service.address, &[("x-internal-identity", &token)]).status,
            200,
            "{case}"
        );

        service.signal("TERM");
        service
            .wait_exit(Duration::from_secs(10))
            .unwrap_or_else(|| panic!("{case}: the service exits"));
        let logs = service.stdout.text();
        assert!(
            !logs.contains("presence-is-enough"),
            "{case}: the file is logged"
        );
        assert!(
            !logs.contains("not-a-jwk-set"),
            "{case}: the file is logged"
        );
        println!("{case}: unready (503), ready once a usable file arrived, kept it when broken");
    }
}
