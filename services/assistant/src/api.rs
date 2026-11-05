//! The assistant's client API under `/api/assistant/v1`: the conversation threads
//! ([`conversations`], Laysi/trustiot-v2#645), the questions asked in them ([`questions`],
//! Laysi/trustiot-v2#646), sharing them with the tenant ([`sharing`], Laysi/trustiot-v2#650) and
//! the dashboards ([`dashboards`], Laysi/trustiot-v2#150). Each resource keeps its handlers,
//! types, router and OpenAPI paths (its `ApiDoc`) in a module of its own, so a new endpoint
//! changes only its resource's module (Laysi/trustiot-v2#1095); [`routes`] and [`openapi`] only
//! join the resources. This module holds what they share: the request's identity as the store's [`Principal`], the M1 error responses, the
//! transaction around a request, the body and paging rules, and the cursor codec.
//! services/assistant/README.md, 「API」, describes the endpoints.
//!
//! Every response carries `cache-control: no-store`: conversations and dashboards are private
//! data, and service-kit sets no caching header of its own.

pub mod answer_stream;
pub mod conversations;
pub mod dashboards;
pub mod held_body;
pub mod questions;
pub mod sharing;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::Path;
use axum::extract::rejection::PathRejection;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, SecondsFormat, Utc};
use contract::{ErrorBody, ErrorCode, TenantId, TenantRole, UserId};
use serde::Serialize;
use serde::de::DeserializeOwned;
use service_kit::identity::{IdentityVerifier, Requirement, UserIdentity};
use service_kit::tenant::{TenantDb, TenantDbError, TenantTransaction};
use std::time::Duration;
use utoipa::OpenApi;
use uuid::Uuid;

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

/// What the client API's OpenAPI document has besides its resources' paths: its tag.
#[derive(OpenApi)]
#[openapi(tags((name = "assistant", description = "The AI assistant's conversations and dashboards (services/assistant).")))]
struct ApiDoc;

/// The OpenAPI document of each resource: its operations and the schemas they use, which the
/// resource's module lists in its own `ApiDoc`. A new resource adds its module's document here.
fn resource_documents() -> [utoipa::openapi::OpenApi; 5] {
    [
        conversations::ApiDoc::openapi(),
        questions::ApiDoc::openapi(),
        answer_stream::ApiDoc::openapi(),
        sharing::ApiDoc::openapi(),
        dashboards::ApiDoc::openapi(),
    ]
}

/// The client API's OpenAPI document (merged into `api/openapi.json` by `tools/openapi`): the
/// tag of [`ApiDoc`] and every resource's operations and schemas. utoipa's merge keeps the first
/// of two operations on the same path and method, and of two schemas with the same name, so
/// `tests::every_resource_operation_is_in_the_document_once` holds the resources' operations
/// apart.
pub(crate) fn openapi() -> utoipa::openapi::OpenApi {
    let mut document = ApiDoc::openapi();
    for resource in resource_documents() {
        document.merge(resource);
    }
    document
}

/// The API's routes, each answering with `cache-control: no-store`: the conversations and the
/// questions with `questions` (which runs them, [`crate::questions`]), their answer streams with
/// `streams` (this replica's open streams), sharing with the agent the questions run on (a copy
/// starts a conversation of its own), the dashboards with what `dashboards` holds.
pub fn routes(
    verifier: IdentityVerifier,
    db: TenantDb,
    questions: crate::questions::Questions,
    streams: answer_stream::Streams,
    dashboards: dashboards::Dashboards,
) -> Router {
    conversations::routes(verifier.clone(), db.clone(), questions.clone())
        .merge(sharing::routes(
            verifier.clone(),
            db.clone(),
            questions.shared_agent(),
        ))
        .merge(answer_stream::routes(
            verifier.clone(),
            db.clone(),
            questions.clone(),
            streams,
            dashboards.runner.memory().clone(),
        ))
        .merge(questions::routes(verifier.clone(), db.clone(), questions))
        .merge(dashboards::routes(verifier, db, dashboards))
        .route_layer(axum::middleware::map_response(no_store))
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
    if let Err(failure) = set_lock_timeout(&mut tx).await {
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

/// A transaction in `user`'s tenant whose statements wait at most [`REQUEST_LOCK_TIMEOUT`] for a
/// lock, like [`begin`]'s, for work that answers no request itself: a question's background
/// task ([`crate::questions`]), which continues the request with its verified identity. A
/// failure is a [`StoreError`] (its class only), for the caller to log or record.
///
/// # Errors
///
/// [`StoreError::WrongTenant`] without a tenant, or the database failure.
pub(crate) async fn begin_tx(
    db: &TenantDb,
    user: &UserIdentity,
) -> Result<TenantTransaction, StoreError> {
    let mut tx = db.begin(user).await?;
    if let Err(failure) = set_lock_timeout(&mut tx).await {
        // A rollback that fails leaves nothing behind: the connection is closed then.
        let _ = tx.rollback().await;
        return Err(failure.into());
    }
    Ok(tx)
}

/// Sets [`REQUEST_LOCK_TIMEOUT`] as `tx`'s `lock_timeout`: a statement that waits longer for a
/// lock fails with SQLSTATE `55P03` and gives the connection back. [`begin`] sets it for every
/// request; the `dashboard` tool's save sets it too, since it locks the same rows.
///
/// # Errors
///
/// A database failure.
pub(crate) async fn set_lock_timeout(tx: &mut TenantTransaction) -> Result<(), sqlx::Error> {
    sqlx::query(SET_LOCK_TIMEOUT)
        .bind(format!("{}ms", REQUEST_LOCK_TIMEOUT.as_millis()))
        .execute(&mut **tx)
        .await
        .map(|_| ())
}

/// Commits `tx` when the request's work succeeded and rolls it back otherwise. `E` is the
/// resource's store error: a failed commit is its database failure.
pub(crate) async fn finish<T, E: From<sqlx::Error>>(
    tx: TenantTransaction,
    result: Result<T, E>,
) -> Result<T, E> {
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

/// The response for a store failure, for [`store_failure_of`] with the request's user and
/// tenant.
pub(crate) fn store_failure(
    user: &UserIdentity,
    conversation: Option<Uuid>,
    action: &'static str,
    failure: &StoreError,
) -> Response {
    store_failure_of(
        user.user_id(),
        user.tenant_id(),
        conversation,
        action,
        failure,
    )
}

/// The response for a store failure of a request by `user_id` in `tenant_id`. Every "no such
/// conversation (or question) of yours" is the one [`not_found`] answer. A start over the
/// daily limit is 429 `rate_limited` with `Retry-After`, and a question over the user's running
/// limit 429 `rate_limited` without it (no one knows when a question ends), each logged at WARN
/// with the user and tenant. A question that cannot start because one runs in the
/// conversation, the history is full, or one that cannot be stopped because it ended, is 409
/// `conflict` with its key. A failure that is not the request's fault is logged at ERROR with
/// its class only (a [`StoreError`] carries no database text, title or content) and answered
/// 503 `unavailable`.
pub(crate) fn store_failure_of(
    user_id: UserId,
    tenant_id: Option<TenantId>,
    conversation: Option<Uuid>,
    action: &'static str,
    failure: &StoreError,
) -> Response {
    match failure {
        StoreError::NotStored | StoreError::NoSuchRun => not_found(),
        StoreError::AlreadyRunning => error(
            ErrorBody::new(ErrorCode::Conflict).with_message_key(conversations::QUESTION_RUNNING),
        ),
        StoreError::HistoryFull => {
            error(ErrorBody::new(ErrorCode::Conflict).with_message_key(questions::HISTORY_FULL))
        }
        StoreError::RunEnded => {
            error(ErrorBody::new(ErrorCode::Conflict).with_message_key(questions::RUN_ENDED))
        }
        StoreError::NothingToRegenerate => error(
            ErrorBody::new(ErrorCode::Conflict).with_message_key(questions::NOTHING_TO_REGENERATE),
        ),
        StoreError::CopiedRound => {
            error(ErrorBody::new(ErrorCode::Conflict).with_message_key(questions::COPIED_ANSWER))
        }
        StoreError::StaleAnswer => {
            error(ErrorBody::new(ErrorCode::Conflict).with_message_key(questions::REGENERATE_STALE))
        }
        StoreError::RunLimited { max } => {
            tracing::warn!(
                user_id = %user_id,
                tenant_id = tenant_id.map(|id| id.to_string()).as_deref(),
                conversation_id = conversation.map(|id| id.to_string()).as_deref(),
                action,
                max,
                "question refused: the user reached the running limit"
            );
            error(
                ErrorBody::new(ErrorCode::RateLimited)
                    .with_message_key(questions::RUNNING_LIMITED)
                    .with_detail("max", *max),
            )
        }
        StoreError::StartLimited { retry_after_secs } => {
            tracing::warn!(
                user_id = %user_id,
                tenant_id = tenant_id.map(|id| id.to_string()).as_deref(),
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
                user_id = %user_id,
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

/// What to log of a failure to open a tenant transaction; never the database's text. The
/// `dashboard` tool logs its own transactions' failures with it.
pub(crate) fn db_error_kind(failure: &TenantDbError) -> String {
    match failure {
        TenantDbError::NoTenant => "no tenant".to_owned(),
        TenantDbError::UnsafeSession(_) => "unsafe session".to_owned(),
        TenantDbError::Database(error) => sqlx_error_kind(error),
        _ => "tenant database".to_owned(),
    }
}

/// The SQLSTATE of a database error, or the kind of a client error; never the database's text.
pub(crate) fn sqlx_error_kind(failure: &sqlx::Error) -> String {
    match failure {
        sqlx::Error::Database(database) => database
            .code()
            .map_or_else(|| "database".to_owned(), |code| format!("SQLSTATE {code}")),
        sqlx::Error::PoolTimedOut => "pool timed out".to_owned(),
        _ => "database client".to_owned(),
    }
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
    /// The conversations other members of the tenant shared (Laysi/trustiot-v2#650).
    Shared,
}

impl CursorKind {
    fn tag(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Archived => "archived",
            Self::Deleted => "deleted",
            Self::Shared => "shared",
        }
    }
}

/// Where a page ended: the list, and the sort time and ID of the page's last item. As text
/// (`next_cursor`, then `cursor`) it is `<list>_<time>_<id>`, the time in RFC 3339 with
/// microseconds, Postgres' precision, so it carries the database's value exactly; clients treat
/// it as opaque. It needs no signature: the store looks only at what the caller may see (their
/// own conversations, or those shared with them), so a changed cursor only moves where the
/// caller's page starts.
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

    /// The keys of an OpenAPI path item that are operations.
    const METHODS: [&str; 8] = [
        "get", "put", "post", "delete", "options", "head", "patch", "trace",
    ];

    /// The schemas of `document`'s components, by name.
    fn schemas(document: &utoipa::openapi::OpenApi) -> serde_json::Map<String, serde_json::Value> {
        let value = serde_json::to_value(document).expect("the document serializes");
        value["components"]["schemas"]
            .as_object()
            .cloned()
            .unwrap_or_default()
    }

    /// `METHOD /path` of every operation of `document`.
    fn operations(document: &utoipa::openapi::OpenApi) -> Vec<String> {
        let value = serde_json::to_value(document).expect("the document serializes");
        let mut operations = Vec::new();
        for (path, item) in value["paths"].as_object().expect("paths") {
            for method in item.as_object().expect("a path item").keys() {
                if !METHODS.contains(&method.as_str()) {
                    continue;
                }
                operations.push(format!("{} {path}", method.to_uppercase()));
            }
        }
        operations
    }

    #[test]
    fn every_resource_operation_is_in_the_document_once() {
        let mut declared = Vec::new();
        for resource in resource_documents() {
            let operations = operations(&resource);
            assert!(!operations.is_empty(), "a resource without operations");
            declared.extend(operations);
        }
        let mut unique = declared.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            declared.len(),
            "two resources declare the same operation, and the merge would drop one: {declared:?}"
        );
        let mut merged = operations(&openapi());
        merged.sort();
        assert_eq!(merged, unique);
    }

    #[test]
    fn resources_that_name_the_same_schema_mean_the_same_schema() {
        // utoipa's merge keeps the first schema of a name: a second, different one would be
        // dropped and its operations' references would point at the first.
        let mut seen = serde_json::Map::new();
        for resource in resource_documents() {
            for (name, schema) in schemas(&resource) {
                if let Some(first) = seen.get(&name) {
                    assert_eq!(first, &schema, "two resources define the schema {name}");
                } else {
                    seen.insert(name, schema);
                }
            }
        }
        assert_eq!(schemas(&openapi()), seen);
    }

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
        assert_eq!(Cursor::decode(&text, CursorKind::Shared), None);
        // A cursor of the "shared with me" list is good for that list alone.
        let shared = Cursor {
            kind: CursorKind::Shared,
            ..cursor
        }
        .encode();
        assert!(shared.starts_with("shared_"), "{shared}");
        assert_eq!(
            Cursor::decode(&shared, CursorKind::Shared).map(|cursor| (cursor.at, cursor.id)),
            Some((at, id))
        );
        for other in [
            CursorKind::Active,
            CursorKind::Archived,
            CursorKind::Deleted,
        ] {
            assert_eq!(Cursor::decode(&shared, other), None, "{other:?}");
        }
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
