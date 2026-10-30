//! Certificate renewal over HTTPS (decision 12; rebuild checklist D2; Laysi/trustiot-v2#81).
//!
//! A gateway renews its MQTT client certificate outside MQTT: when its certificate expired or was
//! revoked, mTLS to the broker fails, so the renewal goes over HTTPS on 443 instead, and the
//! gateway proves who it is with its TPM key. Both steps are a `POST` of a JSON object to
//! [`RENEWAL_PATH`] on the gateway host, told apart by the body's `step`:
//!
//! 1. [`RenewalRequest::Challenge`] → 200 [`Challenge`]: a fresh nonce bound to the hardware ID and
//!    [`Purpose::CertRenewal`], valid for [`CHALLENGE_TTL_SECS`].
//! 2. The gateway builds [`Challenge::signing_input`] itself and signs it with its TPM key (ECDSA
//!    P-256, SHA-256, the signature DER), then sends [`RenewalRequest::Renew`] → 201
//!    [`RenewedCertificate`]: a new certificate for the registered key.
//!
//! The cloud answers every refusal with an M1 error body (`contract::ErrorBody`) and the status
//! that services/certs/README.md ("換發") lists; the gateway backs off and starts again from
//! step 1. The JSON encoding lives in the gateway agent and the certs service (`serde_json`);
//! this crate only has the types, the limits, the validators and the signing input, so both
//! sides build the same bytes. This crate's README, "憑證換發", is the specification for humans.
//!
//! Unknown fields are ignored on both sides, like the MQTT payloads' unknown map keys, so a newer
//! agent's extra field does not break an older service's renewal.

use serde::{Deserialize, Serialize};

use crate::ids::GatewayId;

/// The path of both steps on the gateway host (the chart's `ingress.publicPaths.certRenewal`, an
/// Exact match).
pub const RENEWAL_PATH: &str = "/certs/renew";

/// Largest request body the service accepts, in bytes. A renew request is about 250 bytes.
pub const MAX_REQUEST_BYTES: usize = 1024;

/// Octets of a nonce: 256 random bits.
pub const NONCE_BYTES: usize = 32;

/// Length of a nonce as text: [`NONCE_BYTES`] in base64url without padding.
pub const NONCE_LEN: usize = 43;

/// How long a challenge may be redeemed, in seconds, from when the service issued it.
pub const CHALLENGE_TTL_SECS: u64 = 30;

/// Largest DER ECDSA P-256 signature (`SEQUENCE { r INTEGER, s INTEGER }`), in bytes.
pub const MAX_SIGNATURE_BYTES: usize = 72;

/// Longest signature as text: [`MAX_SIGNATURE_BYTES`] in standard base64 with padding.
pub const MAX_SIGNATURE_LEN: usize = MAX_SIGNATURE_BYTES.div_ceil(3) * 4;

/// The first line of every signing input. It names the protocol and its version, and makes the
/// signed bytes impossible to mistake for what the same TPM key signs in TLS: TLS 1.3's
/// CertificateVerify starts with 64 bytes 0x20, TLS 1.2 signs handshake messages that start with
/// 0x01 (docs/feasibility/2026-10-12-tpm-poc.md, "換發挑戰簽章").
pub const SIGNING_INPUT_PREFIX: &str = "trustiot-v2 challenge v1\n";

/// What a challenge may be redeemed for. Only certificate renewal exists; a later purpose (for
/// example a rebinding) gets its own value, and a challenge of one purpose is refused for another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Purpose {
    /// A new gateway certificate (`cert-renewal`).
    #[serde(rename = "cert-renewal")]
    CertRenewal,
}

impl Purpose {
    /// The purpose as the wire and the signing input write it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CertRenewal => "cert-renewal",
        }
    }
}

/// A request to [`RENEWAL_PATH`], told apart by its `step`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "step")]
pub enum RenewalRequest {
    /// `{"step": "challenge", "hardware_id": ...}`.
    #[serde(rename = "challenge")]
    Challenge(ChallengeRequest),
    /// `{"step": "renew", "hardware_id": ..., "nonce": ..., "signature": ...}`.
    #[serde(rename = "renew")]
    Renew(RenewRequest),
}

/// Step 1: a challenge for the gateway's hardware ID. The answer looks the same whether or not
/// the hardware ID is registered, bound or decommissioned: a refusal comes only at step 2.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChallengeRequest {
    /// The gateway's hardware ID: its certificate's common name.
    pub hardware_id: GatewayId,
}

/// The answer to step 1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Challenge {
    /// The hardware ID the challenge is bound to: the request's.
    pub hardware_id: GatewayId,
    /// What it may be redeemed for.
    pub purpose: Purpose,
    /// [`NONCE_BYTES`] random bytes, base64url without padding ([`NONCE_LEN`] characters).
    pub nonce: String,
    /// Unix seconds by the service's clock: the challenge may be redeemed until then.
    pub expires: u64,
}

impl Challenge {
    /// The bytes the gateway signs to redeem this challenge; see [`signing_input`].
    #[must_use]
    pub fn signing_input(&self) -> Vec<u8> {
        signing_input(&self.hardware_id, self.purpose, &self.nonce, self.expires)
    }
}

/// Step 2: the signature over the challenge's signing input. The service rebuilds the signing
/// input from what it stored for the nonce, never from this request, and verifies the signature
/// with the TPM public key registered at the factory for `hardware_id`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenewRequest {
    /// The gateway's hardware ID; must be the one the challenge was issued for.
    pub hardware_id: GatewayId,
    /// The challenge's nonce, as [`Challenge::nonce`] had it.
    pub nonce: String,
    /// ECDSA P-256 with SHA-256 over [`Challenge::signing_input`]: the DER `ECDSA-Sig-Value` (at
    /// most [`MAX_SIGNATURE_BYTES`]) in standard base64 with padding.
    pub signature: String,
}

/// The answer to step 2, with the same fields and formats as the factory registration's answer
/// (`contract::certs::GatewayRegistration`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenewedCertificate {
    /// The gateway's hardware ID: the certificate's common name.
    pub hardware_id: GatewayId,
    /// The new certificate's serial number, lower-case hex.
    pub serial: String,
    /// The issuing CA that signed it: the SHA-256 of its public key, lower-case hex.
    pub ca_key_id: String,
    /// When the certificate becomes valid, RFC 3339 in UTC.
    pub not_before: String,
    /// When it expires, RFC 3339 in UTC.
    pub not_after: String,
    /// The certificate, PEM: subject `CN=<hardware_id>`, the registered public key, extended key
    /// usage client authentication only.
    pub certificate: String,
    /// The certificate, then the issuing CA certificate that signed it, PEM: the chain the gateway
    /// presents to the MQTT broker.
    pub certificate_chain: String,
    /// The root CA certificate, PEM: the gateway's trust anchor for the broker.
    pub root_certificate: String,
}

/// The bytes a gateway signs to redeem a challenge:
///
/// ```text
/// trustiot-v2 challenge v1\n
/// device=<hardware ID>\n
/// purpose=<purpose>\n
/// nonce=<nonce, base64url>\n
/// expires=<Unix seconds, decimal>\n
/// ```
///
/// No field can contain a newline: the hardware ID rule allows none, the purpose is fixed, the
/// nonce is base64url and the expiry a number.
#[must_use]
pub fn signing_input(
    hardware_id: &GatewayId,
    purpose: Purpose,
    nonce: &str,
    expires: u64,
) -> Vec<u8> {
    format!(
        "{SIGNING_INPUT_PREFIX}device={}\npurpose={}\nnonce={nonce}\nexpires={expires}\n",
        hardware_id.as_str(),
        purpose.as_str()
    )
    .into_bytes()
}

/// Whether `text` is a nonce as [`Challenge::nonce`] has it: exactly [`NONCE_LEN`] characters of
/// the base64url alphabet that encode [`NONCE_BYTES`] bytes canonically (the last character's
/// two spare bits are zero), so one nonce has exactly one spelling.
#[must_use]
pub fn is_nonce(text: &str) -> bool {
    const LAST: &[u8] = b"AEIMQUYcgkosw048";
    let bytes = text.as_bytes();
    bytes.len() == NONCE_LEN
        && bytes.iter().all(|&byte| is_base64url(byte))
        && bytes.last().is_some_and(|last| LAST.contains(last))
}

/// Whether `text` could be a signature as [`RenewRequest::signature`] has it: standard base64
/// with padding, a multiple of 4 characters from 4 to [`MAX_SIGNATURE_LEN`]. Whether it decodes
/// to a DER signature that verifies is the service's check.
#[must_use]
pub fn is_signature_text(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_SIGNATURE_LEN || !bytes.len().is_multiple_of(4) {
        return false;
    }
    let padding = bytes.iter().rev().take_while(|&&byte| byte == b'=').count();
    padding <= 2
        && bytes[..bytes.len() - padding]
            .iter()
            .all(|&byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
}

fn is_base64url(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const NONCE: &str = "q83vEjRWeJCrze8SNFZ4kKvN7xI0VniQq83vEjRWeJA";

    fn gateway() -> GatewayId {
        GatewayId::new("tg-0001").expect("a hardware ID")
    }

    #[test]
    fn the_signing_input_is_the_feasibility_tests_format() {
        let challenge = Challenge {
            hardware_id: gateway(),
            purpose: Purpose::CertRenewal,
            nonce: NONCE.to_owned(),
            expires: 1_761_000_030,
        };
        assert_eq!(
            String::from_utf8(challenge.signing_input()).expect("UTF-8"),
            "trustiot-v2 challenge v1\ndevice=tg-0001\npurpose=cert-renewal\n\
             nonce=q83vEjRWeJCrze8SNFZ4kKvN7xI0VniQq83vEjRWeJA\nexpires=1761000030\n"
        );
    }

    #[test]
    fn the_requests_and_answers_have_their_json_shape() {
        let challenge = RenewalRequest::Challenge(ChallengeRequest {
            hardware_id: gateway(),
        });
        assert_eq!(
            serde_json::to_value(&challenge).expect("encode"),
            json!({"step": "challenge", "hardware_id": "tg-0001"})
        );
        let renew = RenewalRequest::Renew(RenewRequest {
            hardware_id: gateway(),
            nonce: NONCE.to_owned(),
            signature: "MEUCIQ==".to_owned(),
        });
        let encoded = json!({
            "step": "renew",
            "hardware_id": "tg-0001",
            "nonce": NONCE,
            "signature": "MEUCIQ==",
        });
        assert_eq!(serde_json::to_value(&renew).expect("encode"), encoded);
        assert_eq!(
            serde_json::from_value::<RenewalRequest>(encoded).expect("decode"),
            renew
        );
        // Unknown fields are ignored, an unknown step or an invalid hardware ID is not.
        let extra = json!({"step": "challenge", "hardware_id": "tg-0001", "agent": "1.2.0"});
        assert_eq!(
            serde_json::from_value::<RenewalRequest>(extra).expect("decode"),
            challenge
        );
        for refused in [
            json!({"step": "rebind", "hardware_id": "tg-0001"}),
            json!({"hardware_id": "tg-0001"}),
            json!({"step": "challenge", "hardware_id": "tg/0001"}),
            json!({"step": "renew", "hardware_id": "tg-0001", "nonce": NONCE}),
        ] {
            assert!(
                serde_json::from_value::<RenewalRequest>(refused.clone()).is_err(),
                "{refused}"
            );
        }

        let answer = json!({
            "hardware_id": "tg-0001",
            "purpose": "cert-renewal",
            "nonce": NONCE,
            "expires": 1_761_000_030_u64,
        });
        let decoded: Challenge = serde_json::from_value(answer.clone()).expect("decode");
        assert_eq!(decoded.purpose, Purpose::CertRenewal);
        assert_eq!(serde_json::to_value(&decoded).expect("encode"), answer);
    }

    #[test]
    fn nonces_have_one_spelling() {
        let head = |len: usize| NONCE.chars().take(len).collect::<String>();
        assert!(is_nonce(NONCE));
        assert!(is_nonce(&"A".repeat(NONCE_LEN)));
        assert!(is_nonce(&format!("{}_-w", "z".repeat(NONCE_LEN - 3))));
        for refused in [
            String::new(),
            head(NONCE_LEN - 1),
            format!("{NONCE}A"),
            // The last character's two spare bits are set: another spelling of a nonce.
            format!("{}B", head(NONCE_LEN - 1)),
            format!("{}=", head(NONCE_LEN - 1)),
            format!("{}+A", head(NONCE_LEN - 2)),
            format!("{}\nA", head(NONCE_LEN - 2)),
        ] {
            assert!(!is_nonce(&refused), "{refused:?}");
        }
    }

    #[test]
    fn signatures_are_padded_base64_of_at_most_72_bytes() {
        assert_eq!(MAX_SIGNATURE_LEN, 96);
        for accepted in ["MEUCIQ==", "MEQCIA+/", &"A".repeat(MAX_SIGNATURE_LEN)] {
            assert!(is_signature_text(accepted), "{accepted}");
        }
        for refused in [
            "",
            "MEUCIQ=",
            "MEUCI===",
            "MEUC-Q==",
            "ME=CIQ==",
            &"A".repeat(MAX_SIGNATURE_LEN + 4),
        ] {
            assert!(!is_signature_text(refused), "{refused}");
        }
    }
}
