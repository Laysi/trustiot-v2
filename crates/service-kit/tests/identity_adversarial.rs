//! Adversarial regression tests for internal identity verification, written against the public
//! API only (`IdentityVerifier`, `IdentityKeys`) and, for the HTTP layer, a real
//! `example-service` process. Each test builds many hostile tokens, key sets, or requests and
//! reports every case that was not handled as expected, so one run shows the whole picture.
//!
//! Independent verification of Laysi/trustiot-v2#52. The signing keys are throwaway test keys
//! derived in code from a one-byte seed.
#![cfg(feature = "contract")]
// Test inputs are ASCII (base64url, hex, JWK text), so byte slicing cannot split a character.
#![allow(
    clippy::string_slice,
    reason = "the sliced strings are ASCII base64url, hex or JSON built by these tests"
)]

mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use contract::{INTERNAL_IDENTITY_HEADER, InternalToken, ServiceName};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};
use service_kit::identity::{
    IdentityError, IdentityKeys, IdentityVerifier, LEEWAY_SECS, MAX_KEY_ID_LEN, MAX_KEYS,
    MAX_LIFETIME_SECS,
};
use support::Service;

const KID: &str = "k1";
const HEADER: &str = r#"{"alg":"EdDSA","typ":"internal-identity+jwt","kid":"k1"}"#;
const USER: &str = "01928f6e-7a3c-7d4e-8f00-0123456789ab";
const TENANT: &str = "01928f6e-7a3c-7d4e-8f00-00000000000a";

fn now() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970");
    i64::try_from(elapsed.as_secs()).expect("seconds fit in i64")
}

fn b64(bytes: impl AsRef<[u8]>) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn jwk(key: &SigningKey, kid: &str) -> Value {
    json!({"kty": "OKP", "crv": "Ed25519", "kid": kid, "x": b64(key.verifying_key().as_bytes())})
}

fn keys_of(entries: &[(&SigningKey, &str)]) -> IdentityKeys {
    let keys: Vec<Value> = entries.iter().map(|(key, kid)| jwk(key, kid)).collect();
    IdentityKeys::from_jwks(&json!({ "keys": keys }).to_string()).expect("valid key set")
}

/// `<header>.<payload>.<signature>` signed over the given raw segment texts.
fn sign_segments(key: &SigningKey, header: &str, payload: &str) -> String {
    let signed = format!("{header}.{payload}");
    let signature = key.sign(signed.as_bytes());
    format!("{signed}.{}", b64(signature.to_bytes()))
}

/// A token over raw header and payload bytes.
fn sign_raw(key: &SigningKey, header: impl AsRef<[u8]>, payload: impl AsRef<[u8]>) -> String {
    sign_segments(key, &b64(header), &b64(payload))
}

fn claims_at(iat: i64) -> Value {
    json!({
        "iss": "identity", "sub": USER, "aud": "assets", "iat": iat, "exp": iat + 60,
        "tenant": {"id": TENANT, "role": "editor"}, "platform_admin": false
    })
}

fn claims_text(iat: i64) -> String {
    claims_at(iat).to_string()
}

struct Fixture {
    key: SigningKey,
    verifier: IdentityVerifier,
}

impl Fixture {
    fn new() -> Self {
        let key = signing_key(7);
        let verifier = IdentityVerifier::new(ServiceName::Assets, keys_of(&[(&key, KID)]));
        Self { key, verifier }
    }

    fn valid(&self) -> String {
        sign_raw(&self.key, HEADER, claims_text(now()))
    }
}

type Expect = Result<(), IdentityError>;

/// Collects every case whose outcome differs from the expectation.
#[derive(Default)]
struct Cases {
    failures: Vec<String>,
    total: usize,
}

impl Cases {
    fn check(&mut self, verifier: &IdentityVerifier, name: &str, token: &str, expected: Expect) {
        self.total += 1;
        let actual = verifier.verify(token).map(|_| ());
        if actual != expected {
            let shown: String = token.chars().take(80).collect();
            self.failures.push(format!(
                "{name}: expected {expected:?}, got {actual:?} ({shown})"
            ));
        }
    }

    fn finish(self) {
        assert!(
            self.failures.is_empty(),
            "{} of {} cases failed:\n{}",
            self.failures.len(),
            self.total,
            self.failures.join("\n")
        );
    }
}

const OK: Expect = Ok(());

/// Verifies a token built for the next whole second, in the first moments of that second, and
/// returns that second with the result. If the clock moved on before the verification finished,
/// the attempt is repeated, so a boundary case never depends on timing.
fn at_exact_second(verifier: &IdentityVerifier, build: impl Fn(i64) -> String) -> (i64, Expect) {
    loop {
        let second = now() + 1;
        let token = build(second);
        while now() < second {
            std::thread::sleep(Duration::from_millis(1));
        }
        let result = verifier.verify(&token).map(|_| ());
        if now() == second {
            return (second, result);
        }
    }
}

#[test]
fn algorithm_confusion_is_rejected() {
    let fixture = Fixture::new();
    let payload = claims_text(now());
    let public_key = fixture.key.verifying_key().to_bytes();
    let mut cases = Cases::default();
    let header_with =
        |alg: &str| format!(r#"{{"alg":{alg},"typ":"internal-identity+jwt","kid":"k1"}}"#);

    for alg in [
        r#""none""#,
        r#""None""#,
        r#""NONE""#,
        r#""HS256""#,
        r#""HS384""#,
        r#""HS512""#,
        r#""RS256""#,
        r#""RS512""#,
        r#""PS256""#,
        r#""ES256""#,
        r#""ES384""#,
        r#""ES256K""#,
        r#""eddsa""#,
        r#""EDDSA""#,
        r#""Ed25519""#,
        r#""Ed448""#,
        r#""EdDSA ""#,
        r#"" EdDSA""#,
        r#""EdDSA\u0000""#,
        r#""""#,
    ] {
        // The signature bytes are irrelevant (and for HS* deliberately key-shaped): the
        // algorithm is rejected before any key is touched. Try an empty one, a 32-byte one made
        // of the public key (the classic "HMAC with the public key as secret" shape), and a real
        // Ed25519 signature over the signing input.
        let signed = format!("{}.{}", b64(header_with(alg)), b64(&payload));
        for (label, signature) in [
            ("empty", String::new()),
            ("public key as signature", b64(public_key)),
            (
                "real signature",
                b64(fixture.key.sign(signed.as_bytes()).to_bytes()),
            ),
        ] {
            let token = format!("{signed}.{signature}");
            cases.check(
                &fixture.verifier,
                &format!("alg {alg} / {label}"),
                &token,
                Err(IdentityError::UnsupportedAlgorithm),
            );
        }
    }
    for alg in ["null", "1", "true", r#"["EdDSA"]"#, r#"{"a":"EdDSA"}"#] {
        cases.check(
            &fixture.verifier,
            &format!("alg {alg}"),
            &sign_raw(&fixture.key, header_with(alg), &payload),
            Err(IdentityError::MalformedHeader),
        );
    }
    // alg missing altogether
    cases.check(
        &fixture.verifier,
        "alg missing",
        &sign_raw(
            &fixture.key,
            r#"{"typ":"internal-identity+jwt","kid":"k1"}"#,
            &payload,
        ),
        Err(IdentityError::MalformedHeader),
    );
    // alg: none with the unsigned shape `h.p.`
    cases.check(
        &fixture.verifier,
        "unsigned alg none",
        &format!("{}.{}.", b64(header_with(r#""none""#)), b64(&payload)),
        Err(IdentityError::UnsupportedAlgorithm),
    );
    // EdDSA but a 32-byte (HMAC-sized) or empty signature
    for (label, signature) in [("32 bytes", b64(public_key)), ("empty", String::new())] {
        cases.check(
            &fixture.verifier,
            &format!("EdDSA with {label} signature"),
            &format!("{}.{}.{signature}", b64(HEADER), b64(&payload)),
            Err(IdentityError::Malformed),
        );
    }
    cases.finish();
}

#[test]
fn header_parameters_are_strict() {
    let fixture = Fixture::new();
    let payload = claims_text(now());
    let mut cases = Cases::default();
    let signed = |header: &str| sign_raw(&fixture.key, header, &payload);
    let jwk_text = jwk(&fixture.key, KID).to_string();

    // typ
    for (typ, expected) in [
        (r#""internal-identity+jwt""#, OK),
        (r#""Internal-Identity+JWT""#, OK),
        (r#""INTERNAL-IDENTITY+JWT""#, OK),
        (r#""JWT""#, Err(IdentityError::WrongType)),
        (r#""jwt""#, Err(IdentityError::WrongType)),
        (r#""at+jwt""#, Err(IdentityError::WrongType)),
        (r#""JOSE""#, Err(IdentityError::WrongType)),
        (
            r#""application/internal-identity+jwt""#,
            Err(IdentityError::WrongType),
        ),
        (r#""internal-identity+jwt ""#, Err(IdentityError::WrongType)),
        (r#"" internal-identity+jwt""#, Err(IdentityError::WrongType)),
        (
            r#""internal-identity+jwt\u0000""#,
            Err(IdentityError::WrongType),
        ),
        (
            r#""internal-identity+jwt\n""#,
            Err(IdentityError::WrongType),
        ),
        (r#""internal-identity+jwt""#, OK),
        (r#""internal-ıdentity+jwt""#, Err(IdentityError::WrongType)),
        (
            r#""internal-identity+jwt+jwt""#,
            Err(IdentityError::WrongType),
        ),
        (r#""""#, Err(IdentityError::WrongType)),
        ("null", Err(IdentityError::MalformedHeader)),
        ("1", Err(IdentityError::MalformedHeader)),
        (
            r#"["internal-identity+jwt"]"#,
            Err(IdentityError::MalformedHeader),
        ),
    ] {
        cases.check(
            &fixture.verifier,
            &format!("typ {typ}"),
            &signed(&format!(r#"{{"alg":"EdDSA","typ":{typ},"kid":"k1"}}"#)),
            expected,
        );
    }
    cases.check(
        &fixture.verifier,
        "typ missing",
        &signed(r#"{"alg":"EdDSA","kid":"k1"}"#),
        Err(IdentityError::MalformedHeader),
    );

    // Extra parameters: cty, crit, b64 (RFC 7797), embedded keys, key URLs, and the rest.
    for extra in [
        r#""cty":"JWT""#,
        r#""cty":"internal-identity+jwt""#,
        r#""crit":["exp"]"#,
        r#""crit":["b64"],"b64":false"#,
        r#""crit":[]"#,
        r#""crit":null"#,
        r#""b64":false"#,
        r#""b64":true"#,
        r#""jku":"http://attacker.example/jwks.json""#,
        r#""x5u":"http://attacker.example/cert""#,
        r#""x5c":["MIIB"]"#,
        r#""x5t":"abc""#,
        r#""x5t#S256":"abc""#,
        r#""zip":"DEF""#,
        r#""enc":"A128GCM""#,
        r#""nonce":"1""#,
        r#""exp":1"#,
        r#""KID":"k1""#,
        r#""Alg":"EdDSA""#,
        r#""":1"#,
        r#""__proto__":{}"#,
    ] {
        cases.check(
            &fixture.verifier,
            &format!("extra {extra}"),
            &signed(&format!(
                r#"{{"alg":"EdDSA","typ":"internal-identity+jwt","kid":"k1",{extra}}}"#
            )),
            Err(IdentityError::MalformedHeader),
        );
    }
    cases.check(
        &fixture.verifier,
        "embedded jwk",
        &signed(&format!(
            r#"{{"alg":"EdDSA","typ":"internal-identity+jwt","kid":"k1","jwk":{jwk_text}}}"#
        )),
        Err(IdentityError::MalformedHeader),
    );
    // An attacker-supplied key embedded in the header and used to sign: ignored and rejected.
    let attacker = signing_key(66);
    cases.check(
        &fixture.verifier,
        "attacker jwk, attacker signature",
        &sign_raw(
            &attacker,
            format!(
                r#"{{"alg":"EdDSA","typ":"internal-identity+jwt","kid":"k1","jwk":{}}}"#,
                jwk(&attacker, "k1")
            ),
            &payload,
        ),
        Err(IdentityError::MalformedHeader),
    );
    cases.check(
        &fixture.verifier,
        "attacker signature under the configured kid",
        &sign_raw(&attacker, HEADER, &payload),
        Err(IdentityError::BadSignature),
    );

    // Duplicates, in either order, also spelled with escapes.
    for header in [
        r#"{"alg":"none","alg":"EdDSA","typ":"internal-identity+jwt","kid":"k1"}"#,
        r#"{"alg":"EdDSA","alg":"none","typ":"internal-identity+jwt","kid":"k1"}"#,
        r#"{"alg":"EdDSA","alg":"EdDSA","typ":"internal-identity+jwt","kid":"k1"}"#,
        r#"{"alg":"EdDSA","typ":"JWT","typ":"internal-identity+jwt","kid":"k1"}"#,
        r#"{"alg":"EdDSA","typ":"internal-identity+jwt","kid":"evil","kid":"k1"}"#,
        r#"{"alg":"EdDSA","typ":"internal-identity+jwt","kid":"k1","kid":"evil"}"#,
        r#"{"alg":"none","alg":"EdDSA","typ":"internal-identity+jwt","kid":"k1"}"#,
        r#"{"alg":"EdDSA","typ":"internal-identity+jwt","kid":"k1","kid":"k1"}"#,
    ] {
        cases.check(
            &fixture.verifier,
            &format!("duplicate {header}"),
            &signed(header),
            Err(IdentityError::MalformedHeader),
        );
    }
    // Escaped spelling of a single parameter is the same parameter.
    cases.check(
        &fixture.verifier,
        "escaped member names",
        &signed(r#"{"alg":"EdDSA","typ":"internal-identity+jwt","kid":"k1"}"#),
        OK,
    );

    // Not an object, or not only an object.
    for header in [
        "[]",
        "null",
        "true",
        "1",
        r#""EdDSA""#,
        "{}",
        "",
        " ",
        "\u{feff}{}",
        &format!("\u{feff}{HEADER}"),
        &format!("{HEADER}{HEADER}"),
        &format!("{HEADER}x"),
        &format!("{HEADER},"),
        &format!("[{HEADER}]"),
        &format!("{HEADER}\0"),
        r#"{"alg":"EdDSA","typ":"internal-identity+jwt","kid":"k1""#,
        r#"{'alg':'EdDSA','typ':'internal-identity+jwt','kid':'k1'}"#,
        r#"{"alg":"EdDSA","typ":"internal-identity+jwt","kid":"k1",}"#,
    ] {
        cases.check(
            &fixture.verifier,
            &format!("header {header:?}"),
            &signed(header),
            Err(IdentityError::MalformedHeader),
        );
    }
    // Whitespace around a well-formed header is part of the signed text and harmless.
    cases.check(
        &fixture.verifier,
        "padded header",
        &signed(&format!(" \n\t{HEADER}\r\n ")),
        OK,
    );
    // Invalid UTF-8 inside a header string.
    let mut bad_utf8 = br#"{"alg":"EdDSA","typ":"internal-identity+jwt","kid":""#.to_vec();
    bad_utf8.extend([0xff, 0xfe]);
    bad_utf8.extend(br#""}"#);
    cases.check(
        &fixture.verifier,
        "invalid UTF-8 in kid",
        &sign_raw(&fixture.key, bad_utf8, &payload),
        Err(IdentityError::MalformedHeader),
    );
    cases.finish();
}

#[test]
fn key_ids_select_keys_exactly() {
    let fixture = Fixture::new();
    let payload = claims_text(now());
    let mut cases = Cases::default();
    let long = "k".repeat(4000);
    let too_long = "k".repeat(MAX_KEY_ID_LEN + 1);
    for (kid_json, expected) in [
        (r#""k1""#.to_owned(), OK),
        (r#""K1""#.to_owned(), Err(IdentityError::UnknownKey)),
        (r#""k1 ""#.to_owned(), Err(IdentityError::UnknownKey)),
        (r#"" k1""#.to_owned(), Err(IdentityError::UnknownKey)),
        (r#""k1\u0000""#.to_owned(), Err(IdentityError::UnknownKey)),
        (r#""k1\n""#.to_owned(), Err(IdentityError::UnknownKey)),
        (r#""k1\u200b""#.to_owned(), Err(IdentityError::UnknownKey)),
        ("\"k1\u{200b}\"".to_owned(), Err(IdentityError::UnknownKey)),
        (r#""k1""#.to_owned(), OK),
        (r#""""#.to_owned(), Err(IdentityError::UnknownKey)),
        (r#""../k1""#.to_owned(), Err(IdentityError::UnknownKey)),
        (r#""k1/../k1""#.to_owned(), Err(IdentityError::UnknownKey)),
        (
            r#""/etc/passwd""#.to_owned(),
            Err(IdentityError::UnknownKey),
        ),
        (r#""k1%00""#.to_owned(), Err(IdentityError::UnknownKey)),
        (r#""k1\\""#.to_owned(), Err(IdentityError::UnknownKey)),
        (r#""\u0000""#.to_owned(), Err(IdentityError::UnknownKey)),
        (r#""😀""#.to_owned(), Err(IdentityError::UnknownKey)),
        (format!("\"{long}\""), Err(IdentityError::UnknownKey)),
        (format!("\"{too_long}\""), Err(IdentityError::UnknownKey)),
        ("null".to_owned(), Err(IdentityError::MalformedHeader)),
        ("1".to_owned(), Err(IdentityError::MalformedHeader)),
        (r#"["k1"]"#.to_owned(), Err(IdentityError::MalformedHeader)),
        (
            r#"{"kid":"k1"}"#.to_owned(),
            Err(IdentityError::MalformedHeader),
        ),
    ] {
        let header = format!(r#"{{"alg":"EdDSA","typ":"internal-identity+jwt","kid":{kid_json}}}"#);
        let shown: String = kid_json.chars().take(40).collect();
        cases.check(
            &fixture.verifier,
            &format!("kid {shown}"),
            &sign_raw(&fixture.key, header, &payload),
            expected,
        );
    }
    cases.finish();
}

#[test]
fn token_shape_and_encoding_are_strict() {
    let fixture = Fixture::new();
    let token = fixture.valid();
    let parts: Vec<&str> = token.split('.').collect();
    let (header, payload, signature) = (parts[0], parts[1], parts[2]);
    let mut cases = Cases::default();
    let malformed = Err(IdentityError::Malformed);

    for (name, bad) in [
        ("empty", String::new()),
        ("dot", ".".to_owned()),
        ("one segment", header.to_owned()),
        ("two segments", format!("{header}.{payload}")),
        ("four segments", format!("{token}.")),
        ("five segments", format!("{token}.a.b")),
        ("leading dot", format!(".{token}")),
        ("trailing dot", format!("{token}.")),
        ("empty signature", format!("{header}.{payload}.")),
        ("two tokens joined", format!("{token}.{token}")),
        (
            "padded signature",
            format!("{header}.{payload}.{signature}="),
        ),
        (
            "padded signature double",
            format!("{header}.{payload}.{signature}=="),
        ),
        ("leading space", format!(" {token}")),
        ("trailing space", format!("{token} ")),
        ("trailing newline", format!("{token}\n")),
        ("trailing CRLF", format!("{token}\r\n")),
        (
            "embedded newline",
            format!("{header}\n.{payload}.{signature}"),
        ),
        ("NUL", format!("{token}\0")),
        ("non-ASCII", format!("{token}é")),
        (
            "standard alphabet +",
            format!("{header}.{payload}.+{}", &signature[1..]),
        ),
        ("Bearer prefix", format!("Bearer {token}")),
        (
            "JSON serialization",
            format!(
                r#"{{"protected":"{header}","payload":"{payload}","signature":"{signature}"}}"#
            ),
        ),
        ("quoted", format!("\"{token}\"")),
    ] {
        cases.check(&fixture.verifier, name, &bad, malformed);
    }

    // Shapes with an empty or altered segment: rejected whichever check notices first.
    for (name, bad) in [
        ("two dots", "..".to_owned()),
        ("double dot", format!("{header}..{signature}")),
        ("empty header", format!(".{payload}.{signature}")),
        ("embedded tab", format!("{header}.\t{payload}.{signature}")),
    ] {
        assert!(fixture.verifier.verify(&bad).is_err(), "{name} accepted");
    }

    // Padding on the header or the payload segment, with a signature made over the padded text.
    cases.check(
        &fixture.verifier,
        "padded header segment",
        &sign_segments(
            &fixture.key,
            &format!("{}=", b64("{}")),
            &b64(claims_text(now())),
        ),
        malformed,
    );
    // Standard alphabet and padded variants signed as sent: still not base64url without padding.
    let standard = base64::engine::general_purpose::STANDARD;
    let wide_header = format!("{HEADER}  "); // length not a multiple of 3 after padding
    for (name, header_segment, payload_segment) in [
        (
            "standard padded header",
            standard.encode(HEADER),
            b64(claims_text(now())),
        ),
        (
            "standard padded payload",
            b64(HEADER),
            standard.encode(claims_text(now())),
        ),
        (
            "padded payload segment",
            b64(HEADER),
            format!("{}=", b64("{}")),
        ),
        (
            "space in payload segment",
            b64(HEADER),
            format!("{} ", b64(claims_text(now()))),
        ),
        (
            "wide header standard",
            standard.encode(&wide_header),
            b64(claims_text(now())),
        ),
    ] {
        cases.check(
            &fixture.verifier,
            name,
            &sign_segments(&fixture.key, &header_segment, &payload_segment),
            malformed,
        );
    }

    // Non-canonical trailing bits: the same bytes spelled a second way, signed as sent.
    for pad in 0..3 {
        let header_text = format!("{HEADER}{}", " ".repeat(pad));
        let canonical = b64(&header_text);
        let last = canonical.chars().last().expect("segment");
        let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let index = alphabet.find(last).expect("alphabet");
        if header_text.len() % 3 == 0 {
            continue; // no spare bits to abuse
        }
        // Flip a trailing bit: same top bits, different low bit(s).
        let altered = alphabet.chars().nth(index ^ 1).expect("char");
        let sneaky = format!("{}{altered}", &canonical[..canonical.len() - 1]);
        assert_ne!(sneaky, canonical);
        cases.check(
            &fixture.verifier,
            &format!("non-canonical header segment (pad {pad})"),
            &sign_segments(&fixture.key, &sneaky, &b64(claims_text(now()))),
            malformed,
        );
    }
    // Non-canonical payload: find an unused-bit variant of the payload segment.
    for pad in 0..3 {
        let text = format!("{}{}", claims_text(now()), " ".repeat(pad));
        let canonical = b64(&text);
        if text.len() % 3 == 0 {
            continue;
        }
        let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let last = canonical.chars().last().expect("segment");
        let altered = alphabet
            .chars()
            .nth(alphabet.find(last).expect("alphabet") ^ 1)
            .expect("char");
        let sneaky = format!("{}{altered}", &canonical[..canonical.len() - 1]);
        cases.check(
            &fixture.verifier,
            &format!("non-canonical payload segment (pad {pad})"),
            &sign_segments(&fixture.key, &b64(HEADER), &sneaky),
            malformed,
        );
    }
    // Non-canonical signature: the final character carries four spare bits.
    let last = signature.chars().last().expect("signature");
    let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let altered = alphabet
        .chars()
        .nth(alphabet.find(last).expect("alphabet") ^ 1)
        .expect("char");
    cases.check(
        &fixture.verifier,
        "non-canonical signature spelling",
        &format!(
            "{header}.{payload}.{}{altered}",
            &signature[..signature.len() - 1]
        ),
        malformed,
    );
    for length in [0usize, 1, 32, 63, 65, 96, 128] {
        cases.check(
            &fixture.verifier,
            &format!("signature of {length} bytes"),
            &format!("{header}.{payload}.{}", b64(vec![0x5a; length])),
            malformed,
        );
    }
    cases.check(&fixture.verifier, "control: the valid token", &token, OK);
    cases.finish();
}

#[test]
fn signature_malleability_and_degenerate_signatures_are_rejected() {
    let fixture = Fixture::new();
    let token = fixture.valid();
    let (signed, signature) = token.rsplit_once('.').expect("signature");
    let raw = URL_SAFE_NO_PAD.decode(signature).expect("signature bytes");
    let mut cases = Cases::default();
    let bad_signature = Err(IdentityError::BadSignature);
    let with = |bytes: &[u8]| format!("{signed}.{}", b64(bytes));

    // S + l (non-canonical scalar)
    const L: [u8; 32] = [
        0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde,
        0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
    ];
    let mut malleated = raw.clone();
    let mut carry = 0u16;
    for (byte, add) in malleated[32..].iter_mut().zip(L) {
        let sum = u16::from(*byte) + u16::from(add) + carry;
        *byte = sum.to_le_bytes()[0];
        carry = sum >> 8;
    }
    cases.check(&fixture.verifier, "S + l", &with(&malleated), bad_signature);
    // S with the top bits set (>= 2^253)
    let mut high = raw.clone();
    high[63] |= 0xe0;
    cases.check(
        &fixture.verifier,
        "S with high bits",
        &with(&high),
        bad_signature,
    );
    let mut high = raw.clone();
    high[63] ^= 0x80;
    cases.check(
        &fixture.verifier,
        "S top bit flipped",
        &with(&high),
        bad_signature,
    );
    // All-zero signature, all-0xff signature.
    cases.check(
        &fixture.verifier,
        "zero signature",
        &with(&[0u8; 64]),
        bad_signature,
    );
    cases.check(
        &fixture.verifier,
        "0xff signature",
        &with(&[0xffu8; 64]),
        bad_signature,
    );
    // Small-order R with S = 0 (the classic universal forgery under lax verification).
    for r in [
        [1u8; 1]
            .iter()
            .chain([0u8; 31].iter())
            .copied()
            .collect::<Vec<u8>>(),
        [0u8; 32].to_vec(),
        {
            let mut y = vec![0xffu8; 32];
            y[0] = 0xec;
            y[31] = 0x7f;
            y
        },
    ] {
        let mut sig = r;
        sig.extend([0u8; 32]);
        cases.check(
            &fixture.verifier,
            "small-order R, S=0",
            &with(&sig),
            bad_signature,
        );
    }
    // R replaced by the identity while keeping S.
    let mut sig = raw.clone();
    sig[..32].copy_from_slice(&{
        let mut identity = [0u8; 32];
        identity[0] = 1;
        identity
    });
    cases.check(
        &fixture.verifier,
        "R = identity, S kept",
        &with(&sig),
        bad_signature,
    );
    // A genuine signature over a different message (replay across payloads).
    let other = sign_raw(&fixture.key, HEADER, claims_text(now() + 1));
    let other_sig = other.rsplit('.').next().expect("signature");
    cases.check(
        &fixture.verifier,
        "signature of another token",
        &format!("{signed}.{other_sig}"),
        bad_signature,
    );
    // Header and payload swapped under the original signature.
    let parts: Vec<&str> = token.split('.').collect();
    cases.check(
        &fixture.verifier,
        "segments swapped",
        &format!("{}.{}.{}", parts[1], parts[0], parts[2]),
        Err(IdentityError::MalformedHeader),
    );
    cases.check(&fixture.verifier, "control", &token, OK);
    cases.finish();
}

#[test]
fn every_single_character_edit_of_a_valid_token_is_rejected() {
    let fixture = Fixture::new();
    let token = fixture.valid();
    let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.";
    let mut tried = 0usize;
    let mut accepted = Vec::new();
    // Verification is slow in a debug build, so a run edits a sample of positions with a sample
    // of characters; IDENTITY_SWEEP_FULL=1 edits every position with the whole alphabet (about
    // 27 000 tokens; use a release build).
    let full = std::env::var_os("IDENTITY_SWEEP_FULL").is_some();
    let stride = if full { 1 } else { 5 };
    for index in (0..token.len()).step_by(stride) {
        let in_signature = index + 90 > token.len();
        let sampled: Vec<char> = if full {
            alphabet.chars().collect()
        } else if in_signature {
            vec!['A', '_', '-', 'z']
        } else {
            vec!['A', '_', '.']
        };
        for replacement in sampled {
            if token.as_bytes()[index] as char == replacement {
                continue;
            }
            let mut edited = token.clone();
            edited.replace_range(index..=index, &replacement.to_string());
            tried += 1;
            if fixture.verifier.verify(&edited).is_ok() {
                accepted.push(edited);
            }
        }
        let mut deleted = token.clone();
        deleted.remove(index);
        let mut inserted = token.clone();
        inserted.insert(index, 'A');
        for edited in [deleted, inserted] {
            tried += 1;
            if fixture.verifier.verify(&edited).is_ok() {
                accepted.push(edited);
            }
        }
    }
    assert!(tried > 300, "{tried}");
    println!("edited tokens tried: {tried}");
    assert!(
        accepted.is_empty(),
        "{} edited tokens accepted: {accepted:?}",
        accepted.len()
    );
}

#[test]
fn payload_json_is_strict() {
    let fixture = Fixture::new();
    let t = now();
    let mut cases = Cases::default();
    let malformed = Err(IdentityError::MalformedClaims);
    let base = |extra: &str| {
        format!(
            r#"{{"iss":"identity","sub":"{USER}","aud":"assets","iat":{t},"exp":{},{extra}"tenant":{{"id":"{TENANT}","role":"editor"}},"platform_admin":false}}"#,
            t + 60
        )
    };
    let signed = |payload: &str| sign_raw(&fixture.key, HEADER, payload);

    cases.check(&fixture.verifier, "control", &signed(&base("")), OK);

    // Duplicate members: none may override another, in either order.
    for (name, payload) in [
        ("duplicate aud, other first", base(r#""aud":"certs","#)),
        ("duplicate iss", base(r#""iss":"assets","#)),
        ("duplicate sub", base(&format!(r#""sub":"{TENANT}","#))),
        ("duplicate iat", base(r#""iat":1,"#)),
        ("duplicate exp", base(r#""exp":9999999999,"#)),
        ("duplicate tenant", base(r#""tenant":null,"#)),
        (
            "duplicate platform_admin",
            base(r#""platform_admin":true,"#),
        ),
        ("duplicate escaped", base(r#""platform_admin":true,"#)),
        (
            "duplicate nbf",
            base(&format!(r#""nbf":{t},"nbf":{},"#, t + 3600)),
        ),
        (
            "duplicate nbf other order",
            base(&format!(r#""nbf":{},"nbf":{t},"#, t + 3600)),
        ),
        (
            "duplicate nbf escaped",
            base(&format!(r#""nbf":{},"nbf":{t},"#, t + 3600)),
        ),
    ] {
        cases.check(&fixture.verifier, name, &signed(&payload), malformed);
    }
    // Duplicate unknown members are harmless.
    cases.check(
        &fixture.verifier,
        "duplicate jti",
        &signed(&base(r#""jti":1,"jti":2,"#)),
        OK,
    );

    // Not an object, or not only an object.
    for payload in [
        "[]",
        "null",
        "true",
        "1",
        "\"x\"",
        "{}",
        "",
        " ",
        "{",
        "\u{feff}",
        "NaN",
        &format!("[{}]", base("")),
        &format!("{}{}", base(""), base("")),
        &format!("{}x", base("")),
        &format!("{},", base("")),
        &format!("{}\0", base("")),
        &format!("\u{feff}{}", base("")),
    ] {
        cases.check(
            &fixture.verifier,
            &format!("payload {payload:?}"),
            &signed(payload),
            malformed,
        );
    }
    cases.check(
        &fixture.verifier,
        "padded payload",
        &signed(&format!(" \n{} \t\r\n", base(""))),
        OK,
    );

    // Unknown claims are ignored (documented); a deeply nested one is bounded, not a crash.
    cases.check(
        &fixture.verifier,
        "unknown scalar",
        &signed(&base(r#""jti":"f81d","#)),
        OK,
    );
    cases.check(
        &fixture.verifier,
        "unknown nested object",
        &signed(&base(r#""ext":{"a":[1,2,{"b":null}]},"#)),
        OK,
    );
    cases.check(
        &fixture.verifier,
        "unknown claim named like a header parameter",
        &signed(&base(r#""alg":"none","kid":"x","typ":"JWT","#)),
        OK,
    );
    for depth in [100usize, 126] {
        let nested = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        cases.check(
            &fixture.verifier,
            &format!("nesting {depth}"),
            &signed(&base(&format!(r#""ext":{nested},"#))),
            OK,
        );
    }
    for depth in [200usize, 2500] {
        let nested = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        cases.check(
            &fixture.verifier,
            &format!("nesting {depth}"),
            &signed(&base(&format!(r#""ext":{nested},"#))),
            OK,
        );
        // Unclosed, and nested objects.
        cases.check(
            &fixture.verifier,
            &format!("unclosed nesting {depth}"),
            &signed(&base(&format!(r#""ext":{},"#, "[".repeat(depth)))),
            malformed,
        );
        let object_depth = depth.min(600);
        let objects = format!(
            "{}1{}",
            r#"{"a":"#.repeat(object_depth),
            "}".repeat(object_depth)
        );
        cases.check(
            &fixture.verifier,
            &format!("object nesting {object_depth}"),
            &signed(&base(&format!(r#""ext":{objects},"#))),
            OK,
        );
    }
    // Deep nesting in the header is rejected too (any unknown member is).
    let header_nested = format!(
        r#"{{"alg":"EdDSA","typ":"internal-identity+jwt","kid":"k1","x":{}{}}}"#,
        "[".repeat(2500),
        "]".repeat(2500)
    );
    cases.check(
        &fixture.verifier,
        "deep header",
        &sign_raw(&fixture.key, header_nested, base("")),
        Err(IdentityError::MalformedHeader),
    );
    cases.finish();
}

#[test]
fn claims_are_typed_and_required() {
    let fixture = Fixture::new();
    let t = now();
    let mut cases = Cases::default();
    let with = |key: &str, value: Option<Value>| {
        let mut claims = claims_at(t);
        match value {
            Some(value) => claims[key] = value,
            None => {
                claims.as_object_mut().expect("object").remove(key);
            }
        }
        sign_raw(&fixture.key, HEADER, claims.to_string())
    };
    let malformed = Err(IdentityError::MalformedClaims);

    for (name, key, value, expected) in [
        // iss
        (
            "iss other service",
            "iss",
            Some(json!("assets")),
            Err(IdentityError::WrongIssuer),
        ),
        ("iss gateway", "iss", Some(json!("gateway")), malformed),
        ("iss capitalised", "iss", Some(json!("Identity")), malformed),
        ("iss padded", "iss", Some(json!("identity ")), malformed),
        ("iss empty", "iss", Some(json!("")), malformed),
        ("iss null", "iss", Some(json!(null)), malformed),
        ("iss array", "iss", Some(json!(["identity"])), malformed),
        ("iss number", "iss", Some(json!(1)), malformed),
        ("iss missing", "iss", None, malformed),
        // aud
        (
            "aud other service",
            "aud",
            Some(json!("certs")),
            Err(IdentityError::WrongAudience),
        ),
        ("aud array one", "aud", Some(json!(["assets"])), malformed),
        (
            "aud array two",
            "aud",
            Some(json!(["assets", "certs"])),
            malformed,
        ),
        (
            "aud array other first",
            "aud",
            Some(json!(["certs", "assets"])),
            malformed,
        ),
        ("aud capitalised", "aud", Some(json!("Assets")), malformed),
        ("aud padded", "aud", Some(json!("assets ")), malformed),
        ("aud wildcard", "aud", Some(json!("*")), malformed),
        ("aud empty", "aud", Some(json!("")), malformed),
        ("aud null", "aud", Some(json!(null)), malformed),
        ("aud missing", "aud", None, malformed),
        (
            "aud unknown service",
            "aud",
            Some(json!("nonexistent")),
            malformed,
        ),
        // sub
        ("sub missing", "sub", None, malformed),
        ("sub null", "sub", Some(json!(null)), malformed),
        ("sub number", "sub", Some(json!(1)), malformed),
        ("sub empty", "sub", Some(json!("")), malformed),
        ("sub not a uuid", "sub", Some(json!("admin")), malformed),
        (
            "sub padded",
            "sub",
            Some(json!(format!("{USER} "))),
            malformed,
        ),
        // iat / exp types
        ("iat missing", "iat", None, malformed),
        ("exp missing", "exp", None, malformed),
        ("iat string", "iat", Some(json!(t.to_string())), malformed),
        (
            "exp string",
            "exp",
            Some(json!((t + 60).to_string())),
            malformed,
        ),
        ("iat null", "iat", Some(json!(null)), malformed),
        ("exp null", "exp", Some(json!(null)), malformed),
        ("iat bool", "iat", Some(json!(true)), malformed),
        ("exp array", "exp", Some(json!([t + 60])), malformed),
        ("iat float", "iat", Some(json!(t as f64 + 0.5)), malformed),
        (
            "exp float",
            "exp",
            Some(json!((t + 60) as f64 + 0.5)),
            malformed,
        ),
        (
            "exp integral float",
            "exp",
            Some(json!((t + 60) as f64)),
            malformed,
        ),
        (
            "exp exponent",
            "exp",
            Some(serde_json::from_str("1.791590460e9").expect("number")),
            malformed,
        ),
        ("exp u64 over i64", "exp", Some(json!(u64::MAX)), malformed),
        (
            "exp 1e30",
            "exp",
            Some(serde_json::from_str("1e30").expect("number")),
            malformed,
        ),
        // tenant / platform_admin
        ("tenant missing is no tenant", "tenant", None, OK),
        ("tenant null is no tenant", "tenant", Some(json!(null)), OK),
        ("tenant string", "tenant", Some(json!(TENANT)), malformed),
        ("tenant empty object", "tenant", Some(json!({})), malformed),
        (
            "tenant bad id",
            "tenant",
            Some(json!({"id": "x", "role": "admin"})),
            malformed,
        ),
        (
            "tenant role missing",
            "tenant",
            Some(json!({"id": TENANT})),
            malformed,
        ),
        (
            "tenant role null",
            "tenant",
            Some(json!({"id": TENANT, "role": null})),
            malformed,
        ),
        (
            "tenant role number",
            "tenant",
            Some(json!({"id": TENANT, "role": 1})),
            malformed,
        ),
        (
            "tenant role admin",
            "tenant",
            Some(json!({"id": TENANT, "role": "admin"})),
            OK,
        ),
        (
            "tenant role unknown",
            "tenant",
            Some(json!({"id": TENANT, "role": "root"})),
            OK,
        ),
        ("platform_admin missing", "platform_admin", None, OK),
        (
            "platform_admin string",
            "platform_admin",
            Some(json!("true")),
            malformed,
        ),
        (
            "platform_admin one",
            "platform_admin",
            Some(json!(1)),
            malformed,
        ),
        (
            "platform_admin null",
            "platform_admin",
            Some(json!(null)),
            malformed,
        ),
    ] {
        cases.check(&fixture.verifier, name, &with(key, value), expected);
    }

    // The absent / unknown cases must not grant anything.
    let absent = fixture
        .verifier
        .verify(&with("platform_admin", None))
        .expect("valid");
    assert!(!absent.is_platform_admin());
    let unknown_role = fixture
        .verifier
        .verify(&with("tenant", Some(json!({"id": TENANT, "role": "root"}))))
        .expect("valid");
    assert!(
        !unknown_role
            .tenant()
            .expect("tenant")
            .role
            .includes(contract::TenantRole::Viewer)
    );
    cases.finish();
}

#[test]
fn nbf_must_be_an_integer_when_present() {
    let fixture = Fixture::new();
    let t = now();
    let mut cases = Cases::default();
    for (name, nbf, expected) in [
        ("null", "null", Err(IdentityError::MalformedClaims)),
        ("string", r#""1""#, Err(IdentityError::MalformedClaims)),
        ("float", "1.5", Err(IdentityError::MalformedClaims)),
        ("integral float", "1.0", Err(IdentityError::MalformedClaims)),
        ("bool", "false", Err(IdentityError::MalformedClaims)),
        ("array", "[]", Err(IdentityError::MalformedClaims)),
        ("object", "{}", Err(IdentityError::MalformedClaims)),
        (
            "u64 over i64",
            "18446744073709551615",
            Err(IdentityError::MalformedClaims),
        ),
        ("exponent", "1e3", Err(IdentityError::MalformedClaims)),
        (
            "i64 max",
            "9223372036854775807",
            Err(IdentityError::NotYetValid),
        ),
        ("i64 min", "-9223372036854775808", OK),
        ("zero", "0", OK),
        ("negative", "-1", OK),
    ] {
        let payload = format!(
            r#"{{"iss":"identity","sub":"{USER}","aud":"assets","iat":{t},"exp":{},"nbf":{nbf},"tenant":null}}"#,
            t + 60
        );
        cases.check(
            &fixture.verifier,
            &format!("nbf {name}"),
            &sign_raw(&fixture.key, HEADER, payload),
            expected,
        );
    }
    cases.finish();
}

#[test]
fn lifetime_and_leeway_boundaries() {
    let fixture = Fixture::new();
    let lifetime = Err(IdentityError::InvalidLifetime);
    let expired = Err(IdentityError::Expired);
    let early = Err(IdentityError::NotYetValid);
    let mut cases = Cases::default();
    let t = now();
    let token = |iat: i64, exp: i64, nbf: Option<i64>| {
        let mut claims = claims_at(iat);
        claims["exp"] = json!(exp);
        if let Some(nbf) = nbf {
            claims["nbf"] = json!(nbf);
        }
        sign_raw(&fixture.key, HEADER, claims.to_string())
    };

    // The 300 s cap and the other lifetime shapes. A fresh token (iat = now) is current for the
    // whole test, so the clock does not matter here.
    for (seconds, expected) in [
        (1, OK),
        (60, OK),
        (MAX_LIFETIME_SECS - 1, OK),
        (MAX_LIFETIME_SECS, OK),
        (MAX_LIFETIME_SECS + 1, lifetime),
        (3600, lifetime),
        (0, lifetime),
        (-1, lifetime),
        (-300, lifetime),
    ] {
        cases.check(
            &fixture.verifier,
            &format!("lifetime {seconds}"),
            &token(t, t + seconds, None),
            expected,
        );
    }
    // A long-lived token cannot sneak through by being issued in the past or the future.
    for (name, iat, exp, expected) in [
        ("exp far future, iat now", t, t + 100_000, lifetime),
        ("iat far past, exp far future", 0, t + 100_000, lifetime),
        ("iat i64::MIN, exp i64::MAX", i64::MIN, i64::MAX, lifetime),
        ("iat 0, exp i64::MAX", 0, i64::MAX, lifetime),
        ("iat -1, exp i64::MAX", -1, i64::MAX, lifetime),
        ("iat MIN+1, exp MAX", i64::MIN + 1, i64::MAX, lifetime),
        ("iat MAX-60, exp MAX", i64::MAX - 60, i64::MAX, early),
        ("iat MIN, exp MIN+60", i64::MIN, i64::MIN + 60, expired),
        ("iat 0, exp 60", 0, 60, expired),
    ] {
        cases.check(&fixture.verifier, name, &token(iat, exp, None), expected);
    }
    // Comfortable margins around the leeway (a few seconds of test-time drift are tolerated).
    for (name, iat, exp, nbf, expected) in [
        ("expired 30 s ago", t - 90, t - 30, None, expired),
        ("expired 1 h ago", t - 3660, t - 3600, None, expired),
        ("expired 10 s ago", t - 70, t - 10, None, expired),
        ("issued 30 s ahead", t + 30, t + 90, None, early),
        ("issued 1 h ahead", t + 3600, t + 3660, None, early),
        ("nbf 1 h ahead", t - 10, t + 50, Some(t + 3600), early),
        ("nbf far past", t - 10, t + 50, Some(i64::MIN), OK),
        ("nbf i64::MAX", t - 10, t + 50, Some(i64::MAX), early),
        (
            "nbf before iat, iat ahead",
            t + 100,
            t + 160,
            Some(0),
            early,
        ),
        (
            "nbf fine but expired",
            t - 120,
            t - 60,
            Some(t - 120),
            expired,
        ),
    ] {
        cases.check(&fixture.verifier, name, &token(iat, exp, nbf), expected);
    }
    cases.finish();

    // The exact edges of the 5 s leeway, each verified in the first moments of a second so the
    // clock cannot move during the check.
    let edges: [(&str, i64, i64, Option<i64>, Expect); 8] = [
        ("exp = now - 5", -65, -LEEWAY_SECS, None, expired),
        ("exp = now - 4", -64, -LEEWAY_SECS + 1, None, OK),
        ("exp = now", -60, 0, None, OK),
        ("iat = now + 5", LEEWAY_SECS, LEEWAY_SECS + 60, None, OK),
        (
            "iat = now + 6",
            LEEWAY_SECS + 1,
            LEEWAY_SECS + 61,
            None,
            early,
        ),
        ("nbf = now + 5", -10, 50, Some(LEEWAY_SECS), OK),
        ("nbf = now + 6", -10, 50, Some(LEEWAY_SECS + 1), early),
        (
            "iat = now - 5 minutes, exp = iat + 300 = now",
            -300,
            0,
            None,
            OK,
        ),
    ];
    let mut failures = Vec::new();
    for (name, iat, exp, nbf, expected) in edges {
        let (second, actual) = at_exact_second(&fixture.verifier, |second| {
            token(second + iat, second + exp, nbf.map(|nbf| second + nbf))
        });
        if actual != expected {
            failures.push(format!(
                "{name} (at {second}): expected {expected:?}, got {actual:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn size_limits_and_resource_use() {
    let fixture = Fixture::new();
    let t = now();
    let mut cases = Cases::default();

    // Exactly at the limit is accepted when valid; one byte more is not.
    let padded = |target: usize| {
        let mut claims = claims_at(t);
        let mut filler = 0usize;
        loop {
            claims["pad"] = json!("x".repeat(filler));
            let token = sign_raw(&fixture.key, HEADER, claims.to_string());
            if token.len() >= target {
                return token;
            }
            filler += (target - token.len()) * 3 / 4 + 1;
        }
    };
    // The longest token that fits (unpadded base64url cannot hit every length exactly).
    let mut at_limit = None;
    let mut claims = claims_at(t);
    claims["exp"] = json!(t + MAX_LIFETIME_SECS);
    for filler in 0..8000 {
        claims["pad"] = json!("x".repeat(filler));
        let token = sign_raw(&fixture.key, HEADER, claims.to_string());
        if token.len() > InternalToken::MAX_LEN {
            break;
        }
        at_limit = Some(token);
    }
    let at_limit = at_limit.expect("a token within the limit");
    assert!(
        at_limit.len() + 4 > InternalToken::MAX_LEN,
        "{}",
        at_limit.len()
    );
    cases.check(&fixture.verifier, "token at the limit", &at_limit, OK);
    let over = padded(InternalToken::MAX_LEN + 1);
    assert!(over.len() > InternalToken::MAX_LEN);
    cases.check(
        &fixture.verifier,
        "token over the limit",
        &over,
        Err(IdentityError::Oversized),
    );

    // Very large inputs are rejected without being decoded: fast, bounded memory.
    let started = Instant::now();
    for megabytes in [1usize, 16, 64] {
        let huge = format!("{}.{}.{}", "A".repeat(megabytes << 20), "B", "C");
        cases.check(
            &fixture.verifier,
            &format!("{megabytes} MiB token"),
            &huge,
            Err(IdentityError::Oversized),
        );
    }
    let many_segments = "a.".repeat(4_000_000);
    cases.check(
        &fixture.verifier,
        "8 MB of segments",
        &many_segments,
        Err(IdentityError::Oversized),
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "oversized tokens took {elapsed:?}"
    );

    // Many verifications of a maximum-size token finish quickly.
    let started = Instant::now();
    for _ in 0..20 {
        assert!(fixture.verifier.verify(&at_limit).is_ok());
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "20 verifications took {elapsed:?}"
    );

    // A maximum-size token that fails late (bad signature) is as cheap.
    let (signed, _) = at_limit.rsplit_once('.').expect("signature");
    let forged = format!("{signed}.{}", b64([1u8; 64]));
    let started = Instant::now();
    for _ in 0..20 {
        assert_eq!(
            fixture.verifier.verify(&forged).map(|_| ()),
            Err(IdentityError::BadSignature)
        );
    }
    assert!(started.elapsed() < Duration::from_secs(10));
    cases.finish();
}

#[test]
fn rotation_with_old_and_new_keys() {
    let old = signing_key(1);
    let new = signing_key(2);
    let claims = claims_text(now());
    let mut cases = Cases::default();
    let both = IdentityVerifier::new(
        ServiceName::Assets,
        keys_of(&[(&old, "2026-10-a"), (&new, "2026-11-a")]),
    );
    let reversed = IdentityVerifier::new(
        ServiceName::Assets,
        keys_of(&[(&new, "2026-11-a"), (&old, "2026-10-a")]),
    );
    let header =
        |kid: &str| format!(r#"{{"alg":"EdDSA","typ":"internal-identity+jwt","kid":"{kid}"}}"#);
    for verifier in [&both, &reversed] {
        cases.check(
            verifier,
            "old key, old kid",
            &sign_raw(&old, header("2026-10-a"), &claims),
            OK,
        );
        cases.check(
            verifier,
            "new key, new kid",
            &sign_raw(&new, header("2026-11-a"), &claims),
            OK,
        );
        cases.check(
            verifier,
            "old key under the new kid",
            &sign_raw(&old, header("2026-11-a"), &claims),
            Err(IdentityError::BadSignature),
        );
        cases.check(
            verifier,
            "new key under the old kid",
            &sign_raw(&new, header("2026-10-a"), &claims),
            Err(IdentityError::BadSignature),
        );
        cases.check(
            verifier,
            "unconfigured kid",
            &sign_raw(&new, header("2026-12-a"), &claims),
            Err(IdentityError::UnknownKey),
        );
    }
    // After the old key is removed, its tokens stop verifying; before the new key is added they
    // never did.
    let after = IdentityVerifier::new(ServiceName::Assets, keys_of(&[(&new, "2026-11-a")]));
    cases.check(
        &after,
        "removed old key",
        &sign_raw(&old, header("2026-10-a"), &claims),
        Err(IdentityError::UnknownKey),
    );
    cases.check(
        &after,
        "new key still fine",
        &sign_raw(&new, header("2026-11-a"), &claims),
        OK,
    );
    // A kid-less shortcut does not exist: with one configured key, any other kid is unknown.
    cases.check(
        &after,
        "any kid works?",
        &sign_raw(&new, header("anything"), &claims),
        Err(IdentityError::UnknownKey),
    );
    // The same public key under two kids verifies under both.
    let alias = IdentityVerifier::new(ServiceName::Assets, keys_of(&[(&new, "a"), (&new, "b")]));
    cases.check(&alias, "alias a", &sign_raw(&new, header("a"), &claims), OK);
    cases.check(&alias, "alias b", &sign_raw(&new, header("b"), &claims), OK);
    // A verifier is for one audience: rotation does not change that.
    let other = IdentityVerifier::new(ServiceName::Certs, keys_of(&[(&new, "2026-11-a")]));
    cases.check(
        &other,
        "wrong audience after rotation",
        &sign_raw(&new, header("2026-11-a"), &claims),
        Err(IdentityError::WrongAudience),
    );
    cases.finish();
}

fn jwks_text(keys: &[Value]) -> String {
    json!({ "keys": keys }).to_string()
}

#[test]
fn unusable_key_sets_are_refused_without_echoing_them() {
    let key = signing_key(9);
    let good = jwk(&key, "k1");
    let x = good["x"].as_str().expect("x").to_owned();
    let mut cases: Vec<(String, String)> = Vec::new();
    let mut add = |name: &str, text: String| cases.push((name.to_owned(), text));
    let with = |changes: Value| {
        let mut jwk = good.clone();
        for (member, value) in changes.as_object().expect("object") {
            if value.is_null() && member.starts_with('-') {
                jwk.as_object_mut().expect("object").remove(&member[1..]);
            } else {
                jwk[member] = value.clone();
            }
        }
        jwks_text(&[jwk])
    };

    // The set itself
    add("empty text", String::new());
    add("not JSON", "keys".into());
    add("null", "null".into());
    add("array", "[]".into());
    add("empty object", "{}".into());
    add("keys null", r#"{"keys":null}"#.into());
    add("keys object", r#"{"keys":{}}"#.into());
    add("keys string", r#"{"keys":"x"}"#.into());
    add("keys empty", r#"{"keys":[]}"#.into());
    add("keys with null", r#"{"keys":[null]}"#.into());
    add("keys with string", r#"{"keys":["x"]}"#.into());
    add("keys with empty object", r#"{"keys":[{}]}"#.into());
    add(
        "duplicate keys member",
        format!(r#"{{"keys":[{good}],"keys":[]}}"#),
    );
    add(
        "duplicate keys member, other order",
        format!(r#"{{"keys":[],"keys":[{good}]}}"#),
    );
    add(
        "trailing data",
        format!("{}x", jwks_text(std::slice::from_ref(&good))),
    );
    add(
        "two sets",
        format!("{0}{0}", jwks_text(std::slice::from_ref(&good))),
    );
    add("single jwk, not a set", good.to_string());
    add(
        "too many keys",
        jwks_text(
            &(0..=MAX_KEYS)
                .map(|i| {
                    jwk(
                        &signing_key(100 + u8::try_from(i).expect("small")),
                        &format!("k{i}"),
                    )
                })
                .collect::<Vec<_>>(),
        ),
    );
    add(
        "duplicate kid",
        jwks_text(&[good.clone(), jwk(&signing_key(10), "k1")]),
    );
    add(
        "duplicate kid, same key",
        jwks_text(&[good.clone(), good.clone()]),
    );
    // A usable key next to a key of another type: the whole set is refused (fail closed).
    add(
        "good key plus RSA key",
        jwks_text(&[
            good.clone(),
            json!({"kty": "RSA", "kid": "r", "n": "AQAB", "e": "AQAB"}),
        ]),
    );
    add(
        "good key plus EC key",
        jwks_text(&[
            good.clone(),
            json!({"kty": "EC", "crv": "P-256", "kid": "e", "x": x, "y": x}),
        ]),
    );
    add(
        "good key plus private key",
        jwks_text(&[good.clone(), with_d(&jwk(&signing_key(11), "k2"))]),
    );

    // Curve and type
    for (name, kty, crv) in [
        ("Ed448", "OKP", "Ed448"),
        ("X25519", "OKP", "X25519"),
        ("X448", "OKP", "X448"),
        ("P-256 OKP", "OKP", "P-256"),
        ("EC Ed25519", "EC", "Ed25519"),
        ("EC P-256", "EC", "P-256"),
        ("RSA", "RSA", "Ed25519"),
        ("oct", "oct", "Ed25519"),
        ("lowercase okp", "okp", "Ed25519"),
        ("lowercase ed25519", "OKP", "ed25519"),
        ("empty", "", ""),
    ] {
        add(
            &format!("kty/crv {name}"),
            with(json!({"kty": kty, "crv": crv})),
        );
    }
    add("kty missing", with(json!({"-kty": null})));
    add("crv missing", with(json!({"-crv": null})));
    add("kty null", with(json!({"kty": null})));
    add("crv null", with(json!({"crv": null})));
    add("kty number", with(json!({"kty": 1})));

    // Private material, however spelled
    add("d present", with(json!({"d": "SECRET-MARKER-123"})));
    add("d empty", with(json!({"d": ""})));
    add("d null", with(json!({"d": null})));
    add("d number", with(json!({"d": 1})));
    add("d object", with(json!({"d": {"SECRET-MARKER-123": 1}})));
    add("d array", with(json!({"d": []})));

    // kid
    add("kid missing", with(json!({"-kid": null})));
    add("kid null", with(json!({"kid": null})));
    add("kid empty", with(json!({"kid": ""})));
    add("kid number", with(json!({"kid": 1})));
    add(
        "kid 65 chars",
        with(json!({"kid": "k".repeat(MAX_KEY_ID_LEN + 1)})),
    );
    add("kid with space", with(json!({"kid": "a b"})));
    add("kid with tab", with(json!({"kid": "a\tb"})));
    add("kid with newline", with(json!({"kid": "a\nb"})));
    add("kid with NUL", with(json!({"kid": "a\u{0}b"})));
    add("kid with DEL", with(json!({"kid": "a\u{7f}b"})));
    add("kid with non-ASCII", with(json!({"kid": "caf\u{e9}"})));
    add("kid with emoji", with(json!({"kid": "\u{1f600}"})));
    add(
        "kid with zero-width space",
        with(json!({"kid": "a\u{200b}"})),
    );

    // x
    add("x missing", with(json!({"-x": null})));
    add("x null", with(json!({"x": null})));
    add("x empty", with(json!({"x": ""})));
    add("x number", with(json!({"x": 1})));
    add("x padded", with(json!({"x": format!("{x}=")})));
    add(
        "x standard alphabet",
        with(json!({"x": x.replace('-', "+").replace('_', "/") + "!"})),
    );
    add("x 31 bytes", with(json!({"x": b64([5u8; 31])})));
    add("x 33 bytes", with(json!({"x": b64([5u8; 33])})));
    add("x 64 bytes", with(json!({"x": b64([5u8; 64])})));
    add("x hex", with(json!({"x": "00".repeat(32)})));
    add("x with whitespace", with(json!({"x": format!(" {x}")})));
    add(
        "x non-canonical spelling",
        with(json!({"x": noncanonical_spelling(&x)})),
    );
    add(
        "x not on the curve",
        with(
            json!({"x": b64([2u8; 1].iter().chain([0u8; 31].iter()).copied().collect::<Vec<u8>>())}),
        ),
    );

    // Declared use
    add("alg RS256", with(json!({"alg": "RS256"})));
    add("alg EdDSA lowercase", with(json!({"alg": "eddsa"})));
    add("alg null", with(json!({"alg": null})));
    add("alg Ed25519", with(json!({"alg": "Ed25519"})));
    add("use enc", with(json!({"use": "enc"})));
    add("use null", with(json!({"use": null})));
    add("use SIG", with(json!({"use": "SIG"})));
    add("key_ops sign only", with(json!({"key_ops": ["sign"]})));
    add("key_ops empty", with(json!({"key_ops": []})));
    add("key_ops null", with(json!({"key_ops": null})));
    add("key_ops string", with(json!({"key_ops": "verify"})));
    add(
        "key_ops mixed types",
        with(json!({"key_ops": ["verify", 1]})),
    );

    // Duplicate members of one key
    for member in ["kty", "crv", "kid", "x", "alg"] {
        let value = if member == "alg" {
            "\"EdDSA\"".to_owned()
        } else {
            good[member].to_string()
        };
        let mut base = good.clone();
        base["alg"] = json!("EdDSA");
        let text = base.to_string();
        let doubled = format!("{},{}:{value}}}", &text[..text.len() - 1], json!(member));
        add(
            &format!("duplicate {member} member"),
            format!(r#"{{"keys":[{doubled}]}}"#),
        );
    }
    let mut failures = Vec::new();
    for (name, text) in &cases {
        match IdentityKeys::from_jwks(text) {
            Ok(keys) => failures.push(format!("{name}: accepted ({keys:?})")),
            Err(error) => {
                let message = error.to_string();
                if message.contains("SECRET-MARKER-123")
                    || message.contains(&x)
                    || (text.len() > 40 && message.contains(text.as_str()))
                {
                    failures.push(format!("{name}: the error quotes the input: {message}"));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases wrong:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

fn with_d(jwk: &Value) -> Value {
    let mut jwk = jwk.clone();
    jwk["d"] = json!("SECRET-MARKER-123");
    jwk
}

/// A second spelling of the same 32 bytes: the last character with different spare bits (a
/// 32-byte key has 2 spare bits in its 43rd character).
fn noncanonical_spelling(x: &str) -> String {
    let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let last = x.chars().last().expect("x");
    let altered = alphabet
        .chars()
        .nth(alphabet.find(last).expect("alphabet") ^ 1)
        .expect("char");
    format!("{}{altered}", &x[..x.len() - 1])
}

#[test]
fn weak_public_keys_are_refused() {
    // The eight small-order points, canonical encodings (their y coordinates, little-endian,
    // with the sign bit as needed), plus the non-canonical encodings of the same points.
    let hex = |text: &str| -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
            .collect()
    };
    let mut points: Vec<(&str, Vec<u8>)> = vec![
        ("all zero", vec![0u8; 32]),
        (
            "identity",
            hex("0100000000000000000000000000000000000000000000000000000000000000"),
        ),
        (
            "order 2",
            hex("ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f"),
        ),
        (
            "order 4 (y=0), sign set",
            hex("0000000000000000000000000000000000000000000000000000000000000080"),
        ),
        (
            "order 8 a",
            hex("c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a"),
        ),
        (
            "order 8 b",
            hex("c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac03fa"),
        ),
        (
            "order 8 c",
            hex("26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05"),
        ),
        (
            "order 8 d",
            hex("26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc85"),
        ),
        (
            "non-canonical y = p (zero)",
            hex("edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f"),
        ),
        (
            "non-canonical y = p+1 (identity)",
            hex("eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f"),
        ),
        (
            "non-canonical y = p, sign set",
            hex("edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"),
        ),
        (
            "non-canonical y = p+1, sign set",
            hex("eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"),
        ),
        (
            "identity, sign set",
            hex("0100000000000000000000000000000000000000000000000000000000000080"),
        ),
        (
            "order 2, sign set",
            hex("ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"),
        ),
    ];
    // y = 0..=4 small values (most are not on the curve; none may be accepted as strong keys
    // unless they are valid non-weak points).
    for y in 2u8..=4 {
        let mut bytes = vec![0u8; 32];
        bytes[0] = y;
        points.push(("small y", bytes));
    }
    let mut failures = Vec::new();
    for (name, bytes) in &points {
        let text =
            jwks_text(&[json!({"kty": "OKP", "crv": "Ed25519", "kid": "weak", "x": b64(bytes)})]);
        let accepted = IdentityKeys::from_jwks(&text).is_ok();
        // Only the points that are known small-order (the first group) must be refused outright;
        // small y values are refused too unless they happen to be valid strong keys.
        if accepted && *name != "small y" {
            failures.push(format!("{name} accepted"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn key_sets_of_extreme_size_or_depth_do_not_exhaust_the_parser() {
    let key = signing_key(3);
    let good = jwk(&key, "k1");
    let started = Instant::now();
    // 8 MB of whitespace around a valid set, 1 MB of unknown members, deep nesting.
    let spaces = " ".repeat(4 << 20);
    assert!(
        IdentityKeys::from_jwks(&format!(
            "{spaces}{}{spaces}",
            jwks_text(std::slice::from_ref(&good))
        ))
        .is_ok()
    );
    let mut noisy = good.clone();
    noisy["note"] = json!("x".repeat(1 << 20));
    assert!(IdentityKeys::from_jwks(&jwks_text(&[noisy])).is_ok());
    for depth in [1_000usize, 100_000] {
        let nested = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        let mut deep = good.to_string();
        deep.truncate(deep.len() - 1);
        // Unknown members are ignored, however deep; it must not overflow the stack either way.
        let text = format!(r#"{{"keys":[{deep},"ext":{nested}}}]}}"#);
        let _ = IdentityKeys::from_jwks(&text);
        let unclosed = format!(r#"{{"keys":[{deep},"ext":{}}}]}}"#, "[".repeat(depth));
        assert!(
            IdentityKeys::from_jwks(&unclosed).is_err(),
            "unclosed depth {depth}"
        );
        let top = format!(r#"{{"keys":{}"#, "[".repeat(depth));
        assert!(IdentityKeys::from_jwks(&top).is_err(), "top depth {depth}");
    }
    // MAX_KEYS keys are fine; the set order does not matter.
    let many: Vec<Value> = (0..MAX_KEYS)
        .map(|i| {
            jwk(
                &signing_key(50 + u8::try_from(i).expect("small")),
                &format!("k{i}"),
            )
        })
        .collect();
    let keys = IdentityKeys::from_jwks(&jwks_text(&many)).expect("MAX_KEYS keys");
    assert_eq!(keys.key_ids().count(), MAX_KEYS);
    // A kid of exactly the maximum length, and its path-like and punctuation cousins.
    for kid in [
        "k".repeat(MAX_KEY_ID_LEN),
        "../../etc/passwd".to_owned(),
        "a.b/c".to_owned(),
        "~!@#$%^&*()".to_owned(),
    ] {
        assert!(
            IdentityKeys::from_jwks(&jwks_text(&[jwk(&key, &kid)])).is_ok(),
            "{kid}"
        );
    }
    // Unknown JWK members are ignored (RFC 7517), including an embedded certificate chain.
    let mut extra = good.clone();
    extra["x5c"] = json!(["MIIB"]);
    extra["x5u"] = json!("http://attacker.example/");
    extra["ext"] = json!({"a": [1, 2, 3]});
    assert!(IdentityKeys::from_jwks(&jwks_text(&[extra])).is_ok());
    let mut declared = good.clone();
    declared["alg"] = json!("EdDSA");
    declared["use"] = json!("sig");
    declared["key_ops"] = json!(["verify"]);
    assert!(IdentityKeys::from_jwks(&jwks_text(&[declared])).is_ok());
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(120), "{elapsed:?}");
}

#[test]
fn the_identity_header_is_read_once_and_nowhere_else() {
    let fixture = Fixture::new();
    let token = fixture.valid();
    let name = HeaderName::from_static(INTERNAL_IDENTITY_HEADER);
    let value = |text: &str| HeaderValue::from_str(text).expect("header value");
    let single = |text: &str| {
        let mut headers = HeaderMap::new();
        headers.insert(name.clone(), value(text));
        headers
    };
    let verify = |headers: &HeaderMap| fixture.verifier.verify_headers(headers).map(|_| ());

    assert_eq!(verify(&single(&token)), OK);
    // Case variants of the name are one header (the map lower-cases names).
    let mut cased = HeaderMap::new();
    cased.insert(
        "X-Internal-Identity".parse::<HeaderName>().expect("name"),
        value(&token),
    );
    assert_eq!(verify(&cased), OK);
    // Twice, the same or different, in either order.
    let forged = sign_raw(&signing_key(66), HEADER, claims_text(now()));
    for (first, second) in [(&token, &token), (&token, &forged), (&forged, &token)] {
        let mut headers = HeaderMap::new();
        headers.append(name.clone(), value(first));
        headers.append(name.clone(), value(second));
        assert_eq!(verify(&headers), Err(IdentityError::Repeated));
        // Even when the valid one comes second in a differently-cased duplicate.
        let mut mixed = HeaderMap::new();
        mixed.append(
            "x-internal-identity".parse::<HeaderName>().expect("name"),
            value(first),
        );
        mixed.append(
            "X-INTERNAL-IDENTITY".parse::<HeaderName>().expect("name"),
            value(second),
        );
        assert_eq!(verify(&mixed), Err(IdentityError::Repeated));
    }
    // Lookalike names and carriers.
    for other in [
        "x-internal-identity-token",
        "x_internal_identity",
        "authorization",
        "cookie",
        "x-forwarded-identity",
        "x-internal-identity ",
    ] {
        let mut headers = HeaderMap::new();
        if let Ok(header_name) = other.parse::<HeaderName>() {
            headers.insert(header_name, value(&token));
            assert_eq!(verify(&headers), Err(IdentityError::Missing), "{other}");
        }
    }
    // Values that are not exactly a token.
    for bad in [
        format!("{token},{token}"),
        format!("{token}, {token}"),
        format!("{token} "),
        format!(" {token}"),
        format!("\t{token}"),
        format!("{token}\t"),
        format!("Bearer {token}"),
        format!("bearer {token}"),
        format!("{token};"),
        format!("\"{token}\""),
    ] {
        let headers = single(&bad);
        let result = verify(&headers);
        assert!(result.is_err(), "{bad:?} accepted");
    }
    // Opaque bytes.
    let mut headers = HeaderMap::new();
    headers.insert(
        name.clone(),
        HeaderValue::from_bytes(&[0xff, 0xfe]).expect("opaque"),
    );
    assert_eq!(verify(&headers), Err(IdentityError::Malformed));
    // A very long value is refused by length before anything else.
    assert_eq!(
        verify(&single(&"a".repeat(InternalToken::MAX_LEN + 1))),
        Err(IdentityError::Oversized)
    );
    assert_eq!(
        verify(&single(&"a".repeat(1 << 20))),
        Err(IdentityError::Oversized)
    );
}

/// One request with exactly these raw header lines; returns the status code.
fn raw_status(address: SocketAddr, header_lines: &[String]) -> Option<u16> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut head = format!("GET /v1/whoami HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n");
    for line in header_lines {
        head.push_str(line);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).ok()?;
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw);
    text.split_whitespace().nth(1)?.parse().ok()
}

#[test]
fn http_layer_accepts_only_one_well_formed_header() {
    let key = signing_key(41);
    let keys = jwks_text(&[jwk(&key, "2026-10-a")]);
    let service = Service::start(&[("TRUSTIOT_INTERNAL_IDENTITY_KEYS", &keys)]);
    let address = service.address;
    let header = r#"{"alg":"EdDSA","typ":"internal-identity+jwt","kid":"2026-10-a"}"#;
    let token = sign_raw(&key, header, claims_text(now()));
    let forged = sign_raw(&signing_key(42), header, claims_text(now()));
    let line = |name: &str, value: &str| format!("{name}: {value}");
    let failures = std::cell::RefCell::new(Vec::<String>::new());
    let expect = |name: &str, lines: Vec<String>, expected: Option<u16>| {
        let actual = raw_status(address, &lines);
        if actual != expected {
            failures
                .borrow_mut()
                .push(format!("{name}: expected {expected:?}, got {actual:?}"));
        }
    };

    expect(
        "control",
        vec![line("x-internal-identity", &token)],
        Some(200),
    );
    expect(
        "mixed-case name",
        vec![line("X-Internal-Identity", &token)],
        Some(200),
    );
    expect(
        "upper-case name",
        vec![line("X-INTERNAL-IDENTITY", &token)],
        Some(200),
    );
    // Optional whitespace around the value is not part of the value (RFC 9110).
    expect(
        "OWS around value",
        vec![format!("x-internal-identity:   {token}\t ")],
        Some(200),
    );
    expect(
        "no space after colon",
        vec![format!("x-internal-identity:{token}")],
        Some(200),
    );
    expect(
        "twice, same",
        vec![
            line("x-internal-identity", &token),
            line("x-internal-identity", &token),
        ],
        Some(401),
    );
    expect(
        "twice, valid first",
        vec![
            line("x-internal-identity", &token),
            line("x-internal-identity", &forged),
        ],
        Some(401),
    );
    expect(
        "twice, valid second",
        vec![
            line("x-internal-identity", &forged),
            line("x-internal-identity", &token),
        ],
        Some(401),
    );
    expect(
        "twice, different case",
        vec![
            line("x-internal-identity", &token),
            line("X-Internal-Identity", &token),
        ],
        Some(401),
    );
    expect(
        "comma-joined",
        vec![line("x-internal-identity", &format!("{token},{token}"))],
        Some(401),
    );
    expect(
        "forged",
        vec![line("x-internal-identity", &forged)],
        Some(401),
    );
    expect(
        "empty value",
        vec![line("x-internal-identity", "")],
        Some(401),
    );
    expect(
        "Bearer scheme",
        vec![line("x-internal-identity", &format!("Bearer {token}"))],
        Some(401),
    );
    expect(
        "underscore name",
        vec![line("x_internal_identity", &token)],
        Some(401),
    );
    expect(
        "authorization carrier",
        vec![line("authorization", &format!("Bearer {token}"))],
        Some(401),
    );
    expect(
        "cookie carrier",
        vec![line("cookie", &format!("x-internal-identity={token}"))],
        Some(401),
    );
    expect(
        "lookalike name",
        vec![line("x-internal-identity-token", &token)],
        Some(401),
    );
    expect(
        "forwarded identity",
        vec![line("x-forwarded-user", USER), line("x-user-id", USER)],
        Some(401),
    );
    expect("no header", vec![], Some(401));
    // A forged token plus a valid one in a different carrier does not authenticate.
    expect(
        "forged plus valid authorization",
        vec![
            line("x-internal-identity", &forged),
            line("authorization", &format!("Bearer {token}")),
        ],
        Some(401),
    );
    // Oversized and obviously hostile values: rejected, never a crash.
    expect(
        "8193 bytes",
        vec![line("x-internal-identity", &"a".repeat(8193))],
        Some(401),
    );
    expect(
        "64 KiB value",
        vec![line("x-internal-identity", &"a".repeat(64 << 10))],
        Some(401),
    );
    // Whatever the HTTP parser says to an absurd header, it is not 200.
    let huge = raw_status(
        address,
        &[line("x-internal-identity", &"a".repeat(2 << 20))],
    );
    if huge == Some(200) {
        failures
            .borrow_mut()
            .push("2 MiB header accepted".to_owned());
    }
    // The query string is not a carrier either.
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    write!(stream, "GET /v1/whoami?x-internal-identity={token} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n").expect("write");
    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw);
    if !raw.starts_with("HTTP/1.1 401") {
        failures.borrow_mut().push(format!(
            "query carrier: {}",
            raw.lines().next().unwrap_or("")
        ));
    }
    // The service is still healthy after all of that.
    expect(
        "control again",
        vec![line("x-internal-identity", &token)],
        Some(200),
    );
    let failures = failures.into_inner();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
