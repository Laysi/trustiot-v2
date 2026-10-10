//! Internal identity verification end to end, from a real `example-service` process: the keys
//! come from the platform setting `TRUSTIOT_INTERNAL_IDENTITY_KEYS`, requests carry tokens in
//! `x-internal-identity`, and `/v1/whoami` uses the `UserIdentity` extractor. Rejected requests
//! answer 401 with the M1 body `unauthenticated`, and neither the token nor the key set reaches
//! the logs.
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
use support::{Response, Service, request, spawn_example, wait_exit};

const KEYS_VARIABLE: &str = "TRUSTIOT_INTERNAL_IDENTITY_KEYS";
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
    let mut service = Service::start(&[(KEYS_VARIABLE, &keys)]);
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

/// Acceptance: while keys rotate, tokens signed under the new key id verify alongside tokens
/// under the old one.
#[test]
fn tokens_under_either_key_verify_during_a_rotation() {
    let old = TestKey::new(21);
    let new = TestKey::new(22);
    let keys = jwks(&[(&old, "2026-10-a"), (&new, "2026-11-a")]);
    let service = Service::start(&[(KEYS_VARIABLE, &keys)]);
    let now = unix_now();
    for (kid, key) in [("2026-10-a", &old), ("2026-11-a", &new)] {
        let token = key.sign(kid, &claims(now, "assets"));
        let response = whoami(service.address, &[("x-internal-identity", &token)]);
        assert_eq!(response.status, 200, "{kid}: {response:?}");
        println!("kid {kid} with both keys configured: {}", response.status);
    }
    // A key id outside the set is rejected, whatever key signed it.
    let unknown = new.sign("2026-12-a", &claims(now, "assets"));
    let response = whoami(service.address, &[("x-internal-identity", &unknown)]);
    assert_unauthenticated(&response, "unknown key id");
    println!(
        "kid 2026-12-a (not configured): {} {}",
        response.status, response.body
    );
}

/// A key set the service cannot use stops it before it serves traffic, and the error names the
/// variable without its value.
#[test]
fn an_unusable_key_set_stops_startup() {
    let key = TestKey::new(31);
    let mut with_private_part = key.jwk("2026-10-a");
    with_private_part["d"] = json!("presence-is-enough");
    for (case, keys) in [
        (
            "private key",
            json!({ "keys": [with_private_part] }).to_string(),
        ),
        ("not JSON", "not-a-jwk-set".to_owned()),
        ("no keys", json!({ "keys": [] }).to_string()),
    ] {
        let (mut child, stdout, _stderr) = spawn_example(&[(KEYS_VARIABLE, &keys)]);
        let status = wait_exit(&mut child, Duration::from_secs(10))
            .unwrap_or_else(|| panic!("{case}: the service exits"));
        assert_eq!(status.code(), Some(1), "{case}");
        let error = stdout
            .wait_for_message(
                "invalid configuration; not starting",
                Duration::from_secs(2),
            )
            .unwrap_or_else(|| panic!("{case}: the error is logged"));
        let text = error["fields"]["error"].as_str().expect("error text");
        assert!(
            text.contains("platform.internal_identity_keys"),
            "{case}: {text}"
        );
        assert!(text.contains(KEYS_VARIABLE), "{case}: {text}");
        // The process has exited; read its whole output before checking what is absent from it.
        assert!(
            stdout.wait_closed(support::OUTPUT_DRAIN_TIMEOUT),
            "{case}: the output was not read to its end"
        );
        assert!(
            !stdout.text().contains(&keys),
            "{case}: the value is logged"
        );
        assert!(
            !stdout.text().contains("presence-is-enough"),
            "{case}: part of the value is logged"
        );
        println!("{case}: {status}; error: {text}");
    }
}
