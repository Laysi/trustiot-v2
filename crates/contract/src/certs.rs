//! The certs service's client API (`services/certs`, Laysi/trustiot-v2#534): production
//! workstation certificates, which only platform administrators issue and revoke, through the
//! ingress with a session cookie (`/api/certs/v1/...`). The service's README ("工作站憑證")
//! describes the profile and the steps. Also the MQTT broker's server certificate
//! ([`BrokerCertificate`], Laysi/trustiot-v2#557), on an internal port only the broker reaches.
//!
//! The requests refuse unknown fields: a client that sends a subject, an extension or a
//! validity of its own learns at once that the CA decides them. The response may gain optional
//! fields; clients ignore the ones they do not know.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The body of `POST /api/certs/v1/workstation-certificates`. Unknown fields are refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueWorkstationCertificate {
    /// The workstation's name: 1-48 ASCII letters, digits, `.`, `_` or `-`, starting with a
    /// letter or digit. The certificate's subject is `CN=workstation:<name>`.
    #[schema(
        pattern = "^[A-Za-z0-9][A-Za-z0-9._-]{0,47}$",
        max_length = 48,
        example = "line-a-ws-01"
    )]
    pub workstation: String,
    /// A PKCS #10 certificate signing request, PEM (`-----BEGIN CERTIFICATE REQUEST-----`), at
    /// most 4096 bytes as DER, signed with ecdsa-with-SHA256 by the workstation's ECDSA P-256
    /// key. Only its public key is used, once its signature shows the workstation holds the
    /// private key: the CA decides the subject, the extensions and the validity, and the private
    /// key never leaves the workstation.
    pub certificate_request: String,
}

/// The body of `POST /api/certs/v1/workstation-certificates/{serial}/revoke`. Unknown fields are
/// refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RevokeWorkstationCertificate {
    /// Why it is revoked. Absent leaves the reason out of the CRL, as RFC 5280 recommends over
    /// `unspecified`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RevocationReason>,
}

/// A reason a workstation certificate can be revoked for: the RFC 5280 CRLReason codes that
/// apply to an end entity, without `certificateHold` (a revocation here is final).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RevocationReason {
    /// unspecified (0).
    Unspecified,
    /// keyCompromise (1): the private key may be known to someone else.
    KeyCompromise,
    /// affiliationChanged (3).
    AffiliationChanged,
    /// superseded (4): a new certificate replaces it, for example a renewal.
    Superseded,
    /// cessationOfOperation (5): the workstation is retired.
    CessationOfOperation,
    /// privilegeWithdrawn (9).
    PrivilegeWithdrawn,
}

impl RevocationReason {
    /// Every reason.
    pub const ALL: [Self; 6] = [
        Self::Unspecified,
        Self::KeyCompromise,
        Self::AffiliationChanged,
        Self::Superseded,
        Self::CessationOfOperation,
        Self::PrivilegeWithdrawn,
    ];

    /// The RFC 5280 CRLReason code, as the ledger and the CRL record it.
    #[must_use]
    pub const fn code(self) -> i16 {
        match self {
            Self::Unspecified => 0,
            Self::KeyCompromise => 1,
            Self::AffiliationChanged => 3,
            Self::Superseded => 4,
            Self::CessationOfOperation => 5,
            Self::PrivilegeWithdrawn => 9,
        }
    }

    /// The reason of a CRLReason code; `None` for the codes this API does not use.
    #[must_use]
    pub fn from_code(code: i16) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| reason.code() == code)
    }

    /// The name in JSON, for example `key_compromise`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::KeyCompromise => "key_compromise",
            Self::AffiliationChanged => "affiliation_changed",
            Self::Superseded => "superseded",
            Self::CessationOfOperation => "cessation_of_operation",
            Self::PrivilegeWithdrawn => "privilege_withdrawn",
        }
    }

    /// The reason named `name` in JSON.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| reason.as_str() == name)
    }
}

/// A workstation certificate as the ledger records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct WorkstationCertificate {
    /// The serial number, lower-case hex (32 digits for the certificates this service signs):
    /// the `{serial}` of the revoke request. Keep it; it is how the certificate is revoked.
    #[schema(
        pattern = "^[0-9a-f]{2,40}$",
        example = "4f3a9c1e2b7d6058a1c3e5f708192a3b"
    )]
    pub serial: String,
    /// The workstation's name.
    pub workstation: String,
    /// The certificate's common name: `workstation:<workstation>`.
    pub subject: String,
    /// The issuing CA that signed it: the SHA-256 of its public key, 64 lower-case hex digits.
    #[schema(pattern = "^[0-9a-f]{64}$")]
    pub ca_key_id: String,
    /// When it becomes valid: five minutes before it was signed.
    pub not_before: DateTime<Utc>,
    /// When it expires; a renewal is another issue request before then.
    pub not_after: DateTime<Utc>,
    /// The certificate, PEM. It goes to the workstation, which keeps it with its private key.
    pub certificate: String,
    /// When it was revoked; absent while it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<DateTime<Utc>>,
    /// Why it was revoked, when a reason was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revocation_reason: Option<RevocationReason>,
}

/// The MQTT broker's server certificate, as `POST /internal/certs/v1/broker-certificate` answers
/// it (Laysi/trustiot-v2#557; services/certs/README.md, "broker 伺服器憑證"). An internal
/// endpoint on a port the mesh opens to the broker alone, so not in the OpenAPI document. The
/// request body is the CSR itself, DER, `Content-Type: application/pkcs10`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerCertificate {
    /// The serial number, lower-case hex.
    pub serial: String,
    /// The issuing CA that signed it: the SHA-256 of its public key, 64 lower-case hex digits.
    pub ca_key_id: String,
    /// Its DNS names (subject alternative names): the fixed name every client verifies first,
    /// then the broker's Service name when the deployment has one.
    pub names: Vec<String>,
    /// When it becomes valid: five minutes before it was signed.
    pub not_before: DateTime<Utc>,
    /// When it expires; renew once half of its validity has passed.
    pub not_after: DateTime<Utc>,
    /// The certificate, then the issuing CA certificate that signed it, PEM: the chain a TLS
    /// server sends (`tls.crt` of a `kubernetes.io/tls` Secret). Clients trust the root, which
    /// is not in it.
    pub certificate_chain: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_round_trip_by_name_and_code() {
        for reason in RevocationReason::ALL {
            assert_eq!(RevocationReason::from_code(reason.code()), Some(reason));
            assert_eq!(RevocationReason::parse(reason.as_str()), Some(reason));
            assert_eq!(
                serde_json::to_value(reason).unwrap(),
                serde_json::Value::String(reason.as_str().to_owned())
            );
        }
        // certificateHold, removeFromCRL and the CA reasons are not used here.
        for code in [2, 6, 7, 8, 10, -1, 11] {
            assert_eq!(RevocationReason::from_code(code), None, "{code}");
        }
    }

    #[test]
    fn requests_refuse_unknown_fields() {
        let issue: Result<IssueWorkstationCertificate, _> = serde_json::from_str(
            r#"{"workstation": "ws-1", "certificate_request": "x", "subject": "gw-1"}"#,
        );
        assert!(issue.is_err());
        let revoke: Result<RevokeWorkstationCertificate, _> =
            serde_json::from_str(r#"{"reason": "superseded", "at": "now"}"#);
        assert!(revoke.is_err());
        let empty: RevokeWorkstationCertificate = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.reason, None);
    }
}
