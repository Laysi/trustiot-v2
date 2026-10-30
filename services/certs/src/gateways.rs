//! Revoking a gateway's certificates (Laysi/trustiot-v2#636; `README.md`, "閘道器憑證的撤銷"),
//! the three steps of decision 6 in the order decided on 2026-10-24:
//!
//! 1. Every certificate of the gateway that is neither revoked nor expired is revoked in the
//!    ledger, in one statement ([`GatewayRevocation::revoke`]). Reentrant: a certificate revoked
//!    before is left as it is (revocation is final, `0002_workstation_certificates.sql`), and a
//!    second call finds nothing left to revoke.
//! 2. Each issuing CA among them gets a new CRL that lists them, one per CA
//!    ([`crate::crl::Reissuer::after_revocations`], Laysi/trustiot-v2#635).
//! 3. The broker's permission sync compares the gateway at once ([`BrokerSync::gateway`],
//!    Laysi/trustiot-v2#633): with no valid certificate left the gateway should have no
//!    dynamic-security client, so the sync sends `deleteClient`, which also drops the gateway's
//!    connection. The replica that revoked sends it itself, on its own connection to the broker,
//!    under the sync's job lock (a whole comparison of another replica delays it by its length).
//!
//! The broker's trust sidecar then loads the new CRL within one `mqttBroker.trustInterval` and
//! sends the broker SIGHUP; from then on the TLS handshake refuses the certificates. Between the
//! kick and that reload the gateway can reconnect, but it has no client and may do nothing.
//!
//! A gateway still registered and bound can renew afterwards ([`crate::renewal`]) and so come
//! back with a new certificate, unless the reason is keyCompromise: that ends its renewals for
//! good, since the renewal proves the gateway with the very key reported compromised.
//!
//! Once step 1 has started, the rest runs on a task of its own, so a caller that goes away (a
//! client that disconnects, the suspension of Laysi/trustiot-v2#108 or the unbinding of
//! Laysi/trustiot-v2#115 being cancelled) leaves no revocation without its CRL and its kick.
//!
//! The platform administrators' API:
//!
//! | Request | Success |
//! | --- | --- |
//! | `POST /api/certs/v1/gateways/{hardware_id}/revoke` | 200 [`RevokedGateway`] |
//!
//! Errors follow M1:
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | Not a platform administrator, or a token another service exchanged (`act`) | 403 `permission_denied` |
//! | `Content-Type` not `application/json` (parameters allowed) | 400 `invalid_request` |
//! | A body over [`MAX_BODY_BYTES`] | 400 `invalid_request`, key `gateway_revocation.body_too_large`, `{"max_bytes"}` |
//! | A body that is not a JSON object | 400 `invalid_request`, key `gateway_revocation.body_invalid` |
//! | An unknown field, or a reason that is not one of [`RevocationReason`] | 422 `validation_failed`, one field error per problem |
//! | A `hardware_id` that is not a gateway ID, or no gateway is registered with it | 404 `not_found` |
//! | The gateway has no certificate that is neither revoked nor expired | 409 `conflict`, key `gateway_revocation.no_valid_certificate` |
//! | The database failed | 503 `unavailable`; nothing was revoked |
//!
//! The checks run in that order, so a refused request reaches neither the database nor the
//! broker. Every response carries `cache-control: no-store`. Each revocation logs one INFO line
//! with who asked (the administrator's user id), the hardware ID, the serial numbers and their
//! CAs; never a certificate.

use std::collections::BTreeMap;
use std::fmt;

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{FromRef, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::post;
use chrono::{DateTime, Utc};
use contract::certs::{RevocationReason, RevokeGateway, RevokedGateway, RevokedGatewayCertificate};
use contract::{ErrorBody, ErrorCode, FieldError, MessageKey, UserId};
use protocol::GatewayId;
use service_kit::identity::{IdentityVerifier, UserIdentity};
use sqlx::{PgPool, Row as _};
use tracing::Instrument as _;

use crate::api::{self, BodyKeys, Problems, error, ok, refuse};
use crate::crl::Reissuer;
use crate::dynsec::BrokerSync;
use crate::issue;

/// Revoking every valid certificate of one gateway, as the ingress gateway forwards it.
pub const REVOKE_PATH: &str = "/api/certs/v1/gateways/{hardware_id}/revoke";

/// The largest body accepted: `{"reason": "..."}` with room.
pub const MAX_BODY_BYTES: usize = 1024;

/// The body is larger than [`MAX_BODY_BYTES`] (400); `details.max_bytes`.
pub const BODY_TOO_LARGE: MessageKey = MessageKey::new("gateway_revocation.body_too_large");
/// The body is not a JSON object (400).
pub const BODY_INVALID: MessageKey = MessageKey::new("gateway_revocation.body_invalid");
/// A field the request does not have (422).
pub const UNKNOWN_FIELD: MessageKey = MessageKey::new("gateway_revocation.unknown_field");
/// The revocation reason is not one of [`RevocationReason`] (422).
pub const REASON_INVALID: MessageKey = MessageKey::new("gateway_revocation.reason_invalid");
/// The gateway has no certificate that is neither revoked nor expired (409): revoked already,
/// or every certificate it had has expired.
pub const NO_VALID_CERTIFICATE: MessageKey =
    MessageKey::new("gateway_revocation.no_valid_certificate");

/// Revokes every certificate of `$1` (a hardware ID) that is neither revoked nor expired, with
/// the reason `$2`; returns the rows it revoked. `revoked_at` is the clock when each row is
/// updated, not the transaction's start: the CRL job compares it with its CRL's thisUpdate to
/// find revocations a CRL may have missed ([`crate::crl::refresh`]).
/// Serializes the revocation of a gateway's certificates (`$1`, its hardware ID) with the
/// signing of its renewed ones, held until the transaction ends. Both take it first: a renewal
/// that signs while a revocation waits is revoked by it once it commits, and a renewal that waits
/// for a revocation sees it (READ COMMITTED reads what committed before each statement), so a
/// renewal never escapes a revocation that commits around it, and a keyCompromise revocation
/// always ends the renewals (`crate::renewal`). Class `0x67777276` ("gwrv"); the hardware ID is
/// hashed into the second key.
pub const LOCK_GATEWAY_CERTIFICATES: &str =
    "SELECT pg_advisory_xact_lock(1735881334, hashtext($1))";

/// Whether a gateway certificate of `$1` was revoked for keyCompromise (1): the gateway's
/// registered key is not trusted any more, so it cannot renew.
pub const KEY_COMPROMISED: &str = "SELECT EXISTS (
    SELECT 1 FROM certs.issued_certificates
    WHERE profile = 'gateway' AND subject = $1 AND revocation_reason = 1)";

const REVOKE: &str = "UPDATE certs.issued_certificates
SET revoked_at = clock_timestamp(), revocation_reason = $2
WHERE profile = 'gateway' AND subject = $1 AND revoked_at IS NULL
  AND not_after > clock_timestamp()
RETURNING serial, ca_key_id, not_before, not_after, revoked_at, revocation_reason";

/// Whether `$1` has any gateway certificate at all: exactly the registered hardware IDs. The
/// factory registration records a gateway's first certificate in the transaction that registers
/// it (`gateway_registrations.certificate_serial` references the ledger), only registered gateways
/// are renewed, and no role may delete a ledger row. So this answers "registered?" without the
/// platform role, which alone reads the registrations.
const HAS_CERTIFICATE: &str = "SELECT EXISTS (
    SELECT 1 FROM certs.issued_certificates WHERE profile = 'gateway' AND subject = $1)";

/// Who asked for a revocation, as its log line names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Requester {
    /// A platform administrator through the API.
    PlatformAdmin(UserId),
    /// The service itself, for `purpose`: a tenant's suspension (Laysi/trustiot-v2#108), a
    /// gateway's unbinding or decommissioning (Laysi/trustiot-v2#115).
    Service { purpose: &'static str },
}

/// A certificate [`GatewayRevocation::revoke`] revoked, as the ledger now records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevokedCertificate {
    pub serial: Vec<u8>,
    /// The issuing CA that signed it.
    pub ca_key_id: String,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    pub revoked_at: DateTime<Utc>,
    pub reason: Option<RevocationReason>,
}

/// Why nothing was revoked.
#[derive(Debug, thiserror::Error)]
pub enum RevocationError {
    /// No gateway is registered with the hardware ID.
    #[error("no gateway is registered with this hardware id")]
    NotRegistered,
    /// Every certificate of the gateway is revoked already or has expired.
    #[error("the gateway has no certificate that is neither revoked nor expired")]
    NoValidCertificate,
    /// The database failed: nothing was revoked, unless the connection broke while committing.
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
    /// The revocation's task ended without an answer (a panic). Whether the ledger changed is
    /// unknown; calling again is safe.
    #[error("the revocation task failed: {0}")]
    Interrupted(String),
}

/// The three steps for one gateway, for the API and for the suspension (Laysi/trustiot-v2#108)
/// and unbinding (Laysi/trustiot-v2#115) that call it. Cloning is cheap.
#[derive(Clone)]
pub struct GatewayRevocation {
    pool: PgPool,
    crls: Reissuer,
    broker_sync: BrokerSync,
}

impl fmt::Debug for GatewayRevocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayRevocation")
            .field("broker_sync", &self.broker_sync)
            .finish_non_exhaustive()
    }
}

impl GatewayRevocation {
    /// Revocations in the ledger of the service role's `pool`, new CRLs from `crls`, and the
    /// broker's permissions through `broker_sync`.
    #[must_use]
    pub fn new(pool: PgPool, crls: Reissuer, broker_sync: BrokerSync) -> Self {
        Self {
            pool,
            crls,
            broker_sync,
        }
    }

    /// Revokes every certificate of `hardware_id` that is neither revoked nor expired, with
    /// `reason`, then gives each of their issuing CAs a new CRL, then asks the broker's
    /// permission sync for the gateway; returns what it revoked, by serial number. Runs on a
    /// task of its own, so dropping the returned future does not stop it halfway. A new CRL that
    /// fails is logged and caught up by the CRL job; the sync is asked only, never awaited (it
    /// logs its outcome).
    ///
    /// # Errors
    ///
    /// [`RevocationError::NotRegistered`] and [`RevocationError::NoValidCertificate`] change
    /// nothing; [`RevocationError::Database`] when the ledger could not be read or written, with
    /// nothing revoked (unless the connection broke while committing).
    pub async fn revoke(
        &self,
        hardware_id: &GatewayId,
        reason: Option<RevocationReason>,
        requester: Requester,
    ) -> Result<Vec<RevokedCertificate>, RevocationError> {
        let this = self.clone();
        let hardware_id = hardware_id.clone();
        tokio::spawn(
            async move { this.revoke_now(&hardware_id, reason, &requester).await }
                .instrument(tracing::Span::current()),
        )
        .await
        .map_err(|error| RevocationError::Interrupted(error.to_string()))?
    }

    async fn revoke_now(
        &self,
        hardware_id: &GatewayId,
        reason: Option<RevocationReason>,
        requester: &Requester,
    ) -> Result<Vec<RevokedCertificate>, RevocationError> {
        // 1. The ledger. Committed from here on.
        let revoked = revoke_in_ledger(&self.pool, hardware_id, reason).await?;
        let serials = revoked
            .iter()
            .map(|certificate| issue::hex(&certificate.serial))
            .collect::<Vec<_>>()
            .join(",");
        let mut by_ca: BTreeMap<&str, Vec<&[u8]>> = BTreeMap::new();
        for certificate in &revoked {
            by_ca
                .entry(certificate.ca_key_id.as_str())
                .or_default()
                .push(&certificate.serial);
        }
        let ca_key_ids = by_ca.keys().copied().collect::<Vec<_>>().join(",");
        let reason_name = reason.map_or("none", RevocationReason::as_str);
        // The audit line goes out before the slower steps.
        match requester {
            Requester::PlatformAdmin(user_id) => tracing::info!(
                requested_by = "platform_admin",
                user_id = %user_id,
                hardware_id = hardware_id.as_str(),
                serials = %serials,
                ca_key_ids = %ca_key_ids,
                reason = reason_name,
                "revoked a gateway's certificates"
            ),
            Requester::Service { purpose } => tracing::info!(
                requested_by = "service",
                purpose,
                hardware_id = hardware_id.as_str(),
                serials = %serials,
                ca_key_ids = %ca_key_ids,
                reason = reason_name,
                "revoked a gateway's certificates"
            ),
        }
        // 2. A new CRL per CA, one CA after the other, so one revocation takes at most one of
        //    the reissuer's turns.
        for (ca_key_id, serials) in &by_ca {
            self.crls.after_revocations(ca_key_id, serials).await;
        }
        // 3. The broker: no valid certificate left, so no client, and the connection drops.
        self.broker_sync.gateway(hardware_id);
        Ok(revoked)
    }
}

/// Step 1 in one transaction: the revoked rows, sorted by serial number, or why there are none.
async fn revoke_in_ledger(
    pool: &PgPool,
    hardware_id: &GatewayId,
    reason: Option<RevocationReason>,
) -> Result<Vec<RevokedCertificate>, RevocationError> {
    let mut tx = pool.begin().await?;
    sqlx::query(LOCK_GATEWAY_CERTIFICATES)
        .bind(hardware_id.as_str())
        .execute(&mut *tx)
        .await?;
    let rows = sqlx::query(REVOKE)
        .bind(hardware_id.as_str())
        .bind(reason.map(RevocationReason::code))
        .fetch_all(&mut *tx)
        .await?;
    if rows.is_empty() {
        let registered: bool = sqlx::query_scalar(HAS_CERTIFICATE)
            .bind(hardware_id.as_str())
            .fetch_one(&mut *tx)
            .await?;
        tx.rollback().await?;
        return Err(if registered {
            RevocationError::NoValidCertificate
        } else {
            RevocationError::NotRegistered
        });
    }
    let mut revoked = rows
        .iter()
        .map(|row| {
            let code: Option<i16> = row.try_get("revocation_reason")?;
            Ok(RevokedCertificate {
                serial: row.try_get("serial")?,
                ca_key_id: row.try_get("ca_key_id")?,
                not_before: row.try_get("not_before")?,
                not_after: row.try_get("not_after")?,
                revoked_at: row.try_get("revoked_at")?,
                reason: code.and_then(RevocationReason::from_code),
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    tx.commit().await?;
    revoked.sort_by(|a, b| a.serial.cmp(&b.serial));
    Ok(revoked)
}

/// The state of the route.
#[derive(Clone)]
struct Routes {
    verifier: IdentityVerifier,
    revocation: GatewayRevocation,
}

impl FromRef<Routes> for IdentityVerifier {
    fn from_ref(state: &Routes) -> Self {
        state.verifier.clone()
    }
}

/// The route of the platform administrators' API: tokens verified with `verifier`, revocations
/// by `revocation`.
pub fn routes(verifier: IdentityVerifier, revocation: GatewayRevocation) -> Router {
    Router::new()
        .route(REVOKE_PATH, post(revoke_gateway))
        .route_layer(axum::middleware::map_response(api::no_store))
        .with_state(Routes {
            verifier,
            revocation,
        })
}

/// The message keys of a body that is not a JSON object or has unknown fields.
const BODY_KEYS: BodyKeys = BodyKeys {
    invalid: BODY_INVALID,
    unknown_field: UNKNOWN_FIELD,
};

/// Revokes every certificate of a gateway that is neither revoked nor expired: decision 6's
/// three steps, see the module's documentation.
#[utoipa::path(
    post,
    path = "/api/certs/v1/gateways/{hardware_id}/revoke",
    tag = "certs",
    operation_id = "revokeGateway",
    params(("hardware_id" = String, Path, description = "The gateway's hardware ID, as registered at the factory.")),
    request_body = RevokeGateway,
    responses(
        (status = 200, description = "Revoked now, each certificate listed with its serial number. Before this response the ledger records the revocations and each of their issuing CAs gets a CRL that lists them (should that fail, the CRL job lists them within one check interval, default 5 minutes), and the broker is asked to drop the gateway's permissions, which also disconnects it. The broker refuses the certificates in the TLS handshake once it has loaded the new CRL, within one trust interval (default 30 seconds). A renewal may later give the gateway a new certificate if it is still bound, unless the reason is `key_compromise`, which ends its renewals for good.", body = RevokedGateway),
        (status = 400, description = "The `Content-Type` is not `application/json`, the body is larger than 1 KiB (`gateway_revocation.body_too_large`), or it is not a JSON object (`gateway_revocation.body_invalid`) (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "Not a platform administrator (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No gateway is registered with this hardware ID (`not_found`).", body = ErrorBody),
        (status = 409, description = "The gateway has no certificate that is neither revoked nor expired (`conflict`, `gateway_revocation.no_valid_certificate`). Nothing changed.", body = ErrorBody),
        (status = 422, description = "An unknown field (`gateway_revocation.unknown_field`), or a reason that is not one of the listed ones (`gateway_revocation.reason_invalid`) (`validation_failed`). Nothing was revoked.", body = ErrorBody),
        (status = 500, description = "The revocation's task failed (`internal`). Whether anything was revoked is unknown; calling again tells (409 when it was).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`). Most likely nothing was revoked; should the connection have broken while committing, the revocation may stand without its new CRL and broker step, which the CRL job and the next whole comparison catch up. Calling again tells (409 when it was revoked).", body = ErrorBody),
    )
)]
async fn revoke_gateway(
    State(state): State<Routes>,
    user: UserIdentity,
    hardware_id: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Some(refused) = refuse(&user, "revoke a gateway's certificates") {
        return refused;
    }
    let body = match api::read_json(&headers, body, MAX_BODY_BYTES, BODY_TOO_LARGE).await {
        Ok(body) => body,
        Err(refused) => return error(refused),
    };
    let reason = match parse_revoke(&body) {
        Ok(request) => request.reason,
        Err(refused) => return error(refused),
    };
    let Some(hardware_id) = parse_hardware_id(hardware_id) else {
        return error(ErrorBody::new(ErrorCode::NotFound));
    };
    match state
        .revocation
        .revoke(
            &hardware_id,
            reason,
            Requester::PlatformAdmin(user.user_id()),
        )
        .await
    {
        Ok(revoked) => ok(StatusCode::OK, &view(&hardware_id, &revoked)),
        Err(RevocationError::NotRegistered) => error(ErrorBody::new(ErrorCode::NotFound)),
        Err(RevocationError::NoValidCertificate) => {
            error(ErrorBody::new(ErrorCode::Conflict).with_message_key(NO_VALID_CERTIFICATE))
        }
        Err(RevocationError::Database(problem)) => {
            tracing::error!(
                error = %problem,
                requested_by = "platform_admin",
                user_id = %user.user_id(),
                hardware_id = hardware_id.as_str(),
                "could not revoke a gateway's certificates"
            );
            error(ErrorBody::new(ErrorCode::Unavailable))
        }
        Err(problem @ RevocationError::Interrupted(_)) => {
            tracing::error!(
                error = %problem,
                requested_by = "platform_admin",
                user_id = %user.user_id(),
                hardware_id = hardware_id.as_str(),
                "could not revoke a gateway's certificates"
            );
            error(ErrorBody::new(ErrorCode::Internal))
        }
    }
}

/// A revoke request; the error body otherwise. Mirrors [`RevokeGateway`].
fn parse_revoke(body: &[u8]) -> Result<RevokeGateway, ErrorBody> {
    let mut problems = Problems::default();
    let fields = api::object(body, &["reason"], BODY_KEYS, &mut problems)?;
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
    Ok(RevokeGateway { reason })
}

/// The hardware ID of the path; `None` (answered 404, like a hardware ID nobody registered)
/// when it is not a gateway ID.
fn parse_hardware_id(path: Result<Path<String>, PathRejection>) -> Option<GatewayId> {
    let Path(text) = path.ok()?;
    GatewayId::new(text).ok()
}

/// What the API answers.
fn view(hardware_id: &GatewayId, revoked: &[RevokedCertificate]) -> RevokedGateway {
    RevokedGateway {
        hardware_id: hardware_id.as_str().to_owned(),
        revoked: revoked
            .iter()
            .map(|certificate| RevokedGatewayCertificate {
                serial: issue::hex(&certificate.serial),
                ca_key_id: certificate.ca_key_id.clone(),
                not_before: certificate.not_before,
                not_after: certificate.not_after,
                revoked_at: certificate.revoked_at,
                revocation_reason: certificate.reason,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

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
    fn revoke_requests_take_an_optional_known_reason() {
        assert_eq!(parse_revoke(b"{}").unwrap().reason, None);
        assert_eq!(
            parse_revoke(&bytes(&json!({"reason": null})))
                .unwrap()
                .reason,
            None
        );
        for reason in RevocationReason::ALL {
            assert_eq!(
                parse_revoke(&bytes(&json!({"reason": reason.as_str()})))
                    .unwrap()
                    .reason,
                Some(reason)
            );
        }
        for reason in [json!("certificate_hold"), json!(1), json!("KEY_COMPROMISE")] {
            let refused = parse_revoke(&bytes(&json!({ "reason": reason }))).unwrap_err();
            assert_eq!(refused.code(), ErrorCode::ValidationFailed);
            assert_eq!(
                field_errors(&refused),
                [("/reason".to_owned(), REASON_INVALID.as_str().to_owned())]
            );
        }
    }

    #[test]
    fn unknown_fields_and_bodies_that_are_not_objects_are_refused() {
        let refused = parse_revoke(&bytes(&json!({"serial": "4f", "reason": "x"}))).unwrap_err();
        let mut errors = field_errors(&refused);
        errors.sort();
        assert_eq!(
            errors,
            [
                ("/reason".to_owned(), REASON_INVALID.as_str().to_owned()),
                ("/serial".to_owned(), UNKNOWN_FIELD.as_str().to_owned()),
            ]
        );
        for not_an_object in [json!([]), json!("x"), json!(null)] {
            let refused = parse_revoke(&bytes(&not_an_object)).unwrap_err();
            assert_eq!(refused.code(), ErrorCode::InvalidRequest);
            assert_eq!(refused.message_key(), &BODY_INVALID);
        }
        let mut many = json!({});
        for index in 0..25 {
            many[format!("extra{index}")] = json!(1);
        }
        let refused = parse_revoke(&bytes(&many)).unwrap_err();
        assert_eq!(field_errors(&refused).len(), MAX_FIELD_ERRORS);
    }

    #[test]
    fn hardware_ids_must_be_gateway_ids() {
        let parse = |text: &str| parse_hardware_id(Ok(Path(text.to_owned())));
        assert_eq!(
            parse("gw-636.a_1").map(|id| id.as_str().to_owned()),
            Some("gw-636.a_1".to_owned())
        );
        for refused in [
            "",
            "-gw",
            "gw:1",
            "service:certs",
            "gw/1",
            "gw 1",
            &"a".repeat(65),
        ] {
            assert!(parse(refused).is_none(), "{refused:?}");
        }
    }
}
