//! Independent attacks on the tokens the identity service signs: every way to alter, re-sign,
//! re-address or time-shift one must fail `crates/service-kit`'s verifier (Laysi/trustiot-v2#52),
//! and the rotation window the runbook promises must hold. In-process: no database or container.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use contract::{ServiceName, TenantGrant, TenantRole};
use ed25519_dalek::{Signer, SigningKey};
use identity::issuer::{Issuer, TOKEN_LIFETIME_SECS, unix_now};
use identity::session::SessionGrant;
use identity_keys::SigningKeys;
use serde_json::{Value, json};
use service_kit::identity::{IdentityError, IdentityKeys, IdentityVerifier};

const USER: &str = "01928f6e-7a3c-7d4e-8f00-0123456789ab";
const TENANT: &str = "01928f6e-7a3c-7d4e-8f00-00000000000a";

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn keys(entries: &[(u8, &str)]) -> IdentityKeys {
    let keys: Vec<Value> = entries
        .iter()
        .map(|(seed, kid)| {
            json!({"kty": "OKP", "crv": "Ed25519", "kid": kid,
                   "x": URL_SAFE_NO_PAD.encode(key(*seed).verifying_key().as_bytes())})
        })
        .collect();
    IdentityKeys::from_jwks(&json!({ "keys": keys }).to_string()).expect("valid key set")
}

/// An issuer whose signing keys file holds only the key of `seed`, under the key id `entries`
/// publishes it with, signing from the start; `entries` are the published public keys.
fn issuer(seed: u8, entries: &[(u8, &str)]) -> Issuer {
    let kid = entries
        .iter()
        .find(|(candidate, _)| *candidate == seed)
        .map(|(_, kid)| *kid)
        .expect("the key is published");
    let document = json!({"keys": [{
        "kid": kid, "generation": 1,
        "x": URL_SAFE_NO_PAD.encode(key(seed).verifying_key().as_bytes()),
        "d": URL_SAFE_NO_PAD.encode(key(seed).to_bytes()),
        "published_at": 0, "signs_from": 0
    }]});
    let signing = SigningKeys::parse(&document.to_string()).expect("valid signing keys");
    Issuer::with_keys(signing, keys(entries).into(), 0)
}

fn grant() -> SessionGrant {
    SessionGrant {
        user: USER.parse().expect("user"),
        tenant: Some(TenantGrant {
            id: TENANT.parse().expect("tenant"),
            role: TenantRole::Viewer,
        }),
        platform_admin: false,
    }
}

fn b64(value: &Value) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).expect("json"))
}

/// `<header>.<payload>.<signature>` made by hand with `signer`.
fn forge(header: &Value, payload: &Value, signer: &SigningKey) -> String {
    let signed = format!("{}.{}", b64(header), b64(payload));
    let signature = signer.sign(signed.as_bytes());
    format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
}

fn header(kid: &str) -> Value {
    json!({"alg": "EdDSA", "typ": "internal-identity+jwt", "kid": kid})
}

fn claims(now: i64) -> Value {
    json!({"iss": "identity", "sub": USER, "aud": "assets", "iat": now, "exp": now + 60,
           "tenant": {"id": TENANT, "role": "viewer"}, "platform_admin": false})
}

fn verify(token: &str) -> Result<(), IdentityError> {
    IdentityVerifier::new(ServiceName::Assets, keys(&[(1, "k1"), (2, "k2")]))
        .verify(token)
        .map(|_| ())
}

fn parts(token: &str) -> Vec<String> {
    token.split('.').map(str::to_owned).collect()
}

#[test]
fn the_baseline_forgery_helper_produces_an_accepted_token() {
    // Guards every rejection below: the helper's tokens are valid until one thing changes.
    let now = unix_now();
    assert_eq!(verify(&forge(&header("k1"), &claims(now), &key(1))), Ok(()));
    assert_eq!(verify(&forge(&header("k2"), &claims(now), &key(2))), Ok(()));
}

#[test]
fn a_token_signed_with_another_key_or_under_another_kid_is_rejected() {
    let now = unix_now();
    // Unknown signer under a known kid, and under an unknown kid.
    assert_eq!(
        verify(&forge(&header("k1"), &claims(now), &key(9))),
        Err(IdentityError::BadSignature)
    );
    assert_eq!(
        verify(&forge(&header("k9"), &claims(now), &key(9))),
        Err(IdentityError::UnknownKey)
    );
    // A valid key of the set, but named under the other key's kid.
    assert_eq!(
        verify(&forge(&header("k2"), &claims(now), &key(1))),
        Err(IdentityError::BadSignature)
    );
    // No fallback: kid case, whitespace and an empty kid are other names.
    for kid in ["K1", "k1 ", " k1", "", "k1\0"] {
        assert_eq!(
            verify(&forge(&header(kid), &claims(now), &key(1))),
            Err(IdentityError::UnknownKey),
            "kid {kid:?}"
        );
    }
}

#[test]
fn algorithm_confusion_is_rejected_before_any_key_is_used() {
    let now = unix_now();
    let payload = claims(now);
    for alg in [
        "none", "None", "NONE", "HS256", "HS512", "ES256", "RS256", "PS256", "eddsa", "Ed25519",
        "EdDSA ", "", "ES256K",
    ] {
        let mut head = header("k1");
        head["alg"] = json!(alg);
        assert_eq!(
            verify(&forge(&head, &payload, &key(1))),
            Err(IdentityError::UnsupportedAlgorithm),
            "alg {alg:?}"
        );
        // The unsigned form: an empty signature segment.
        let unsigned = format!("{}.{}.", b64(&head), b64(&payload));
        assert!(verify(&unsigned).is_err(), "unsigned alg {alg:?}");
    }
    // `alg` missing, null, an array, a number.
    for head in [
        json!({"typ": "internal-identity+jwt", "kid": "k1"}),
        json!({"alg": null, "typ": "internal-identity+jwt", "kid": "k1"}),
        json!({"alg": ["EdDSA"], "typ": "internal-identity+jwt", "kid": "k1"}),
        json!({"alg": 1, "typ": "internal-identity+jwt", "kid": "k1"}),
    ] {
        assert_eq!(
            verify(&forge(&head, &payload, &key(1))),
            Err(IdentityError::MalformedHeader),
            "{head}"
        );
    }
    // HMAC "signature" keyed with the public key bytes, the classic confusion.
    let public = key(1).verifying_key().to_bytes();
    let mut head = header("k1");
    head["alg"] = json!("HS256");
    let signing_input = format!("{}.{}", b64(&head), b64(&payload));
    // Not a real HMAC (no hmac crate here); any 32-byte value must fail on the algorithm alone.
    let fake_mac = URL_SAFE_NO_PAD.encode(public);
    assert_eq!(
        verify(&format!("{signing_input}.{fake_mac}")),
        Err(IdentityError::UnsupportedAlgorithm)
    );
}

#[test]
fn header_extras_and_type_confusion_are_rejected() {
    let now = unix_now();
    let payload = claims(now);
    for extra in [
        ("jwk", json!({"kty": "OKP", "crv": "Ed25519", "x": "AAAA"})),
        ("jku", json!("https://attacker.example/keys")),
        ("x5u", json!("https://attacker.example/cert")),
        ("crit", json!(["exp"])),
        ("b64", json!(false)),
        ("cty", json!("JWT")),
        ("zip", json!("DEF")),
    ] {
        let mut head = header("k1");
        head[extra.0] = extra.1;
        assert_eq!(
            verify(&forge(&head, &payload, &key(1))),
            Err(IdentityError::MalformedHeader),
            "extra header {}",
            extra.0
        );
    }
    for typ in ["JWT", "at+jwt", "internal-identity", "jwt", ""] {
        let mut head = header("k1");
        head["typ"] = json!(typ);
        assert_eq!(
            verify(&forge(&head, &payload, &key(1))),
            Err(IdentityError::WrongType),
            "typ {typ:?}"
        );
    }
    // A header that repeats `alg`: serde rejects the duplicate.
    let duplicate = r#"{"alg":"none","alg":"EdDSA","typ":"internal-identity+jwt","kid":"k1"}"#;
    let signed = format!("{}.{}", URL_SAFE_NO_PAD.encode(duplicate), b64(&payload));
    let signature = URL_SAFE_NO_PAD.encode(key(1).sign(signed.as_bytes()).to_bytes());
    assert_eq!(
        verify(&format!("{signed}.{signature}")),
        Err(IdentityError::MalformedHeader)
    );
}

#[test]
fn payload_or_header_tampering_after_signing_is_rejected() {
    let now = unix_now();
    let real = issuer(1, &[(1, "k1")])
        .issue(&grant(), ServiceName::Assets, now)
        .expect("token");
    assert_eq!(verify(real.expose()), Ok(()));
    let original = parts(real.expose());

    // Change one claim at a time, keep the signature.
    let payload: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&original[1]).expect("b64")).expect("json");
    for (name, value) in [
        ("aud", json!("certs")),
        ("sub", json!("01928f6e-7a3c-7d4e-8f00-0000000000ff")),
        ("exp", json!(now + 3600)),
        ("platform_admin", json!(true)),
        ("tenant", json!({"id": TENANT, "role": "admin"})),
        ("iss", json!("assets")),
    ] {
        let mut altered = payload.clone();
        altered[name] = value;
        let token = format!("{}.{}.{}", original[0], b64(&altered), original[2]);
        assert_eq!(
            verify(&token),
            Err(IdentityError::BadSignature),
            "altered {name}"
        );
    }
    // Header changed (kid to the other valid key), signature kept.
    let token = format!("{}.{}.{}", b64(&header("k2")), original[1], original[2]);
    assert_eq!(verify(&token), Err(IdentityError::BadSignature));

    // Signature bit flips, truncation, extension, padding, empty segments, extra segments.
    let mut signature = URL_SAFE_NO_PAD.decode(&original[2]).expect("b64");
    for index in [0, 31, 32, 63] {
        let mut flipped = signature.clone();
        flipped[index] ^= 0x01;
        let token = format!(
            "{}.{}.{}",
            original[0],
            original[1],
            URL_SAFE_NO_PAD.encode(&flipped)
        );
        assert!(verify(&token).is_err(), "flip at {index}");
    }
    // Non-canonical S (S + L, the group order): strict verification must refuse it.
    let order: [u8; 32] = [
        0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde,
        0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
    ];
    let mut carry = 0_u16;
    for (byte, add) in signature[32..].iter_mut().zip(order) {
        let sum = u16::from(*byte) + u16::from(add) + carry;
        *byte = (sum & 0xff) as u8;
        carry = sum >> 8;
    }
    let malleated = format!(
        "{}.{}.{}",
        original[0],
        original[1],
        URL_SAFE_NO_PAD.encode(&signature)
    );
    assert!(verify(&malleated).is_err(), "S + L is accepted");
    signature.truncate(63);
    for token in [
        format!(
            "{}.{}.{}",
            original[0],
            original[1],
            URL_SAFE_NO_PAD.encode(&signature)
        ),
        format!("{}.{}.", original[0], original[1]),
        format!("{}.{}.{}.", original[0], original[1], original[2]),
        format!("{}.{}.{}.x", original[0], original[1], original[2]),
        format!("{}.{}", original[0], original[1]),
        format!("{}.{}.{}=", original[0], original[1], original[2]),
        format!(" {}", real.expose()),
        format!("{} ", real.expose()),
        format!("{}\n", real.expose()),
        format!("Bearer {}", real.expose()),
        String::new(),
        "...".to_owned(),
    ] {
        assert!(verify(&token).is_err(), "{token:?}");
    }
}

#[test]
fn audience_and_issuer_are_exact() {
    let now = unix_now();
    for (member, value, expected) in [
        ("aud", json!("certs"), IdentityError::WrongAudience),
        ("aud", json!("identity"), IdentityError::WrongAudience),
        ("aud", json!("Assets"), IdentityError::MalformedClaims),
        ("aud", json!(["assets"]), IdentityError::MalformedClaims),
        (
            "aud",
            json!(["assets", "certs"]),
            IdentityError::MalformedClaims,
        ),
        ("aud", json!(null), IdentityError::MalformedClaims),
        ("iss", json!("assets"), IdentityError::WrongIssuer),
        ("iss", json!("ai-gateway"), IdentityError::MalformedClaims),
    ] {
        let mut payload = claims(now);
        payload[member] = value.clone();
        assert_eq!(
            verify(&forge(&header("k1"), &payload, &key(1))),
            Err(expected),
            "{member}={value}"
        );
    }
    // The real issuer, every audience: only the addressed service accepts.
    let issuer = issuer(1, &[(1, "k1")]);
    for audience in [ServiceName::Assets, ServiceName::Certs, ServiceName::Alerts] {
        let token = issuer.issue(&grant(), audience, now).expect("token");
        for other in [ServiceName::Assets, ServiceName::Certs, ServiceName::Alerts] {
            let result = IdentityVerifier::new(other, keys(&[(1, "k1")])).verify(token.expose());
            assert_eq!(result.is_ok(), other == audience, "{audience} at {other}");
        }
    }
}

/// Expiry, issued-at and clock skew, with tokens issued at a chosen time and checked now.
#[test]
fn expiry_issued_at_and_skew_boundaries() {
    let issuer = issuer(1, &[(1, "k1")]);
    let token_at = |offset: i64| {
        issuer
            .issue(&grant(), ServiceName::Assets, unix_now() + offset)
            .expect("token")
    };
    let accepted = |offset: i64| verify(token_at(offset).expose());
    assert_eq!(TOKEN_LIFETIME_SECS, 60);
    // Issued now: fine.
    assert_eq!(accepted(0), Ok(()));
    // exp = iat + 60. Verifier leeway is 5 s: iat -64 gives exp = now - 4 (inside the leeway),
    // iat -66 gives exp = now - 6 (outside). The margin of one second absorbs a tick of the
    // clock between issuing and verifying.
    assert_eq!(accepted(-63), Ok(()), "exp 3 s ago is within the leeway");
    assert_eq!(accepted(-67), Err(IdentityError::Expired), "exp 7 s ago");
    assert_eq!(accepted(-3600), Err(IdentityError::Expired));
    assert_eq!(accepted(-86_400 * 365), Err(IdentityError::Expired));
    // Issued in the future: within the leeway is accepted, beyond it is not yet valid (an
    // identity pod whose clock runs more than 5 s ahead is refused, never trusted).
    assert_eq!(accepted(3), Ok(()), "iat 3 s ahead");
    assert_eq!(
        accepted(8),
        Err(IdentityError::NotYetValid),
        "iat 8 s ahead"
    );
    assert_eq!(accepted(3600), Err(IdentityError::NotYetValid));
    // Extreme clocks: no overflow, no acceptance. The epoch (what a clock before 1970 reads as)
    // and the far future.
    for now in [0_i64, i64::MAX, i64::MAX - 30, i64::MIN, -1] {
        let outcome = issuer.issue(&grant(), ServiceName::Assets, now);
        if let Ok(token) = outcome {
            assert!(verify(token.expose()).is_err(), "clock {now}");
        }
    }
    // Lifetimes the issuer never produces but a forger might: all refused.
    let now = unix_now();
    for (iat, exp) in [
        (now, now),
        (now, now - 1),
        (now, now + 301),
        (now, now + 86_400),
        (now, i64::MAX),
        (i64::MIN, i64::MAX),
    ] {
        let mut payload = claims(now);
        payload["iat"] = json!(iat);
        payload["exp"] = json!(exp);
        assert_eq!(
            verify(&forge(&header("k1"), &payload, &key(1))),
            Err(IdentityError::InvalidLifetime),
            "iat {iat} exp {exp}"
        );
    }
    // A 300 s lifetime is the most any verifier accepts; the identity service issues 60.
    let mut payload = claims(now);
    payload["exp"] = json!(now + 300);
    assert_eq!(verify(&forge(&header("k1"), &payload, &key(1))), Ok(()));
}

/// The rotation (docs/architecture/secrets.md, 「內部身分簽章金鑰」): with both public keys
/// published, tokens of the old and the new key are both accepted; the old public key can leave
/// only after the old tokens have expired.
#[test]
fn rotation_keeps_old_tokens_valid_until_they_expire() {
    let both = keys(&[(1, "2026-10-a"), (2, "2027-04-a")]);
    let old_issuer = issuer(1, &[(1, "2026-10-a"), (2, "2027-04-a")]);
    let new_issuer = issuer(2, &[(1, "2026-10-a"), (2, "2027-04-a")]);
    let now = unix_now();
    assert_eq!(old_issuer.signing_kid(now).as_deref(), Some("2026-10-a"));
    assert_eq!(new_issuer.signing_kid(now).as_deref(), Some("2027-04-a"));
    let old = old_issuer
        .issue(&grant(), ServiceName::Assets, now)
        .expect("old token");
    let new = new_issuer
        .issue(&grant(), ServiceName::Assets, now)
        .expect("new token");
    // Both public keys published: every service accepts both tokens.
    let verifier = IdentityVerifier::new(ServiceName::Assets, both);
    assert!(verifier.verify(old.expose()).is_ok());
    assert!(verifier.verify(new.expose()).is_ok());
    // A service that has not read the new public key yet refuses the new token, which is why
    // the new key waits internalIdentity.keyPropagationSeconds before it signs.
    let only_old = IdentityVerifier::new(ServiceName::Assets, keys(&[(1, "2026-10-a")]));
    assert_eq!(
        only_old.verify(new.expose()).err(),
        Some(IdentityError::UnknownKey)
    );
    // The old key retired too early (old token still within its lifetime): the old token is
    // refused, which is why retiring waits 305 s past the switch (crates/identity-keys).
    let only_new = IdentityVerifier::new(ServiceName::Assets, keys(&[(2, "2027-04-a")]));
    assert_eq!(
        only_new.verify(old.expose()).err(),
        Some(IdentityError::UnknownKey)
    );
    assert!(only_new.verify(new.expose()).is_ok());
    // A leaked old key cannot mint tokens once its public key is removed.
    let forged = forge(&header("2026-10-a"), &claims(now), &key(1));
    assert_eq!(
        only_new.verify(&forged).err(),
        Some(IdentityError::UnknownKey)
    );
}
