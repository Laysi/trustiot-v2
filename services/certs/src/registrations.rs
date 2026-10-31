//! Factory registration (Laysi/trustiot-v2#79, rebuild checklist D1, decision 12;
//! services/certs/README.md, "出廠登錄"): a production workstation registers a gateway's hardware
//! ID, its TPM key's public key and its TPM's endorsement key, and gets the gateway's initial
//! certificate. It is reached through the ingress like the workstation certificate API: a
//! platform administrator signs in on the workstation, and the identity service turns the
//! session cookie into an internal identity token addressed to this service.
//!
//! The request needs two identities, and either alone is refused:
//!
//! - **the platform administrator** (A2), from the internal identity token, as in
//!   [`crate::workstations`];
//! - **the workstation**, from a signature over the request with its workstation certificate's
//!   key (Laysi/trustiot-v2#534): the ingress terminates TLS before the service (Cloudflare and
//!   the mesh), so a client certificate cannot reach it, and the workstation signs instead. The
//!   headers are [`WORKSTATION_CERTIFICATE_HEADER`] (the certificate, DER, base64),
//!   [`WORKSTATION_TIMESTAMP_HEADER`], [`WORKSTATION_NONCE_HEADER`] and
//!   [`WORKSTATION_SIGNATURE_HEADER`] (ECDSA P-256 with SHA-256 over
//!   [`workstation_signing_input`]: the method, the path, the timestamp, the nonce and the
//!   body's SHA-256). The certificate must chain to a loaded issuing CA, be valid now, have the
//!   workstation profile (the TrustIoT workstation purpose as its extended key usage, so a gateway
//!   certificate is refused), be in the ledger and not revoked; the timestamp within
//!   [`WORKSTATION_CLOCK_SKEW_SECS`] of the service's clock; and the nonce new for that
//!   certificate.
//!
//! | Request | Success |
//! | --- | --- |
//! | `POST /api/certs/v1/gateway-registrations`, a new hardware ID | 201 [`GatewayRegistration`] |
//! | The same hardware ID, public key and endorsement key again | 200 [`GatewayRegistration`], the record and certificate of the first registration |
//!
//! Errors follow M1, checked in this order:
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | Not a platform administrator, or a token another service exchanged (`act`) | 403 `permission_denied` |
//! | `Content-Type` not `application/json`, a body over [`MAX_BODY_BYTES`] or not a JSON object | 400 `invalid_request` (keys `gateway_registration.body_too_large`, `gateway_registration.body_invalid`) |
//! | No issuing CA is loaded, so no workstation can be checked | 503 `unavailable`, key `gateway_registration.ca_not_ready` |
//! | The workstation signature is missing or does not hold | 403 `permission_denied`, key `gateway_registration.workstation_refused`, `details.reason` one of [`WorkstationRefusal`] |
//! | An unknown or missing field, a hardware ID, CSR or endorsement key that breaks its rule | 422 `validation_failed`, one field error per problem |
//! | The workstation certificate is revoked or not in the ledger, or the nonce was used | 403 as above (`revoked`, `untrusted`, `replayed`) |
//! | The hardware ID is registered with another public key or endorsement key | 409 `conflict`, key `gateway_registration.public_key_mismatch` or `.endorsement_key_mismatch` |
//! | The hardware ID was decommissioned | 409, key `gateway_registration.decommissioned` |
//! | A new hardware ID with a public key already registered, or an endorsement key of an active gateway | 409, key `gateway_registration.public_key_in_use` or `.endorsement_key_in_use` |
//! | No issuing CA can sign now | 503, key `gateway_registration.ca_not_ready` |
//! | The database failed | 503 `unavailable`; nothing was registered |
//!
//! A new registration signs the gateway certificate, records it in the ledger, records the
//! registration and writes one `certs.gateway_registered` event (platform-wide, the
//! administrator as the actor) in one transaction of the platform role, so the event exists
//! exactly when the registration does. A repeated registration writes nothing but the nonce, so
//! retrying after a lost response neither signs another certificate nor writes another event.
//! Registrations of the same hardware ID run one after the other ([`LOCK_REGISTRATION`]), so
//! two identical ones that arrive at once get the same answers as one after the other: 201,
//! then 200 with the same certificate.
//! Both answers carry the root certificate and the root CRL adopted with it, if any
//! ([`CaState::authority_and_root_crl`], one snapshot, so the CRL in an answer was checked
//! against the root in that same answer; Laysi/trustiot-v2#832).
//! Every response carries `cache-control: no-store`. Each registration logs one INFO line with
//! the administrator's user id, the workstation, the hardware ID, the serial number and the CA
//! key id; never the CSR, the keys or the certificate.

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{FromRef, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use axum::routing::post;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use contract::certs::{
    GatewayRegistration, RegisterGateway, WORKSTATION_CERTIFICATE_HEADER,
    WORKSTATION_CLOCK_SKEW_SECS, WORKSTATION_NONCE_HEADER, WORKSTATION_SIGNATURE_HEADER,
    WORKSTATION_TIMESTAMP_HEADER, workstation_signing_input,
};
use contract::event::Actor;
use contract::events::certs::{GATEWAY_REGISTRATION_AGGREGATE, GatewayRegistered};
use contract::{ErrorBody, ErrorCode, FieldError, MessageKey};
use outbox::Aggregate;
use protocol::GatewayId;
use service_kit::identity::{IdentityVerifier, UserIdentity};
use service_kit::tenant::{PlatformActor, PlatformDb, PlatformTransaction, TenantDbError};
use sha2::{Digest, Sha256};
use sqlx::Row as _;
use sqlx::postgres::PgRow;
use x509_parser::asn1_rs::BitString;
use x509_parser::certificate::X509Certificate;
use x509_parser::oid_registry::{
    OID_EC_P256, OID_KEY_TYPE_EC_PUBLIC_KEY, OID_NIST_EC_P384, OID_PKCS1_RSAENCRYPTION,
    OID_SIG_ECDSA_WITH_SHA256,
};
use x509_parser::prelude::FromDer;
use x509_parser::public_key::PublicKey;
use x509_parser::x509::{AlgorithmIdentifier, SubjectPublicKeyInfo};

use crate::api::{self, BodyKeys, Problems, error, ok, refuse};
use crate::authority::{Authority, CaState, unix_now};
use crate::dynsec::BrokerSync;
use crate::issue::{
    self, IssueError, IssuedCertificate, MAX_REQUEST_BYTES, Refusal, WORKSTATION_PURPOSE_DER,
    WORKSTATION_SUBJECT_PREFIX, WorkstationName,
};
use crate::pem;
use crate::root_crl::RootCrl;

/// The collection, as the ingress gateway forwards it (no prefix is stripped).
pub const REGISTRATIONS_PATH: &str = "/api/certs/v1/gateway-registrations";

/// The largest body accepted: a CSR of [`MAX_REQUEST_BYTES`] and an endorsement key of
/// [`MAX_ENDORSEMENT_KEY_BYTES`] as PEM, and a hardware ID, fit with room.
pub const MAX_BODY_BYTES: usize = 16 * 1024;

/// Largest endorsement key accepted, DER: an RSA 4096 key is about 550 bytes.
pub const MAX_ENDORSEMENT_KEY_BYTES: usize = 2048;

/// Largest workstation certificate accepted, DER (a workstation certificate is about 500 bytes).
pub const MAX_WORKSTATION_CERTIFICATE_BYTES: usize = 4096;

/// Largest workstation signature accepted, DER: an ECDSA P-256 `Ecdsa-Sig-Value` is at most 72
/// bytes.
const MAX_SIGNATURE_BYTES: usize = 128;

/// How long after its timestamp a nonce is kept: past [`WORKSTATION_CLOCK_SKEW_SECS`] its
/// request is refused as stale anyway; a minute more for the clock between the check and the
/// insert.
const NONCE_RETENTION_SECS: i64 = WORKSTATION_CLOCK_SKEW_SECS + 60;

/// Names the platform role's transactions in its INFO log line (`platform database access`).
const PURPOSE: &str = "gateway-registration";

/// Serializes the registrations of one hardware ID (`$1`), held until the transaction ends
/// (Laysi/trustiot-v2#782). A registration takes it before it looks the hardware ID up, so a
/// concurrent registration of the same hardware ID has either committed before that lookup,
/// which then finds it (a repeat or a mismatch), or waits until this transaction ends. Without
/// it, READ COMMITTED let the second of two identical registrations miss the first in the
/// lookup and then see it committed in the public key check, answering `public_key_in_use`
/// instead of the repeat's 200. Class `0x67777267` ("gwrg"), apart from the other two-key
/// classes of this service ([`issue::LOCK_WORKSTATION`], [`crate::crl::LOCK_CA_CRL`],
/// [`crate::gateways::LOCK_GATEWAY_CERTIFICATES`]); the hardware ID is hashed into the second
/// key, so a hash collision only makes two registrations wait for each other.
pub const LOCK_REGISTRATION: &str = "SELECT pg_advisory_xact_lock(1735881319, hashtext($1))";

/// The body is larger than [`MAX_BODY_BYTES`] (400); `details.max_bytes`.
pub const BODY_TOO_LARGE: MessageKey = MessageKey::new("gateway_registration.body_too_large");
/// The body is not a JSON object (400).
pub const BODY_INVALID: MessageKey = MessageKey::new("gateway_registration.body_invalid");
/// A field the request does not have (422).
pub const UNKNOWN_FIELD: MessageKey = MessageKey::new("gateway_registration.unknown_field");
/// A required field is missing or `null` (422).
pub const FIELD_REQUIRED: MessageKey = MessageKey::new("gateway_registration.field_required");
/// The hardware ID breaks the gateway ID rule (422); `details.max`.
pub const HARDWARE_ID_INVALID: MessageKey =
    MessageKey::new("gateway_registration.hardware_id_invalid");
/// The certificate request is not one PEM certificate signing request of at most
/// `details.max_bytes` bytes as DER (422).
pub const REQUEST_INVALID: MessageKey = MessageKey::new("gateway_registration.request_invalid");
/// The certificate request is for another key than ECDSA P-256, or not signed with
/// ecdsa-with-SHA256 (422).
pub const REQUEST_UNSUPPORTED: MessageKey =
    MessageKey::new("gateway_registration.request_unsupported");
/// The certificate request's signature does not verify with its own key (422).
pub const REQUEST_SIGNATURE_INVALID: MessageKey =
    MessageKey::new("gateway_registration.request_signature_invalid");
/// The endorsement key is not one PEM public key of at most `details.max_bytes` bytes as DER
/// (422).
pub const ENDORSEMENT_KEY_INVALID: MessageKey =
    MessageKey::new("gateway_registration.endorsement_key_invalid");
/// The endorsement key is not RSA of 2048, 3072 or 4096 bits, nor ECDSA P-256 or P-384 (422).
pub const ENDORSEMENT_KEY_UNSUPPORTED: MessageKey =
    MessageKey::new("gateway_registration.endorsement_key_unsupported");
/// The request's workstation identity does not hold (403); `details.reason`.
pub const WORKSTATION_REFUSED: MessageKey =
    MessageKey::new("gateway_registration.workstation_refused");
/// The hardware ID is registered with another public key (409).
pub const PUBLIC_KEY_MISMATCH: MessageKey =
    MessageKey::new("gateway_registration.public_key_mismatch");
/// The hardware ID is registered with another endorsement key (409).
pub const ENDORSEMENT_KEY_MISMATCH: MessageKey =
    MessageKey::new("gateway_registration.endorsement_key_mismatch");
/// The hardware ID was decommissioned, and for now a decommissioned hardware ID is not
/// registered again (provisional until Laysi/trustiot-v2#115) (409).
pub const DECOMMISSIONED: MessageKey = MessageKey::new("gateway_registration.decommissioned");
/// Another hardware ID is registered with this public key (409).
pub const PUBLIC_KEY_IN_USE: MessageKey = MessageKey::new("gateway_registration.public_key_in_use");
/// An active gateway of another hardware ID has this endorsement key (409).
pub const ENDORSEMENT_KEY_IN_USE: MessageKey =
    MessageKey::new("gateway_registration.endorsement_key_in_use");
/// No issuing CA can check the workstation or sign now (503).
pub const CA_NOT_READY: MessageKey = MessageKey::new("gateway_registration.ca_not_ready");

const BODY_KEYS: BodyKeys = BodyKeys {
    invalid: BODY_INVALID,
    unknown_field: UNKNOWN_FIELD,
};

/// Why a request's workstation identity was refused: `details.reason` of
/// `gateway_registration.workstation_refused`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkstationRefusal {
    /// A `trustiot-workstation-*` header is missing.
    Missing,
    /// A header is repeated, or its value does not decode.
    Malformed,
    /// The certificate does not chain to a loaded issuing CA, or the ledger does not have it.
    Untrusted,
    /// The certificate is not valid now.
    Expired,
    /// The certificate is not a workstation certificate (a gateway certificate, say).
    NotWorkstation,
    /// The signature does not verify with the certificate's key.
    SignatureInvalid,
    /// The timestamp is more than [`WORKSTATION_CLOCK_SKEW_SECS`] from the service's clock.
    Stale,
    /// The certificate is revoked.
    Revoked,
    /// The nonce was already used with this certificate.
    Replayed,
}

impl WorkstationRefusal {
    /// The reason as `details.reason` names it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Malformed => "malformed",
            Self::Untrusted => "untrusted",
            Self::Expired => "expired",
            Self::NotWorkstation => "not_workstation",
            Self::SignatureInvalid => "signature_invalid",
            Self::Stale => "stale",
            Self::Revoked => "revoked",
            Self::Replayed => "replayed",
        }
    }

    fn body(self) -> ErrorBody {
        ErrorBody::new(ErrorCode::PermissionDenied)
            .with_message_key(WORKSTATION_REFUSED)
            .with_detail("reason", self.as_str())
    }
}

/// A workstation whose signature over the request holds; the ledger and the nonce are checked
/// in the registration's transaction.
#[derive(Debug)]
pub struct SignedBy {
    /// The workstation's name, from its certificate.
    pub name: WorkstationName,
    /// Its certificate's serial number.
    pub serial: Vec<u8>,
    /// Its certificate, DER.
    pub certificate: Vec<u8>,
    /// The request's timestamp, Unix seconds.
    pub timestamp: i64,
    /// The request's nonce.
    pub nonce: String,
}

/// Checks the workstation's signature over a request (method, path with query, body), at `now`
/// (Unix seconds), against the issuing CAs of `authority`. See the module documentation; the
/// ledger and the nonce are checked by the caller.
///
/// # Errors
///
/// The reason the workstation identity does not hold.
pub fn verify_workstation(
    headers: &HeaderMap,
    method: &str,
    path_and_query: &str,
    body: &[u8],
    authority: &Authority,
    now: i64,
) -> Result<SignedBy, WorkstationRefusal> {
    let certificate_text = one_header(headers, WORKSTATION_CERTIFICATE_HEADER)?;
    let signature_text = one_header(headers, WORKSTATION_SIGNATURE_HEADER)?;
    let timestamp_text = one_header(headers, WORKSTATION_TIMESTAMP_HEADER)?;
    let nonce = one_header(headers, WORKSTATION_NONCE_HEADER)?;

    let certificate = decode_base64(certificate_text, MAX_WORKSTATION_CERTIFICATE_BYTES)?;
    let signature = decode_base64(signature_text, MAX_SIGNATURE_BYTES)?;
    let timestamp = parse_timestamp(timestamp_text).ok_or(WorkstationRefusal::Malformed)?;
    if !is_nonce(nonce) {
        return Err(WorkstationRefusal::Malformed);
    }
    let (rest, parsed) =
        X509Certificate::from_der(&certificate).map_err(|_| WorkstationRefusal::Malformed)?;
    if !rest.is_empty() {
        return Err(WorkstationRefusal::Malformed);
    }
    if !signed_by_issuing_ca(&parsed, authority) {
        return Err(WorkstationRefusal::Untrusted);
    }
    let validity = parsed.validity();
    if !(validity.not_before.timestamp() <= now && now < validity.not_after.timestamp()) {
        return Err(WorkstationRefusal::Expired);
    }
    let name = workstation_name(&parsed).ok_or(WorkstationRefusal::NotWorkstation)?;

    let body_sha256: [u8; 32] = Sha256::digest(body).into();
    let input = workstation_signing_input(method, path_and_query, timestamp, nonce, &body_sha256);
    let algorithm = AlgorithmIdentifier::new(OID_SIG_ECDSA_WITH_SHA256, None);
    x509_parser::verify::verify_signature(
        parsed.public_key(),
        &algorithm,
        &BitString::new(0, &signature),
        &input,
    )
    .map_err(|_| WorkstationRefusal::SignatureInvalid)?;
    if now.abs_diff(timestamp) > WORKSTATION_CLOCK_SKEW_SECS.unsigned_abs() {
        return Err(WorkstationRefusal::Stale);
    }
    let serial = parsed.tbs_certificate.raw_serial().to_vec();
    Ok(SignedBy {
        name,
        serial,
        certificate,
        timestamp,
        nonce: nonce.to_owned(),
    })
}

/// The one value of header `name`, as text.
fn one_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, WorkstationRefusal> {
    let mut values = headers.get_all(name).iter();
    match (values.next(), values.next()) {
        (None, _) => Err(WorkstationRefusal::Missing),
        (Some(value), None) => value.to_str().map_err(|_| WorkstationRefusal::Malformed),
        (Some(_), Some(_)) => Err(WorkstationRefusal::Malformed),
    }
}

/// Standard base64 with padding of at most `max_bytes` bytes.
fn decode_base64(text: &str, max_bytes: usize) -> Result<Vec<u8>, WorkstationRefusal> {
    if text.is_empty() || text.len() > max_bytes.div_ceil(3) * 4 {
        return Err(WorkstationRefusal::Malformed);
    }
    STANDARD
        .decode(text)
        .map_err(|_| WorkstationRefusal::Malformed)
}

/// Unix seconds: 1 to 19 decimal digits, no sign.
fn parse_timestamp(text: &str) -> Option<i64> {
    if text.is_empty() || text.len() > 19 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// 22 to 64 characters of the base64url alphabet.
fn is_nonce(text: &str) -> bool {
    (22..=64).contains(&text.len())
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Whether one of the loaded issuing CAs signed `certificate`: its subject is the certificate's
/// issuer and its key verifies the signature.
fn signed_by_issuing_ca(certificate: &X509Certificate<'_>, authority: &Authority) -> bool {
    authority.issuing().iter().any(|ca| {
        let Ok((_, issuer)) = X509Certificate::from_der(ca.certificate_der()) else {
            return false;
        };
        issuer.subject().as_raw() == certificate.issuer().as_raw()
            && certificate
                .verify_signature(Some(issuer.public_key()))
                .is_ok()
    })
}

/// The workstation's name, when `certificate` has the workstation profile: not a CA, a key
/// usage with digitalSignature (it signs requests), the workstation purpose as its extended key usage and none of the TLS ones, and a subject of one
/// common name `workstation:<name>`.
fn workstation_name(certificate: &X509Certificate<'_>) -> Option<WorkstationName> {
    if certificate.is_ca() || !certificate.key_usage().ok()??.value.digital_signature() {
        return None;
    }
    let usage = certificate.extended_key_usage().ok()??.value;
    let purpose = usage
        .other
        .iter()
        .any(|oid| oid.as_bytes() == WORKSTATION_PURPOSE_DER.as_slice());
    if !purpose || usage.any || usage.client_auth || usage.server_auth {
        return None;
    }
    let mut attributes = certificate.subject().iter_attributes();
    let (Some(attribute), None) = (attributes.next(), attributes.next()) else {
        return None;
    };
    if attribute.attr_type() != &x509_parser::oid_registry::OID_X509_COMMON_NAME {
        return None;
    }
    let common_name = attribute.as_str().ok()?;
    WorkstationName::parse(common_name.strip_prefix(WORKSTATION_SUBJECT_PREFIX)?)
}

/// A registration request, once every field holds.
#[derive(Debug)]
pub struct Registration {
    /// The hardware ID.
    pub hardware_id: GatewayId,
    /// The CSR, DER.
    pub request: Vec<u8>,
    /// The CSR's public key, DER SubjectPublicKeyInfo as the CA writes it.
    pub public_key: Vec<u8>,
    /// The endorsement key, DER SubjectPublicKeyInfo as sent.
    pub endorsement_key: Vec<u8>,
}

/// The fields of a registration request; the error body otherwise. Mirrors [`RegisterGateway`].
///
/// # Errors
///
/// 400 when the body is not a JSON object, 422 with one field error per problem.
pub fn parse_registration(body: &[u8]) -> Result<Registration, ErrorBody> {
    let mut problems = Problems::default();
    let fields = api::object(
        body,
        &["hardware_id", "certificate_request", "endorsement_key"],
        BODY_KEYS,
        &mut problems,
    )?;
    let present = |name: &str| fields.get(name).filter(|value| !value.is_null());
    let field = |name: &str, problems: &mut Problems| {
        let value = present(name);
        if value.is_none() {
            problems.add(FieldError::new(format!("/{name}"), FIELD_REQUIRED));
        }
        value
    };
    let hardware_id = field("hardware_id", &mut problems).and_then(|value| {
        let id = value.as_str().and_then(|text| GatewayId::new(text).ok());
        if id.is_none() {
            problems.add(
                FieldError::new("/hardware_id", HARDWARE_ID_INVALID)
                    .with_detail("max", protocol::MAX_ID_LEN),
            );
        }
        id
    });
    let request = field("certificate_request", &mut problems).and_then(|value| {
        match value.as_str().map(certificate_request) {
            Some(Ok(parsed)) => Some(parsed),
            Some(Err(key)) => {
                problems.add(request_field_error(key));
                None
            }
            None => {
                problems.add(request_field_error(REQUEST_INVALID));
                None
            }
        }
    });
    let endorsement_key = field("endorsement_key", &mut problems).and_then(|value| {
        match value.as_str().map(endorsement_key) {
            Some(Ok(der)) => Some(der),
            Some(Err(key)) => {
                problems.add(endorsement_field_error(key));
                None
            }
            None => {
                problems.add(endorsement_field_error(ENDORSEMENT_KEY_INVALID));
                None
            }
        }
    });
    problems.into_result()?;
    match (hardware_id, request, endorsement_key) {
        (Some(hardware_id), Some((request, public_key)), Some(endorsement_key)) => {
            Ok(Registration {
                hardware_id,
                request,
                public_key,
                endorsement_key,
            })
        }
        // Unreachable: a missing value added a problem above.
        _ => Err(ErrorBody::new(ErrorCode::ValidationFailed)),
    }
}

/// The DER of a PEM CSR whose signature verifies with its P-256 key, and that key as DER
/// SubjectPublicKeyInfo; the message key of the problem otherwise.
fn certificate_request(text: &str) -> Result<(Vec<u8>, Vec<u8>), MessageKey> {
    let Ok(mut blocks) = pem::decode_all(text, pem::CERTIFICATE_REQUEST) else {
        return Err(REQUEST_INVALID);
    };
    if blocks.len() != 1 {
        return Err(REQUEST_INVALID);
    }
    let der = blocks.remove(0);
    match issue::requested_key(&der) {
        Ok(key) => {
            let public_key = rcgen::PublicKeyData::subject_public_key_info(&key);
            Ok((der, public_key))
        }
        Err(problem) => Err(request_message(&problem)),
    }
}

fn request_message(problem: &IssueError) -> MessageKey {
    match problem {
        IssueError::UnsupportedSignature | IssueError::UnsupportedKey => REQUEST_UNSUPPORTED,
        IssueError::BadSignature => REQUEST_SIGNATURE_INVALID,
        _ => REQUEST_INVALID,
    }
}

fn request_field_error(key: MessageKey) -> FieldError {
    let error = FieldError::new("/certificate_request", key.clone());
    if key == REQUEST_INVALID {
        error.with_detail("max_bytes", MAX_REQUEST_BYTES)
    } else {
        error
    }
}

/// The DER of a PEM public key that is RSA of 2048, 3072 or 4096 bits or ECDSA P-256 or P-384
/// (the endorsement keys TPM 2.0 makes); the message key of the problem otherwise.
fn endorsement_key(text: &str) -> Result<Vec<u8>, MessageKey> {
    let Ok(mut blocks) = pem::decode_all(text, pem::PUBLIC_KEY) else {
        return Err(ENDORSEMENT_KEY_INVALID);
    };
    if blocks.len() != 1 {
        return Err(ENDORSEMENT_KEY_INVALID);
    }
    let der = blocks.remove(0);
    if der.len() > MAX_ENDORSEMENT_KEY_BYTES {
        return Err(ENDORSEMENT_KEY_INVALID);
    }
    let Ok((rest, key)) = SubjectPublicKeyInfo::from_der(&der) else {
        return Err(ENDORSEMENT_KEY_INVALID);
    };
    if !rest.is_empty() {
        return Err(ENDORSEMENT_KEY_INVALID);
    }
    if supported_endorsement_key(&key) {
        Ok(der)
    } else {
        Err(ENDORSEMENT_KEY_UNSUPPORTED)
    }
}

fn supported_endorsement_key(key: &SubjectPublicKeyInfo<'_>) -> bool {
    let algorithm = &key.algorithm;
    if algorithm.algorithm == OID_PKCS1_RSAENCRYPTION {
        let Ok(PublicKey::RSA(rsa)) = key.parsed() else {
            return false;
        };
        let exponent_ok = rsa
            .try_exponent()
            .is_ok_and(|exponent| exponent >= 3 && exponent % 2 == 1);
        return exponent_ok && matches!(modulus_bits(rsa.modulus), 2048 | 3072 | 4096);
    }
    if algorithm.algorithm != OID_KEY_TYPE_EC_PUBLIC_KEY {
        return false;
    }
    let Some(curve) = algorithm
        .parameters
        .as_ref()
        .and_then(|parameters| parameters.as_oid().ok())
    else {
        return false;
    };
    // An uncompressed point: 0x04, then both coordinates.
    let point = key.subject_public_key.data.as_ref();
    let coordinate = if curve == OID_EC_P256 {
        32
    } else if curve == OID_NIST_EC_P384 {
        48
    } else {
        return false;
    };
    point.len() == 1 + 2 * coordinate && point.first() == Some(&0x04)
}

/// The bit length of an RSA modulus, from its DER INTEGER content octets.
fn modulus_bits(modulus: &[u8]) -> usize {
    let start = modulus.iter().position(|&byte| byte != 0);
    let Some(start) = start else {
        return 0;
    };
    let significant = &modulus[start..];
    significant.len() * 8 - significant[0].leading_zeros() as usize
}

fn endorsement_field_error(key: MessageKey) -> FieldError {
    let error = FieldError::new("/endorsement_key", key.clone());
    if key == ENDORSEMENT_KEY_INVALID {
        error.with_detail("max_bytes", MAX_ENDORSEMENT_KEY_BYTES)
    } else {
        error
    }
}

/// The state of the route.
#[derive(Clone)]
pub struct Routes {
    verifier: IdentityVerifier,
    db: PlatformDb,
    ca: CaState,
    validity: Duration,
    broker_sync: BrokerSync,
}

impl FromRef<Routes> for IdentityVerifier {
    fn from_ref(state: &Routes) -> Self {
        state.verifier.clone()
    }
}

/// The registration route: tokens verified with `verifier`, everything written as the platform
/// role through `db`, certificates signed by the CA of `ca`, valid for `validity`
/// (`gateway.validity`); each new registration asks `broker_sync` to compare the gateway's
/// broker permissions once it has committed (Laysi/trustiot-v2#633).
pub fn routes(
    verifier: IdentityVerifier,
    db: PlatformDb,
    ca: CaState,
    validity: Duration,
    broker_sync: BrokerSync,
) -> Router {
    Router::new()
        .route(REGISTRATIONS_PATH, post(register_gateway))
        .route_layer(axum::middleware::map_response(api::no_store))
        .with_state(Routes {
            verifier,
            db,
            ca,
            validity,
            broker_sync,
        })
}

/// Registers a gateway at the factory and returns its initial certificate.
#[utoipa::path(
    post,
    path = "/api/certs/v1/gateway-registrations",
    tag = "certs",
    operation_id = "registerGateway",
    request_body = RegisterGateway,
    params(
        ("trustiot-workstation-certificate" = String, Header, description = "The workstation certificate, DER, standard base64 with padding."),
        ("trustiot-workstation-timestamp" = String, Header, description = "When the workstation signed the request, Unix seconds; within 300 seconds of the service's clock."),
        ("trustiot-workstation-nonce" = String, Header, description = "22 to 64 base64url characters, never used twice with the same certificate."),
        ("trustiot-workstation-signature" = String, Header, description = "ECDSA P-256 with SHA-256 by the workstation certificate's key over `trustiot-workstation-request-v1`, the method, the path, the timestamp, the nonce and the lower-case hex SHA-256 of the body, one per line; the DER signature in standard base64 with padding."),
    ),
    responses(
        (status = 201, description = "Registered: the gateway certificate was signed and recorded with the registration, and `certs.gateway_registered` was written. The certificate's subject is `CN=<hardware_id>`, its extended key usage client authentication only, valid for the configured gateway validity (default 90 days).", body = GatewayRegistration),
        (status = 200, description = "The same hardware ID, public key and endorsement key were registered before: the first registration and its certificate. Nothing was signed or written.", body = GatewayRegistration),
        (status = 400, description = "The `Content-Type` is not `application/json`, the body is larger than 16 KiB (`gateway_registration.body_too_large`), or it is not a JSON object (`gateway_registration.body_invalid`) (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "Not a platform administrator (`permission_denied`), or the workstation signature does not hold (`gateway_registration.workstation_refused`, `details.reason`: `missing`, `malformed`, `untrusted`, `expired`, `not_workstation`, `signature_invalid`, `stale`, `revoked` or `replayed`).", body = ErrorBody),
        (status = 409, description = "The hardware ID is registered with another public key (`gateway_registration.public_key_mismatch`) or endorsement key (`gateway_registration.endorsement_key_mismatch`), or was decommissioned (`gateway_registration.decommissioned`); or another gateway has the public key (`gateway_registration.public_key_in_use`) or, active, the endorsement key (`gateway_registration.endorsement_key_in_use`) (`conflict`). Nothing was registered.", body = ErrorBody),
        (status = 422, description = "An unknown or missing field, a hardware ID that breaks the gateway ID rule (`gateway_registration.hardware_id_invalid`), a certificate request that is not one PEM CSR (`gateway_registration.request_invalid`), not for ECDSA P-256 with ecdsa-with-SHA256 (`gateway_registration.request_unsupported`) or not signed by its own key (`gateway_registration.request_signature_invalid`), or an endorsement key that is not one PEM public key (`gateway_registration.endorsement_key_invalid`) or not RSA 2048/3072/4096 or ECDSA P-256/P-384 (`gateway_registration.endorsement_key_unsupported`) (`validation_failed`). Nothing was registered.", body = ErrorBody),
        (status = 503, description = "No issuing CA is loaded or can sign yet (`gateway_registration.ca_not_ready`), or the database failed (`unavailable`). Nothing was registered.", body = ErrorBody),
    )
)]
async fn register_gateway(
    State(state): State<Routes>,
    user: UserIdentity,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Some(refused) = refuse(&user, "register a gateway") {
        return refused;
    }
    let body = match api::read_json(&headers, body, MAX_BODY_BYTES, BODY_TOO_LARGE).await {
        Ok(body) => body,
        Err(refused) => return error(refused),
    };
    // The body's shape first (400), as the error table orders it; its fields only once the
    // workstation is known (422).
    if !matches!(
        serde_json::from_slice::<serde_json::Value>(&body),
        Ok(serde_json::Value::Object(_))
    ) {
        return error(ErrorBody::new(ErrorCode::InvalidRequest).with_message_key(BODY_INVALID));
    }
    // One snapshot: the root CRL, if any, was checked against this authority's root, so the
    // answer never carries a root CRL with a root it was not checked against.
    let (authority, root_crl) = state.ca.authority_and_root_crl();
    let Some(authority) = authority else {
        tracing::warn!("refused a gateway registration: no issuing CA is loaded");
        return error(not_ready());
    };
    let path_and_query = uri
        .path_and_query()
        .map_or_else(|| uri.path(), |path| path.as_str());
    let signed = match verify_workstation(
        &headers,
        method.as_str(),
        path_and_query,
        &body,
        &authority,
        unix_now(),
    ) {
        Ok(signed) => signed,
        Err(refusal) => {
            tracing::info!(
                user_id = %user.user_id(),
                reason = refusal.as_str(),
                "refused a gateway registration: the workstation identity does not hold"
            );
            return error(refusal.body());
        }
    };
    let registration = match parse_registration(&body) {
        Ok(registration) => registration,
        Err(refused) => return error(refused),
    };
    // One retry: a concurrent registration of another hardware ID with the same public key or
    // endorsement key makes the first attempt's insert fail (23505) and roll back; the second
    // finds the committed row and answers 409 as for a key in use. Registrations of the same
    // hardware ID wait for each other on `LOCK_REGISTRATION` instead; a 23505 on the hardware ID
    // would end the same way, the retry finding the row and answering as for a repeat.
    let anchor = Anchor {
        authority: &authority,
        root_crl: root_crl.as_deref(),
    };
    let mut outcome = register(&state, &user, anchor, &signed, &registration).await;
    if matches!(outcome, Err(Failure::Race)) {
        outcome = register(&state, &user, anchor, &signed, &registration).await;
    }
    match outcome {
        Ok((status, view)) => {
            if status == StatusCode::CREATED {
                tracing::info!(
                    user_id = %user.user_id(),
                    workstation = %signed.name.as_str(),
                    hardware_id = %registration.hardware_id,
                    serial = %view.serial,
                    ca_key_id = %view.ca_key_id,
                    not_after = %view.not_after,
                    "registered a gateway"
                );
                // Committed: the broker's permissions follow now, not at the next whole
                // comparison.
                state.broker_sync.gateway(&registration.hardware_id);
            } else {
                tracing::info!(
                    user_id = %user.user_id(),
                    workstation = %signed.name.as_str(),
                    hardware_id = %registration.hardware_id,
                    serial = %view.serial,
                    "a gateway registration repeated an existing one"
                );
            }
            ok(status, &view)
        }
        Err(Failure::Refused(body)) => error(body),
        Err(Failure::Race) => {
            tracing::error!(
                hardware_id = %registration.hardware_id,
                "a gateway registration lost two races in a row"
            );
            error(ErrorBody::new(ErrorCode::Unavailable))
        }
    }
}

fn not_ready() -> ErrorBody {
    ErrorBody::new(ErrorCode::Unavailable).with_message_key(CA_NOT_READY)
}

fn conflict(key: MessageKey) -> ErrorBody {
    ErrorBody::new(ErrorCode::Conflict).with_message_key(key)
}

/// Why an attempt did not register.
#[derive(Debug)]
enum Failure {
    /// The error body to answer.
    Refused(ErrorBody),
    /// A unique violation: another registration committed the same row first.
    Race,
}

impl From<ErrorBody> for Failure {
    fn from(body: ErrorBody) -> Self {
        Self::Refused(body)
    }
}

fn database(error: &dyn std::fmt::Display) -> Failure {
    tracing::error!(error = %error, "could not register a gateway");
    Failure::Refused(ErrorBody::new(ErrorCode::Unavailable))
}

fn sql(error: &sqlx::Error) -> Failure {
    if let sqlx::Error::Database(problem) = error
        && problem.code().as_deref() == Some("23505")
    {
        return Failure::Race;
    }
    database(error)
}

/// The authority an answer is built from and the root CRL adopted with it, read at once
/// ([`CaState::authority_and_root_crl`]).
#[derive(Clone, Copy)]
struct Anchor<'a> {
    authority: &'a Authority,
    /// The root CRL adopted, checked against `authority`'s root; `None` while there is none.
    root_crl: Option<&'a RootCrl>,
}

impl Anchor<'_> {
    /// The answer's `root_certificate`: the root of the authority, PEM.
    fn root_certificate(&self) -> String {
        pem::encode(pem::CERTIFICATE, self.authority.root_der())
    }

    /// The answer's `root_crl`: the PEM the agent loads, or `None` (the field is absent).
    fn root_crl(&self) -> Option<String> {
        self.root_crl.map(|crl| crl.pem().to_owned())
    }
}

/// One attempt, in one transaction of the platform role: the workstation's ledger row, the
/// nonce, then a repeat, a conflict or a new registration.
async fn register(
    state: &Routes,
    user: &UserIdentity,
    anchor: Anchor<'_>,
    signed: &SignedBy,
    registration: &Registration,
) -> Result<(StatusCode, GatewayRegistration), Failure> {
    let authority = anchor.authority;
    let Some(actor) = PlatformActor::admin(user) else {
        // `refuse` let only platform administrators through.
        return Err(ErrorBody::new(ErrorCode::PermissionDenied).into());
    };
    let mut tx = state
        .db
        .begin(actor, PURPOSE)
        .await
        .map_err(|e: TenantDbError| database(&e))?;
    check_ledger(&mut tx, signed).await?;
    // Held until commit: no other registration of this hardware ID commits between the lookup
    // and the key check below, so a key found in use there belongs to another hardware ID. Taken
    // before the nonce's statement, so a registration waiting here holds no row locks of expired
    // nonces that registrations of other hardware IDs would wait for.
    sqlx::query(LOCK_REGISTRATION)
        .bind(registration.hardware_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(|e| sql(&e))?;
    // The DELETE runs although nothing reads it: a data-modifying statement in WITH without
    // RETURNING "will be executed nonetheless"
    // (https://www.postgresql.org/docs/current/queries-with.html#QUERIES-WITH-MODIFYING). It
    // removes only nonces whose requests are refused as stale anyway (`NONCE_RETENTION_SECS`), so
    // it does not matter which of the two parts PostgreSQL runs first.
    let fresh = sqlx::query(
        "WITH expired AS (
             DELETE FROM certs.workstation_request_nonces WHERE expires_at < now()
         )
         INSERT INTO certs.workstation_request_nonces (certificate_serial, nonce, expires_at)
         VALUES ($1, $2, $3)
         ON CONFLICT DO NOTHING",
    )
    .bind(&signed.serial)
    .bind(&signed.nonce)
    .bind(issue::utc(
        signed.timestamp.saturating_add(NONCE_RETENTION_SECS),
    ))
    .execute(&mut *tx)
    .await
    .map_err(|e| sql(&e))?
    .rows_affected();
    if fresh == 0 {
        return Err(WorkstationRefusal::Replayed.body().into());
    }

    let public_key_sha256: [u8; 32] = Sha256::digest(&registration.public_key).into();
    let endorsement_key_sha256: [u8; 32] = Sha256::digest(&registration.endorsement_key).into();
    let existing = sqlx::query(
        "SELECT r.public_key, r.endorsement_key_sha256, r.decommissioned_at IS NOT NULL
                    AS decommissioned,
                r.workstation, r.registered_at, c.serial, c.ca_key_id, c.not_before, c.not_after,
                c.certificate
         FROM certs.gateway_registrations r
         JOIN certs.issued_certificates c ON c.serial = r.certificate_serial
         WHERE r.hardware_id = $1",
    )
    .bind(registration.hardware_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| sql(&e))?;
    if let Some(row) = existing {
        let answer = repeat(registration, &endorsement_key_sha256, &row, anchor);
        // The nonce is spent either way.
        tx.commit().await.map_err(|e| database(&e))?;
        return answer;
    }
    // NULL when no row matches.
    let (key_taken, endorsement_taken): (Option<bool>, Option<bool>) = sqlx::query_as(
        "SELECT bool_or(public_key = $1),
                bool_or(endorsement_key_sha256 = $2 AND decommissioned_at IS NULL)
         FROM certs.gateway_registrations
         WHERE public_key = $1 OR endorsement_key_sha256 = $2",
    )
    .bind(&registration.public_key)
    .bind(endorsement_key_sha256.as_slice())
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| sql(&e))?;
    let taken = if key_taken == Some(true) {
        Some(PUBLIC_KEY_IN_USE)
    } else if endorsement_taken == Some(true) {
        Some(ENDORSEMENT_KEY_IN_USE)
    } else {
        None
    };
    if let Some(key) = taken {
        // The nonce is spent either way.
        tx.commit().await.map_err(|e| database(&e))?;
        return Err(conflict(key).into());
    }

    let issued = sign(state, authority, &mut tx, registration).await?;
    let registered_at: DateTime<Utc> = sqlx::query_scalar(
        "INSERT INTO certs.gateway_registrations
             (hardware_id, public_key, endorsement_key, endorsement_key_sha256,
              certificate_serial, workstation, workstation_certificate_serial, registered_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8::uuid)
         RETURNING registered_at",
    )
    .bind(registration.hardware_id.as_str())
    .bind(&registration.public_key)
    .bind(&registration.endorsement_key)
    .bind(endorsement_key_sha256.as_slice())
    .bind(&issued.serial)
    .bind(signed.name.as_str())
    .bind(&signed.serial)
    .bind(user.user_id().to_string())
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| sql(&e))?;

    let event = GatewayRegistered {
        hardware_id: registration.hardware_id.clone(),
        public_key_sha256: issue::hex(&public_key_sha256),
        endorsement_key_sha256: issue::hex(&endorsement_key_sha256),
        certificate_serial: issue::hex(&issued.serial),
        workstation: signed.name.as_str().to_owned(),
    };
    let aggregate = Aggregate::new(
        GATEWAY_REGISTRATION_AGGREGATE,
        registration.hardware_id.as_str(),
    )
    .map_err(|e| database(&e))?;
    outbox::publish_as(
        tx.transaction(),
        &aggregate,
        &event,
        None,
        Actor::User {
            user_id: user.user_id(),
        },
    )
    .await
    .map_err(|e| database(&e))?;
    tx.commit().await.map_err(|e| database(&e))?;

    Ok((
        StatusCode::CREATED,
        GatewayRegistration {
            hardware_id: registration.hardware_id.as_str().to_owned(),
            public_key_sha256: event.public_key_sha256,
            endorsement_key_sha256: event.endorsement_key_sha256,
            workstation: event.workstation,
            registered_at,
            serial: event.certificate_serial,
            ca_key_id: issued.ca_key_id,
            not_before: issue::utc(issued.not_before),
            not_after: issue::utc(issued.not_after),
            certificate: pem::encode(pem::CERTIFICATE, &issued.der),
            // `issuer_der` is the certificate of the issuing CA that just signed: the loaded
            // one whose key id is `ca_key_id`.
            certificate_chain: format!(
                "{}{}",
                pem::encode(pem::CERTIFICATE, &issued.der),
                pem::encode(pem::CERTIFICATE, &issued.issuer_der)
            ),
            root_certificate: anchor.root_certificate(),
            root_crl: anchor.root_crl(),
        },
    ))
}

/// Refuses the workstation unless the ledger has its certificate as a workstation certificate
/// that is not revoked. The ledger is read in the registration's transaction, so a revocation
/// committed before it takes effect at once, before the next CRL.
async fn check_ledger(tx: &mut PlatformTransaction, signed: &SignedBy) -> Result<(), Failure> {
    // Held until the registration commits, so a concurrent revocation waits for it or is seen.
    sqlx::query(issue::LOCK_WORKSTATION_SHARED)
        .bind(&signed.serial)
        .execute(&mut **tx)
        .await
        .map_err(|e| sql(&e))?;
    let row: Option<(bool, bool)> = sqlx::query_as(
        "SELECT revoked_at IS NOT NULL, certificate = $2
         FROM certs.issued_certificates
         WHERE serial = $1 AND profile = 'workstation'",
    )
    .bind(&signed.serial)
    .bind(&signed.certificate)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| sql(&e))?;
    let refusal = match row {
        Some((false, true)) => return Ok(()),
        Some((true, true)) => WorkstationRefusal::Revoked,
        // Signed by our CA but not recorded, or recorded with other content: a certificate
        // this ledger does not vouch for.
        Some((_, false)) | None => WorkstationRefusal::Untrusted,
    };
    tracing::info!(
        workstation = %signed.name.as_str(),
        serial = %issue::hex(&signed.serial),
        reason = refusal.as_str(),
        "refused a gateway registration: the workstation certificate"
    );
    Err(refusal.body().into())
}

/// The answer to a registration of a hardware ID that is already registered: the first
/// registration when the keys are the same and it is active, a conflict otherwise. The root
/// certificate and the root CRL are the current ones of `anchor`, and the chain's issuing CA the
/// one of its authority that signed the certificate ([`certificate_chain`]).
fn repeat(
    registration: &Registration,
    endorsement_key_sha256: &[u8; 32],
    row: &PgRow,
    anchor: Anchor<'_>,
) -> Result<(StatusCode, GatewayRegistration), Failure> {
    let read = |e: sqlx::Error| database(&e);
    let decommissioned: bool = row.try_get("decommissioned").map_err(read)?;
    if decommissioned {
        return Err(conflict(DECOMMISSIONED).into());
    }
    let public_key: Vec<u8> = row.try_get("public_key").map_err(read)?;
    if public_key != registration.public_key {
        return Err(conflict(PUBLIC_KEY_MISMATCH).into());
    }
    let stored_endorsement: Vec<u8> = row.try_get("endorsement_key_sha256").map_err(read)?;
    if stored_endorsement.as_slice() != endorsement_key_sha256.as_slice() {
        return Err(conflict(ENDORSEMENT_KEY_MISMATCH).into());
    }
    let serial: Vec<u8> = row.try_get("serial").map_err(read)?;
    let certificate: Vec<u8> = row.try_get("certificate").map_err(read)?;
    let ca_key_id: String = row.try_get("ca_key_id").map_err(read)?;
    let public_key_sha256: [u8; 32] = Sha256::digest(&public_key).into();
    Ok((
        StatusCode::OK,
        GatewayRegistration {
            hardware_id: registration.hardware_id.as_str().to_owned(),
            public_key_sha256: issue::hex(&public_key_sha256),
            endorsement_key_sha256: issue::hex(endorsement_key_sha256),
            workstation: row.try_get("workstation").map_err(read)?,
            registered_at: row.try_get("registered_at").map_err(read)?,
            serial: issue::hex(&serial),
            not_before: row.try_get("not_before").map_err(read)?,
            not_after: row.try_get("not_after").map_err(read)?,
            certificate: pem::encode(pem::CERTIFICATE, &certificate),
            certificate_chain: certificate_chain(anchor.authority, &ca_key_id, &certificate),
            root_certificate: anchor.root_certificate(),
            root_crl: anchor.root_crl(),
            ca_key_id,
        },
    ))
}

/// `certificate` (DER), then the certificate of the issuing CA of `authority` whose key id is
/// `ca_key_id`, PEM. When that issuing CA is no longer loaded (rotation step 3 removed it after
/// every certificate it signed expired, services/certs/README.md, "輪替"), the chain is
/// `certificate` alone and a WARN names the key id: a repeat still answers with the first
/// registration.
fn certificate_chain(authority: &Authority, ca_key_id: &str, certificate: &[u8]) -> String {
    let mut chain = pem::encode(pem::CERTIFICATE, certificate);
    match authority
        .issuing()
        .iter()
        .find(|issuing| issuing.key_id() == ca_key_id)
    {
        Some(issuing) => chain.push_str(&pem::encode(pem::CERTIFICATE, issuing.certificate_der())),
        None => tracing::warn!(
            ca_key_id = %ca_key_id,
            "the issuing CA of a repeated gateway registration is no longer loaded, so its \
             certificate chain is the gateway certificate alone"
        ),
    }
    chain
}

/// Signs the gateway certificate and records it in the ledger within `tx`, through the path
/// every certificate takes ([`issue::sign_and_record_in`]): the newest valid issuing CA that has
/// a CRL, read in `tx`. The registration and its event commit with it, or nothing does.
async fn sign(
    state: &Routes,
    authority: &Authority,
    tx: &mut PlatformTransaction,
    registration: &Registration,
) -> Result<IssuedCertificate, Failure> {
    issue::sign_and_record_in(tx, authority, "a gateway certificate", |ca, now| {
        issue::gateway_certificate(
            ca,
            &registration.request,
            &registration.hardware_id,
            state.validity,
            now,
        )
    })
    .await
    .map_err(|refusal| match refusal {
        Refusal::NotReady => not_ready().into(),
        // Not expected: the request was parsed with the same checks.
        Refusal::Request(refused) => ErrorBody::new(ErrorCode::ValidationFailed)
            .with_field_error(request_field_error(request_message(&refused)))
            .into(),
        Refusal::Failed => ErrorBody::new(ErrorCode::Internal).into(),
        Refusal::Database => ErrorBody::new(ErrorCode::Unavailable).into(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn reason(body: &ErrorBody) -> Option<&Value> {
        body.details().get("reason")
    }

    fn bytes(value: &Value) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }

    fn field_errors(body: &ErrorBody) -> Vec<(String, String)> {
        let mut errors: Vec<(String, String)> = body
            .field_errors()
            .iter()
            .map(|error| (error.field.clone(), error.message_key.as_str().to_owned()))
            .collect();
        errors.sort();
        errors
    }

    #[test]
    fn registration_requests_name_each_problem() {
        let refused = parse_registration(&bytes(&json!({
            "hardware_id": "gw/1",
            "certificate_request": "not PEM",
            "endorsement_key": 7,
            "tenant": "t",
        })))
        .unwrap_err();
        assert_eq!(
            field_errors(&refused),
            [
                (
                    "/certificate_request".to_owned(),
                    REQUEST_INVALID.as_str().to_owned()
                ),
                (
                    "/endorsement_key".to_owned(),
                    ENDORSEMENT_KEY_INVALID.as_str().to_owned()
                ),
                (
                    "/hardware_id".to_owned(),
                    HARDWARE_ID_INVALID.as_str().to_owned()
                ),
                ("/tenant".to_owned(), UNKNOWN_FIELD.as_str().to_owned()),
            ]
        );
        let missing = parse_registration(&bytes(&json!({"hardware_id": null}))).unwrap_err();
        assert_eq!(
            field_errors(&missing),
            [
                (
                    "/certificate_request".to_owned(),
                    FIELD_REQUIRED.as_str().to_owned()
                ),
                (
                    "/endorsement_key".to_owned(),
                    FIELD_REQUIRED.as_str().to_owned()
                ),
                (
                    "/hardware_id".to_owned(),
                    FIELD_REQUIRED.as_str().to_owned()
                ),
            ]
        );
        for not_an_object in [json!([]), json!("x"), json!(null)] {
            let refused = parse_registration(&bytes(&not_an_object)).unwrap_err();
            assert_eq!(refused.code(), ErrorCode::InvalidRequest);
        }
    }

    #[test]
    fn modulus_bits_ignore_the_sign_octet() {
        let mut modulus = vec![0x00, 0x80];
        modulus.extend(std::iter::repeat_n(0xff, 255));
        assert_eq!(modulus_bits(&modulus), 2048);
        assert_eq!(modulus_bits(&[0x00, 0x01]), 1);
        assert_eq!(modulus_bits(&[0x7f; 256]), 2047);
        assert_eq!(modulus_bits(&[]), 0);
        assert_eq!(modulus_bits(&[0, 0]), 0);
    }

    #[test]
    fn workstation_headers_are_one_each_and_well_formed() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            one_header(&headers, WORKSTATION_NONCE_HEADER),
            Err(WorkstationRefusal::Missing)
        );
        headers.append(WORKSTATION_NONCE_HEADER, "a".parse().unwrap());
        assert_eq!(one_header(&headers, WORKSTATION_NONCE_HEADER), Ok("a"));
        headers.append(WORKSTATION_NONCE_HEADER, "b".parse().unwrap());
        assert_eq!(
            one_header(&headers, WORKSTATION_NONCE_HEADER),
            Err(WorkstationRefusal::Malformed)
        );
        for (text, valid) in [
            ("1793000000", Some(1_793_000_000)),
            ("0", Some(0)),
            ("", None),
            ("-1", None),
            ("+1", None),
            ("1.5", None),
            ("12345678901234567890", None),
            (" 1", None),
        ] {
            assert_eq!(parse_timestamp(text), valid, "{text:?}");
        }
        assert!(is_nonce(&"A".repeat(22)));
        assert!(is_nonce("bm9uY2Utbm9uY2Utbm9uY2U_-"));
        assert!(!is_nonce(&"A".repeat(21)));
        assert!(!is_nonce(&"A".repeat(65)));
        assert!(!is_nonce("bm9uY2Utbm9uY2Utbm9uY2U="));
        assert_eq!(decode_base64("", 10), Err(WorkstationRefusal::Malformed));
        assert_eq!(decode_base64("AAAA", 3), Ok(vec![0, 0, 0]));
        assert_eq!(
            decode_base64("AAAAAAAA", 3),
            Err(WorkstationRefusal::Malformed)
        );
        assert_eq!(decode_base64("AA-_", 3), Err(WorkstationRefusal::Malformed));
    }

    #[test]
    fn the_registration_lock_has_a_class_of_its_own() {
        let class = |statement: &str| -> i64 {
            statement
                .split_once('(')
                .and_then(|(_, rest)| rest.split_once(','))
                .and_then(|(class, _)| class.trim().parse().ok())
                .unwrap_or_else(|| panic!("no class in {statement:?}"))
        };
        assert_eq!(class(LOCK_REGISTRATION), 0x6777_7267);
        assert_eq!(
            class(issue::LOCK_WORKSTATION),
            class(issue::LOCK_WORKSTATION_SHARED)
        );
        let mut classes = [
            LOCK_REGISTRATION,
            issue::LOCK_WORKSTATION,
            crate::crl::LOCK_CA_CRL,
            crate::gateways::LOCK_GATEWAY_CERTIFICATES,
        ]
        .map(class);
        classes.sort_unstable();
        assert!(
            classes.windows(2).all(|pair| pair[0] != pair[1]),
            "{classes:?}"
        );
    }

    #[test]
    fn refusals_name_their_reason() {
        let body = WorkstationRefusal::NotWorkstation.body();
        assert_eq!(body.code(), ErrorCode::PermissionDenied);
        assert_eq!(*body.message_key(), WORKSTATION_REFUSED);
        assert_eq!(reason(&body), Some(&json!("not_workstation")));
    }
}
