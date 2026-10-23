//! Production workstation certificates (Laysi/trustiot-v2#534; services/certs/README.md,
//! "工作站憑證"): the API platform administrators issue and revoke them with. It is reached
//! through the ingress with a session cookie (an `ingress.routes` entry `{host: app, path:
//! /api/certs, match: PathPrefix, service: certs}`), which the identity service turns into an
//! internal identity token addressed to this service (decision 27).
//!
//! | Request | Success |
//! | --- | --- |
//! | `POST /api/certs/v1/workstation-certificates` | 201 [`WorkstationCertificate`] |
//! | `POST /api/certs/v1/workstation-certificates/{serial}/revoke` | 200 [`WorkstationCertificate`], revoked |
//!
//! Errors follow M1:
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | Not a platform administrator, or a token another service exchanged (`act`) | 403 `permission_denied` |
//! | `Content-Type` not `application/json` (parameters allowed) | 400 `invalid_request` |
//! | A body over [`MAX_BODY_BYTES`] | 400 `invalid_request`, key `workstation_certificate.body_too_large`, `{"max_bytes"}` |
//! | A body that is not a JSON object | 400 `invalid_request`, key `workstation_certificate.body_invalid` |
//! | An unknown field, a missing field, or a value that breaks its rule | 422 `validation_failed`, one field error per problem |
//! | A `certificate_request` that is not one PEM CSR of at most 4096 bytes | 422, field `/certificate_request`, key `workstation_certificate.request_invalid` |
//! | A CSR for another key than ECDSA P-256 or not signed with ecdsa-with-SHA256 | 422, key `workstation_certificate.request_unsupported` |
//! | A CSR whose signature does not verify with its key (no proof of possession) | 422, key `workstation_certificate.request_signature_invalid` |
//! | A `serial` that is not hex, or no workstation certificate has it | 404 `not_found` |
//! | Revoking a certificate that is already revoked | 409 `conflict`, key `workstation_certificate.already_revoked` |
//! | No issuing CA can sign now: none loaded, none valid with a CRL yet, or the newest expires within an hour | 503 `unavailable`, key `workstation_certificate.ca_not_ready` |
//! | The database failed | 503 `unavailable`; nothing was issued or revoked |
//!
//! The checks run in that order, so a refused request reaches neither the CA nor the database.
//! Every response carries `cache-control: no-store`. Each issue and revocation logs one INFO
//! line with the administrator's user id, the serial number, the CA key id and the workstation
//! name; never the CSR or the certificate.

use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::rejection::PathRejection;
use axum::extract::{FromRef, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::post;
use chrono::{DateTime, Utc};
use contract::certs::{
    IssueWorkstationCertificate, RevocationReason, RevokeWorkstationCertificate,
    WorkstationCertificate,
};
use contract::{ErrorBody, ErrorCode, FieldError, MessageKey};
use serde_json::{Map, Value};
use service_kit::identity::{IdentityVerifier, UserIdentity};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row as _};

use crate::api::{self, BodyKeys, Problems, error, ok, refuse};
use crate::authority::CaState;
use crate::issue::{
    self, IssueError, IssuedCertificate, MAX_REQUEST_BYTES, MAX_WORKSTATION_NAME_LEN, Refusal,
    WORKSTATION_SUBJECT_PREFIX, WorkstationName,
};
use crate::pem;

/// The collection, as the ingress gateway forwards it (no prefix is stripped).
pub const CERTIFICATES_PATH: &str = "/api/certs/v1/workstation-certificates";

/// Revoking one certificate.
pub const REVOKE_PATH: &str = "/api/certs/v1/workstation-certificates/{serial}/revoke";

/// The largest body accepted: a CSR of [`MAX_REQUEST_BYTES`] as PEM and a name fit with room.
pub const MAX_BODY_BYTES: usize = 16 * 1024;

/// The body is larger than [`MAX_BODY_BYTES`] (400); `details.max_bytes`.
pub const BODY_TOO_LARGE: MessageKey = MessageKey::new("workstation_certificate.body_too_large");
/// The body is not a JSON object (400).
pub const BODY_INVALID: MessageKey = MessageKey::new("workstation_certificate.body_invalid");
/// A field the request does not have (422).
pub const UNKNOWN_FIELD: MessageKey = MessageKey::new("workstation_certificate.unknown_field");
/// A required field is missing or `null` (422).
pub const FIELD_REQUIRED: MessageKey = MessageKey::new("workstation_certificate.field_required");
/// The workstation name breaks its rule (422); `details.max`.
pub const NAME_INVALID: MessageKey = MessageKey::new("workstation_certificate.name_invalid");
/// The certificate request is not one PEM certificate signing request of at most
/// `details.max_bytes` bytes as DER (422).
pub const REQUEST_INVALID: MessageKey = MessageKey::new("workstation_certificate.request_invalid");
/// The certificate request is for another key than ECDSA P-256, or not signed with
/// ecdsa-with-SHA256 (422).
pub const REQUEST_UNSUPPORTED: MessageKey =
    MessageKey::new("workstation_certificate.request_unsupported");
/// The certificate request's signature does not verify with its own key: whoever made it does
/// not hold the private key (422).
pub const REQUEST_SIGNATURE_INVALID: MessageKey =
    MessageKey::new("workstation_certificate.request_signature_invalid");
/// The revocation reason is not one of [`RevocationReason`] (422).
pub const REASON_INVALID: MessageKey = MessageKey::new("workstation_certificate.reason_invalid");
/// The certificate is already revoked (409).
pub const ALREADY_REVOKED: MessageKey = MessageKey::new("workstation_certificate.already_revoked");
/// No issuing CA can sign now (503); retrying later helps once the CA is set up or its first
/// CRL is out.
pub const CA_NOT_READY: MessageKey = MessageKey::new("workstation_certificate.ca_not_ready");

/// The state of the routes.
#[derive(Clone)]
pub struct Routes {
    verifier: IdentityVerifier,
    pool: PgPool,
    ca: CaState,
    validity: Duration,
}

impl FromRef<Routes> for IdentityVerifier {
    fn from_ref(state: &Routes) -> Self {
        state.verifier.clone()
    }
}

/// The workstation certificate routes: tokens verified with `verifier`, the ledger in `pool`,
/// certificates signed by the CA of `ca`, valid for `validity` (`workstation.validity`).
pub fn routes(verifier: IdentityVerifier, pool: PgPool, ca: CaState, validity: Duration) -> Router {
    Router::new()
        .route(CERTIFICATES_PATH, post(issue_certificate))
        .route(REVOKE_PATH, post(revoke_certificate))
        .route_layer(axum::middleware::map_response(api::no_store))
        .with_state(Routes {
            verifier,
            pool,
            ca,
            validity,
        })
}

/// The message keys of a body that is not a JSON object or has unknown fields.
const BODY_KEYS: BodyKeys = BodyKeys {
    invalid: BODY_INVALID,
    unknown_field: UNKNOWN_FIELD,
};

/// The body, at most [`MAX_BODY_BYTES`], after the content type check.
async fn read_json(headers: &HeaderMap, body: Body) -> Result<Bytes, ErrorBody> {
    api::read_json(headers, body, MAX_BODY_BYTES, BODY_TOO_LARGE).await
}

/// The body as a JSON object, with a field error for each field not in `allowed`.
fn object(
    body: &[u8],
    allowed: &[&str],
    problems: &mut Problems,
) -> Result<Map<String, Value>, ErrorBody> {
    api::object(body, allowed, BODY_KEYS, problems)
}

/// Issues a workstation certificate for the public key of a CSR. The CA decides everything
/// else; the private key never reaches the service.
#[utoipa::path(
    post,
    path = "/api/certs/v1/workstation-certificates",
    tag = "certs",
    operation_id = "issueWorkstationCertificate",
    request_body = IssueWorkstationCertificate,
    responses(
        (status = 201, description = "Issued and recorded in the ledger. The subject is `CN=workstation:<workstation>`, the extended key usage the TrustIoT workstation purpose only (never client authentication, so the MQTT broker refuses it), valid for the configured workstation validity (default 180 days). Keep `serial`: it is how the certificate is revoked.", body = WorkstationCertificate),
        (status = 400, description = "The `Content-Type` is not `application/json`, the body is larger than 16 KiB (`workstation_certificate.body_too_large`), or it is not a JSON object (`workstation_certificate.body_invalid`) (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "Not a platform administrator (`permission_denied`).", body = ErrorBody),
        (status = 422, description = "An unknown or missing field, a name that breaks its rule (`workstation_certificate.name_invalid`), or a certificate request that is not one PEM CSR (`workstation_certificate.request_invalid`), not for ECDSA P-256 with ecdsa-with-SHA256 (`workstation_certificate.request_unsupported`) or not signed by its own key (`workstation_certificate.request_signature_invalid`) (`validation_failed`). Nothing was issued.", body = ErrorBody),
        (status = 503, description = "No issuing CA can sign yet (`workstation_certificate.ca_not_ready`), or the database failed (`unavailable`). Nothing was issued.", body = ErrorBody),
    )
)]
async fn issue_certificate(
    State(state): State<Routes>,
    user: UserIdentity,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Some(refused) = refuse(&user, "issue a workstation certificate") {
        return refused;
    }
    let body = match read_json(&headers, body).await {
        Ok(body) => body,
        Err(refused) => return error(refused),
    };
    let (name, request) = match parse_issue(&body) {
        Ok(parsed) => parsed,
        Err(refused) => return error(refused),
    };
    match issue_for(&state, &name, &request).await {
        Ok(issued) => {
            tracing::info!(
                user_id = %user.user_id(),
                serial = %issue::hex(&issued.serial),
                ca_key_id = %issued.ca_key_id,
                workstation = %name.as_str(),
                not_after = issued.not_after,
                "issued a workstation certificate"
            );
            ok(StatusCode::CREATED, &issued_view(&issued, &name))
        }
        Err(refused) => error(refused),
    }
}

/// Signs and records the certificate in one transaction, with the newest issuing CA that has a
/// CRL; the error body to answer otherwise.
async fn issue_for(
    state: &Routes,
    name: &WorkstationName,
    request: &[u8],
) -> Result<IssuedCertificate, ErrorBody> {
    issue::sign_and_record(
        &state.pool,
        &state.ca,
        "a workstation certificate",
        |ca, now| issue::workstation_certificate(ca, request, name, state.validity, now),
    )
    .await
    .map_err(|refusal| match refusal {
        Refusal::NotReady => ErrorBody::new(ErrorCode::Unavailable).with_message_key(CA_NOT_READY),
        Refusal::Request(refused) => request_error(&refused),
        Refusal::Failed => ErrorBody::new(ErrorCode::Internal),
        Refusal::Database => ErrorBody::new(ErrorCode::Unavailable),
    })
}

/// Revokes a workstation certificate. Its issuing CA's next scheduled CRL lists it; the
/// registration API (Laysi/trustiot-v2#79) reads the ledger, so it refuses the certificate at
/// once.
#[utoipa::path(
    post,
    path = "/api/certs/v1/workstation-certificates/{serial}/revoke",
    tag = "certs",
    operation_id = "revokeWorkstationCertificate",
    params(("serial" = String, Path, description = "The certificate's serial number, hex, as `serial` of the issue response.")),
    request_body = RevokeWorkstationCertificate,
    responses(
        (status = 200, description = "Revoked now; the ledger records the time and the reason, and the issuing CA's next scheduled CRL lists it.", body = WorkstationCertificate),
        (status = 400, description = "The `Content-Type` is not `application/json`, the body is larger than 16 KiB (`workstation_certificate.body_too_large`), or it is not a JSON object (`workstation_certificate.body_invalid`) (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "Not a platform administrator (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No workstation certificate has this serial number (`not_found`).", body = ErrorBody),
        (status = 409, description = "It is already revoked (`conflict`, `workstation_certificate.already_revoked`).", body = ErrorBody),
        (status = 422, description = "An unknown field, or a reason that is not one of the listed ones (`workstation_certificate.reason_invalid`) (`validation_failed`). Nothing was revoked.", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`). Nothing was revoked.", body = ErrorBody),
    )
)]
async fn revoke_certificate(
    State(state): State<Routes>,
    user: UserIdentity,
    serial: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Some(refused) = refuse(&user, "revoke a workstation certificate") {
        return refused;
    }
    let body = match read_json(&headers, body).await {
        Ok(body) => body,
        Err(refused) => return error(refused),
    };
    let reason = match parse_revoke(&body) {
        Ok(request) => request.reason,
        Err(refused) => return error(refused),
    };
    let Some(serial) = parse_serial(serial) else {
        return error(ErrorBody::new(ErrorCode::NotFound));
    };
    let failed = |problem: &sqlx::Error| {
        tracing::error!(
            error = %problem,
            serial = %issue::hex(&serial),
            "could not revoke a workstation certificate"
        );
        error(ErrorBody::new(ErrorCode::Unavailable))
    };
    let revoked = revoke(&state.pool, &serial, reason).await;
    let row = match revoked {
        Ok(Some(row)) => row,
        Ok(None) => {
            let known = sqlx::query_scalar::<_, bool>(
                "SELECT revoked_at IS NOT NULL FROM certs.issued_certificates
                 WHERE serial = $1 AND profile = 'workstation'",
            )
            .bind(&serial)
            .fetch_optional(&state.pool)
            .await;
            return match known {
                Ok(Some(_)) => {
                    error(ErrorBody::new(ErrorCode::Conflict).with_message_key(ALREADY_REVOKED))
                }
                Ok(None) => error(ErrorBody::new(ErrorCode::NotFound)),
                Err(problem) => failed(&problem),
            };
        }
        Err(problem) => return failed(&problem),
    };
    match ledger_view(&serial, &row) {
        Ok(view) => {
            tracing::info!(
                user_id = %user.user_id(),
                serial = %view.serial,
                ca_key_id = %view.ca_key_id,
                workstation = %view.workstation,
                reason = reason.map_or("none", RevocationReason::as_str),
                "revoked a workstation certificate"
            );
            ok(StatusCode::OK, &view)
        }
        // Revoked and committed; only reading the row back failed.
        Err(problem) => failed(&problem),
    }
}

/// Revokes the workstation certificate `serial` unless it is revoked already, in a transaction
/// that first takes [`issue::LOCK_WORKSTATION`], so a factory registration that already read the
/// certificate as valid commits before the revocation does; the revoked row, or `None`.
async fn revoke(
    pool: &PgPool,
    serial: &[u8],
    reason: Option<RevocationReason>,
) -> Result<Option<PgRow>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(issue::LOCK_WORKSTATION)
        .bind(serial)
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query(
        "UPDATE certs.issued_certificates
         SET revoked_at = now(), revocation_reason = $2
         WHERE serial = $1 AND profile = 'workstation' AND revoked_at IS NULL
         RETURNING ca_key_id, subject, not_before, not_after, certificate, revoked_at,
                   revocation_reason",
    )
    .bind(serial)
    .bind(reason.map(RevocationReason::code))
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row)
}

/// The workstation name and the CSR (DER) of an issue request, once the CSR is shown to be a
/// P-256 key's own request; the error body otherwise. Mirrors
/// [`IssueWorkstationCertificate`].
fn parse_issue(body: &[u8]) -> Result<(WorkstationName, Vec<u8>), ErrorBody> {
    let mut problems = Problems::default();
    let fields = object(body, &["workstation", "certificate_request"], &mut problems)?;
    let present = |name: &str| fields.get(name).filter(|value| !value.is_null());
    let name = match present("workstation") {
        None => {
            problems.add(FieldError::new("/workstation", FIELD_REQUIRED));
            None
        }
        Some(value) => {
            let name = value.as_str().and_then(WorkstationName::parse);
            if name.is_none() {
                problems.add(
                    FieldError::new("/workstation", NAME_INVALID)
                        .with_detail("max", MAX_WORKSTATION_NAME_LEN),
                );
            }
            name
        }
    };
    let request = match present("certificate_request") {
        None => {
            problems.add(FieldError::new("/certificate_request", FIELD_REQUIRED));
            None
        }
        Some(value) => match value.as_str().map(certificate_request) {
            Some(Ok(der)) => Some(der),
            Some(Err(key)) => {
                problems.add(request_field_error(key));
                None
            }
            None => {
                problems.add(request_field_error(REQUEST_INVALID));
                None
            }
        },
    };
    problems.into_result()?;
    match (name, request) {
        (Some(name), Some(request)) => Ok((name, request)),
        // Unreachable: a missing value added a problem above.
        _ => Err(ErrorBody::new(ErrorCode::ValidationFailed)),
    }
}

/// The DER of a PEM CSR whose signature verifies with its P-256 key; the message key of the
/// problem otherwise.
fn certificate_request(text: &str) -> Result<Vec<u8>, MessageKey> {
    let Ok(mut blocks) = pem::decode_all(text, pem::CERTIFICATE_REQUEST) else {
        return Err(REQUEST_INVALID);
    };
    if blocks.len() != 1 {
        return Err(REQUEST_INVALID);
    }
    let der = blocks.remove(0);
    match issue::requested_key(&der) {
        Ok(_) => Ok(der),
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
    let invalid = key == REQUEST_INVALID;
    let error = FieldError::new("/certificate_request", key);
    if invalid {
        error.with_detail("max_bytes", MAX_REQUEST_BYTES)
    } else {
        error
    }
}

/// The 422 for a CSR the signing refused after the request was parsed (not expected: the parse
/// checks the same).
fn request_error(problem: &IssueError) -> ErrorBody {
    ErrorBody::new(ErrorCode::ValidationFailed)
        .with_field_error(request_field_error(request_message(problem)))
}

/// A revoke request; the error body otherwise. Mirrors [`RevokeWorkstationCertificate`].
fn parse_revoke(body: &[u8]) -> Result<RevokeWorkstationCertificate, ErrorBody> {
    let mut problems = Problems::default();
    let fields = object(body, &["reason"], &mut problems)?;
    let reason = match fields.get("reason").filter(|value| !value.is_null()) {
        None => None,
        Some(value) => {
            let reason = value.as_str().and_then(RevocationReason::parse);
            if reason.is_none() {
                problems.add(FieldError::new("/reason", REASON_INVALID));
            }
            reason
        }
    };
    problems.into_result()?;
    Ok(RevokeWorkstationCertificate { reason })
}

/// The serial number of the path, 1 to 20 octets written as hex digits (either case); `None`
/// (answered 404) otherwise.
fn parse_serial(path: Result<Path<String>, PathRejection>) -> Option<Vec<u8>> {
    let Path(text) = path.ok()?;
    let digits = text.as_bytes();
    if !(2..=40).contains(&digits.len()) || digits.len() % 2 != 0 {
        return None;
    }
    digits
        .chunks(2)
        .map(|pair| Some((hex_digit(pair[0])? << 4) | hex_digit(pair[1])?))
        .collect()
}

fn hex_digit(byte: u8) -> Option<u8> {
    char::from(byte)
        .to_digit(16)
        .and_then(|digit| u8::try_from(digit).ok())
}

/// What the issue request answers.
fn issued_view(issued: &IssuedCertificate, name: &WorkstationName) -> WorkstationCertificate {
    WorkstationCertificate {
        serial: issue::hex(&issued.serial),
        workstation: name.as_str().to_owned(),
        subject: issued.subject.clone(),
        ca_key_id: issued.ca_key_id.clone(),
        not_before: issue::utc(issued.not_before),
        not_after: issue::utc(issued.not_after),
        certificate: pem::encode(pem::CERTIFICATE, &issued.der),
        revoked_at: None,
        revocation_reason: None,
    }
}

/// A ledger row of a workstation certificate, as the API shows it.
fn ledger_view(serial: &[u8], row: &PgRow) -> Result<WorkstationCertificate, sqlx::Error> {
    let subject: String = row.try_get("subject")?;
    let workstation = subject
        .strip_prefix(WORKSTATION_SUBJECT_PREFIX)
        .unwrap_or(&subject)
        .to_owned();
    let der: Vec<u8> = row.try_get("certificate")?;
    let reason: Option<i16> = row.try_get("revocation_reason")?;
    Ok(WorkstationCertificate {
        serial: issue::hex(serial),
        workstation,
        subject,
        ca_key_id: row.try_get("ca_key_id")?,
        not_before: row.try_get::<DateTime<Utc>, _>("not_before")?,
        not_after: row.try_get::<DateTime<Utc>, _>("not_after")?,
        certificate: pem::encode(pem::CERTIFICATE, &der),
        revoked_at: row.try_get("revoked_at")?,
        revocation_reason: reason.and_then(RevocationReason::from_code),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::api::MAX_FIELD_ERRORS;

    fn bytes(value: &Value) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }

    fn field_errors(body: &ErrorBody) -> Vec<(String, String)> {
        body.field_errors()
            .iter()
            .map(|error| (error.field.clone(), error.message_key.as_str().to_owned()))
            .collect()
    }

    #[test]
    fn serials_are_hex_octets() {
        let parse = |text: &str| parse_serial(Ok(Path(text.to_owned())));
        assert_eq!(parse("4f0A"), Some(vec![0x4f, 0x0a]));
        assert_eq!(parse(&"ab".repeat(20)).map(|serial| serial.len()), Some(20));
        for refused in ["", "4", "4f0", "zz", "4f 0", &"ab".repeat(21), "+f"] {
            assert_eq!(parse(refused), None, "{refused:?}");
        }
    }

    #[test]
    fn issue_requests_name_each_problem() {
        let refused = parse_issue(&bytes(&json!({
            "workstation": "-bad",
            "certificate_request": "not PEM",
            "subject": "gw-1",
            "a/b~c": 1,
        })))
        .unwrap_err();
        let mut errors = field_errors(&refused);
        errors.sort();
        assert_eq!(
            errors,
            [
                ("/a~1b~0c".to_owned(), UNKNOWN_FIELD.as_str().to_owned()),
                (
                    "/certificate_request".to_owned(),
                    REQUEST_INVALID.as_str().to_owned()
                ),
                ("/subject".to_owned(), UNKNOWN_FIELD.as_str().to_owned()),
                ("/workstation".to_owned(), NAME_INVALID.as_str().to_owned()),
            ]
        );
        let missing = parse_issue(&bytes(&json!({"workstation": null}))).unwrap_err();
        let mut errors = field_errors(&missing);
        errors.sort();
        assert_eq!(
            errors,
            [
                (
                    "/certificate_request".to_owned(),
                    FIELD_REQUIRED.as_str().to_owned()
                ),
                (
                    "/workstation".to_owned(),
                    FIELD_REQUIRED.as_str().to_owned()
                ),
            ]
        );
        for not_an_object in [json!([]), json!("x"), json!(null)] {
            let refused = parse_issue(&bytes(&not_an_object)).unwrap_err();
            assert_eq!(refused.code(), ErrorCode::InvalidRequest);
        }
        assert_eq!(
            parse_issue(b"{").unwrap_err().code(),
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn many_unknown_fields_are_counted_not_listed() {
        let mut body = json!({});
        for index in 0..25 {
            body[format!("extra{index}")] = json!(1);
        }
        let refused = parse_revoke(&bytes(&body)).unwrap_err();
        assert_eq!(field_errors(&refused).len(), MAX_FIELD_ERRORS);
        assert_eq!(refused.details()["omitted_field_errors"], json!(15));
    }

    #[test]
    fn revoke_requests_take_an_optional_known_reason() {
        assert_eq!(parse_revoke(b"{}").unwrap().reason, None);
        assert_eq!(
            parse_revoke(&bytes(&json!({"reason": null})))
                .unwrap()
                .reason,
            None
        );
        assert_eq!(
            parse_revoke(&bytes(&json!({"reason": "key_compromise"})))
                .unwrap()
                .reason,
            Some(RevocationReason::KeyCompromise)
        );
        for reason in [json!("certificate_hold"), json!(1), json!("KEY_COMPROMISE")] {
            let refused = parse_revoke(&bytes(&json!({ "reason": reason }))).unwrap_err();
            assert_eq!(
                field_errors(&refused),
                [("/reason".to_owned(), REASON_INVALID.as_str().to_owned())]
            );
        }
    }
}
