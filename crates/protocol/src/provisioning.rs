//! The factory provisioning report (decision 12; Laysi/trustiot-v2#613): what the gateway agent's
//! `trustiot-agent provision` subcommand prints on standard output as one JSON object, and what
//! the production workstation tool (`factory/workstation`, Laysi/trustiot-v2#616) reads over
//! ssh before it calls the registration API. The workstation's credentials never reach the
//! gateway, and the gateway never calls the API itself: the report is the only thing that
//! travels, and it holds public data only.
//!
//! The JSON encoding lives in the agent and the workstation (`serde_json`); this crate has the
//! types, so both sides read and write the same fields. This crate's README, "出廠佈建報告", is
//! the specification for humans.
//!
//! - [`ProvisioningReport::version`] is [`REPORT_VERSION`]; a report of any other version is
//!   refused when it is read. A change that a reader must understand starts a new version.
//! - Unknown fields are ignored, as in [`crate::renewal`], so a newer agent's extra field does
//!   not break an older workstation.
//! - Nothing in the report says that something was verified or is trusted: the EK certificate
//!   is reported as [`EkCertificate::ForThisKey`], which means only that its public key is the
//!   EK's (services/certs/README.md, 「EK 只記錄、比對，不驗證」).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ids::GatewayId;

/// The report format's version.
pub const REPORT_VERSION: u16 = 1;

/// [`REPORT_VERSION`] in a report: written as that number, and any other number is refused when
/// a report is read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ReportVersion;

impl Serialize for ReportVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u16(REPORT_VERSION)
    }
}

impl<'de> Deserialize<'de> for ReportVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let version = u64::deserialize(deserializer)?;
        if version == u64::from(REPORT_VERSION) {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported provisioning report version {version}; this build reads {REPORT_VERSION}"
            )))
        }
    }
}

/// What `trustiot-agent provision` found and did, as one JSON object on standard output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisioningReport {
    /// [`REPORT_VERSION`].
    pub version: ReportVersion,
    /// The hardware ID the subcommand was given: the CSR's common name.
    pub hardware_id: GatewayId,
    /// The persistent TPM handle of the gateway key.
    pub key_handle: KeyHandle,
    /// The gateway key at [`Self::key_handle`].
    pub key: GatewayKey,
    /// The TPM's ECC NIST P-256 endorsement key, PEM (`-----BEGIN PUBLIC KEY-----`, an X.509
    /// SubjectPublicKeyInfo): what the registration API's `endorsement_key` takes.
    pub endorsement_key: String,
    /// The EK certificate the TPM holds, if any.
    pub ek_certificate: EkCertificate,
}

/// The gateway key at the report's handle, told apart by `state`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GatewayKey {
    /// `{"state": "absent"}`: the handle is empty. Only `provision --inspect` reports this; it
    /// creates nothing.
    Absent,
    /// `{"state": "present", "created": ..., "public_key": ..., "certificate_request": ...}`:
    /// the handle holds the gateway key.
    Present {
        /// True when this run created and persisted the key, false when it was already there.
        created: bool,
        /// The key's public key, PEM (`-----BEGIN PUBLIC KEY-----`, an ECDSA P-256
        /// SubjectPublicKeyInfo).
        public_key: String,
        /// A PKCS #10 certificate signing request, PEM (`-----BEGIN CERTIFICATE REQUEST-----`):
        /// subject `CN=<hardware_id>`, the key's public key, signed with ecdsa-with-SHA256 by
        /// the key in the TPM. What the registration API's `certificate_request` takes.
        certificate_request: String,
    },
}

/// The EK certificate at NV index `0x01C0000A`, told apart by `state`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EkCertificate {
    /// `{"state": "absent"}`: the TPM has no EK certificate.
    Absent,
    /// `{"state": "for_this_key", "certificate": ...}`: the certificate's SubjectPublicKeyInfo is
    /// byte for byte [`ProvisioningReport::endorsement_key`]. That is all it means: neither its
    /// signature nor its chain to a TPM manufacturer was checked, so it is no evidence that the
    /// TPM is genuine.
    ForThisKey {
        /// The certificate, PEM (`-----BEGIN CERTIFICATE-----`).
        certificate: String,
    },
}

/// A persistent TPM object handle (`0x81000000` to `0x81ffffff`), written as `0x` and eight
/// lower-case hex digits, for example `0x81000100`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyHandle(u32);

/// Why a value is not a [`KeyHandle`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyHandleError(String);

impl fmt::Display for KeyHandleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for KeyHandleError {}

impl KeyHandle {
    /// The first persistent handle.
    pub const FIRST: u32 = 0x8100_0000;
    /// The last persistent handle.
    pub const LAST: u32 = 0x81ff_ffff;

    /// `handle`, if it is a persistent handle.
    ///
    /// # Errors
    ///
    /// When it is outside `0x81000000` to `0x81ffffff`.
    pub fn new(handle: u32) -> Result<Self, KeyHandleError> {
        if (Self::FIRST..=Self::LAST).contains(&handle) {
            Ok(Self(handle))
        } else {
            Err(KeyHandleError(format!(
                "{handle:#010x} is not a persistent TPM handle (0x81000000 to 0x81ffffff)"
            )))
        }
    }

    /// The handle.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for KeyHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#010x}", self.0)
    }
}

impl FromStr for KeyHandle {
    type Err = KeyHandleError;

    /// `0x` (or `0X`) and one to eight hex digits, in either case; the value must be a persistent
    /// handle.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || {
            let excerpt: String = text.chars().take(32).collect();
            KeyHandleError(format!(
                "invalid TPM handle {excerpt:?}: must be 0x and 1 to 8 hex digits, for example 0x81000100"
            ))
        };
        let digits = text
            .strip_prefix("0x")
            .or_else(|| text.strip_prefix("0X"))
            .ok_or_else(invalid)?;
        if digits.is_empty() || digits.len() > 8 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid());
        }
        let handle = u32::from_str_radix(digits, 16).map_err(|_| invalid())?;
        Self::new(handle)
    }
}

impl Serialize for KeyHandle {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for KeyHandle {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn report(key: GatewayKey, ek_certificate: EkCertificate) -> ProvisioningReport {
        ProvisioningReport {
            version: ReportVersion,
            hardware_id: GatewayId::new("tg-0001").expect("a hardware ID"),
            key_handle: KeyHandle::new(0x8100_0100).expect("a persistent handle"),
            key,
            endorsement_key: "-----BEGIN PUBLIC KEY-----\nEK\n-----END PUBLIC KEY-----\n"
                .to_owned(),
            ek_certificate,
        }
    }

    #[test]
    fn the_report_has_its_json_shape() {
        let provisioned = report(
            GatewayKey::Present {
                created: true,
                public_key: "KEY".to_owned(),
                certificate_request: "CSR".to_owned(),
            },
            EkCertificate::ForThisKey {
                certificate: "CERT".to_owned(),
            },
        );
        let encoded = json!({
            "version": 1,
            "hardware_id": "tg-0001",
            "key_handle": "0x81000100",
            "key": {
                "state": "present",
                "created": true,
                "public_key": "KEY",
                "certificate_request": "CSR",
            },
            "endorsement_key": "-----BEGIN PUBLIC KEY-----\nEK\n-----END PUBLIC KEY-----\n",
            "ek_certificate": {"state": "for_this_key", "certificate": "CERT"},
        });
        assert_eq!(serde_json::to_value(&provisioned).expect("encode"), encoded);
        assert_eq!(
            serde_json::from_value::<ProvisioningReport>(encoded).expect("decode"),
            provisioned
        );

        let inspected = report(GatewayKey::Absent, EkCertificate::Absent);
        let encoded = serde_json::to_value(&inspected).expect("encode");
        assert_eq!(encoded["key"], json!({"state": "absent"}));
        assert_eq!(encoded["ek_certificate"], json!({"state": "absent"}));
        assert_eq!(
            serde_json::from_value::<ProvisioningReport>(encoded).expect("decode"),
            inspected
        );
    }

    #[test]
    fn no_field_or_value_claims_verification_or_trust() {
        let text = serde_json::to_string(&report(
            GatewayKey::Present {
                created: false,
                public_key: String::new(),
                certificate_request: String::new(),
            },
            EkCertificate::ForThisKey {
                certificate: String::new(),
            },
        ))
        .expect("encode")
        .to_lowercase();
        for word in ["verif", "trust", "valid", "genuine", "authentic"] {
            assert!(!text.contains(word), "{word} in {text}");
        }
    }

    #[test]
    fn unknown_fields_are_ignored_and_other_versions_refused() {
        let mut encoded = serde_json::to_value(report(GatewayKey::Absent, EkCertificate::Absent))
            .expect("encode");
        encoded["agent"] = json!("1.2.0");
        encoded["key"]["note"] = json!("extra");
        assert!(serde_json::from_value::<ProvisioningReport>(encoded.clone()).is_ok());

        for (field, value) in [
            ("version", json!(2)),
            ("version", json!(0)),
            ("version", json!("1")),
            ("hardware_id", json!("tg/0001")),
            ("key_handle", json!("0x81000100 ")),
            ("key_handle", json!(2_164_261_120_u32)),
            ("key", json!({"state": "unknown"})),
            ("key", json!({"state": "present", "created": true})),
            (
                "ek_certificate",
                json!({"state": "verified", "certificate": "CERT"}),
            ),
            ("ek_certificate", json!({"state": "for_this_key"})),
        ] {
            let mut refused = encoded.clone();
            refused[field] = value;
            assert!(
                serde_json::from_value::<ProvisioningReport>(refused.clone()).is_err(),
                "{refused}"
            );
        }
        let mut missing = encoded;
        missing
            .as_object_mut()
            .expect("an object")
            .remove("endorsement_key");
        assert!(serde_json::from_value::<ProvisioningReport>(missing).is_err());
    }

    #[test]
    fn key_handles_are_persistent_handles_in_hex() {
        for (text, handle) in [
            ("0x81000100", 0x8100_0100),
            ("0X81000100", 0x8100_0100),
            ("0x81FFFFFF", 0x81ff_ffff),
            ("0x81000000", 0x8100_0000),
        ] {
            let parsed: KeyHandle = text.parse().expect(text);
            assert_eq!(parsed.get(), handle, "{text}");
        }
        assert_eq!(
            KeyHandle::new(0x8100_0100).expect("persistent").to_string(),
            "0x81000100"
        );
        for refused in [
            "",
            "0x",
            "81000100",
            "0x810001000",
            "0x8100010g",
            "+0x81000100",
            "0x+1000100",
            " 0x81000100",
            "0x80000000",
            "0x82000000",
            "0x1",
            "0x40000001",
        ] {
            assert!(refused.parse::<KeyHandle>().is_err(), "{refused:?}");
        }
        assert!(KeyHandle::new(0x8000_0000).is_err());
        assert!(KeyHandle::new(0x8200_0000).is_err());
    }
}
