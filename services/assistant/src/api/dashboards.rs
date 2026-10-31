//! The dashboard routes (Laysi/trustiot-v2#150; `contract::api::dashboard::public`, design
//! record 「公開 API」). There is no route that creates or edits content: only the `dashboard`
//! tool writes versions, and no request body here has a field for SQL, a semantic request or a
//! page.
//!
//! | Request | Who | Success |
//! | --- | --- | --- |
//! | `GET /api/assistant/v1/dashboards` (`?include_archived=true`, `?cursor=`) | viewer and up | 200 [`DashboardList`] of saved dashboards |
//! | `GET /api/assistant/v1/conversations/{conversation_id}/dashboards` (`?cursor=`) | the conversation's owner | 200 [`DashboardList`] of its drafts |
//! | `GET /api/assistant/v1/dashboards/{dashboard_id}` (`?version=`) | whoever can see it | 200 [`DashboardView`] |
//! | `GET /api/assistant/v1/dashboards/{dashboard_id}/versions` (`?cursor=`) | whoever can see it | 200 [`VersionList`] |
//! | `POST /api/assistant/v1/dashboards/{dashboard_id}/publish` | editor and up who own the draft's conversation | 201 [`VersionCreated`] in the saved dashboard |
//! | `POST /api/assistant/v1/dashboards/{dashboard_id}/restore` | a saved dashboard: editor and up; a draft: its conversation's owner | 201 [`VersionCreated`] |
//! | `PUT`, `DELETE /api/assistant/v1/dashboards/{dashboard_id}/archive` | editor and up (a saved dashboard) | 200 [`ArchiveState`] |
//! | `POST /api/assistant/v1/dashboards/{dashboard_id}/dataset-runs` | whoever can see it, run as the caller | 200 [`DatasetResult`] |
//!
//! Every handler first requires the role its operation needs, with service-kit's
//! `UserIdentity::require` (the permission matrix, docs/architecture/permissions.md, section
//! `## assistant`): a refusal answers its 403 `permission_denied` with `cache-control: no-store`
//! and is logged at INFO as `refused to <action>`. Who sees what and who may change what is then
//! the store's ([`store`], which checks the editor role again); a dashboard the caller cannot
//! see answers the same 404 as one that does not exist. Errors (M1):
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | No tenant, a role this build does not know, or a role below the one the operation requires (editor to publish, archive and unarchive; viewer for the rest) | 403 `permission_denied`, from the token alone before the body or the database |
//! | An ID that is not a UUID, or a dashboard the caller cannot see | 404 `not_found` |
//! | A query string with an unknown parameter or a bad value, a bad `cursor` | 400 `invalid_request` |
//! | A body that is not `application/json`, over 4 KiB, or not exactly the request (unknown fields refused) | 400 `invalid_request` |
//! | Errors of the stored dashboard | as `contract::api::dashboard::public`'s keys |
//! | A publish or restore that would add a version past the user's or the tenant's budget for the minute ([`crate::dashboards::limits::VersionBudget`]) | 429 `rate_limited`, `dashboard.versions_limited`, `Retry-After`; nothing was written |
//! | Errors of a dataset run | [`RunError::to_error_body`]; `Retry-After` with `dashboard.runs_limited` |
//! | A lock (another request's or a stuck session's on the dashboard) not granted within [`super::REQUEST_LOCK_TIMEOUT`] | 503 `unavailable`; nothing was written |
//! | The database failed | 503 `unavailable`; nothing was written |
//!
//! Every request's transaction starts with [`super::begin`], so its statements wait at most
//! [`super::REQUEST_LOCK_TIMEOUT`] for a lock: two writes of one dashboard queue on its row for
//! milliseconds, and the later one then conflicts.
//!
//! A write logs one INFO line with the user, tenant, dashboard and version; never a title,
//! a spec or a page.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{FromRef, Path, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use contract::api::dashboard::public::{
    ARCHIVED, ArchiveState, DATASET_FAILED, DATASET_UNKNOWN, DRAFT_REQUIRED, DashboardList,
    DashboardView, DatasetRunRequest, EDITOR_REQUIRED, OWNER_REQUIRED, PublishRequest,
    RESTORE_UNCHANGED, RestoreRequest, SAVED_CHANGED, SAVED_REQUIRED, TOO_MANY_VERSIONS,
    VERSION_CONFLICT, VERSION_NOT_FOUND, VERSIONS_LIMITED, VersionCreated, VersionList,
};
use contract::api::dashboard::result::DatasetResult;
use contract::{DashboardId, ErrorBody, ErrorCode, TenantId, TenantRole};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use service_kit::identity::{IdentityVerifier, OnBehalf, Requirement, UserIdentity};
use service_kit::tenant::TenantDb;

use super::held_body::HeldBody;
use super::{begin, error, finish, not_found, ok, parse_object, read_json};
use crate::callee::{Caller, Operation};
use crate::dashboards::limits::VersionBudget;
use crate::dashboards::runner::{DatasetRunner, HeldResult, RunError};
use crate::dashboards::store::{self, ListCursor, Requester, StoreError, VersionCursor};
use crate::delegation::OnBehalfOf;

/// The largest request body: the bodies are a few numbers and a name.
pub const MAX_BODY_BYTES: usize = 4 * 1024;

/// What the dashboard routes need besides the identity verifier and the pool.
#[derive(Clone)]
pub struct Dashboards {
    /// Exchanges the request's token for one addressed to telemetry-query when a dataset runs;
    /// `None` without `identity.exchange_url`, and dataset runs then fail (503).
    pub on_behalf: Option<OnBehalf>,
    /// The dataset runs, shared with the `dashboard` tool so the caps hold for both.
    pub runner: Arc<DatasetRunner>,
    /// The publishes and restores each user and tenant may start in a minute, in this replica.
    pub versions: Arc<VersionBudget>,
    /// The bound on handing a dataset run's answer body whole to the connection, after which the
    /// answer is cut off and its result let go ([`super::held_body::HELD_BODY_TIMEOUT`] in the
    /// service; tests shorten it).
    pub held_body_timeout: Duration,
}

#[derive(Clone)]
struct Routes {
    verifier: IdentityVerifier,
    /// The service role's pool: one short transaction of the request's tenant per request.
    db: TenantDb,
    dashboards: Dashboards,
}

impl FromRef<Routes> for IdentityVerifier {
    fn from_ref(state: &Routes) -> Self {
        state.verifier.clone()
    }
}

/// The saved dashboards.
pub const DASHBOARDS_PATH: &str = "/api/assistant/v1/dashboards";
/// A conversation's drafts.
pub const CONVERSATION_DASHBOARDS_PATH: &str =
    "/api/assistant/v1/conversations/{conversation_id}/dashboards";
/// One dashboard.
pub const DASHBOARD_PATH: &str = "/api/assistant/v1/dashboards/{dashboard_id}";
/// A dashboard's versions.
pub const VERSIONS_PATH: &str = "/api/assistant/v1/dashboards/{dashboard_id}/versions";
/// Publishing a draft.
pub const PUBLISH_PATH: &str = "/api/assistant/v1/dashboards/{dashboard_id}/publish";
/// Restoring a version.
pub const RESTORE_PATH: &str = "/api/assistant/v1/dashboards/{dashboard_id}/restore";
/// Archiving (`PUT`) and unarchiving (`DELETE`) a saved dashboard.
pub const ARCHIVE_PATH: &str = "/api/assistant/v1/dashboards/{dashboard_id}/archive";
/// Running one dataset of one version.
pub const DATASET_RUNS_PATH: &str = "/api/assistant/v1/dashboards/{dashboard_id}/dataset-runs";

/// The query string of the saved dashboards' list.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    #[serde(default)]
    include_archived: bool,
    cursor: Option<String>,
}

/// The query string of a paged list.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PageQuery {
    cursor: Option<String>,
}

/// The query string of one dashboard.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewQuery {
    version: Option<u32>,
}

/// The dashboard routes.
pub fn routes(verifier: IdentityVerifier, db: TenantDb, dashboards: Dashboards) -> Router {
    Router::new()
        .route(DASHBOARDS_PATH, get(list_saved))
        .route(CONVERSATION_DASHBOARDS_PATH, get(list_drafts))
        .route(DASHBOARD_PATH, get(get_dashboard))
        .route(VERSIONS_PATH, get(list_versions))
        .route(PUBLISH_PATH, post(publish))
        .route(RESTORE_PATH, post(restore))
        .route(ARCHIVE_PATH, put(archive).delete(unarchive))
        .route(DATASET_RUNS_PATH, post(run_dataset))
        .with_state(Routes {
            verifier,
            db,
            dashboards,
        })
}

/// The tenant's saved dashboards.
#[utoipa::path(
    get,
    path = "/api/assistant/v1/dashboards",
    tag = "assistant",
    operation_id = "listDashboards",
    params(
        ("include_archived" = Option<bool>, Query, description = "Archived ones too; without it only the ones not archived."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the page before."),
    ),
    responses(
        (status = 200, description = "The saved dashboards, the most recently updated first, 50 a page.", body = DashboardList),
        (status = 400, description = "An unknown parameter, a bad value or a bad cursor (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn list_saved(
    State(api): State<Routes>,
    user: UserIdentity,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Response {
    const ACTION: &str = "list dashboards";
    let requester = match requester(&user, TenantRole::Viewer, ACTION) {
        Ok(requester) => requester,
        Err(refused) => return refused,
    };
    let Ok(Query(query)) = query else {
        return invalid_request();
    };
    let cursor = match list_cursor(query.cursor.as_deref()) {
        Ok(cursor) => cursor,
        Err(refused) => return refused,
    };
    let mut tx = match begin(&api.db, &user, ACTION).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    let listed =
        store::list_saved(&mut tx, requester.principal, query.include_archived, cursor).await;
    match finish(tx, listed).await {
        Ok(list) => ok(StatusCode::OK, &list),
        Err(failure) => dashboard_failure(&user, None, ACTION, &failure),
    }
}

/// A conversation's drafts, for its owner.
#[utoipa::path(
    get,
    path = "/api/assistant/v1/conversations/{conversation_id}/dashboards",
    tag = "assistant",
    operation_id = "listConversationDashboards",
    params(
        ("conversation_id" = uuid::Uuid, Path, description = "The conversation's ID."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the page before."),
    ),
    responses(
        (status = 200, description = "The conversation's drafts, the most recently updated first, 50 a page.", body = DashboardList),
        (status = 400, description = "An unknown parameter or a bad cursor (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such conversation of the caller's, or it is deleted (`not_found`): another user's is the same as none.", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn list_drafts(
    State(api): State<Routes>,
    user: UserIdentity,
    conversation_id: Result<Path<String>, PathRejection>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    const ACTION: &str = "list a conversation's dashboards";
    let requester = match requester(&user, TenantRole::Viewer, ACTION) {
        Ok(requester) => requester,
        Err(refused) => return refused,
    };
    let Some(conversation_id) = super::parse_id(conversation_id) else {
        return not_found();
    };
    let Ok(Query(query)) = query else {
        return invalid_request();
    };
    let cursor = match list_cursor(query.cursor.as_deref()) {
        Ok(cursor) => cursor,
        Err(refused) => return refused,
    };
    let mut tx = match begin(&api.db, &user, ACTION).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    let listed = store::list_drafts(&mut tx, requester.principal, conversation_id, cursor).await;
    match finish(tx, listed).await {
        Ok(list) => ok(StatusCode::OK, &list),
        Err(failure) => dashboard_failure(&user, None, ACTION, &failure),
    }
}

/// One dashboard with one version (the latest without `version`) and what the caller may do
/// with it.
#[utoipa::path(
    get,
    path = "/api/assistant/v1/dashboards/{dashboard_id}",
    tag = "assistant",
    operation_id = "getDashboard",
    params(
        ("dashboard_id" = DashboardId, Path, description = "The dashboard's ID."),
        ("version" = Option<u32>, Query, description = "The version; the latest without it."),
    ),
    responses(
        (status = 200, description = "The dashboard, the version and the caller's permissions (for the UI; every write checks them again).", body = DashboardView),
        (status = 400, description = "An unknown parameter or a bad version (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such dashboard the caller can see (`not_found`), or no such version (`dashboard.version_not_found`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn get_dashboard(
    State(api): State<Routes>,
    user: UserIdentity,
    dashboard_id: Result<Path<String>, PathRejection>,
    query: Result<Query<ViewQuery>, QueryRejection>,
) -> Response {
    const ACTION: &str = "read a dashboard";
    let requester = match requester(&user, TenantRole::Viewer, ACTION) {
        Ok(requester) => requester,
        Err(refused) => return refused,
    };
    let Some(dashboard_id) = dashboard_id_of(dashboard_id) else {
        return not_found();
    };
    let Ok(Query(query)) = query else {
        return invalid_request();
    };
    let mut tx = match begin(&api.db, &user, ACTION).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    let opened = store::open(&mut tx, requester.principal, dashboard_id, query.version).await;
    let opened = match finish(tx, opened).await {
        Ok(opened) => opened,
        Err(failure) => return dashboard_failure(&user, Some(dashboard_id), ACTION, &failure),
    };
    let permissions = store::permissions(opened.dashboard.kind, opened.access, requester.role);
    ok(
        StatusCode::OK,
        &DashboardView {
            dashboard: opened.dashboard,
            version: opened.version,
            permissions,
        },
    )
}

/// A dashboard's versions, the newest first, without their spec and page.
#[utoipa::path(
    get,
    path = "/api/assistant/v1/dashboards/{dashboard_id}/versions",
    tag = "assistant",
    operation_id = "listDashboardVersions",
    params(
        ("dashboard_id" = DashboardId, Path, description = "The dashboard's ID."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the page before."),
    ),
    responses(
        (status = 200, description = "The versions, the newest first, 50 a page.", body = VersionList),
        (status = 400, description = "An unknown parameter or a bad cursor (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such dashboard the caller can see (`not_found`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn list_versions(
    State(api): State<Routes>,
    user: UserIdentity,
    dashboard_id: Result<Path<String>, PathRejection>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    const ACTION: &str = "list a dashboard's versions";
    let requester = match requester(&user, TenantRole::Viewer, ACTION) {
        Ok(requester) => requester,
        Err(refused) => return refused,
    };
    let Some(dashboard_id) = dashboard_id_of(dashboard_id) else {
        return not_found();
    };
    let Ok(Query(query)) = query else {
        return invalid_request();
    };
    let cursor = match query.cursor.as_deref() {
        None => None,
        Some(text) => match VersionCursor::parse(text) {
            Some(cursor) => Some(cursor),
            None => return invalid_request(),
        },
    };
    let mut tx = match begin(&api.db, &user, ACTION).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    let listed = store::versions(&mut tx, requester.principal, dashboard_id, cursor).await;
    match finish(tx, listed).await {
        Ok(list) => ok(StatusCode::OK, &list),
        Err(failure) => dashboard_failure(&user, Some(dashboard_id), ACTION, &failure),
    }
}

/// Publishes a version of a draft into the tenant's saved dashboards: the first time as a new
/// saved dashboard, later as the next version of the draft's saved dashboard, when that one's
/// latest version is the one the published version is based on.
#[utoipa::path(
    post,
    path = "/api/assistant/v1/dashboards/{dashboard_id}/publish",
    tag = "assistant",
    operation_id = "publishDashboard",
    params(("dashboard_id" = DashboardId, Path, description = "The draft's ID.")),
    request_body = PublishRequest,
    responses(
        (status = 201, description = "The saved dashboard and its new version.", body = VersionCreated),
        (status = 400, description = "The body is not JSON, over 4 KiB, or not exactly the request (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant, or a role below editor (`permission_denied`); not the draft's conversation's owner (`permission_denied`, `dashboard.owner_required`).", body = ErrorBody),
        (status = 404, description = "No such dashboard the caller can see (`not_found`), or no such version (`dashboard.version_not_found`).", body = ErrorBody),
        (status = 409, description = "The saved dashboard's latest version is not the one the draft's version is based on (`dashboard.saved_changed`, `details.latest`), is archived (`dashboard.archived`), or has 10,000 versions (`dashboard.too_many_versions`) (`conflict`).", body = ErrorBody),
        (status = 422, description = "Not a draft (`validation_failed`, `dashboard.draft_required`).", body = ErrorBody),
        (status = 429, description = "The user added 10 versions by publishing and restoring in the last minute, or the tenant's users 30 to its saved dashboards (30 more to its drafts), in this replica (`rate_limited`, `dashboard.versions_limited`, `details.retry_after`, with `Retry-After`). Only a request that passed every other check counts. Nothing was written.", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`). Nothing was written.", body = ErrorBody),
    )
)]
async fn publish(
    State(api): State<Routes>,
    user: UserIdentity,
    dashboard_id: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    const ACTION: &str = "publish a dashboard";
    let requester = match requester(&user, TenantRole::Editor, ACTION) {
        Ok(requester) => requester,
        Err(refused) => return refused,
    };
    let Some(dashboard_id) = dashboard_id_of(dashboard_id) else {
        return not_found();
    };
    let request: PublishRequest = match read_request(&headers, body).await {
        Ok(request) => request,
        Err(refused) => return refused,
    };
    let mut tx = match begin(&api.db, &user, ACTION).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    let admit = |kind| api.dashboards.versions.admit(requester.principal, kind);
    let published = store::publish(&mut tx, &user, dashboard_id, request.version, admit).await;
    if let Err(StoreError::Limited { retry_after }) = published {
        // Rolled back: the store wrote nothing before it asked.
        drop(tx);
        return versions_limited(&requester, retry_after, "publish");
    }
    match finish(tx, published).await {
        Ok(written) => created(&written, &requester, dashboard_id, "published a dashboard"),
        Err(failure) => dashboard_failure(&user, Some(dashboard_id), ACTION, &failure),
    }
}

/// Adds a copy of an older version as the dashboard's next version.
#[utoipa::path(
    post,
    path = "/api/assistant/v1/dashboards/{dashboard_id}/restore",
    tag = "assistant",
    operation_id = "restoreDashboard",
    params(("dashboard_id" = DashboardId, Path, description = "The dashboard's ID.")),
    request_body = RestoreRequest,
    responses(
        (status = 201, description = "The dashboard's new version, a copy of `from_version`.", body = VersionCreated),
        (status = 400, description = "The body is not JSON, over 4 KiB, or not exactly the request (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`); a viewer restoring a saved dashboard (`permission_denied`, `dashboard.editor_required`), or someone other than a draft's conversation owner (`permission_denied`, `dashboard.owner_required`).", body = ErrorBody),
        (status = 404, description = "No such dashboard the caller can see (`not_found`), or no such version (`dashboard.version_not_found`).", body = ErrorBody),
        (status = 409, description = "`base_version` is not the latest (`dashboard.version_conflict`, `details.latest`), the content of `from_version` is the latest version's (`dashboard.restore_unchanged`), the dashboard is archived (`dashboard.archived`), or it has 10,000 versions (`dashboard.too_many_versions`) (`conflict`).", body = ErrorBody),
        (status = 429, description = "The user added 10 versions by publishing and restoring in the last minute, or the tenant's users 30 to its saved dashboards (30 more to its drafts), in this replica (`rate_limited`, `dashboard.versions_limited`, `details.retry_after`, with `Retry-After`). Only a request that passed every other check counts. Nothing was written.", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`). Nothing was written.", body = ErrorBody),
    )
)]
async fn restore(
    State(api): State<Routes>,
    user: UserIdentity,
    dashboard_id: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    const ACTION: &str = "restore a dashboard";
    let requester = match requester(&user, TenantRole::Viewer, ACTION) {
        Ok(requester) => requester,
        Err(refused) => return refused,
    };
    let Some(dashboard_id) = dashboard_id_of(dashboard_id) else {
        return not_found();
    };
    let request: RestoreRequest = match read_request(&headers, body).await {
        Ok(request) => request,
        Err(refused) => return refused,
    };
    let mut tx = match begin(&api.db, &user, ACTION).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    let restored = store::restore(
        &mut tx,
        &user,
        dashboard_id,
        request.from_version,
        request.base_version,
        |kind| api.dashboards.versions.admit(requester.principal, kind),
    )
    .await;
    if let Err(StoreError::Limited { retry_after }) = restored {
        // Rolled back: the store wrote nothing before it asked.
        drop(tx);
        return versions_limited(&requester, retry_after, "restore");
    }
    match finish(tx, restored).await {
        Ok(written) => created(
            &written,
            &requester,
            dashboard_id,
            "restored a dashboard version",
        ),
        Err(failure) => dashboard_failure(&user, Some(dashboard_id), ACTION, &failure),
    }
}

/// Archives a saved dashboard: it leaves the tenant's list (unless archived ones are asked for)
/// and takes no new version until it is unarchived. Archiving it again keeps the first time.
#[utoipa::path(
    put,
    path = "/api/assistant/v1/dashboards/{dashboard_id}/archive",
    tag = "assistant",
    operation_id = "archiveDashboard",
    params(("dashboard_id" = DashboardId, Path, description = "The saved dashboard's ID.")),
    responses(
        (status = 200, description = "The dashboard is archived.", body = ArchiveState),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant, or a role below editor (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such dashboard the caller can see (`not_found`).", body = ErrorBody),
        (status = 422, description = "A draft (`validation_failed`, `dashboard.saved_required`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`). Nothing was written.", body = ErrorBody),
    )
)]
async fn archive(
    State(api): State<Routes>,
    user: UserIdentity,
    dashboard_id: Result<Path<String>, PathRejection>,
) -> Response {
    set_archived(api, user, dashboard_id, true).await
}

/// Unarchives a saved dashboard.
#[utoipa::path(
    delete,
    path = "/api/assistant/v1/dashboards/{dashboard_id}/archive",
    tag = "assistant",
    operation_id = "unarchiveDashboard",
    params(("dashboard_id" = DashboardId, Path, description = "The saved dashboard's ID.")),
    responses(
        (status = 200, description = "The dashboard is not archived.", body = ArchiveState),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant, or a role below editor (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such dashboard the caller can see (`not_found`).", body = ErrorBody),
        (status = 422, description = "A draft (`validation_failed`, `dashboard.saved_required`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`). Nothing was written.", body = ErrorBody),
    )
)]
async fn unarchive(
    State(api): State<Routes>,
    user: UserIdentity,
    dashboard_id: Result<Path<String>, PathRejection>,
) -> Response {
    set_archived(api, user, dashboard_id, false).await
}

async fn set_archived(
    api: Routes,
    user: UserIdentity,
    dashboard_id: Result<Path<String>, PathRejection>,
    archived: bool,
) -> Response {
    let action = if archived {
        "archive a dashboard"
    } else {
        "unarchive a dashboard"
    };
    let requester = match requester(&user, TenantRole::Editor, action) {
        Ok(requester) => requester,
        Err(refused) => return refused,
    };
    let Some(dashboard_id) = dashboard_id_of(dashboard_id) else {
        return not_found();
    };
    let mut tx = match begin(&api.db, &user, action).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    let changed = store::set_archived(&mut tx, &user, dashboard_id, archived).await;
    let state = match finish(tx, changed).await {
        Ok(state) => state,
        Err(failure) => return dashboard_failure(&user, Some(dashboard_id), action, &failure),
    };
    tracing::info!(
        user_id = %requester.principal.user_id,
        tenant_id = %requester.principal.tenant_id,
        dashboard_id = %dashboard_id,
        archived = state.archived_at.is_some(),
        "changed whether a dashboard is archived"
    );
    ok(StatusCode::OK, &state)
}

/// Runs one dataset of one version as the caller and returns its table. Nothing is kept: every
/// run reads again.
#[utoipa::path(
    post,
    path = "/api/assistant/v1/dashboards/{dashboard_id}/dataset-runs",
    tag = "assistant",
    operation_id = "runDashboardDataset",
    params(("dashboard_id" = DashboardId, Path, description = "The dashboard's ID.")),
    request_body = DatasetRunRequest,
    responses(
        (status = 200, description = "The dataset's table, read with the caller's tenant, role and queryable period.", body = DatasetResult),
        (status = 400, description = "The body is not JSON, over 4 KiB, or not exactly the request (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`); the range lies wholly before the caller's queryable period (`quota_exceeded`, `telemetry.outside_queryable_period`, `details.earliest`).", body = ErrorBody),
        (status = 404, description = "No such dashboard the caller can see (`not_found`), or no such version (`dashboard.version_not_found`).", body = ErrorBody),
        (status = 422, description = "No such dataset (`dashboard.dataset_unknown`); more rows than the dataset allows (`dashboard.dataset_too_many_rows`, `details.max`); a result over 2 MiB (`dashboard.dataset_result_too_large`); a query telemetry-query refuses now (`dashboard.dataset_refused`); a query too heavy (`telemetry.query_too_heavy`) or with too many points (`telemetry.too_many_points`, `details.max`) (`validation_failed`).", body = ErrorBody),
        (status = 429, description = "The tenant ran 60 datasets in the last minute, has 2 running in this replica, or this replica's memory for results had no room for the answer within the wait (`dashboard.runs_limited`, with `Retry-After`), or its query quota is used up (`dashboard.query_quota_exhausted`) (`rate_limited`).", body = ErrorBody),
        (status = 503, description = "The tenant's telemetry account is not ready (`telemetry.account_not_ready`), or the run failed (`dashboard.dataset_failed`) (`unavailable`).", body = ErrorBody),
        (status = 504, description = "The query ran longer than the tenant's limit (`timeout`, `dashboard.dataset_timeout`).", body = ErrorBody),
    )
)]
async fn run_dataset(
    State(api): State<Routes>,
    user: UserIdentity,
    dashboard_id: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    const ACTION: &str = "run a dashboard's dataset";
    let requester = match requester(&user, TenantRole::Viewer, ACTION) {
        Ok(requester) => requester,
        Err(refused) => return refused,
    };
    let Some(dashboard_id) = dashboard_id_of(dashboard_id) else {
        return not_found();
    };
    let request: DatasetRunRequest = match read_request(&headers, body).await {
        Ok(request) => request,
        Err(refused) => return refused,
    };
    // The transaction ends before the run calls telemetry-query: no connection is held then.
    let mut tx = match begin(&api.db, &user, ACTION).await {
        Ok(tx) => tx,
        Err(refused) => return refused,
    };
    let dataset = store::dataset(
        &mut tx,
        requester.principal,
        dashboard_id,
        request.version,
        &request.dataset,
    )
    .await;
    let dataset = match finish(tx, dataset).await {
        Ok(Some(dataset)) => dataset,
        Ok(None) => {
            return error(
                ErrorBody::new(ErrorCode::ValidationFailed).with_message_key(DATASET_UNKNOWN),
            );
        }
        Err(failure) => return dashboard_failure(&user, Some(dashboard_id), ACTION, &failure),
    };
    let Some(on_behalf) = &api.dashboards.on_behalf else {
        tracing::warn!(
            tenant_id = %requester.principal.tenant_id,
            "a dataset run cannot call telemetry-query: identity.exchange_url is not set"
        );
        return error(ErrorBody::new(ErrorCode::Unavailable).with_message_key(DATASET_FAILED));
    };
    let delegation = OnBehalfOf {
        on_behalf,
        user: &user,
    };
    let caller = Caller {
        delegation: &delegation,
        tenant_id: requester.principal.tenant_id,
        operation: Operation::DatasetRun,
        run_id: None,
        conversation_id: None,
    };
    // The answer's body holds the result's memory and its room among the tenant's answers until
    // its last piece goes to the connection, and cuts the answer off when the connection has not
    // taken it whole within the bound.
    match api.dashboards.runner.run_answer(&caller, &dataset).await {
        Ok(result) => held_result(
            result,
            requester.principal.tenant_id,
            api.dashboards.held_body_timeout,
        ),
        Err(failure) => run_error(&failure),
    }
}

/// The 200 of a run: the result's JSON as the body, as `axum::Json` writes it, holding the
/// result's reserved memory until its last piece goes to the connection or the body is dropped
/// (the client went away). A body the connection has not taken whole within `bound` lets the
/// result go and ends the response ([`HeldBody`]).
fn held_result(result: HeldResult, tenant: TenantId, bound: Duration) -> Response {
    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, "application/json"),
            (CACHE_CONTROL, "no-store"),
        ],
        Body::new(HeldBody::new(result, tenant, bound)),
    )
        .into_response()
}

/// The 429 of a publish or restore that would add a version past the user's or the tenant's
/// budget for the minute, with `Retry-After`. Nothing was written.
fn versions_limited(
    requester: &Requester,
    retry_after: Duration,
    action: &'static str,
) -> Response {
    let mut response = error(versions_limited_body(retry_after));
    let seconds = retry_after.as_secs().max(1);
    tracing::info!(
        user_id = %requester.principal.user_id,
        tenant_id = %requester.principal.tenant_id,
        action,
        retry_after = seconds,
        "refused a dashboard version: the user or tenant used the minute's budget"
    );
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from(seconds));
    response
}

fn versions_limited_body(retry_after: Duration) -> ErrorBody {
    ErrorBody::new(ErrorCode::RateLimited)
        .with_message_key(VERSIONS_LIMITED)
        .with_detail("retry_after", retry_after.as_secs().max(1))
}

/// The answer to a failed run, with `Retry-After` when the caps refused it.
fn run_error(failure: &RunError) -> Response {
    let mut response = error(failure.to_error_body());
    if let RunError::Limited { retry_after } = failure {
        response
            .headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from(retry_after.as_secs().max(1)));
    }
    response
}

/// The requester of a request, once it has at least the role `needed` in its tenant: the
/// operation's row of the permission matrix (docs/architecture/permissions.md, section
/// `## assistant`), checked with service-kit's `UserIdentity::require` before the body or the
/// database. A refusal is its 403 `permission_denied` with `cache-control: no-store`, logged at
/// INFO as `refused to <action>`.
#[allow(
    clippy::result_large_err,
    reason = "the error is the response to answer at once"
)]
fn requester(
    user: &UserIdentity,
    needed: TenantRole,
    action: &'static str,
) -> Result<Requester, Response> {
    if let Err(refused) = user.require(Requirement::Tenant(needed), action) {
        return Err(refused.into_response());
    }
    // A tenant requirement that passed means a tenant; answer the same refusal regardless.
    Requester::of(user).ok_or_else(|| error(ErrorBody::new(ErrorCode::PermissionDenied)))
}

/// The path's dashboard ID; `None` (answered with [`not_found`]) when it is not a UUID.
fn dashboard_id_of(path: Result<Path<String>, PathRejection>) -> Option<DashboardId> {
    super::parse_id(path).map(DashboardId::from_uuid)
}

/// The JSON body of a request as `T`, which refuses fields it does not know: 400
/// `invalid_request` when the content type is not JSON, the body is over [`MAX_BODY_BYTES`],
/// or it is not exactly a `T` written as a JSON object ([`super::parse_object`]: not an array of
/// its fields, and the derived `Deserialize` refuses a duplicate field).
#[allow(
    clippy::result_large_err,
    reason = "the error is the response to answer at once"
)]
async fn read_request<T: DeserializeOwned>(headers: &HeaderMap, body: Body) -> Result<T, Response> {
    let bytes = read_json(headers, body, MAX_BODY_BYTES)
        .await
        .map_err(|_| invalid_request())?;
    parse_object(&bytes).ok_or_else(invalid_request)
}

/// The 201 of a new version, logged.
fn created(
    created: &VersionCreated,
    requester: &Requester,
    from: DashboardId,
    message: &'static str,
) -> Response {
    tracing::info!(
        user_id = %requester.principal.user_id,
        tenant_id = %requester.principal.tenant_id,
        from = %from,
        dashboard_id = %created.dashboard_id,
        version = created.version,
        "{message}"
    );
    ok(StatusCode::CREATED, created)
}

/// The cursor of a list, `None` without one: 400 for a cursor this service did not write.
#[allow(
    clippy::result_large_err,
    reason = "the error is the response to answer at once"
)]
fn list_cursor(text: Option<&str>) -> Result<Option<ListCursor>, Response> {
    match text {
        None => Ok(None),
        Some(text) => ListCursor::parse(text)
            .map(Some)
            .ok_or_else(invalid_request),
    }
}

fn invalid_request() -> Response {
    error(ErrorBody::new(ErrorCode::InvalidRequest))
}

/// The answer to a store failure: every dashboard the caller cannot see is the one
/// [`not_found`] answer, byte for byte the conversations'. Failures that are not the request's
/// fault are logged at ERROR with their kind only (`dashboard request failed`).
fn dashboard_failure(
    user: &UserIdentity,
    dashboard_id: Option<DashboardId>,
    action: &'static str,
    failure: &StoreError,
) -> Response {
    let keyed = |code, key| ErrorBody::new(code).with_message_key(key);
    let body = match failure {
        StoreError::NotFound => return not_found(),
        StoreError::VersionNotFound => keyed(ErrorCode::NotFound, VERSION_NOT_FOUND),
        StoreError::VersionConflict { latest } => {
            keyed(ErrorCode::Conflict, VERSION_CONFLICT).with_detail("latest", *latest)
        }
        StoreError::SavedChanged { latest } => {
            keyed(ErrorCode::Conflict, SAVED_CHANGED).with_detail("latest", *latest)
        }
        StoreError::Archived => keyed(ErrorCode::Conflict, ARCHIVED),
        StoreError::Unchanged => keyed(ErrorCode::Conflict, RESTORE_UNCHANGED),
        // The publish and restore handlers answer it with `Retry-After` before this.
        StoreError::Limited { retry_after } => versions_limited_body(*retry_after),
        StoreError::TooManyVersions => keyed(ErrorCode::Conflict, TOO_MANY_VERSIONS),
        StoreError::EditorRequired => keyed(ErrorCode::PermissionDenied, EDITOR_REQUIRED),
        StoreError::OwnerRequired => keyed(ErrorCode::PermissionDenied, OWNER_REQUIRED),
        StoreError::DraftRequired => keyed(ErrorCode::ValidationFailed, DRAFT_REQUIRED),
        StoreError::SavedRequired => keyed(ErrorCode::ValidationFailed, SAVED_REQUIRED),
        // Another write of the same dashboard came first: trying again reads the new state.
        StoreError::Concurrent => ErrorBody::new(ErrorCode::Conflict),
        StoreError::DraftExists { .. }
        | StoreError::InvalidRebase
        | StoreError::Invalid(_)
        | StoreError::Unstorable(_)
        | StoreError::WrongTenant => {
            // The API never writes content: these are the tool's, so one here is a bug.
            tracing::error!(
                user_id = %user.user_id(),
                dashboard_id = dashboard_id.map(|id| id.to_string()).as_deref(),
                action,
                error_kind = failure.log_kind(),
                "dashboard request failed"
            );
            ErrorBody::new(ErrorCode::Internal)
        }
        StoreError::Corrupt(_) | StoreError::Database(_) => {
            tracing::error!(
                user_id = %user.user_id(),
                dashboard_id = dashboard_id.map(|id| id.to_string()).as_deref(),
                action,
                error_kind = failure.log_kind(),
                "dashboard request failed"
            );
            ErrorBody::new(ErrorCode::Unavailable)
        }
    };
    error(body)
}
