//! Signs internal identity tokens (decision 27) in the format `crates/service-kit` verifies:
//! a JWS compact serialization whose protected header is exactly
//! `{"alg":"EdDSA","typ":"internal-identity+jwt","kid":"<key id>"}` and whose payload is
//! [`InternalClaims`] as JSON, signed with Ed25519.
//!
//! The private key is a PKCS#8 PEM file (`openssl genpkey -algorithm ed25519`) mounted from a
//! Kubernetes Secret into this service only. Its key id is not configured separately: at startup
//! the issuer signs a probe token under every key id of the deployment's public keys
//! (`TRUSTIOT_INTERNAL_IDENTITY_KEYS`) and verifies it with service-kit's verifier, exactly as a
//! service would. The one key id that verifies is the one it signs with; when none does, the
//! public key was never deployed and the service refuses to start, so it can never issue tokens
//! that the services reject. Rotation is therefore "deploy the new public key, then swap the
//! Secret" (services/identity/README.md).

use std::fmt;
use std::io::Read;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use contract::{InternalClaims, InternalToken, ServiceName, UserId};
use ed25519_dalek::pkcs8::DecodePrivateKey;
use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;
use service_kit::identity::{ALGORITHM, IdentityKeys, IdentityVerifier, TOKEN_TYPE};
use zeroize::Zeroizing;

use crate::session::SessionGrant;

/// Lifetime of every token, in seconds: `exp - iat`. Short, because a token is a bearer
/// credential for its audience until it expires (plus the verifiers' 5 s of clock skew), and
/// because rotation waits for the longest lifetime the verifiers accept (300 s). Matches the
/// 2026-10-10 PoC.
pub const TOKEN_LIFETIME_SECS: i64 = 60;

/// The platform setting with the path of the signing key file: `platform.identity_signing_key_file`,
/// set by the chart as `TRUSTIOT_IDENTITY_SIGNING_KEY_FILE` on the identity service only.
pub const KEY_FILE_SETTING: &str = "TRUSTIOT_IDENTITY_SIGNING_KEY_FILE";

/// Largest key file read. A PKCS#8 PEM Ed25519 key is about 120 bytes.
const MAX_KEY_FILE_BYTES: u64 = 16 * 1024;

/// The subject of the startup probe token; never issued to anyone.
const PROBE_SUBJECT: &str = "00000000-0000-0000-0000-000000000000";

/// The protected header, serialized in this field order.
#[derive(Serialize)]
struct Header<'a> {
    alg: &'static str,
    typ: &'static str,
    kid: &'a str,
}

/// Signs tokens with the identity service's private key under the key id the deployment
/// publishes for it.
pub struct Issuer {
    key: SigningKey,
    kid: String,
}

impl Issuer {
    /// Reads the PKCS#8 PEM key at `path` and finds its key id in `keys` (see the module
    /// documentation).
    ///
    /// # Errors
    ///
    /// When the file cannot be read, is not an Ed25519 private key in PKCS#8 PEM, or its public
    /// key is not exactly one entry of `keys`. Errors never contain the file's content.
    pub fn load(path: &Path, keys: &IdentityKeys) -> Result<Self, KeyError> {
        let file = std::fs::File::open(path).map_err(|error| KeyError::Unreadable(error.kind()))?;
        let size = file
            .metadata()
            .map_err(|error| KeyError::Unreadable(error.kind()))?
            .len();
        if size > MAX_KEY_FILE_BYTES {
            return Err(KeyError::TooLarge);
        }
        let mut text = Zeroizing::new(String::new());
        file.take(MAX_KEY_FILE_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(|_| KeyError::NotPkcs8Ed25519)?;
        Self::from_pkcs8_pem(&text, keys)
    }

    /// Like [`Issuer::load`], from the PEM text.
    ///
    /// # Errors
    ///
    /// As [`Issuer::load`], except reading the file.
    pub fn from_pkcs8_pem(pem: &str, keys: &IdentityKeys) -> Result<Self, KeyError> {
        let key = SigningKey::from_pkcs8_pem(pem).map_err(|_| KeyError::NotPkcs8Ed25519)?;
        let verifier = IdentityVerifier::new(ServiceName::Identity, keys.clone());
        let probe = ProbeClaims::at(unix_now());
        let mut matching = Vec::new();
        for kid in keys.key_ids() {
            let token = sign(&key, kid, &probe.0).map_err(|_| KeyError::NotPkcs8Ed25519)?;
            if verifier.verify(&token).is_ok() {
                matching.push(kid.to_owned());
            }
        }
        match <[String; 1]>::try_from(matching) {
            Ok([kid]) => Ok(Self { key, kid }),
            Err(matching) if matching.is_empty() => Err(KeyError::NotDeployed),
            Err(matching) => Err(KeyError::AmbiguousKeyIds(matching)),
        }
    }

    /// The key id every token names.
    #[must_use]
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// A token for `grant`, addressed to `audience`, issued at `now` (seconds since the Unix
    /// epoch) and valid for [`TOKEN_LIFETIME_SECS`].
    ///
    /// # Errors
    ///
    /// Only if the claims cannot be serialized or the result is not a valid header value, which
    /// the types rule out; callers answer 503.
    pub fn issue(
        &self,
        grant: &SessionGrant,
        audience: ServiceName,
        now: i64,
    ) -> Result<InternalToken, IssueError> {
        let claims = InternalClaims {
            iss: ServiceName::Identity,
            sub: grant.user,
            aud: audience,
            iat: now,
            exp: now.checked_add(TOKEN_LIFETIME_SECS).ok_or(IssueError)?,
            tenant: grant.tenant,
            platform_admin: grant.platform_admin,
        };
        let token = sign(&self.key, &self.kid, &claims)?;
        InternalToken::new(token).map_err(|_| IssueError)
    }
}

impl fmt::Debug for Issuer {
    /// Only the key id: the private key never reaches a log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Issuer")
            .field("kid", &self.kid)
            .finish_non_exhaustive()
    }
}

/// `<header>.<payload>.<signature>`, each base64url without padding; the signature covers the
/// first two segments as written.
fn sign(key: &SigningKey, kid: &str, claims: &InternalClaims) -> Result<String, IssueError> {
    let header = Header {
        alg: ALGORITHM,
        typ: TOKEN_TYPE,
        kid,
    };
    let header = serde_json::to_vec(&header).map_err(|_| IssueError)?;
    let payload = serde_json::to_vec(claims).map_err(|_| IssueError)?;
    let signed = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header),
        URL_SAFE_NO_PAD.encode(payload)
    );
    let signature = key.sign(signed.as_bytes());
    Ok(format!(
        "{signed}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

/// Claims of the startup probe: addressed to the identity service itself, for a user that does
/// not exist.
struct ProbeClaims(InternalClaims);

impl ProbeClaims {
    fn at(now: i64) -> Self {
        Self(InternalClaims {
            iss: ServiceName::Identity,
            sub: PROBE_SUBJECT
                .parse::<UserId>()
                .unwrap_or_else(|_| UserId::from_uuid(Default::default())),
            aud: ServiceName::Identity,
            iat: now,
            exp: now.saturating_add(TOKEN_LIFETIME_SECS),
            tenant: None,
            platform_admin: false,
        })
    }
}

/// Seconds since the Unix epoch. A clock before the epoch reads as 0, so tokens come out expired
/// and services reject them rather than accept them.
#[must_use]
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

/// Why the signing key cannot be used. Messages never quote the key file.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// The file could not be opened or its size read.
    #[error("{KEY_FILE_SETTING}: the signing key file cannot be read ({0})")]
    Unreadable(std::io::ErrorKind),
    /// The file is larger than a key file can be.
    #[error("{KEY_FILE_SETTING}: the signing key file is too large to be a key")]
    TooLarge,
    /// The file is not an Ed25519 private key in PKCS#8 PEM.
    #[error(
        "{KEY_FILE_SETTING}: the signing key file is not an Ed25519 private key in PKCS#8 PEM \
         (`openssl genpkey -algorithm ed25519`)"
    )]
    NotPkcs8Ed25519,
    /// No key id of the deployment's public keys verifies tokens signed with this key.
    #[error(
        "{KEY_FILE_SETTING}: the signing key's public key is not in \
         TRUSTIOT_INTERNAL_IDENTITY_KEYS, so services would reject every token; deploy the public \
         key to every service first (key rotation step 1)"
    )]
    NotDeployed,
    /// More than one key id holds this key's public key.
    #[error(
        "{KEY_FILE_SETTING}: the signing key's public key is listed under more than one key id in \
         TRUSTIOT_INTERNAL_IDENTITY_KEYS ({})",
        .0.join(", ")
    )]
    AmbiguousKeyIds(Vec<String>),
}

/// A token could not be built; the types make this unreachable in practice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("could not build an internal identity token")]
pub struct IssueError;

#[cfg(test)]
pub(crate) mod test_keys {
    //! Throwaway keys derived in code from a one-byte seed: nothing outside the tests trusts
    //! them, and no key material is stored in the repository.

    use ed25519_dalek::SigningKey;
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
    use serde_json::json;
    use service_kit::identity::IdentityKeys;

    use super::*;

    pub(crate) fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// The key as `openssl genpkey -algorithm ed25519` writes it.
    pub(crate) fn pem(seed: u8) -> String {
        signing_key(seed)
            .to_pkcs8_pem(LineEnding::LF)
            .expect("PKCS#8 PEM")
            .to_string()
    }

    /// The public JWK `x` member.
    pub(crate) fn x(seed: u8) -> String {
        URL_SAFE_NO_PAD.encode(signing_key(seed).verifying_key().as_bytes())
    }

    /// A key set with these (seed, kid) entries.
    pub(crate) fn keys(entries: &[(u8, &str)]) -> IdentityKeys {
        let keys: Vec<_> = entries
            .iter()
            .map(|(seed, kid)| json!({"kty": "OKP", "crv": "Ed25519", "kid": kid, "x": x(*seed)}))
            .collect();
        IdentityKeys::from_jwks(&json!({ "keys": keys }).to_string()).expect("valid key set")
    }

    pub(crate) fn issuer(seed: u8, entries: &[(u8, &str)]) -> Issuer {
        Issuer::from_pkcs8_pem(&pem(seed), &keys(entries)).expect("usable key")
    }
}

#[cfg(test)]
mod tests {
    use contract::{TenantGrant, TenantRole};
    use serde_json::{Value, json};
    use service_kit::identity::IdentityError;

    use super::test_keys::{issuer, keys, pem, signing_key};
    use super::*;

    const USER: &str = "01928f6e-7a3c-7d4e-8f00-0123456789ab";
    const TENANT: &str = "01928f6e-7a3c-7d4e-8f00-00000000000a";

    fn grant() -> SessionGrant {
        SessionGrant {
            user: USER.parse().expect("user"),
            tenant: Some(TenantGrant {
                id: TENANT.parse().expect("tenant"),
                role: TenantRole::Editor,
            }),
            platform_admin: true,
        }
    }

    fn segment(token: &str, index: usize) -> Value {
        let part = token.split('.').nth(index).expect("segment");
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).expect("base64url")).expect("JSON")
    }

    #[test]
    fn tokens_pass_service_kit_verification_for_their_audience_only() {
        let issuer = issuer(1, &[(1, "2026-10-a")]);
        let token = issuer
            .issue(&grant(), ServiceName::Assets, unix_now())
            .expect("token");
        let keys = keys(&[(1, "2026-10-a")]);
        let user = IdentityVerifier::new(ServiceName::Assets, keys.clone())
            .verify(token.expose())
            .expect("assets accepts its token");
        assert_eq!(user.user_id(), grant().user);
        assert_eq!(user.tenant(), grant().tenant);
        assert!(user.is_platform_admin());
        // The same token replayed at another service is refused there.
        for other in [
            ServiceName::Certs,
            ServiceName::Identity,
            ServiceName::Alerts,
        ] {
            assert_eq!(
                IdentityVerifier::new(other, keys.clone())
                    .verify(token.expose())
                    .err(),
                Some(IdentityError::WrongAudience),
                "{other}"
            );
        }
    }

    #[test]
    fn the_header_and_claims_are_exactly_the_contract() {
        let issuer = issuer(1, &[(1, "2026-10-a")]);
        let now = 1_791_590_400;
        let token = issuer
            .issue(&grant(), ServiceName::Certs, now)
            .expect("token");
        assert_eq!(token.expose().split('.').count(), 3);
        assert_eq!(
            segment(token.expose(), 0),
            json!({"alg": "EdDSA", "typ": "internal-identity+jwt", "kid": "2026-10-a"})
        );
        assert_eq!(
            segment(token.expose(), 1),
            json!({
                "iss": "identity",
                "sub": USER,
                "aud": "certs",
                "iat": now,
                "exp": now + 60,
                "tenant": {"id": TENANT, "role": "editor"},
                "platform_admin": true
            })
        );
    }

    /// The replay window of a captured token is its 60 s lifetime plus the verifiers' 5 s of
    /// clock skew, and no more.
    #[test]
    fn a_token_stops_working_after_its_lifetime_and_the_skew() {
        let issuer = issuer(1, &[(1, "k")]);
        let verifier = IdentityVerifier::new(ServiceName::Assets, keys(&[(1, "k")]));
        let now = unix_now();
        let fresh = issuer
            .issue(&grant(), ServiceName::Assets, now - 60)
            .expect("token");
        assert!(verifier.verify(fresh.expose()).is_ok(), "within exp + 5 s");
        let stale = issuer
            .issue(&grant(), ServiceName::Assets, now - TOKEN_LIFETIME_SECS - 5)
            .expect("token");
        assert_eq!(
            verifier.verify(stale.expose()).err(),
            Some(IdentityError::Expired)
        );
    }

    #[test]
    fn the_key_id_is_the_one_whose_public_key_matches() {
        // During rotation both keys are deployed; each private key finds its own key id.
        let entries = [(1, "2026-10-a"), (2, "2027-04-a")];
        assert_eq!(issuer(1, &entries).kid(), "2026-10-a");
        assert_eq!(issuer(2, &entries).kid(), "2027-04-a");
        assert_eq!(issuer(2, &[(2, "only")]).kid(), "only");
    }

    #[test]
    fn a_key_the_services_do_not_have_is_refused() {
        // Rotation step 2 before step 1: the new private key's public key is not deployed.
        assert_eq!(
            Issuer::from_pkcs8_pem(&pem(2), &keys(&[(1, "2026-10-a")])).err(),
            Some(KeyError::NotDeployed)
        );
        assert_eq!(
            Issuer::from_pkcs8_pem(&pem(1), &keys(&[(1, "a"), (2, "b"), (1, "c")])).err(),
            Some(KeyError::AmbiguousKeyIds(vec![
                "a".to_owned(),
                "c".to_owned()
            ]))
        );
    }

    /// An X25519 private key in PKCS#8 PEM: same OID family as Ed25519 (1.3.101.110), different
    /// algorithm. Built at run time so no private key block sits in the source.
    fn x25519_pem() -> String {
        let mut der = vec![
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x6e, 0x04, 0x22,
            0x04, 0x20,
        ];
        der.extend([7_u8; 32]);
        let label = "PRIVATE KEY";
        format!(
            "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
            base64::engine::general_purpose::STANDARD.encode(der)
        )
    }

    #[test]
    fn only_an_ed25519_pkcs8_pem_is_a_signing_key() {
        let keys = keys(&[(1, "k")]);
        let der_base64 = pem(1)
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect::<String>();
        for bad in [
            String::new(),
            "not a key".to_owned(),
            der_base64,
            pem(1).replace("PRIVATE KEY", "PUBLIC KEY"),
            x25519_pem(),
        ] {
            assert_eq!(
                Issuer::from_pkcs8_pem(&bad, &keys).err(),
                Some(KeyError::NotPkcs8Ed25519),
                "{bad}"
            );
        }
    }

    /// Acceptance (Laysi/trustiot-v2#61): the private key never appears in error messages or
    /// debug output, in any of its encodings.
    #[test]
    fn errors_and_debug_output_never_show_the_private_key() {
        let seed = [1_u8; 32];
        let pem_text = pem(1);
        let body: String = pem_text
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect();
        let secrets = [
            body.clone(),
            URL_SAFE_NO_PAD.encode(seed),
            base64::engine::general_purpose::STANDARD.encode(seed),
            seed.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        ];
        let mut outputs = vec![format!("{:?}", issuer(1, &[(1, "k")]))];
        for keys in [keys(&[(2, "k")]), keys(&[(1, "a"), (1, "b")])] {
            let error = Issuer::from_pkcs8_pem(&pem_text, &keys).expect_err("refused");
            outputs.push(error.to_string());
            outputs.push(format!("{error:?}"));
        }
        let broken = pem_text.replace(body.get(..10).expect("PEM body"), "AAAAAAAAAA");
        let error = Issuer::from_pkcs8_pem(&broken, &keys(&[(1, "k")])).expect_err("refused");
        outputs.push(error.to_string());
        for output in outputs {
            for secret in &secrets {
                assert!(!output.contains(secret.as_str()), "{output}");
            }
        }
        assert_eq!(
            signing_key(1).to_bytes(),
            seed,
            "the secrets above are this key"
        );
    }
}
