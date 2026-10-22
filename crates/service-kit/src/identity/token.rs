//! Verification of one token: JWS compact serialization (RFC 7515) with one fixed algorithm,
//! then the claims. Every check happens here, in this order:
//!
//! 1. Size: longer than [`InternalToken::MAX_LEN`] is rejected before anything is decoded.
//! 2. Shape: exactly three base64url segments without padding.
//! 3. Header: exactly `alg`, `typ` and `kid`; `alg` must be [`ALGORITHM`] and `typ`
//!    [`TOKEN_TYPE`]. The algorithm is never chosen from the token, so `none`, HMAC with the
//!    public key as the secret, and any other algorithm are rejected before a key is touched.
//! 4. Key: `kid` must name a configured key; there is no fallback to another key.
//! 5. Signature: Ed25519 strict verification over the first two segments, as sent.
//! 6. Claims, decoded only after the signature holds: issuer, audience, lifetime, expiry and
//!    not-before, with [`LEEWAY_SECS`] of clock skew.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use contract::{InternalClaims, InternalToken, ServiceName};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;

use super::{
    ALGORITHM, IdentityError, IdentityKeys, LEEWAY_SECS, MAX_KEY_ID_LEN, MAX_LIFETIME_SECS,
    TOKEN_TYPE,
};

/// The protected header. Any other parameter (`crit`, `jku`, `jwk`, `x5u`, `b64`, ...) and any
/// duplicate parameter fail deserialization, so the token cannot ask for anything this verifier
/// does not do.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    alg: String,
    typ: String,
    kid: String,
}

/// The registered not-before claim, which [`InternalClaims`] does not carry; the identity
/// service need not send it, but a token that carries one is held to it. Present means an
/// integer: `"nbf": null` is malformed, not absent.
#[derive(Deserialize)]
struct NotBefore {
    #[serde(default, deserialize_with = "super::present")]
    nbf: Option<i64>,
}

/// The audiences a verification accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Audience {
    /// Only tokens addressed to this service: what every service accepts for its own requests.
    Only(ServiceName),
    /// Tokens addressed to any service: only for the identity service's token exchange
    /// ([`super::ExchangeVerifier`]), which issues a new token from the claims and never acts on
    /// them.
    Any,
}

/// Verifies `token` for `audience` at `now` (seconds since the Unix epoch) and returns its
/// claims.
pub(super) fn verify(
    token: &str,
    keys: &IdentityKeys,
    audience: Audience,
    now: i64,
) -> Result<InternalClaims, IdentityError> {
    if token.len() > InternalToken::MAX_LEN {
        return Err(IdentityError::Oversized);
    }
    let mut segments = token.split('.');
    let (Some(header_segment), Some(payload_segment), Some(signature_segment), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err(IdentityError::Malformed);
    };

    let header: Header = serde_json::from_slice(&decode(header_segment)?)
        .map_err(|_| IdentityError::MalformedHeader)?;
    if header.alg != ALGORITHM {
        return Err(IdentityError::UnsupportedAlgorithm);
    }
    if !header.typ.eq_ignore_ascii_case(TOKEN_TYPE) {
        return Err(IdentityError::WrongType);
    }
    let key = keys.get(&header.kid).ok_or(IdentityError::UnknownKey)?;

    // The signing input is the header and payload segments exactly as received.
    let signed_len = header_segment.len() + 1 + payload_segment.len();
    let signed = token
        .as_bytes()
        .get(..signed_len)
        .ok_or(IdentityError::Malformed)?;
    check_signature(key, signed, signature_segment)?;

    let payload = decode(payload_segment)?;
    let claims: InternalClaims =
        serde_json::from_slice(&payload).map_err(|_| IdentityError::MalformedClaims)?;
    let NotBefore { nbf } =
        serde_json::from_slice(&payload).map_err(|_| IdentityError::MalformedClaims)?;
    check_claims(&claims, nbf, audience, now)?;
    Ok(claims)
}

/// Strict Ed25519 verification: rejects non-canonical signatures and small-order points, so a
/// signature cannot be altered into another valid one.
fn check_signature(
    key: &VerifyingKey,
    signed: &[u8],
    signature_segment: &str,
) -> Result<(), IdentityError> {
    let signature =
        Signature::from_slice(&decode(signature_segment)?).map_err(|_| IdentityError::Malformed)?;
    key.verify_strict(signed, &signature)
        .map_err(|_| IdentityError::BadSignature)
}

fn check_claims(
    claims: &InternalClaims,
    nbf: Option<i64>,
    audience: Audience,
    now: i64,
) -> Result<(), IdentityError> {
    if claims.iss != ServiceName::Identity {
        return Err(IdentityError::WrongIssuer);
    }
    if let Audience::Only(audience) = audience
        && claims.aud != audience
    {
        return Err(IdentityError::WrongAudience);
    }
    let lifetime = claims.exp.checked_sub(claims.iat);
    if !lifetime.is_some_and(|seconds| seconds > 0 && seconds <= MAX_LIFETIME_SECS) {
        return Err(IdentityError::InvalidLifetime);
    }
    // RFC 7519: the token is valid only before `exp`; skew may extend that by the leeway.
    if now >= claims.exp.saturating_add(LEEWAY_SECS) {
        return Err(IdentityError::Expired);
    }
    // A token is not valid before it was issued, nor before its `nbf` when it has one.
    let not_before = nbf.map_or(claims.iat, |nbf| nbf.max(claims.iat));
    if not_before > now.saturating_add(LEEWAY_SECS) {
        return Err(IdentityError::NotYetValid);
    }
    Ok(())
}

/// The key id a token's header names, for the log line of an `unknown_key` rejection: only when
/// the header is the one this verifier accepts and the key id could be one (1 to
/// [`MAX_KEY_ID_LEN`] visible ASCII characters), so the log never carries other text from the
/// request.
pub(super) fn header_kid(token: &str) -> Option<String> {
    let header_segment = token.split('.').next()?;
    let header: Header = serde_json::from_slice(&decode(header_segment).ok()?).ok()?;
    let usable = (1..=MAX_KEY_ID_LEN).contains(&header.kid.len())
        && header.kid.bytes().all(|byte| byte.is_ascii_graphic());
    usable.then_some(header.kid)
}

/// Strict base64url without padding: padding, the standard alphabet, and non-zero trailing bits
/// are all rejected, so each token has exactly one encoding.
fn decode(segment: &str) -> Result<Vec<u8>, IdentityError> {
    URL_SAFE_NO_PAD
        .decode(segment)
        .map_err(|_| IdentityError::Malformed)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::identity::test_keys::{
        NOW, TestKey, claims_at, encode_json, header, keys, sign_with_header,
    };

    fn verify_assets(token: &str, keys: &IdentityKeys) -> Result<InternalClaims, IdentityError> {
        verify(token, keys, Audience::Only(ServiceName::Assets), NOW)
    }

    #[test]
    fn a_valid_token_yields_its_claims() {
        let signer = TestKey::new(1);
        let claims = claims_at(NOW);
        let token = signer.sign("k1", &claims);
        assert_eq!(
            verify_assets(&token, &keys(&[(&signer, "k1")])),
            Ok(serde_json::from_value(claims).expect("claims"))
        );
    }

    /// RFC 8037 appendix A.4: the example JWS signed with the example Ed25519 key. Proves that
    /// signatures are checked over the segments as sent, the way every JOSE library signs them.
    #[test]
    fn signatures_follow_rfc_8037() {
        let jwks = json!({"keys": [{
            "kty": "OKP", "crv": "Ed25519", "kid": "rfc8037",
            "x": "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"
        }]})
        .to_string();
        let keys = IdentityKeys::from_jwks(&jwks).expect("RFC 8037 key");
        let key = keys.get("rfc8037").expect("key");
        let signed = b"eyJhbGciOiJFZERTQSJ9.RXhhbXBsZSBvZiBFZDI1NTE5IHNpZ25pbmc";
        let signature = "hgyY0il_MGCjP0JzlnLWG1PPOt7-09PGcvMg3AIbQR6dWbhijcNR4ki4iylGjg5BhVsPt9g7sVvpAr_MuM0KAg";
        assert_eq!(check_signature(key, signed, signature), Ok(()));
        assert_eq!(
            check_signature(key, b"eyJhbGciOiJFZERTQSJ9.RXhhbXBsZQ", signature),
            Err(IdentityError::BadSignature)
        );
    }

    #[test]
    fn forged_signatures_are_rejected() {
        let signer = TestKey::new(1);
        let forger = TestKey::new(2);
        let keys = keys(&[(&signer, "k1")]);
        let claims = claims_at(NOW);

        // Signed by another key under the configured key id.
        let forged = forger.sign("k1", &claims);
        assert_eq!(
            verify_assets(&forged, &keys),
            Err(IdentityError::BadSignature)
        );

        // A genuine token whose payload was changed to another audience.
        let genuine = signer.sign("k1", &claims);
        let mut parts: Vec<String> = genuine.split('.').map(str::to_owned).collect();
        let mut altered = claims.clone();
        altered["aud"] = json!("certs");
        parts[1] = encode_json(&altered);
        assert_eq!(
            verify(
                &parts.join("."),
                &keys,
                Audience::Only(ServiceName::Certs),
                NOW
            ),
            Err(IdentityError::BadSignature)
        );

        // A genuine token with one signature bit flipped.
        let mut signature = URL_SAFE_NO_PAD
            .decode(genuine.rsplit('.').next().expect("signature"))
            .expect("base64url");
        signature[10] ^= 0x01;
        let (signed, _) = genuine.rsplit_once('.').expect("three segments");
        let flipped = format!("{signed}.{}", URL_SAFE_NO_PAD.encode(&signature));
        assert_eq!(
            verify_assets(&flipped, &keys),
            Err(IdentityError::BadSignature)
        );

        // A non-canonical signature: S + l, valid under lax verification only.
        let mut malleable = URL_SAFE_NO_PAD
            .decode(genuine.rsplit('.').next().expect("signature"))
            .expect("base64url");
        add_group_order(&mut malleable[32..]);
        let malleated = format!("{signed}.{}", URL_SAFE_NO_PAD.encode(&malleable));
        assert_ne!(malleated, genuine);
        assert_eq!(
            verify_assets(&malleated, &keys),
            Err(IdentityError::BadSignature)
        );
    }

    /// Adds the Ed25519 group order l to a little-endian scalar.
    fn add_group_order(scalar: &mut [u8]) {
        const L: [u8; 32] = [
            0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9,
            0xde, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x10,
        ];
        let mut carry = 0u16;
        for (byte, add) in scalar.iter_mut().zip(L) {
            let sum = u16::from(*byte) + u16::from(add) + carry;
            *byte = sum.to_le_bytes()[0];
            carry = sum >> 8;
        }
    }

    #[test]
    fn the_algorithm_is_fixed_by_the_verifier() {
        let signer = TestKey::new(1);
        let keys = keys(&[(&signer, "k1")]);
        let claims = claims_at(NOW);
        for alg in [
            "none", "HS256", "RS256", "ES256", "PS256", "eddsa", "Ed25519", "",
        ] {
            let token = sign_with_header(&signer, &header(alg, TOKEN_TYPE, "k1"), &claims);
            assert_eq!(
                verify_assets(&token, &keys),
                Err(IdentityError::UnsupportedAlgorithm),
                "{alg}"
            );
        }
        // `alg: none` with an empty signature never reaches a key.
        let unsigned = format!(
            "{}.{}.",
            encode_json(&header("none", TOKEN_TYPE, "k1")),
            encode_json(&claims)
        );
        assert_eq!(
            verify_assets(&unsigned, &keys),
            Err(IdentityError::UnsupportedAlgorithm)
        );
    }

    #[test]
    fn the_token_type_is_explicit() {
        let signer = TestKey::new(1);
        let keys = keys(&[(&signer, "k1")]);
        let claims = claims_at(NOW);
        let typed = sign_with_header(
            &signer,
            &header(ALGORITHM, "Internal-Identity+JWT", "k1"),
            &claims,
        );
        assert!(
            verify_assets(&typed, &keys).is_ok(),
            "typ is case-insensitive"
        );
        for typ in ["JWT", "at+jwt", ""] {
            let token = sign_with_header(&signer, &header(ALGORITHM, typ, "k1"), &claims);
            assert_eq!(
                verify_assets(&token, &keys),
                Err(IdentityError::WrongType),
                "{typ}"
            );
        }
    }

    #[test]
    fn the_header_has_exactly_alg_typ_and_kid() {
        let signer = TestKey::new(1);
        let keys = keys(&[(&signer, "k1")]);
        let claims = claims_at(NOW);
        let base = header(ALGORITHM, TOKEN_TYPE, "k1");
        let mut cases = Vec::new();
        for extra in [
            json!({"crit": ["exp"]}),
            json!({"jku": "http://attacker.example/jwks"}),
            json!({"jwk": signer.jwk("k1")}),
            json!({"x5u": "http://attacker.example/cert"}),
            json!({"b64": false}),
            json!({"cty": "JWT"}),
        ] {
            let mut header = base.clone();
            header
                .as_object_mut()
                .expect("object")
                .extend(extra.as_object().expect("object").clone());
            cases.push(encode_json(&header));
        }
        for missing in ["alg", "typ", "kid"] {
            let mut header = base.clone();
            header.as_object_mut().expect("object").remove(missing);
            cases.push(encode_json(&header));
        }
        let duplicate =
            format!(r#"{{"alg":"none","alg":"{ALGORITHM}","typ":"{TOKEN_TYPE}","kid":"k1"}}"#);
        cases.push(URL_SAFE_NO_PAD.encode(duplicate));
        cases.push(URL_SAFE_NO_PAD.encode("not json"));
        cases.push(encode_json(
            &json!({"alg": ALGORITHM, "typ": TOKEN_TYPE, "kid": 1}),
        ));
        for encoded_header in cases {
            let token = signer.sign_segments(&encoded_header, &encode_json(&claims));
            assert_eq!(
                verify_assets(&token, &keys),
                Err(IdentityError::MalformedHeader),
                "{encoded_header}"
            );
        }
    }

    #[test]
    fn keys_are_chosen_by_key_id_only() {
        let signer = TestKey::new(1);
        let keys = keys(&[(&signer, "k1")]);
        let token = signer.sign("k2", &claims_at(NOW));
        assert_eq!(verify_assets(&token, &keys), Err(IdentityError::UnknownKey));
    }

    #[test]
    fn issuer_and_audience_must_match() {
        let signer = TestKey::new(1);
        let keys = keys(&[(&signer, "k1")]);
        let token = signer.sign("k1", &claims_at(NOW));
        assert!(verify(&token, &keys, Audience::Only(ServiceName::Assets), NOW).is_ok());
        assert_eq!(
            verify(&token, &keys, Audience::Only(ServiceName::Certs), NOW),
            Err(IdentityError::WrongAudience)
        );

        let mut claims = claims_at(NOW);
        claims["iss"] = json!("assets");
        let token = signer.sign("k1", &claims);
        assert_eq!(
            verify_assets(&token, &keys),
            Err(IdentityError::WrongIssuer)
        );
    }

    /// The token exchange's mode skips the audience check and nothing else.
    #[test]
    fn any_audience_still_checks_everything_else() {
        let signer = TestKey::new(1);
        let keys = keys(&[(&signer, "k1")]);
        for audience in ServiceName::ALL {
            let mut claims = claims_at(NOW);
            claims["aud"] = json!(audience.as_str());
            let verified = verify(&signer.sign("k1", &claims), &keys, Audience::Any, NOW)
                .expect("any audience verifies");
            assert_eq!(verified.aud, audience);
        }

        let mut wrong_issuer = claims_at(NOW);
        wrong_issuer["iss"] = json!("assets");
        let mut too_long = claims_at(NOW);
        too_long["exp"] = json!(NOW + MAX_LIFETIME_SECS + 1);
        let mut unknown_audience = claims_at(NOW);
        unknown_audience["aud"] = json!("ai-gateway");
        let cases = [
            (signer.sign("k1", &wrong_issuer), IdentityError::WrongIssuer),
            (signer.sign("k1", &too_long), IdentityError::InvalidLifetime),
            (
                signer.sign("k1", &claims_at(NOW - 3600)),
                IdentityError::Expired,
            ),
            (
                signer.sign("k1", &claims_at(NOW + 3600)),
                IdentityError::NotYetValid,
            ),
            (
                signer.sign("k1", &unknown_audience),
                IdentityError::MalformedClaims,
            ),
            (
                TestKey::new(2).sign("k1", &claims_at(NOW)),
                IdentityError::BadSignature,
            ),
            (
                signer.sign("k2", &claims_at(NOW)),
                IdentityError::UnknownKey,
            ),
        ];
        for (token, expected) in cases {
            assert_eq!(
                verify(&token, &keys, Audience::Any, NOW),
                Err(expected),
                "{expected:?}"
            );
        }
    }

    #[test]
    fn expiry_and_not_before_allow_the_leeway_only() {
        let signer = TestKey::new(1);
        let keys = keys(&[(&signer, "k1")]);
        let at = |iat: i64, exp: i64| {
            let mut claims = claims_at(iat);
            claims["exp"] = json!(exp);
            signer.sign("k1", &claims)
        };
        // Expiry: valid until exp + leeway, exclusive.
        let token = at(NOW - 60, NOW);
        assert!(verify_assets(&token, &keys).is_ok());
        let token = at(NOW - 60 - LEEWAY_SECS + 1, NOW - LEEWAY_SECS + 1);
        assert!(verify_assets(&token, &keys).is_ok());
        let token = at(NOW - 60 - LEEWAY_SECS, NOW - LEEWAY_SECS);
        assert_eq!(verify_assets(&token, &keys), Err(IdentityError::Expired));
        let token = at(NOW - 3600, NOW - 3540);
        assert_eq!(verify_assets(&token, &keys), Err(IdentityError::Expired));

        // Issued in the future: valid only within the leeway.
        let token = at(NOW + LEEWAY_SECS, NOW + LEEWAY_SECS + 60);
        assert!(verify_assets(&token, &keys).is_ok());
        let token = at(NOW + LEEWAY_SECS + 1, NOW + LEEWAY_SECS + 61);
        assert_eq!(
            verify_assets(&token, &keys),
            Err(IdentityError::NotYetValid)
        );

        // nbf, when present, is held to the same rule.
        let mut claims = claims_at(NOW);
        claims["nbf"] = json!(NOW + LEEWAY_SECS + 1);
        let token = signer.sign("k1", &claims);
        assert_eq!(
            verify_assets(&token, &keys),
            Err(IdentityError::NotYetValid)
        );
        claims["nbf"] = json!(NOW + LEEWAY_SECS);
        let token = signer.sign("k1", &claims);
        assert!(verify_assets(&token, &keys).is_ok());
    }

    #[test]
    fn lifetimes_must_be_short_and_positive() {
        let signer = TestKey::new(1);
        let keys = keys(&[(&signer, "k1")]);
        for (iat, exp, valid) in [
            (NOW, NOW + MAX_LIFETIME_SECS, true),
            (NOW, NOW + MAX_LIFETIME_SECS + 1, false),
            (NOW, NOW, false),
            (NOW, NOW - 1, false),
            (i64::MIN, i64::MAX, false),
        ] {
            let mut claims = claims_at(iat);
            claims["exp"] = json!(exp);
            let token = signer.sign("k1", &claims);
            let result = verify_assets(&token, &keys);
            if valid {
                assert!(result.is_ok(), "{iat}..{exp}: {result:?}");
            } else {
                assert_eq!(result, Err(IdentityError::InvalidLifetime), "{iat}..{exp}");
            }
        }
    }

    #[test]
    fn claims_are_checked_only_after_the_signature() {
        let signer = TestKey::new(1);
        let forger = TestKey::new(2);
        let keys = keys(&[(&signer, "k1")]);
        // Garbage claims under a bad signature report the signature, not the claims.
        let forged = forger.sign_segments(
            &encode_json(&header(ALGORITHM, TOKEN_TYPE, "k1")),
            &URL_SAFE_NO_PAD.encode("not json"),
        );
        assert_eq!(
            verify_assets(&forged, &keys),
            Err(IdentityError::BadSignature)
        );

        let genuine_header = encode_json(&header(ALGORITHM, TOKEN_TYPE, "k1"));
        let claims = claims_at(NOW);
        let mut missing_aud = claims.clone();
        missing_aud.as_object_mut().expect("object").remove("aud");
        let mut array_aud = claims.clone();
        array_aud["aud"] = json!(["assets"]);
        let mut fractional = claims.clone();
        fractional["exp"] = json!(1_791_590_430.5);
        // A present `nbf` must be an integer; `null` is not the same as leaving it out.
        let mut null_nbf = claims.clone();
        null_nbf["nbf"] = json!(null);
        let mut text_nbf = claims.clone();
        text_nbf["nbf"] = json!("later");
        // A second `aud` must not override the first.
        let duplicate_aud = format!(
            r#"{{"iss":"identity","sub":"{}","aud":"certs","aud":"assets","iat":{NOW},"exp":{},"tenant":null}}"#,
            claims["sub"].as_str().expect("sub"),
            NOW + 60
        );
        for payload in [
            URL_SAFE_NO_PAD.encode("not json"),
            encode_json(&missing_aud),
            encode_json(&array_aud),
            encode_json(&fractional),
            encode_json(&null_nbf),
            encode_json(&text_nbf),
            URL_SAFE_NO_PAD.encode(duplicate_aud),
        ] {
            let token = signer.sign_segments(&genuine_header, &payload);
            assert_eq!(
                verify_assets(&token, &keys),
                Err(IdentityError::MalformedClaims),
                "{payload}"
            );
        }
    }

    #[test]
    fn malformed_and_oversized_tokens_are_rejected_before_parsing() {
        let signer = TestKey::new(1);
        let keys = keys(&[(&signer, "k1")]);
        let token = signer.sign("k1", &claims_at(NOW));
        let (signed, signature) = token.rsplit_once('.').expect("three segments");
        let short_signature = URL_SAFE_NO_PAD.encode([0u8; 63]);
        let rest_of_signature: String = signature.chars().skip(1).collect();
        for bad in [
            String::new(),
            "a.b".to_owned(),
            format!("{token}.extra"),
            signed.to_owned(),
            format!("{signed}.{signature}="),
            // `+` and `/` belong to standard base64, not base64url.
            format!("{signed}.+{rest_of_signature}"),
            format!("{signed}./{rest_of_signature}"),
            format!("{signed}.{short_signature}"),
            format!("{signed}. {signature}"),
        ] {
            assert_eq!(
                verify_assets(&bad, &keys),
                Err(IdentityError::Malformed),
                "{bad}"
            );
        }

        let oversized = "a".repeat(InternalToken::MAX_LEN + 1);
        assert_eq!(
            verify_assets(&oversized, &keys),
            Err(IdentityError::Oversized)
        );
        let padded = format!("{token}{}", ".".repeat(InternalToken::MAX_LEN));
        assert_eq!(verify_assets(&padded, &keys), Err(IdentityError::Oversized));
    }

    #[test]
    fn segments_have_exactly_one_encoding() {
        assert_eq!(decode("QQ"), Ok(b"A".to_vec()));
        assert_eq!(decode("_-8"), Ok(vec![0xff, 0xef]));
        assert_eq!(decode(""), Ok(Vec::new()));
        // Non-zero trailing bits would be a second spelling of the same bytes.
        for bad in ["QR", "QQ==", "QQ=", "/+8", "Q", "Q Q", "QQ\n"] {
            assert_eq!(decode(bad), Err(IdentityError::Malformed), "{bad:?}");
        }
    }
}
