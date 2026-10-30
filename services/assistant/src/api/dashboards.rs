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
//! Who sees what and who may change what is the store's ([`store`]); a dashboard the caller
//! cannot see answers the same 404 as one that does not exist. Errors (M1):
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | No tenant, or a role this build does not know | 403 `permission_denied` |
//! | An ID that is not a UUID, or a dashboard the caller cannot see | 404 `not_found` |
//! | A query string with an unknown parameter or a bad value, a bad `cursor` | 400 `invalid_request` |
//! | A body that is not `application/json`, over 4 KiB, or not exactly the request (unknown fields refused) | 400 `invalid_request` |
//! | Errors of the stored dashboard | as `contract::api::dashboard::public`'s keys |
//! | A publish or restore that would add a version past the user's or the tenant's budget for the minute ([`crate::dashboards::limits::VersionBudget`]) | 429 `rate_limited`, `dashboard.versions_limited`, `Retry-After`; nothing was written |
//! | Errors of a dataset run | [`RunError::to_error_body`]; `Retry-After` with `dashboard.runs_limited` |
//! | The database failed | 503 `unavailable`; nothing was written |
//!
//! A write logs one INFO line with the user, tenant, dashboard and version; never a title,
//! a spec or a page.

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
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
use contract::{DashboardId, ErrorBody, ErrorCode, TenantId};
use serde::Deserialize;
use service_kit::identity::UserIdentity;
use service_kit::tenant::TenantTransaction;
use uuid::Uuid;

use super::held_body::HeldBody;
use super::{ApiError, ApiState, begin, commit, error, ok, parse_id, read_body, requester};
use crate::callee::{Caller, Operation};
use crate::dashboards::runner::{HeldResult, RunError};
use crate::dashboards::store::{self, ListCursor, Requester, StoreError, VersionCursor};
use crate::delegation::OnBehalfOf;

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

/// The dashboard routes, without their state.
pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route(DASHBOARDS_PATH, get(list_saved))
        .route(CONVERSATION_DASHBOARDS_PATH, get(list_drafts))
        .route(DASHBOARD_PATH, get(get_dashboard))
        .route(VERSIONS_PATH, get(list_versions))
        .route(PUBLISH_PATH, post(publish))
        .route(RESTORE_PATH, post(restore))
        .route(ARCHIVE_PATH, put(archive).delete(unarchive))
        .route(DATASET_RUNS_PATH, post(run_dataset))
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
    State(api): State<ApiState>,
    user: UserIdentity,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let requester = requester(&user, "list dashboards")?;
    let Ok(Query(query)) = query else {
        return Err(invalid_request());
    };
    let cursor = list_cursor(query.cursor.as_deref())?;
    let mut tx = begin(&api.db, &user).await?;
    let listed =
        store::list_saved(&mut tx, requester.principal, query.include_archived, cursor).await;
    let list = finish(tx, &user, listed, None, "could not list dashboards").await?;
    Ok(ok(StatusCode::OK, &list))
}

/// A conversation's drafts, for its owner.
#[utoipa::path(
    get,
    path = "/api/assistant/v1/conversations/{conversation_id}/dashboards",
    tag = "assistant",
    operation_id = "listConversationDashboards",
    params(
        ("conversation_id" = Uuid, Path, description = "The conversation's ID."),
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the page before."),
    ),
    responses(
        (status = 200, description = "The conversation's drafts, the most recently updated first, 50 a page.", body = DashboardList),
        (status = 400, description = "An unknown parameter or a bad cursor (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such conversation of the caller's (`not_found`): another user's is the same as none.", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn list_drafts(
    State(api): State<ApiState>,
    user: UserIdentity,
    conversation_id: Result<Path<String>, PathRejection>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let requester = requester(&user, "list a conversation's dashboards")?;
    let conversation_id: Uuid = parse_id(conversation_id)?;
    let Ok(Query(query)) = query else {
        return Err(invalid_request());
    };
    let cursor = list_cursor(query.cursor.as_deref())?;
    let mut tx = begin(&api.db, &user).await?;
    let listed = store::list_drafts(&mut tx, requester.principal, conversation_id, cursor).await;
    let message = "could not list a conversation's dashboards";
    let list = finish(tx, &user, listed, None, message).await?;
    Ok(ok(StatusCode::OK, &list))
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
    State(api): State<ApiState>,
    user: UserIdentity,
    dashboard_id: Result<Path<String>, PathRejection>,
    query: Result<Query<ViewQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let requester = requester(&user, "read a dashboard")?;
    let dashboard_id: DashboardId = parse_id(dashboard_id)?;
    let Ok(Query(query)) = query else {
        return Err(invalid_request());
    };
    let mut tx = begin(&api.db, &user).await?;
    let opened = store::open(&mut tx, requester.principal, dashboard_id, query.version).await;
    let message = "could not read a dashboard";
    let opened = finish(tx, &user, opened, Some(dashboard_id), message).await?;
    let permissions = store::permissions(opened.dashboard.kind, opened.access, requester.role);
    Ok(ok(
        StatusCode::OK,
        &DashboardView {
            dashboard: opened.dashboard,
            version: opened.version,
            permissions,
        },
    ))
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
    State(api): State<ApiState>,
    user: UserIdentity,
    dashboard_id: Result<Path<String>, PathRejection>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let requester = requester(&user, "list a dashboard's versions")?;
    let dashboard_id: DashboardId = parse_id(dashboard_id)?;
    let Ok(Query(query)) = query else {
        return Err(invalid_request());
    };
    let cursor = match query.cursor.as_deref() {
        None => None,
        Some(text) => Some(VersionCursor::parse(text).ok_or_else(invalid_request)?),
    };
    let mut tx = begin(&api.db, &user).await?;
    let listed = store::versions(&mut tx, requester.principal, dashboard_id, cursor).await;
    let message = "could not list a dashboard's versions";
    let list = finish(tx, &user, listed, Some(dashboard_id), message).await?;
    Ok(ok(StatusCode::OK, &list))
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
        (status = 403, description = "No tenant, a viewer (`dashboard.editor_required`), or not the conversation's owner (`dashboard.owner_required`) (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such dashboard the caller can see (`not_found`), or no such version (`dashboard.version_not_found`).", body = ErrorBody),
        (status = 409, description = "The saved dashboard's latest version is not the one the draft's version is based on (`dashboard.saved_changed`, `details.latest`), is archived (`dashboard.archived`), or has 10,000 versions (`dashboard.too_many_versions`) (`conflict`).", body = ErrorBody),
        (status = 422, description = "Not a draft (`validation_failed`, `dashboard.draft_required`).", body = ErrorBody),
        (status = 429, description = "The user added 10 versions by publishing and restoring in the last minute, or the tenant's users 30 to its saved dashboards (30 more to its drafts), in this replica (`rate_limited`, `dashboard.versions_limited`, `details.retry_after`, with `Retry-After`). Only a request that passed every other check counts. Nothing was written.", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`). Nothing was written.", body = ErrorBody),
    )
)]
async fn publish(
    State(api): State<ApiState>,
    user: UserIdentity,
    dashboard_id: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let requester = requester(&user, "publish a dashboard")?;
    let dashboard_id: DashboardId = parse_id(dashboard_id)?;
    let request: PublishRequest = read_body(&headers, body).await?;
    let mut tx = begin(&api.db, &user).await?;
    let admit = |kind| api.versions.admit(requester.principal, kind);
    let published = store::publish(&mut tx, requester, dashboard_id, request.version, admit).await;
    if let Err(StoreError::Limited { retry_after }) = published {
        // Rolled back: the store wrote nothing before it asked.
        drop(tx);
        return Ok(versions_limited(&requester, retry_after, "publish"));
    }
    let message = "could not publish a dashboard";
    let written = finish(tx, &user, published, Some(dashboard_id), message).await?;
    Ok(created(
        &written,
        &requester,
        dashboard_id,
        "published a dashboard",
    ))
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
        (status = 403, description = "No tenant, a viewer restoring a saved dashboard (`dashboard.editor_required`), or someone other than a draft's conversation owner (`dashboard.owner_required`) (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such dashboard the caller can see (`not_found`), or no such version (`dashboard.version_not_found`).", body = ErrorBody),
        (status = 409, description = "`base_version` is not the latest (`dashboard.version_conflict`, `details.latest`), the content of `from_version` is the latest version's (`dashboard.restore_unchanged`), the dashboard is archived (`dashboard.archived`), or it has 10,000 versions (`dashboard.too_many_versions`) (`conflict`).", body = ErrorBody),
        (status = 429, description = "The user added 10 versions by publishing and restoring in the last minute, or the tenant's users 30 to its saved dashboards (30 more to its drafts), in this replica (`rate_limited`, `dashboard.versions_limited`, `details.retry_after`, with `Retry-After`). Only a request that passed every other check counts. Nothing was written.", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`). Nothing was written.", body = ErrorBody),
    )
)]
async fn restore(
    State(api): State<ApiState>,
    user: UserIdentity,
    dashboard_id: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let requester = requester(&user, "restore a dashboard")?;
    let dashboard_id: DashboardId = parse_id(dashboard_id)?;
    let request: RestoreRequest = read_body(&headers, body).await?;
    let mut tx = begin(&api.db, &user).await?;
    let restored = store::restore(
        &mut tx,
        requester,
        dashboard_id,
        request.from_version,
        request.base_version,
        |kind| api.versions.admit(requester.principal, kind),
    )
    .await;
    if let Err(StoreError::Limited { retry_after }) = restored {
        // Rolled back: the store wrote nothing before it asked.
        drop(tx);
        return Ok(versions_limited(&requester, retry_after, "restore"));
    }
    let message = "could not restore a dashboard";
    let written = finish(tx, &user, restored, Some(dashboard_id), message).await?;
    Ok(created(
        &written,
        &requester,
        dashboard_id,
        "restored a dashboard version",
    ))
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
        (status = 403, description = "No tenant, or a viewer (`permission_denied`, `dashboard.editor_required`).", body = ErrorBody),
        (status = 404, description = "No such dashboard the caller can see (`not_found`).", body = ErrorBody),
        (status = 422, description = "A draft (`validation_failed`, `dashboard.saved_required`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`). Nothing was written.", body = ErrorBody),
    )
)]
async fn archive(
    State(api): State<ApiState>,
    user: UserIdentity,
    dashboard_id: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
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
        (status = 403, description = "No tenant, or a viewer (`permission_denied`, `dashboard.editor_required`).", body = ErrorBody),
        (status = 404, description = "No such dashboard the caller can see (`not_found`).", body = ErrorBody),
        (status = 422, description = "A draft (`validation_failed`, `dashboard.saved_required`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`). Nothing was written.", body = ErrorBody),
    )
)]
async fn unarchive(
    State(api): State<ApiState>,
    user: UserIdentity,
    dashboard_id: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    set_archived(api, user, dashboard_id, false).await
}

async fn set_archived(
    api: ApiState,
    user: UserIdentity,
    dashboard_id: Result<Path<String>, PathRejection>,
    archived: bool,
) -> Result<Response, ApiError> {
    let action = if archived {
        "archive a dashboard"
    } else {
        "unarchive a dashboard"
    };
    let requester = requester(&user, action)?;
    let dashboard_id: DashboardId = parse_id(dashboard_id)?;
    let mut tx = begin(&api.db, &user).await?;
    let changed = store::set_archived(&mut tx, requester, dashboard_id, archived).await;
    let message = "could not archive a dashboard";
    let state = finish(tx, &user, changed, Some(dashboard_id), message).await?;
    tracing::info!(
        user_id = %requester.principal.user_id,
        tenant_id = %requester.principal.tenant_id,
        dashboard_id = %dashboard_id,
        archived = state.archived_at.is_some(),
        "changed whether a dashboard is archived"
    );
    Ok(ok(StatusCode::OK, &state))
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
    State(api): State<ApiState>,
    user: UserIdentity,
    dashboard_id: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let requester = requester(&user, "run a dashboard's dataset")?;
    let dashboard_id: DashboardId = parse_id(dashboard_id)?;
    let request: DatasetRunRequest = read_body(&headers, body).await?;
    // The transaction ends before the run calls telemetry-query: no connection is held then.
    let mut tx = begin(&api.db, &user).await?;
    let dataset = store::dataset(
        &mut tx,
        requester.principal,
        dashboard_id,
        request.version,
        &request.dataset,
    )
    .await;
    let message = "could not read a dashboard's dataset";
    let Some(dataset) = finish(tx, &user, dataset, Some(dashboard_id), message).await? else {
        return Err(ErrorBody::new(ErrorCode::ValidationFailed)
            .with_message_key(DATASET_UNKNOWN)
            .into());
    };
    let Some(on_behalf) = &api.on_behalf else {
        tracing::warn!(
            tenant_id = %requester.principal.tenant_id,
            "a dataset run cannot call telemetry-query: identity.exchange_url is not set"
        );
        return Err(ErrorBody::new(ErrorCode::Unavailable)
            .with_message_key(DATASET_FAILED)
            .into());
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
    // The answer's body holds the result's memory until its last piece goes to the connection,
    // and cuts the answer off when the connection has not taken it whole within the bound.
    match api.runner.run(&caller, &dataset).await {
        Ok(result) => Ok(held_result(
            result,
            requester.principal.tenant_id,
            api.held_body_timeout,
        )),
        Err(failure) => Ok(run_error(&failure)),
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

/// Ends the request's transaction: commits it after `outcome` succeeded, rolls it back (drops
/// it) and answers the store's failure otherwise.
async fn finish<T>(
    tx: TenantTransaction,
    user: &UserIdentity,
    outcome: Result<T, StoreError>,
    dashboard_id: Option<DashboardId>,
    message: &'static str,
) -> Result<T, ApiError> {
    let value = outcome.map_err(|failure| store_error(user, dashboard_id, &failure, message))?;
    commit(tx, user).await?;
    Ok(value)
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
fn list_cursor(text: Option<&str>) -> Result<Option<ListCursor>, ApiError> {
    match text {
        None => Ok(None),
        Some(text) => ListCursor::parse(text)
            .map(Some)
            .ok_or_else(invalid_request),
    }
}

fn invalid_request() -> ApiError {
    ApiError(ErrorBody::new(ErrorCode::InvalidRequest))
}

/// The answer to a store failure; failures that are not the request's fault are logged with
/// their kind only.
fn store_error(
    user: &UserIdentity,
    dashboard_id: Option<DashboardId>,
    failure: &StoreError,
    message: &'static str,
) -> ApiError {
    let keyed = |code, key| ErrorBody::new(code).with_message_key(key);
    let body = match failure {
        StoreError::NotFound => ErrorBody::new(ErrorCode::NotFound),
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
                error_kind = failure.log_kind(),
                "{message}"
            );
            ErrorBody::new(ErrorCode::Internal)
        }
        StoreError::Corrupt(_) | StoreError::Database(_) => {
            tracing::error!(
                user_id = %user.user_id(),
                dashboard_id = dashboard_id.map(|id| id.to_string()).as_deref(),
                error_kind = failure.log_kind(),
                "{message}"
            );
            ErrorBody::new(ErrorCode::Unavailable)
        }
    };
    ApiError(body)
}
