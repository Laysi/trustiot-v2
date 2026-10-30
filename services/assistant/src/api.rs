//! The assistant's client API under `/api/assistant/v1` (Laysi/trustiot-v2#645): the
//! conversation threads ([`conversations`]). Each resource keeps its handlers and types in a
//! module of its own and adds its router in [`routes`] and its handlers in `ApiDoc`; this
//! module holds what they share: the request's identity as the store's [`Principal`], the M1
//! error responses, the transaction around a request, the body and paging rules, and the cursor
//! codec. services/assistant/README.md, 「API」, describes the endpoints.
//!
//! Every response carries `cache-control: no-store`: conversations are private data, and
//! service-kit sets no caching header of its own.

pub mod conversations;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::Path;
use axum::extract::rejection::PathRejection;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, SecondsFormat, Utc};
use contract::{ErrorBody, ErrorCode, TenantRole};
use serde::Serialize;
use serde::de::DeserializeOwned;
use service_kit::identity::{IdentityVerifier, Requirement, UserIdentity};
use service_kit::tenant::{TenantDb, TenantDbError, TenantTransaction};
use std::sync::Arc;
use std::time::Duration;
use utoipa::OpenApi;
use uuid::Uuid;

use crate::Agent;
use crate::history::Principal;
use crate::store::{MAX_PAGE, MAX_STARTS_PER_DAY, StoreError};

/// Items per page when the request sets no `limit`.
pub const DEFAULT_PAGE: usize = 50;

/// How long a statement of a request's transaction waits for a lock before it fails with
/// SQLSTATE `55P03` and the request answers 503 `unavailable`, set as the transaction's
/// `lock_timeout`. The locks a request meets are the row of the same conversation that another
/// request holds for one short transaction (a rename, a delete, an append, a question's start:
/// milliseconds). A wait of 2 s is far beyond those, so it means a stuck session or a
/// migration's table lock: the request gives its connection back to the pool, which readiness
/// and the purge's job lock share (8 connections by default), instead of holding it for as long
/// as the other session does. With the 5 s a request may wait for a connection
/// ([`crate::ACQUIRE_TIMEOUT`]), a request that meets both still answers within seconds.
pub const REQUEST_LOCK_TIMEOUT: Duration = Duration::from_secs(2);

/// Sets `$1` as the transaction's `lock_timeout`, like `SET LOCAL`.
const SET_LOCK_TIMEOUT: &str = "SELECT set_config('lock_timeout', $1, true)";

/// The client API's OpenAPI document (merged into `api/openapi.json` by `tools/openapi`).
#[derive(OpenApi)]
#[openapi(
    paths(
        conversations::list_conversations,
        conversations::create_conversation,
        conversations::get_conversation,
        conversations::update_conversation,
        conversations::delete_conversation,
        conversations::restore_conversation,
        conversations::list_deleted_conversations,
    ),
    tags((name = "assistant", description = "The AI assistant's conversations (services/assistant)."))
)]
pub(crate) struct ApiDoc;

/// The API's routes, each answering with `cache-control: no-store`.
pub fn routes(verifier: IdentityVerifier, db: TenantDb, agent: Arc<Agent>) -> Router {
    conversations::routes(verifier, db, agent).route_layer(axum::middleware::map_response(no_store))
}

/// Marks every response of the API's routes as not to be stored, including the ones the handlers
/// never build: a 401 that the [`UserIdentity`] extractor answers carries no `cache-control` of
/// its own.
async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The store's principal for the request's verified identity: its user in its current tenant.
/// Every role of the tenant will do (`Requirement::Tenant(TenantRole::Viewer)`, the permission
/// matrix in docs/architecture/permissions.md, section `## assistant`), since a conversation is
/// its owner's alone (the store checks tenant and user). Without a tenant, or with a role this
/// build does not know: service-kit's 403 `permission_denied`, logged as `refused to <action>`.
#[allow(
    clippy::result_large_err,
    reason = "the error is the response to answer at once"
)]
pub(crate) fn principal(user: &UserIdentity, action: &'static str) -> Result<Principal, Response> {
    if let Err(refused) = user.require(Requirement::Tenant(TenantRole::Viewer), action) {
        return Err(refused.into_response());
    }
    match user.tenant() {
        Some(grant) => Ok(Principal {
            tenant_id: grant.id,
            user_id: user.user_id(),
        }),
        // A tenant requirement that passed means a tenant; answer the same refusal regardless.
        None => Err(error(ErrorBody::new(ErrorCode::PermissionDenied))),
    }
}

/// The path's ID; `None` (answered with [`not_found`]) when it is not a UUID.
pub(crate) fn parse_id(path: Result<Path<String>, PathRejection>) -> Option<Uuid> {
    path.ok()?.0.parse().ok()
}

/// A transaction in the request's tenant, whose statements wait at most [`REQUEST_LOCK_TIMEOUT`]
/// for a lock, or the response when none can start: 403 without a tenant, 503 when the database
/// fails (logged with the failure's class only).
#[allow(
    clippy::result_large_err,
    reason = "the error is the response to answer at once"
)]
pub(crate) async fn begin(
    db: &TenantDb,
    user: &UserIdentity,
    action: &'static str,
) -> Result<TenantTransaction, Response> {
    let mut tx = db.begin(user).await.map_err(|failure| match failure {
        TenantDbError::NoTenant => error(ErrorBody::new(ErrorCode::PermissionDenied)),
        other => {
            let kind = match other {
                // The class only: sqlx's text can carry values.
                TenantDbError::Database(error) => StoreError::from(error).to_string(),
                TenantDbError::UnsafeSession(_) => "unsafe database session".to_owned(),
                _ => "database error".to_owned(),
            };
            tracing::error!(
                user_id = %user.user_id(),
                action,
                error = %kind,
                "could not start a database transaction"
            );
            error(ErrorBody::new(ErrorCode::Unavailable))
        }
    })?;
    let set = sqlx::query(SET_LOCK_TIMEOUT)
        .bind(format!("{}ms", REQUEST_LOCK_TIMEOUT.as_millis()))
        .execute(&mut *tx)
        .await;
    if let Err(failure) = set {
        // The class only: sqlx's text can carry values.
        let kind = StoreError::from(failure).to_string();
        tracing::error!(
            user_id = %user.user_id(),
            action,
            error = %kind,
            "could not start a database transaction"
        );
        // A rollback that fails leaves nothing behind: the connection is closed then.
        let _ = tx.rollback().await;
        return Err(error(ErrorBody::new(ErrorCode::Unavailable)));
    }
    Ok(tx)
}

/// Commits `tx` when the request's work succeeded and rolls it back otherwise.
pub(crate) async fn finish<T>(
    tx: TenantTransaction,
    result: Result<T, StoreError>,
) -> Result<T, StoreError> {
    match result {
        Ok(value) => {
            tx.commit().await?;
            Ok(value)
        }
        Err(failure) => {
            // A rollback that fails leaves nothing behind: the connection is closed then.
            let _ = tx.rollback().await;
            Err(failure)
        }
    }
}

/// The response for a store failure. Every "no such conversation of yours" is the one
/// [`not_found`] answer. A start over the daily limit is 429 `rate_limited` with
/// `Retry-After`, logged at WARN with the user and tenant. A failure that is not the request's
/// fault is logged at ERROR with its class only (a [`StoreError`] carries no database text,
/// title or content) and answered 503 `unavailable`.
pub(crate) fn store_failure(
    user: &UserIdentity,
    conversation: Option<Uuid>,
    action: &'static str,
    failure: &StoreError,
) -> Response {
    match failure {
        StoreError::NotStored => not_found(),
        StoreError::AlreadyRunning => error(
            ErrorBody::new(ErrorCode::Conflict).with_message_key(conversations::QUESTION_RUNNING),
        ),
        StoreError::StartLimited { retry_after_secs } => {
            tracing::warn!(
                user_id = %user.user_id(),
                tenant_id = user.tenant_id().map(|id| id.to_string()).as_deref(),
                action,
                max = MAX_STARTS_PER_DAY,
                retry_after_secs,
                "conversation start refused: the user reached the daily limit"
            );
            let mut response = error(
                ErrorBody::new(ErrorCode::RateLimited)
                    .with_message_key(conversations::START_LIMITED)
                    .with_detail("max", MAX_STARTS_PER_DAY),
            );
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(*retry_after_secs));
            response
        }
        _ => {
            tracing::error!(
                user_id = %user.user_id(),
                conversation_id = conversation.map(|id| id.to_string()).as_deref(),
                action,
                error = %failure,
                "conversation request failed"
            );
            error(ErrorBody::new(ErrorCode::Unavailable))
        }
    }
}

/// The one answer for every conversation that is not the caller's to see: never stored,
/// another user's or another tenant's, deleted, or an ID that is not a UUID. Byte for byte the
/// same, so a response does not tell which.
pub(crate) fn not_found() -> Response {
    error(ErrorBody::new(ErrorCode::NotFound))
}

pub(crate) fn ok<T: Serialize>(status: StatusCode, body: &T) -> Response {
    (status, [(CACHE_CONTROL, "no-store")], axum::Json(body)).into_response()
}

/// 204 without a body, with `cache-control: no-store` like every response of the API.
pub(crate) fn no_content() -> Response {
    (StatusCode::NO_CONTENT, [(CACHE_CONTROL, "no-store")]).into_response()
}

pub(crate) fn error(body: ErrorBody) -> Response {
    (
        body.http_status(),
        [(CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}

/// Whether the request has exactly one `Content-Type`, `application/json` with or without
/// parameters, compared without regard to ASCII case. Every write the session cookie
/// authenticates requires it (services/identity/README.md, 「租戶」): a page of another origin
/// cannot send that type with the user's cookie without a CORS preflight, which nothing answers.
pub(crate) fn is_json(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(CONTENT_TYPE).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    value.to_str().is_ok_and(|value| {
        let essence = value.split(';').next().unwrap_or_default();
        essence.trim().eq_ignore_ascii_case("application/json")
    })
}

/// `body` as a `T`, when it is one JSON object of `T`'s fields. serde also reads a struct from
/// a JSON array of its fields' values, which no client is meant to send, so anything but an
/// object is refused here.
pub(crate) fn parse_object<T: DeserializeOwned>(body: &[u8]) -> Option<T> {
    if body.trim_ascii_start().first() != Some(&b'{') {
        return None;
    }
    serde_json::from_slice(body).ok()
}

/// Why a write's body was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BodyRefusal {
    /// The `Content-Type` is not `application/json` (see [`is_json`]).
    NotJson,
    /// The body is larger than the endpoint allows.
    TooLarge,
}

/// The body of a write, after the content type check, at most `max_bytes`.
pub(crate) async fn read_json(
    headers: &HeaderMap,
    body: Body,
    max_bytes: usize,
) -> Result<Bytes, BodyRefusal> {
    if !is_json(headers) {
        return Err(BodyRefusal::NotJson);
    }
    axum::body::to_bytes(body, max_bytes)
        .await
        .map_err(|_| BodyRefusal::TooLarge)
}

/// The `limit` query parameter: [`DEFAULT_PAGE`] when absent, else a number from 1 to
/// [`MAX_PAGE`]; `None` for anything else.
pub(crate) fn parse_limit(raw: Option<&str>) -> Option<usize> {
    match raw {
        None => Some(DEFAULT_PAGE),
        Some(text) => text
            .parse::<usize>()
            .ok()
            .filter(|limit| (1..=MAX_PAGE).contains(limit)),
    }
}

/// The list a cursor belongs to. A cursor of one list is refused by every other, so a client
/// that mixes them up gets a 400 instead of a page from the wrong place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CursorKind {
    /// The conversations that are not archived.
    Active,
    /// The archived conversations.
    Archived,
    /// The deleted conversations that can still be restored.
    Deleted,
}

impl CursorKind {
    fn tag(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
            Self::Deleted => "deleted",
        }
    }
}

/// Where a page ended: the list, and the sort time and ID of the page's last item. As text
/// (`next_cursor`, then `cursor`) it is `<list>_<time>_<id>`, the time in RFC 3339 with
/// microseconds, Postgres' precision, so it carries the database's value exactly; clients treat
/// it as opaque. It needs no signature: the store looks only at the caller's own conversations,
/// so a changed cursor only moves where the caller's page starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cursor {
    pub kind: CursorKind,
    pub at: DateTime<Utc>,
    pub id: Uuid,
}

impl Cursor {
    pub(crate) fn encode(self) -> String {
        format!(
            "{}_{}_{}",
            self.kind.tag(),
            self.at.to_rfc3339_opts(SecondsFormat::Micros, true),
            self.id
        )
    }

    /// The cursor `text` names, when it is one of `kind`'s list exactly as [`Cursor::encode`]
    /// writes it; `None` otherwise.
    pub(crate) fn decode(text: &str, kind: CursorKind) -> Option<Self> {
        let mut parts = text.splitn(3, '_');
        let (tag, time, id) = (parts.next()?, parts.next()?, parts.next()?);
        if tag != kind.tag() {
            return None;
        }
        let cursor = Self {
            kind,
            at: DateTime::parse_from_rfc3339(time).ok()?.with_timezone(&Utc),
            id: id.parse().ok()?,
        };
        // One text per cursor: no other offset, precision or ID spelling.
        (cursor.encode() == text).then_some(cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_the_database_time_and_refuse_other_lists() {
        let at = DateTime::parse_from_rfc3339("2026-10-28T01:02:03.456789Z")
            .expect("a time")
            .with_timezone(&Utc);
        let id: Uuid = "0199f6a4-7c1e-7d2a-9b3c-1234567890ab"
            .parse()
            .expect("a UUID");
        let cursor = Cursor {
            kind: CursorKind::Active,
            at,
            id,
        };
        let text = cursor.encode();
        assert_eq!(
            text,
            "active_2026-10-28T01:02:03.456789Z_0199f6a4-7c1e-7d2a-9b3c-1234567890ab"
        );
        assert_eq!(Cursor::decode(&text, CursorKind::Active), Some(cursor));
        assert_eq!(Cursor::decode(&text, CursorKind::Archived), None);
        assert_eq!(Cursor::decode(&text, CursorKind::Deleted), None);
        let deleted = Cursor {
            kind: CursorKind::Deleted,
            ..cursor
        }
        .encode();
        assert_eq!(Cursor::decode(&deleted, CursorKind::Active), None);

        for bad in [
            "",
            "active",
            "active_2026-10-28T01:02:03.456789Z",
            "active_2026-10-28T01:02:03.456789Z_not-a-uuid",
            // Other spellings of the same position.
            "active_2026-10-28T01:02:03.456789+00:00_0199f6a4-7c1e-7d2a-9b3c-1234567890ab",
            "active_2026-10-28T01:02:03.456789123Z_0199f6a4-7c1e-7d2a-9b3c-1234567890ab",
            "active_2026-10-28T01:02:03Z_0199f6a4-7c1e-7d2a-9b3c-1234567890ab",
            "active_2026-10-28T01:02:03.456789Z_0199F6A4-7C1E-7D2A-9B3C-1234567890AB",
            "Active_2026-10-28T01:02:03.456789Z_0199f6a4-7c1e-7d2a-9b3c-1234567890ab",
        ] {
            assert_eq!(Cursor::decode(bad, CursorKind::Active), None, "{bad}");
        }
    }

    #[test]
    fn limits_are_one_to_a_full_page() {
        assert_eq!(parse_limit(None), Some(DEFAULT_PAGE));
        assert_eq!(parse_limit(Some("1")), Some(1));
        assert_eq!(parse_limit(Some("100")), Some(MAX_PAGE));
        for bad in ["0", "101", "-1", "", "ten", "1.5", " 5"] {
            assert_eq!(parse_limit(Some(bad)), None, "{bad}");
        }
    }

    #[test]
    fn only_one_json_content_type_is_a_json_body() {
        let headers = |values: &[&str]| {
            let mut headers = HeaderMap::new();
            for value in values {
                headers.append(CONTENT_TYPE, HeaderValue::from_str(value).expect("a value"));
            }
            headers
        };
        assert!(is_json(&headers(&["application/json"])));
        assert!(is_json(&headers(&["Application/JSON; charset=utf-8"])));
        for bad in [
            &[][..],
            &["text/plain"],
            &["application/x-www-form-urlencoded"],
            &["multipart/form-data; boundary=x"],
            &["application/json", "application/json"],
            &["application/jsonp"],
        ] {
            assert!(!is_json(&headers(bad)), "{bad:?}");
        }
    }
}
