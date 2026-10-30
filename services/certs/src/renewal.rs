//! The gateway certificate renewal endpoint (Laysi/trustiot-v2#81, rebuild checklist D2, decision
//! 12; README.md, "換發"; the wire format is `protocol::renewal`).
//!
//! A gateway whose certificate is about to expire, has expired or was revoked (for any reason
//! but keyCompromise, which ends its renewals: Laysi/trustiot-v2#636) cannot rely on mTLS, so it
//! renews over plain HTTPS on the ingress's public path `/certs/renew` and proves who it is with
//! its TPM key: it asks for a challenge, signs the challenge's signing input, and gets
//! a new certificate for the public key registered at the factory. Nothing in the request is
//! trusted before the signature holds; until then the endpoint is public, so:
//!
//! - **Challenges are single use and cheap.** The challenge step stores a random nonce with the
//!   hardware ID, the purpose and the expiry (`certs.renewal_challenges`) and answers the same for
//!   every hardware ID, registered or not, so it tells nothing. A redemption deletes the row
//!   before any check, so a challenge is redeemed at most once whatever the outcome. Every insert
//!   removes long-expired rows, and the rate limits bound the inserts, so the table stays small.
//! - **Rate limits come first** ([`limiter`]): right after the body is read, before the other
//!   checks and any database work, per replica: per hardware ID over both steps, and for all
//!   hardware IDs together per step, so a flood of challenge requests leaves the redemptions
//!   their turns. Past a limit the answer is 429 with `Retry-After`, logged at DEBUG only, so
//!   the INFO lines of the refusals after the limits (stale bindings, no CA) are no more than
//!   the limits let through. At most `renewal.max_concurrent` requests per replica do database
//!   work at once; a request that waits longer than [`PERMIT_WAIT`] for its turn gets 503.
//! - **The decision reads one snapshot.** The redemption reads the registration, the gateway's
//!   current binding and its tenant's status in one read-only statement of the platform role
//!   ([`crate::bindings::Projection`]'s audited transaction), verifies the signature with the
//!   registered key over the signing input rebuilt from the stored challenge, and only then looks
//!   at the gateway's state. An unregistered hardware ID gets the same 401 as a wrong signature.
//! - **No decision on stale bindings or tenant states.** While either consumer (the assets
//!   service's events for the bindings, the identity service's for the tenant states,
//!   Laysi/trustiot-v2#778) has no current lag measurement, or its oldest unprocessed event is
//!   older than `renewal.max_consumer_lag`, both steps answer 503: a gateway unbound or moved a
//!   moment ago must not renew under its old binding, nor a tenant suspended a moment ago under
//!   its old state.
//! - **Only an active tenant renews.** A tenant suspended, deleted, in a state this build does
//!   not know, or without any state stored (its `identity.tenant_created` not applied yet) is
//!   refused alike (`crate::bindings::reads_as_suspended`).
//! - **Issued exactly with its event.** The certificate is signed for the registered key (never a
//!   key the request names) and recorded in the ledger in a transaction of the gateway's tenant,
//!   with the tenant event `certs.gateway_certificate_renewed`. Earlier certificates stay valid
//!   until they expire. Once that transaction has committed, the broker's permission sync is asked
//!   to compare this gateway alone ([`crate::dynsec::BrokerSync::gateway`]), so a gateway whose
//!   only valid certificate had expired or was revoked gets its broker client back without
//!   waiting for the next whole comparison.
//! - **Refusals leave a trace without letting traffic grow the log.** Every refusal is a log line
//!   (hardware ID, step, reason; never the nonce or the signature) and `certs.renewal.refused`
//!   with the reason. Refusals of a registered hardware ID that carry a decision (an unknown,
//!   mismatched or expired challenge, a wrong signature, the gateway's state) are counted per
//!   window, and each window becomes one platform-wide `certs.gateway_renewal_refused`
//!   ([`refusals`]).

mod limiter;
mod refusals;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, RETRY_AFTER};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use chrono::SecondsFormat;
use contract::event::Actor;
use contract::events::certs::{
    GATEWAY_CERTIFICATE_AGGREGATE, GatewayCertificateRenewed, gateway_certificate_aggregate_id,
};
use contract::{
    ErrorBody, ErrorCode, FieldError, GatewayBindingId, MessageKey, ServiceName, TenantId,
};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use outbox::{Aggregate, Lag};
use protocol::GatewayId;
use protocol::renewal::{
    CHALLENGE_TTL_SECS, Challenge, MAX_REQUEST_BYTES, MAX_SIGNATURE_BYTES, NONCE_BYTES, Purpose,
    RENEWAL_PATH, RenewedCertificate, is_nonce, is_signature_text, signing_input,
};
use service_kit::Context;
use service_kit::tenant::{BackgroundTenantDb, PlatformDb};
use sqlx::PgPool;
use sqlx::Row as _;
use tokio::sync::Semaphore;
use x509_parser::asn1_rs::BitString;
use x509_parser::oid_registry::OID_SIG_ECDSA_WITH_SHA256;
use x509_parser::prelude::FromDer;
use x509_parser::x509::{AlgorithmIdentifier, SubjectPublicKeyInfo};

use crate::api;
use crate::authority::{Authority, CaState};
use crate::bindings::{Projection, TenantStatus, reads_as_suspended};
use crate::config::RenewalLimits;
use crate::dynsec::BrokerSync;
use crate::gateways;
use crate::issue::{self, Refusal};
use crate::pem;

/// The purpose of the renewal's database work in the audit log.
pub const PURPOSE: &str = "renew-gateway-certificate";

/// How long after its expiry a challenge is kept, so that a late redemption is told it expired
/// (410) rather than that it is unknown (409). Older rows are deleted by the next challenge.
pub const CHALLENGE_GRACE_SECS: i64 = 120;

/// The longest a request waits for its turn to do database work (`renewal.max_concurrent`).
pub const PERMIT_WAIT: Duration = Duration::from_secs(2);

/// `Retry-After` while every turn is taken.
const RETRY_BUSY_SECS: u64 = 1;

/// `Retry-After` while the bindings may be stale: about two lag measurements.
const RETRY_STALE_SECS: u64 = 30;

/// `Retry-After` while no issuing CA can sign.
const RETRY_CA_SECS: u64 = 60;

/// `Retry-After` after a database failure.
const RETRY_DATABASE_SECS: u64 = 5;

/// The body is larger than `protocol::renewal::MAX_REQUEST_BYTES` (400).
pub const BODY_TOO_LARGE: MessageKey = MessageKey::new("certificate_renewal.body_too_large");
/// The body is not a JSON object (400).
pub const BODY_INVALID: MessageKey = MessageKey::new("certificate_renewal.body_invalid");
/// A required field is missing (422).
pub const FIELD_REQUIRED: MessageKey = MessageKey::new("certificate_renewal.field_required");
/// `step` is neither `challenge` nor `renew` (422).
pub const STEP_INVALID: MessageKey = MessageKey::new("certificate_renewal.step_invalid");
/// `hardware_id` breaks the gateway ID rule (422).
pub const HARDWARE_ID_INVALID: MessageKey =
    MessageKey::new("certificate_renewal.hardware_id_invalid");
/// `nonce` is not a challenge's nonce as the service writes it (422).
pub const NONCE_INVALID: MessageKey = MessageKey::new("certificate_renewal.nonce_invalid");
/// `signature` is not a base64 DER signature of at most 72 bytes (422).
pub const SIGNATURE_MALFORMED: MessageKey =
    MessageKey::new("certificate_renewal.signature_malformed");
/// No challenge has the nonce: never handed out, already redeemed, or long expired (409).
pub const CHALLENGE_UNKNOWN: MessageKey = MessageKey::new("certificate_renewal.challenge_unknown");
/// The challenge was handed out for another hardware ID or another purpose (403).
pub const CHALLENGE_MISMATCH: MessageKey =
    MessageKey::new("certificate_renewal.challenge_mismatch");
/// The challenge expired before it was redeemed (410).
pub const CHALLENGE_EXPIRED: MessageKey = MessageKey::new("certificate_renewal.challenge_expired");
/// The signature does not hold for the hardware ID's registered key, or the hardware ID is not
/// registered (401).
pub const SIGNATURE_REFUSED: MessageKey = MessageKey::new("certificate_renewal.signature_refused");
/// The gateway was decommissioned (403).
pub const DECOMMISSIONED: MessageKey = MessageKey::new("certificate_renewal.decommissioned");
/// A certificate of the gateway was revoked for keyCompromise: its registered key is not trusted
/// any more (403).
pub const KEY_COMPROMISED: MessageKey = MessageKey::new("certificate_renewal.key_compromised");
/// The gateway is not bound to a tenant (403).
pub const NOT_BOUND: MessageKey = MessageKey::new("certificate_renewal.not_bound");
/// The service's copy of the bindings may be stale (503).
pub const BINDINGS_STALE: MessageKey = MessageKey::new("certificate_renewal.bindings_stale");
/// No issuing CA can sign yet (503).
pub const CA_NOT_READY: MessageKey = MessageKey::new("certificate_renewal.ca_not_ready");

/// Inserts a challenge with an expiry of `$5` seconds from now by the database clock, and
/// deletes the challenges that expired more than `$4` seconds ago (a data-modifying statement in
/// WITH runs whether or not the query reads it). Returns the expiry, Unix seconds.
const INSERT_CHALLENGE: &str = "WITH expired AS (
    DELETE FROM certs.renewal_challenges
    WHERE expires < floor(extract(epoch FROM now()))::bigint - $4
)
INSERT INTO certs.renewal_challenges (nonce, hardware_id, purpose, expires)
VALUES ($1, $2, $3, floor(extract(epoch FROM now()))::bigint + $5)
RETURNING expires";

/// Takes the challenge of nonce `$1`: deletes it and returns what it was issued for and whether
/// it has expired by the database clock.
const REDEEM_CHALLENGE: &str = "DELETE FROM certs.renewal_challenges WHERE nonce = $1
RETURNING hardware_id, purpose, expires, extract(epoch FROM now()) > expires AS expired";

/// The registration of `$1`, whether any of its certificates was revoked for keyCompromise (1),
/// its latest binding (whether it holds) and that binding's tenant's status, in one statement so
/// they come from one snapshot. No row when `$1` is not registered.
const READ_GATEWAY: &str = "SELECT registration.public_key,
    registration.decommissioned_at IS NOT NULL AS decommissioned,
    EXISTS (
        SELECT 1 FROM certs.issued_certificates AS issued
        WHERE issued.profile = 'gateway' AND issued.subject = registration.hardware_id
            AND issued.revocation_reason = 1
    ) AS key_compromised,
    binding.tenant_id::text AS tenant_id, binding.binding_id::text AS binding_id,
    binding.current, status.status
FROM certs.gateway_registrations AS registration
LEFT JOIN LATERAL (
    SELECT tenant_id, binding_id, unbound_at IS NULL AS current
    FROM certs.gateway_bindings
    WHERE hardware_id = registration.hardware_id
    ORDER BY position_tx DESC, position_id DESC
    LIMIT 1
) AS binding ON true
LEFT JOIN certs.tenant_statuses AS status ON status.tenant_id = binding.tenant_id
WHERE registration.hardware_id = $1";

/// The current lag of the consumers the renewal decides on (`outbox::Consumer::lag`), combined
/// with [`slowest`].
pub type LagReading = Arc<dyn Fn() -> Option<Lag> + Send + Sync>;

/// The lag of several consumers as one: unknown when any is unknown, otherwise the oldest
/// waiting event of all and the events of all together. [`bindings_current`] then holds only
/// when it holds for each of them.
#[must_use]
pub fn slowest(lags: impl IntoIterator<Item = Option<Lag>>) -> Option<Lag> {
    lags.into_iter().try_fold(
        Lag {
            events: 0,
            oldest_age: None,
        },
        |all, lag| {
            let lag = lag?;
            Some(Lag {
                events: all.events.saturating_add(lag.events),
                oldest_age: all.oldest_age.max(lag.oldest_age),
            })
        },
    )
}

/// What the endpoint needs from the service's startup.
pub struct Parts {
    /// The service role's pool: the challenges.
    pub pool: PgPool,
    /// The service role's tenant transactions: the ledger and the tenant event.
    pub background: BackgroundTenantDb,
    /// The platform role's pool: the gateway's registration, binding and tenant status, and the
    /// refusal counts.
    pub platform: PlatformDb,
    /// The issuing CAs.
    pub ca: CaState,
    /// The lag of the consumers that keep the bindings and the tenant states ([`slowest`]).
    pub lag: LagReading,
    /// `renewal.*`.
    pub limits: RenewalLimits,
    /// `gateway.validity`.
    pub validity: Duration,
    /// Asked to compare a gateway's broker permissions once its renewal has committed.
    pub broker_sync: BrokerSync,
}

#[derive(Clone)]
struct Routes {
    pool: PgPool,
    background: BackgroundTenantDb,
    platform: PlatformDb,
    projection: Projection,
    ca: CaState,
    lag: LagReading,
    limits: RenewalLimits,
    validity: Duration,
    limiter: Arc<limiter::Limiter>,
    permits: Arc<Semaphore>,
    metrics: Metrics,
    broker_sync: BrokerSync,
}

/// `POST /certs/renew` (`protocol::renewal::RENEWAL_PATH`), both steps. The router's responses
/// are never stored. Not in the OpenAPI document: a gateway calls it, not the frontend, and its
/// contract is `protocol::renewal` with README.md, "換發".
pub fn routes(parts: Parts) -> Router {
    let permits = usize::try_from(parts.limits.max_concurrent).unwrap_or(1);
    Router::new()
        .route(RENEWAL_PATH, post(renew))
        .route_layer(axum::middleware::map_response(api::no_store))
        .with_state(Routes {
            projection: Projection::new(parts.platform.clone()),
            limiter: Arc::new(limiter::Limiter::new(&parts.limits)),
            permits: Arc::new(Semaphore::new(permits)),
            metrics: Metrics::new(),
            pool: parts.pool,
            background: parts.background,
            platform: parts.platform,
            ca: parts.ca,
            lag: parts.lag,
            limits: parts.limits,
            validity: parts.validity,
            broker_sync: parts.broker_sync,
        })
}

/// Reports the ended windows of refused renewals every `interval` ([`refusals`]).
pub fn start_reporting<T>(context: &Context<T>, platform: PlatformDb, interval: Duration) {
    refusals::start(context, platform, interval);
}

/// Whether the bindings are current enough to decide on: a current lag measurement whose oldest
/// waiting event is at most `max` old.
#[must_use]
pub fn bindings_current(lag: Option<Lag>, max: Duration) -> bool {
    lag.is_some_and(|lag| lag.oldest_age.is_none_or(|age| age <= max))
}

/// Which step a request was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Challenge,
    Renew,
    /// The body could not be read far enough to tell.
    Unknown,
}

impl Step {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Challenge => "challenge",
            Self::Renew => "renew",
            Self::Unknown => "unknown",
        }
    }
}

/// Why a request was refused: the `reason` of the log line and the metric.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reason {
    /// 400 or 422: the body.
    Invalid,
    /// 429, with the seconds to wait.
    RateLimited(u64),
    /// 503: every turn is taken.
    Busy,
    /// 503: the bindings may be stale.
    BindingsStale,
    /// 503: no issuing CA can sign.
    CaNotReady,
    /// 503: the database or signing failed.
    Unavailable,
    /// 409.
    ChallengeUnknown,
    /// 403.
    ChallengeMismatch,
    /// 410.
    ChallengeExpired,
    /// 401: a wrong signature, or a hardware ID that is not registered.
    SignatureInvalid,
    /// 403.
    Decommissioned,
    /// 403: a certificate of the gateway was revoked for keyCompromise.
    KeyCompromised,
    /// 403.
    NotBound,
    /// 403.
    TenantSuspended,
}

impl Reason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid_request",
            Self::RateLimited(_) => "rate_limited",
            Self::Busy => "busy",
            Self::BindingsStale => "bindings_stale",
            Self::CaNotReady => "ca_not_ready",
            Self::Unavailable => "unavailable",
            Self::ChallengeUnknown => "challenge_unknown",
            Self::ChallengeMismatch => "challenge_mismatch",
            Self::ChallengeExpired => "challenge_expired",
            Self::SignatureInvalid => "signature_invalid",
            Self::Decommissioned => "decommissioned",
            Self::KeyCompromised => "key_compromised",
            Self::NotBound => "not_bound",
            Self::TenantSuspended => "tenant_suspended",
        }
    }

    /// Whether the refusal is counted for the hardware ID ([`refusals`]): the ones a decision
    /// about the redemption made. Rate limits, overload and malformed requests are not; they
    /// come before any database work and say nothing about the gateway.
    const fn counted(self) -> bool {
        matches!(
            self,
            Self::ChallengeUnknown
                | Self::ChallengeMismatch
                | Self::ChallengeExpired
                | Self::SignatureInvalid
                | Self::Decommissioned
                | Self::KeyCompromised
                | Self::NotBound
                | Self::TenantSuspended
        )
    }

    /// The status, the body and the `Retry-After` seconds of the answer. M1 has no code for 410,
    /// so an expired challenge answers 410 with `conflict` and its own message key.
    fn answer(self) -> (StatusCode, ErrorBody, Option<u64>) {
        let refused = |code, key| ErrorBody::new(code).with_message_key(key);
        match self {
            Self::Invalid => (
                StatusCode::BAD_REQUEST,
                ErrorBody::new(ErrorCode::InvalidRequest),
                None,
            ),
            Self::RateLimited(seconds) => (
                StatusCode::TOO_MANY_REQUESTS,
                ErrorBody::new(ErrorCode::RateLimited),
                Some(seconds),
            ),
            Self::Busy => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorBody::new(ErrorCode::Unavailable),
                Some(RETRY_BUSY_SECS),
            ),
            Self::BindingsStale => (
                StatusCode::SERVICE_UNAVAILABLE,
                refused(ErrorCode::Unavailable, BINDINGS_STALE),
                Some(RETRY_STALE_SECS),
            ),
            Self::CaNotReady => (
                StatusCode::SERVICE_UNAVAILABLE,
                refused(ErrorCode::Unavailable, CA_NOT_READY),
                Some(RETRY_CA_SECS),
            ),
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorBody::new(ErrorCode::Unavailable),
                Some(RETRY_DATABASE_SECS),
            ),
            Self::ChallengeUnknown => (
                StatusCode::CONFLICT,
                refused(ErrorCode::Conflict, CHALLENGE_UNKNOWN),
                None,
            ),
            Self::ChallengeMismatch => (
                StatusCode::FORBIDDEN,
                refused(ErrorCode::PermissionDenied, CHALLENGE_MISMATCH),
                None,
            ),
            Self::ChallengeExpired => (
                StatusCode::GONE,
                refused(ErrorCode::Conflict, CHALLENGE_EXPIRED),
                None,
            ),
            Self::SignatureInvalid => (
                StatusCode::UNAUTHORIZED,
                refused(ErrorCode::Unauthenticated, SIGNATURE_REFUSED),
                None,
            ),
            Self::Decommissioned => (
                StatusCode::FORBIDDEN,
                refused(ErrorCode::PermissionDenied, DECOMMISSIONED),
                None,
            ),
            Self::KeyCompromised => (
                StatusCode::FORBIDDEN,
                refused(ErrorCode::PermissionDenied, KEY_COMPROMISED),
                None,
            ),
            Self::NotBound => (
                StatusCode::FORBIDDEN,
                refused(ErrorCode::PermissionDenied, NOT_BOUND),
                None,
            ),
            Self::TenantSuspended => (
                StatusCode::FORBIDDEN,
                ErrorBody::new(ErrorCode::TenantSuspended),
                None,
            ),
        }
    }
}

/// The endpoint's instruments (README.md, "換發", "指標").
#[derive(Clone, Debug)]
struct Metrics {
    challenges: Counter<u64>,
    issued: Counter<u64>,
    refused: Counter<u64>,
}

impl Metrics {
    fn new() -> Self {
        let meter = opentelemetry::global::meter("certs");
        Self {
            challenges: meter
                .u64_counter("certs.renewal.challenges")
                .with_unit("{challenge}")
                .with_description("Renewal challenges handed out")
                .build(),
            issued: meter
                .u64_counter("certs.renewal.issued")
                .with_unit("{certificate}")
                .with_description("Gateway certificates renewed")
                .build(),
            refused: meter
                .u64_counter("certs.renewal.refused")
                .with_unit("{request}")
                .with_description("Renewal requests refused, by reason and step")
                .build(),
        }
    }
}

/// A request after its body was read.
enum Request {
    Challenge(GatewayId),
    Renew {
        hardware_id: GatewayId,
        nonce: String,
        nonce_bytes: Vec<u8>,
        signature: Vec<u8>,
    },
}

impl Request {
    fn step(&self) -> Step {
        match self {
            Self::Challenge(_) => Step::Challenge,
            Self::Renew { .. } => Step::Renew,
        }
    }

    /// The step whose global rate limit the request counts against.
    fn limited_step(&self) -> limiter::Step {
        match self {
            Self::Challenge(_) => limiter::Step::Challenge,
            Self::Renew { .. } => limiter::Step::Renew,
        }
    }

    fn hardware_id(&self) -> &GatewayId {
        match self {
            Self::Challenge(hardware_id) | Self::Renew { hardware_id, .. } => hardware_id,
        }
    }
}

async fn renew(State(state): State<Routes>, headers: HeaderMap, body: Body) -> Response {
    let body = match api::read_json(&headers, body, MAX_REQUEST_BYTES, BODY_TOO_LARGE).await {
        Ok(body) => body,
        Err(refused) => return state.invalid(refused),
    };
    let request = match parse(&body) {
        Ok(request) => request,
        Err(refused) => return state.invalid(refused),
    };
    let (step, hardware_id) = (request.step(), request.hardware_id().clone());
    // The limits first: every refusal after them is an INFO line, so over the limits a flood of
    // made-up hardware IDs is answered without one, whatever else is wrong.
    let limited = request.limited_step();
    if let Err(wait) = state
        .limiter
        .check(limited, hardware_id.as_str(), Instant::now())
    {
        let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
        let reason = Reason::RateLimited(seconds.max(1));
        return state.refuse(step, &hardware_id, reason).await;
    }
    if !bindings_current((state.lag)(), state.limits.max_consumer_lag) {
        return state
            .refuse(step, &hardware_id, Reason::BindingsStale)
            .await;
    }
    let Some(authority) = state.ca.authority() else {
        return state.refuse(step, &hardware_id, Reason::CaNotReady).await;
    };
    let permits = Arc::clone(&state.permits);
    let Ok(Ok(_permit)) = tokio::time::timeout(PERMIT_WAIT, permits.acquire_owned()).await else {
        return state.refuse(step, &hardware_id, Reason::Busy).await;
    };
    let outcome = match request {
        Request::Challenge(hardware_id) => state.challenge(&hardware_id).await,
        Request::Renew {
            hardware_id,
            nonce,
            nonce_bytes,
            signature,
        } => {
            state
                .redeem(&authority, &hardware_id, &nonce, &nonce_bytes, &signature)
                .await
        }
    };
    match outcome {
        Ok(response) => response,
        Err(reason) => state.refuse(step, &hardware_id, reason).await,
    }
}

/// The body as one of the two steps. Unknown fields are ignored (`protocol::renewal`).
fn parse(body: &[u8]) -> Result<Request, ErrorBody> {
    let Ok(serde_json::Value::Object(fields)) = serde_json::from_slice(body) else {
        return Err(ErrorBody::new(ErrorCode::InvalidRequest).with_message_key(BODY_INVALID));
    };
    let mut problems = api::Problems::default();
    let step = text_field(
        &fields,
        "step",
        STEP_INVALID,
        |step| matches!(step, "challenge" | "renew"),
        &mut problems,
    );
    let Some(step) = step else {
        return Err(problems
            .into_result()
            .err()
            .unwrap_or_else(|| ErrorBody::new(ErrorCode::InvalidRequest)));
    };
    let hardware_id = text_field(
        &fields,
        "hardware_id",
        HARDWARE_ID_INVALID,
        |id| GatewayId::new(id).is_ok(),
        &mut problems,
    )
    .and_then(|id| GatewayId::new(id).ok());
    if step == "challenge" {
        problems.into_result()?;
        return hardware_id
            .map(Request::Challenge)
            .ok_or_else(|| ErrorBody::new(ErrorCode::InvalidRequest));
    }
    let nonce = text_field(&fields, "nonce", NONCE_INVALID, is_nonce, &mut problems);
    let signature = text_field(
        &fields,
        "signature",
        SIGNATURE_MALFORMED,
        |text| signature_der(text).is_some(),
        &mut problems,
    )
    .and_then(signature_der);
    problems.into_result()?;
    let nonce_bytes = nonce
        .and_then(|nonce| URL_SAFE_NO_PAD.decode(nonce).ok())
        .filter(|bytes| bytes.len() == NONCE_BYTES);
    match (hardware_id, nonce, nonce_bytes, signature) {
        (Some(hardware_id), Some(nonce), Some(nonce_bytes), Some(signature)) => {
            Ok(Request::Renew {
                hardware_id,
                nonce: nonce.to_owned(),
                nonce_bytes,
                signature,
            })
        }
        // The checks above passed, so every field decoded.
        _ => Err(ErrorBody::new(ErrorCode::InvalidRequest)),
    }
}

/// The text of field `name` when it passes `valid`; otherwise a field error (`FIELD_REQUIRED`
/// when it is missing or null, `invalid` when it is not a string that passes) and `None`.
fn text_field<'a>(
    fields: &'a serde_json::Map<String, serde_json::Value>,
    name: &str,
    invalid: MessageKey,
    valid: impl Fn(&str) -> bool,
    problems: &mut api::Problems,
) -> Option<&'a str> {
    let Some(value) = fields.get(name).filter(|value| !value.is_null()) else {
        problems.add(FieldError::new(format!("/{name}"), FIELD_REQUIRED));
        return None;
    };
    let text = value.as_str().filter(|text| valid(text));
    if text.is_none() {
        problems.add(FieldError::new(format!("/{name}"), invalid));
    }
    text
}

/// The DER signature of `text`, when it is standard base64 with padding of 1 to
/// `MAX_SIGNATURE_BYTES` bytes.
fn signature_der(text: &str) -> Option<Vec<u8>> {
    if !is_signature_text(text) {
        return None;
    }
    STANDARD
        .decode(text)
        .ok()
        .filter(|der| (1..=MAX_SIGNATURE_BYTES).contains(&der.len()))
}

/// What the redemption reads about a gateway in one statement ([`READ_GATEWAY`]).
struct Gateway {
    public_key: Vec<u8>,
    decommissioned: bool,
    /// Whether a certificate of it was revoked for keyCompromise.
    key_compromised: bool,
    /// Its tenant and binding while its latest binding holds.
    binding: Option<(TenantId, GatewayBindingId)>,
    /// That binding's tenant's stored status; `None` when there is none.
    status: Option<TenantStatus>,
    /// Whether that tenant counts as suspended (`crate::bindings::reads_as_suspended`).
    suspended: bool,
}

impl Routes {
    /// Answers a body that could not be read. Logged at DEBUG only: it comes before the rate
    /// limits, so a flood of garbage must not become a flood of log lines.
    fn invalid(&self, body: ErrorBody) -> Response {
        self.metrics.refused.add(
            1,
            &[
                KeyValue::new("reason", Reason::Invalid.as_str()),
                KeyValue::new("step", Step::Unknown.as_str()),
            ],
        );
        tracing::debug!(
            message_key = body.message_key().as_str(),
            "refused a certificate renewal request: the body"
        );
        answer(body.http_status(), body, None)
    }

    /// Answers a refusal of `hardware_id`'s request: the metric, the log line, the refusal count
    /// when the reason is counted, and the error.
    async fn refuse(&self, step: Step, hardware_id: &GatewayId, reason: Reason) -> Response {
        self.metrics.refused.add(
            1,
            &[
                KeyValue::new("reason", reason.as_str()),
                KeyValue::new("step", step.as_str()),
            ],
        );
        if matches!(reason, Reason::RateLimited(_)) {
            // Past the limits a flood would otherwise be a flood of INFO lines.
            tracing::debug!(
                hardware_id = %hardware_id,
                step = step.as_str(),
                reason = reason.as_str(),
                "refused a certificate renewal request"
            );
        } else {
            tracing::info!(
                hardware_id = %hardware_id,
                step = step.as_str(),
                reason = reason.as_str(),
                "refused a certificate renewal request"
            );
        }
        if reason.counted() {
            match refusals::count(
                &self.platform,
                hardware_id,
                reason.as_str(),
                self.limits.refusal_window,
            )
            .await
            {
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    hardware_id = %hardware_id,
                    reason = reason.as_str(),
                    error = %error,
                    "could not count a refused certificate renewal"
                ),
            }
        }
        let (status, body, retry_after) = reason.answer();
        answer(status, body, retry_after)
    }

    /// Step 1: stores a new challenge and hands it out.
    async fn challenge(&self, hardware_id: &GatewayId) -> Result<Response, Reason> {
        let mut nonce = [0_u8; NONCE_BYTES];
        getrandom::fill(&mut nonce).map_err(|error| {
            tracing::error!(error = %error, "no randomness for a renewal challenge");
            Reason::Unavailable
        })?;
        let ttl = i64::try_from(CHALLENGE_TTL_SECS).unwrap_or(i64::MAX);
        let expires: i64 = sqlx::query_scalar(INSERT_CHALLENGE)
            .bind(nonce.as_slice())
            .bind(hardware_id.as_str())
            .bind(Purpose::CertRenewal.as_str())
            .bind(CHALLENGE_GRACE_SECS)
            .bind(ttl)
            .fetch_one(&self.pool)
            .await
            .map_err(|error| database("store a renewal challenge", &error))?;
        let expires = u64::try_from(expires).map_err(|_| Reason::Unavailable)?;
        self.metrics.challenges.add(1, &[]);
        tracing::debug!(hardware_id = %hardware_id, expires, "handed out a renewal challenge");
        Ok(api::ok(
            StatusCode::OK,
            &Challenge {
                hardware_id: hardware_id.clone(),
                purpose: Purpose::CertRenewal,
                nonce: URL_SAFE_NO_PAD.encode(nonce),
                expires,
            },
        ))
    }

    /// Step 2: takes the challenge, checks the signature and the gateway's state, and issues.
    async fn redeem(
        &self,
        authority: &Authority,
        hardware_id: &GatewayId,
        nonce: &str,
        nonce_bytes: &[u8],
        signature: &[u8],
    ) -> Result<Response, Reason> {
        // Deleted before any check and committed on its own: a challenge is redeemed at most
        // once, whatever the outcome.
        let taken = sqlx::query(REDEEM_CHALLENGE)
            .bind(nonce_bytes)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| database("redeem a renewal challenge", &error))?;
        let Some(taken) = taken else {
            return Err(Reason::ChallengeUnknown);
        };
        let read = |error: sqlx::Error| database("read a renewal challenge", &error);
        let issued_for: String = taken.try_get("hardware_id").map_err(read)?;
        let purpose: String = taken.try_get("purpose").map_err(read)?;
        let expires: i64 = taken.try_get("expires").map_err(read)?;
        let expired: bool = taken.try_get("expired").map_err(read)?;
        if issued_for != hardware_id.as_str() || purpose != Purpose::CertRenewal.as_str() {
            return Err(Reason::ChallengeMismatch);
        }
        if expired {
            return Err(Reason::ChallengeExpired);
        }
        let expires = u64::try_from(expires).map_err(|_| Reason::Unavailable)?;

        let gateway = self.read_gateway(hardware_id).await?;
        let Some(gateway) = gateway else {
            // The same answer as a wrong signature: the step tells nothing about registration.
            return Err(Reason::SignatureInvalid);
        };
        let key = issue::registered_key(&gateway.public_key).map_err(|_| {
            tracing::error!(
                hardware_id = %hardware_id,
                "the registered public key is not a P-256 SubjectPublicKeyInfo"
            );
            Reason::Unavailable
        })?;
        let input = signing_input(hardware_id, Purpose::CertRenewal, nonce, expires);
        if !signature_holds(&gateway.public_key, signature, &input) {
            return Err(Reason::SignatureInvalid);
        }
        if gateway.decommissioned {
            return Err(Reason::Decommissioned);
        }
        // A keyCompromise revocation says the registered key itself is not trusted any more,
        // so it cannot earn a new certificate (Laysi/trustiot-v2#636): the gateway is replaced
        // and registered anew, as decision 12 has it for a new TPM.
        if gateway.key_compromised {
            return Err(Reason::KeyCompromised);
        }
        let Some((tenant, binding)) = gateway.binding else {
            return Err(Reason::NotBound);
        };
        if gateway.suspended {
            return Err(Reason::TenantSuspended);
        }
        self.issue(authority, hardware_id, &key, tenant, binding)
            .await
    }

    /// Reads the gateway's registration, latest binding and tenant status in one read-only
    /// statement of the platform role, logged with the hardware ID and what was read.
    async fn read_gateway(&self, hardware_id: &GatewayId) -> Result<Option<Gateway>, Reason> {
        let failed = |error: &dyn std::fmt::Display| {
            tracing::error!(
                hardware_id = %hardware_id,
                error = %error,
                "could not read a gateway for a certificate renewal"
            );
            Reason::Unavailable
        };
        let mut tx = self
            .projection
            .begin(PURPOSE)
            .await
            .map_err(|error| failed(&error.log_kind()))?;
        let row = sqlx::query(READ_GATEWAY)
            .bind(hardware_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| failed(&error))?;
        tx.commit().await.map_err(|error| failed(&error))?;
        let Some(row) = row else {
            tracing::info!(
                hardware_id = %hardware_id,
                registered = false,
                "read a gateway for a certificate renewal"
            );
            return Ok(None);
        };
        let gateway = gateway(&row).map_err(|error| failed(&error))?;
        let (tenant_id, binding_id) = gateway.binding.map_or((None, None), |(tenant, binding)| {
            (Some(tenant.to_string()), Some(binding.to_string()))
        });
        tracing::info!(
            hardware_id = %hardware_id,
            registered = true,
            decommissioned = gateway.decommissioned,
            key_compromised = gateway.key_compromised,
            tenant_id = tenant_id.as_deref(),
            binding_id = binding_id.as_deref(),
            tenant_status = gateway.status.map_or("none", TenantStatus::as_str),
            tenant_suspended = gateway.suspended,
            "read a gateway for a certificate renewal"
        );
        Ok(Some(gateway))
    }

    /// Signs and records the certificate for the registered key with the tenant event, in one
    /// transaction of the gateway's tenant.
    async fn issue(
        &self,
        authority: &Authority,
        hardware_id: &GatewayId,
        key: &rcgen::SubjectPublicKeyInfo,
        tenant: TenantId,
        binding: GatewayBindingId,
    ) -> Result<Response, Reason> {
        let what = "a renewed gateway certificate";
        let mut tx = self
            .background
            .begin(tenant, PURPOSE)
            .await
            .map_err(|error| database(what, &error))?;
        // Serialized with revocations of this gateway, then checked again: a keyCompromise
        // revocation that committed after the read above still ends this renewal, and one that
        // waits for this transaction revokes what it signs (`gateways::LOCK_GATEWAY_CERTIFICATES`).
        sqlx::query(gateways::LOCK_GATEWAY_CERTIFICATES)
            .bind(hardware_id.as_str())
            .execute(&mut **tx.transaction())
            .await
            .map_err(|error| database(what, &error))?;
        let compromised: bool = sqlx::query_scalar(gateways::KEY_COMPROMISED)
            .bind(hardware_id.as_str())
            .fetch_one(&mut **tx.transaction())
            .await
            .map_err(|error| database(what, &error))?;
        if compromised {
            return Err(Reason::KeyCompromised);
        }
        let validity = self.validity;
        let issued = issue::sign_and_record_in(&mut tx, authority, what, |ca, now| {
            issue::gateway_certificate_for_key(ca, key, hardware_id, validity, now)
        })
        .await
        .map_err(|refusal| match refusal {
            Refusal::NotReady => Reason::CaNotReady,
            Refusal::Request(problem) => {
                tracing::error!(
                    hardware_id = %hardware_id,
                    problem = %problem,
                    "could not sign a renewed gateway certificate"
                );
                Reason::Unavailable
            }
            Refusal::Failed | Refusal::Database => Reason::Unavailable,
        })?;
        let event = GatewayCertificateRenewed {
            hardware_id: hardware_id.clone(),
            binding_id: binding,
            certificate_serial: issue::hex(&issued.serial),
            ca_key_id: issued.ca_key_id.clone(),
            not_before: issue::utc(issued.not_before),
            not_after: issue::utc(issued.not_after),
        };
        let aggregate = Aggregate::new(
            GATEWAY_CERTIFICATE_AGGREGATE,
            gateway_certificate_aggregate_id(tenant, hardware_id),
        )
        .map_err(|error| database(what, &error))?;
        outbox::publish_as(
            tx.transaction(),
            &aggregate,
            &event,
            Some(tenant),
            Actor::Service {
                service: ServiceName::Certs,
            },
        )
        .await
        .map_err(|error| database(what, &error))?;
        tx.commit().await.map_err(|error| database(what, &error))?;
        self.metrics.issued.add(1, &[]);
        tracing::info!(
            hardware_id = %hardware_id,
            tenant_id = %tenant,
            binding_id = %binding,
            serial = %event.certificate_serial,
            ca_key_id = %event.ca_key_id,
            not_after = %event.not_after,
            "renewed a gateway certificate"
        );
        // Committed: the broker's permissions follow now, not at the next whole comparison. The
        // gateway may have lost its broker client when its only certificate expired or was
        // revoked; the new one brings it back.
        self.broker_sync.gateway(hardware_id);
        let certificate = pem::encode(pem::CERTIFICATE, &issued.der);
        Ok(api::ok(
            StatusCode::CREATED,
            &RenewedCertificate {
                hardware_id: hardware_id.clone(),
                serial: event.certificate_serial,
                ca_key_id: event.ca_key_id,
                not_before: event.not_before.to_rfc3339_opts(SecondsFormat::Secs, true),
                not_after: event.not_after.to_rfc3339_opts(SecondsFormat::Secs, true),
                // `issuer_der` is the certificate of the issuing CA that just signed.
                certificate_chain: format!(
                    "{certificate}{}",
                    pem::encode(pem::CERTIFICATE, &issued.issuer_der)
                ),
                certificate,
                root_certificate: pem::encode(pem::CERTIFICATE, authority.root_der()),
            },
        ))
    }
}

/// A row of [`READ_GATEWAY`].
fn gateway(row: &sqlx::postgres::PgRow) -> Result<Gateway, String> {
    let read = |error: sqlx::Error| error.to_string();
    let current: Option<bool> = row.try_get("current").map_err(read)?;
    let binding = if current == Some(true) {
        let tenant: Option<String> = row.try_get("tenant_id").map_err(read)?;
        let binding: Option<String> = row.try_get("binding_id").map_err(read)?;
        let tenant = tenant
            .and_then(|tenant| tenant.parse::<TenantId>().ok())
            .ok_or("a stored binding has no valid tenant")?;
        let binding = binding
            .and_then(|binding| binding.parse::<GatewayBindingId>().ok())
            .ok_or("a stored binding has no valid binding ID")?;
        Some((tenant, binding))
    } else {
        None
    };
    let status: Option<String> = row.try_get("status").map_err(read)?;
    let status = status
        .map(|status| TenantStatus::parse(&status).ok_or("a stored tenant status breaks the rules"))
        .transpose()?;
    Ok(Gateway {
        public_key: row.try_get("public_key").map_err(read)?,
        decommissioned: row.try_get("decommissioned").map_err(read)?,
        key_compromised: row.try_get("key_compromised").map_err(read)?,
        binding,
        status,
        suspended: reads_as_suspended(status),
    })
}

/// Whether `signature` (DER) is an ECDSA with SHA-256 signature over `input` by `public_key`
/// (DER SubjectPublicKeyInfo).
fn signature_holds(public_key: &[u8], signature: &[u8], input: &[u8]) -> bool {
    let Ok((rest, key)) = SubjectPublicKeyInfo::from_der(public_key) else {
        return false;
    };
    let algorithm = AlgorithmIdentifier::new(OID_SIG_ECDSA_WITH_SHA256, None);
    rest.is_empty()
        && x509_parser::verify::verify_signature(
            &key,
            &algorithm,
            &BitString::new(0, signature),
            input,
        )
        .is_ok()
}

/// Logs a database failure while doing `what`; [`Reason::Unavailable`].
fn database(what: &str, error: &dyn std::fmt::Display) -> Reason {
    tracing::error!(error = %error, "could not {what}");
    Reason::Unavailable
}

/// An error answer with `status`, never stored, with `Retry-After` when given.
fn answer(status: StatusCode, body: ErrorBody, retry_after: Option<u64>) -> Response {
    let mut response = (status, [(CACHE_CONTROL, "no-store")], axum::Json(body)).into_response();
    if let Some(seconds) = retry_after {
        response
            .headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from(seconds));
    }
    response
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn lag(events: u32, oldest_age: Option<u64>) -> Option<Lag> {
        Some(Lag {
            events,
            oldest_age: oldest_age.map(Duration::from_secs),
        })
    }

    #[test]
    fn the_slowest_consumer_decides() {
        let max = Duration::from_secs(60);
        assert_eq!(slowest([lag(0, None), lag(0, None)]), lag(0, None));
        assert_eq!(slowest([lag(2, Some(5)), lag(3, Some(9))]), lag(5, Some(9)));
        assert_eq!(slowest([lag(0, None), lag(1, Some(4))]), lag(1, Some(4)));
        assert_eq!(slowest([lag(1, Some(4)), None]), None);
        assert_eq!(slowest([None, lag(0, None)]), None);
        assert_eq!(
            slowest([lag(u32::MAX, None), lag(1, None)]),
            lag(u32::MAX, None)
        );
        // One consumer past the limit refuses, whatever the other.
        assert!(!bindings_current(
            slowest([lag(0, None), lag(1, Some(61))]),
            max
        ));
        assert!(!bindings_current(slowest([lag(0, None), None]), max));
        assert!(bindings_current(
            slowest([lag(0, None), lag(1, Some(60))]),
            max
        ));
    }

    #[test]
    fn decisions_wait_for_a_current_and_short_lag() {
        let max = Duration::from_secs(60);
        assert!(!bindings_current(None, max), "an unknown lag refuses");
        assert!(bindings_current(lag(0, None), max));
        assert!(bindings_current(lag(3, Some(60)), max));
        assert!(!bindings_current(lag(3, Some(61)), max));
    }

    #[test]
    fn only_decisions_about_the_redemption_are_counted() {
        let counted: Vec<&str> = [
            Reason::Invalid,
            Reason::RateLimited(1),
            Reason::Busy,
            Reason::BindingsStale,
            Reason::CaNotReady,
            Reason::Unavailable,
            Reason::ChallengeUnknown,
            Reason::ChallengeMismatch,
            Reason::ChallengeExpired,
            Reason::SignatureInvalid,
            Reason::Decommissioned,
            Reason::KeyCompromised,
            Reason::NotBound,
            Reason::TenantSuspended,
        ]
        .into_iter()
        .filter(|reason| reason.counted())
        .map(Reason::as_str)
        .collect();
        assert_eq!(
            counted,
            [
                "challenge_unknown",
                "challenge_mismatch",
                "challenge_expired",
                "signature_invalid",
                "decommissioned",
                "key_compromised",
                "not_bound",
                "tenant_suspended"
            ]
        );
        // Every 429 and 503 says when to come back; the others do not.
        for reason in [
            Reason::RateLimited(7),
            Reason::Busy,
            Reason::BindingsStale,
            Reason::CaNotReady,
            Reason::Unavailable,
        ] {
            let (status, body, retry_after) = reason.answer();
            assert!(matches!(status.as_u16(), 429 | 503), "{reason:?}");
            assert_eq!(status, body.http_status(), "{reason:?}");
            assert!(
                retry_after.is_some_and(|seconds| seconds >= 1),
                "{reason:?}"
            );
        }
        assert_eq!(Reason::RateLimited(7).answer().2, Some(7));
        let (status, body, _) = Reason::ChallengeExpired.answer();
        assert_eq!(status, StatusCode::GONE);
        assert_eq!(body.message_key(), &CHALLENGE_EXPIRED);
    }

    fn keys(body: &ErrorBody) -> Vec<(String, String)> {
        body.field_errors()
            .iter()
            .map(|error| (error.field.clone(), error.message_key.as_str().to_owned()))
            .collect()
    }

    const NONCE: &str = "q83vEjRWeJCrze8SNFZ4kKvN7xI0VniQq83vEjRWeJA";

    #[test]
    fn requests_parse_with_a_field_error_for_each_problem() {
        let challenge = parse(br#"{"step": "challenge", "hardware_id": "tg-0001", "x": 1}"#)
            .expect("a challenge request");
        assert!(matches!(challenge, Request::Challenge(ref id) if id.as_str() == "tg-0001"));

        let signature = STANDARD.encode([0x30; 70]);
        let body = json!({"step": "renew", "hardware_id": "tg-0001", "nonce": NONCE,
            "signature": signature});
        let renew = parse(body.to_string().as_bytes()).expect("a renew request");
        let Request::Renew {
            nonce_bytes,
            signature,
            ..
        } = renew
        else {
            panic!("not a renew request");
        };
        assert_eq!(nonce_bytes.len(), NONCE_BYTES);
        assert_eq!(signature.len(), 70);

        for body in [&b"[]"[..], b"not json", b"\"text\""] {
            let refused = parse(body).err().expect("refused");
            assert_eq!(refused.code(), ErrorCode::InvalidRequest);
            assert_eq!(refused.message_key(), &BODY_INVALID);
        }
        let cases: [(serde_json::Value, &[(&str, &MessageKey)]); 5] = [
            (json!({}), &[("/step", &FIELD_REQUIRED)]),
            (
                json!({"step": "rebind", "hardware_id": "tg-0001"}),
                &[("/step", &STEP_INVALID)],
            ),
            (
                json!({"step": "challenge", "hardware_id": "-bad"}),
                &[("/hardware_id", &HARDWARE_ID_INVALID)],
            ),
            (
                json!({"step": "renew", "hardware_id": "tg-0001"}),
                &[("/nonce", &FIELD_REQUIRED), ("/signature", &FIELD_REQUIRED)],
            ),
            (
                json!({"step": "renew", "hardware_id": "tg-0001",
                    "nonce": format!("{NONCE}x"), "signature": STANDARD.encode([1; 73])}),
                &[
                    ("/nonce", &NONCE_INVALID),
                    ("/signature", &SIGNATURE_MALFORMED),
                ],
            ),
        ];
        for (body, expected) in cases {
            let refused = parse(body.to_string().as_bytes()).err().expect("refused");
            assert_eq!(refused.code(), ErrorCode::ValidationFailed, "{body}");
            let expected: Vec<(String, String)> = expected
                .iter()
                .map(|(field, key)| ((*field).to_owned(), key.as_str().to_owned()))
                .collect();
            assert_eq!(keys(&refused), expected, "{body}");
        }
    }
}
