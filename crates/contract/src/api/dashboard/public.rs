//! The bodies of the assistant's public dashboard API (`/api/assistant/v1`, design record
//! 「公開 API」), and its error message keys.
//!
//! | Endpoint | Request | Response | Who |
//! | --- | --- | --- | --- |
//! | `GET /dashboards` (`?include_archived=true`, `?cursor=`) | | [`DashboardList`] of saved dashboards | viewer and up |
//! | `GET /conversations/{conversation_id}/dashboards` | | [`DashboardList`] of the conversation's drafts | the conversation's owner; its shared readers |
//! | `GET /dashboards/{dashboard_id}` (`?version=`, the latest without it) | | [`DashboardView`] | whoever can see it |
//! | `GET /dashboards/{dashboard_id}/versions` (`?cursor=`) | | [`VersionList`], newest first | whoever can see it |
//! | `POST /dashboards/{dashboard_id}/publish` (a draft) | [`PublishRequest`] | [`VersionCreated`] in the saved dashboard | editor and up who own the conversation |
//! | `POST /dashboards/{dashboard_id}/restore` | [`RestoreRequest`] | [`VersionCreated`] | a saved dashboard: editor and up; a draft: its conversation's owner, any role |
//! | `PUT` and `DELETE /dashboards/{dashboard_id}/archive` (a saved one) | | [`ArchiveState`] | editor and up |
//! | `POST /dashboards/{dashboard_id}/dataset-runs` | [`DatasetRunRequest`] | [`DatasetResult`](super::result::DatasetResult) | whoever can see it, run as the caller |
//!
//! No request carries SQL, a semantic request or a page: every request type here refuses fields
//! it does not know, and none has a field for them, so a browser cannot write what runs. Every
//! response is `cache-control: no-store`; a dashboard the caller cannot see is the same 404 as
//! one that does not exist. The role comes from the request's identity on every request.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::RuntimeVersion;
use super::spec::Spec;
use crate::error::MessageKey;
use crate::{DashboardId, UserId};

/// Whether a dashboard is a conversation's draft or a saved one in the tenant's list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = DashboardKind)]
pub enum DashboardKind {
    /// Written by the `dashboard` tool in one conversation; seen by its owner and the
    /// conversation's shared readers; removed with the conversation.
    Draft,
    /// Published by an editor or admin; seen by every member of the tenant.
    Saved,
}

/// One dashboard in a list, and the dashboard part of a [`DashboardView`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct DashboardSummary {
    pub dashboard_id: DashboardId,
    pub kind: DashboardKind,
    /// The latest version's title.
    pub title: String,
    pub latest_version: u32,
    /// Who made the dashboard: a draft's conversation owner, or a saved one's first publisher.
    pub created_by: UserId,
    pub created_at: DateTime<Utc>,
    /// When the latest version was written.
    pub updated_at: DateTime<Utc>,
    /// When it was archived; absent while it is not. Only saved dashboards are archived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<DateTime<Utc>>,
    /// A draft's conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<Uuid>,
    /// A draft's saved dashboard: the one it was taken from or published to, and that
    /// dashboard's version it is based on. Publishing is refused when the saved dashboard has a
    /// newer version than this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved: Option<SavedBase>,
}

/// The saved dashboard a draft is based on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[schema(as = DashboardSavedBase)]
pub struct SavedBase {
    pub dashboard_id: DashboardId,
    pub version: u32,
}

/// A page of dashboards, the most recently updated first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct DashboardList {
    pub dashboards: Vec<DashboardSummary>,
    /// The `cursor` for the next page; absent on the last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// How a version came to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = DashboardVersionOrigin)]
pub enum VersionOrigin {
    /// The `dashboard` tool wrote it into a draft, after the check passed.
    Agent,
    /// A user published a draft's version into the saved dashboard.
    Publish,
    /// A user restored an older version as a new one.
    Restore,
}

/// One version of a dashboard, whole. Versions are only ever added.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[schema(as = DashboardVersion)]
pub struct VersionContent {
    pub version: u32,
    /// The runtime the page was checked with and runs with; a restored or published version
    /// keeps its source's.
    pub runtime: RuntimeVersion,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub spec: Spec,
    /// The page: an HTML fragment with its scripts, run only inside the sandbox
    /// (`frontend/sandbox/README.md`).
    pub page: String,
    pub origin: VersionOrigin,
    /// For [`VersionOrigin::Restore`], the version restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored_from: Option<u32>,
    /// Who asked for it: the conversation's owner for [`VersionOrigin::Agent`], the user who
    /// published or restored otherwise.
    pub created_by: UserId,
    pub created_at: DateTime<Utc>,
}

/// One version in a [`VersionList`]: without its spec and page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[schema(as = DashboardVersionSummary)]
pub struct VersionSummary {
    pub version: u32,
    pub runtime: RuntimeVersion,
    pub title: String,
    pub origin: VersionOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored_from: Option<u32>,
    pub created_by: UserId,
    pub created_at: DateTime<Utc>,
}

/// A page of versions, the newest first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[schema(as = DashboardVersionList)]
pub struct VersionList {
    pub versions: Vec<VersionSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// What the caller may do with the dashboard, decided by the server from the request's role
/// and the caller's relation to the dashboard; the UI shows only the actions allowed here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[schema(as = DashboardPermissions)]
pub struct Permissions {
    /// A draft, and the caller is an editor or admin who owns its conversation.
    pub can_publish: bool,
    /// A saved dashboard and the caller is an editor or admin; or a draft and the caller owns
    /// its conversation.
    pub can_restore: bool,
    /// A saved dashboard and the caller is an editor or admin.
    pub can_archive: bool,
}

/// `GET /dashboards/{dashboard_id}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct DashboardView {
    pub dashboard: DashboardSummary,
    pub version: VersionContent,
    pub permissions: Permissions,
}

/// Publishes a draft's version: the first time as a new saved dashboard, then as a new version
/// of the saved dashboard the draft is based on (refused with 409 `dashboard.saved_changed`
/// when that one has a newer version than the draft's base).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DashboardPublishRequest)]
pub struct PublishRequest {
    /// The draft's version the user looked at; a newer one the tool wrote since is not
    /// published by this request.
    pub version: u32,
}

/// Restores an older version as a new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DashboardRestoreRequest)]
pub struct RestoreRequest {
    /// The version to restore.
    pub from_version: u32,
    /// The latest version the user saw; refused with 409 `dashboard.version_conflict` when the
    /// dashboard has a newer one.
    pub base_version: u32,
}

/// A version written by publish or restore.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[schema(as = DashboardVersionCreated)]
pub struct VersionCreated {
    pub dashboard_id: DashboardId,
    pub version: u32,
}

/// Whether a saved dashboard is archived, after `PUT` or `DELETE …/archive`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[schema(as = DashboardArchiveState)]
pub struct ArchiveState {
    pub dashboard_id: DashboardId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<DateTime<Utc>>,
}

/// Runs one dataset of one version as the caller.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DashboardDatasetRunRequest)]
pub struct DatasetRunRequest {
    pub version: u32,
    /// The dataset's name in that version's spec.
    #[schema(pattern = "^[a-z][a-z0-9_]{0,47}$")]
    pub dataset: String,
}

/// 404 `not_found`: the dashboard is visible but has no such version. A dashboard the caller
/// cannot see keeps the generic `error.not_found`.
pub const VERSION_NOT_FOUND: MessageKey = MessageKey::new("dashboard.version_not_found");
/// 409 `conflict`: a restore's `base_version` is not the latest; details: `latest`.
pub const VERSION_CONFLICT: MessageKey = MessageKey::new("dashboard.version_conflict");
/// 409 `conflict`: publishing a draft whose saved dashboard has a newer version than the
/// draft's base; details: `latest`.
pub const SAVED_CHANGED: MessageKey = MessageKey::new("dashboard.saved_changed");
/// 409 `conflict`: publishing into, or restoring, an archived dashboard.
pub const ARCHIVED: MessageKey = MessageKey::new("dashboard.archived");
/// 403 `permission_denied`: publish, restore or archive of a saved dashboard by a viewer, or
/// publish by a viewer.
pub const EDITOR_REQUIRED: MessageKey = MessageKey::new("dashboard.editor_required");
/// 403 `permission_denied`: publishing or restoring a draft by someone other than its
/// conversation's owner (a shared reader).
pub const OWNER_REQUIRED: MessageKey = MessageKey::new("dashboard.owner_required");
/// 422 `validation_failed`: publish names a saved dashboard.
pub const DRAFT_REQUIRED: MessageKey = MessageKey::new("dashboard.draft_required");
/// 422 `validation_failed`: archive names a draft.
pub const SAVED_REQUIRED: MessageKey = MessageKey::new("dashboard.saved_required");
/// 422 `validation_failed`: the version has no dataset of that name.
pub const DATASET_UNKNOWN: MessageKey = MessageKey::new("dashboard.dataset_unknown");
/// 422 `validation_failed`: the run returned more rows than the dataset's `max_rows`;
/// details: `max`. The message says how to narrow it (ask the assistant for a shorter range
/// or a longer interval).
pub const DATASET_TOO_MANY_ROWS: MessageKey = MessageKey::new("dashboard.dataset_too_many_rows");
/// 422 `validation_failed`: the run's result is larger than `result::MAX_RESULT_BYTES`.
pub const DATASET_RESULT_TOO_LARGE: MessageKey =
    MessageKey::new("dashboard.dataset_result_too_large");
/// 422 `validation_failed`: telemetry-query now refuses the stored query (its rules changed
/// since the check), or the result has a column a dashboard cannot use.
pub const DATASET_REFUSED: MessageKey = MessageKey::new("dashboard.dataset_refused");
/// 504 `timeout`: the dataset's query ran longer than the tenant's limit.
pub const DATASET_TIMEOUT: MessageKey = MessageKey::new("dashboard.dataset_timeout");
/// 503 `unavailable`: the run failed for a reason the user cannot change.
pub const DATASET_FAILED: MessageKey = MessageKey::new("dashboard.dataset_failed");
/// 429 `rate_limited`: the tenant ran 60 datasets in the last minute, or 2 are running; the
/// response carries `Retry-After`.
pub const RUNS_LIMITED: MessageKey = MessageKey::new("dashboard.runs_limited");
/// 429 `rate_limited`: the tenant's ClickHouse quota or concurrent queries are used up.
pub const QUERY_QUOTA_EXHAUSTED: MessageKey = MessageKey::new("dashboard.query_quota_exhausted");

/// Every key of this API.
pub const MESSAGE_KEYS: [MessageKey; 16] = [
    VERSION_NOT_FOUND,
    VERSION_CONFLICT,
    SAVED_CHANGED,
    ARCHIVED,
    EDITOR_REQUIRED,
    OWNER_REQUIRED,
    DRAFT_REQUIRED,
    SAVED_REQUIRED,
    DATASET_UNKNOWN,
    DATASET_TOO_MANY_ROWS,
    DATASET_RESULT_TOO_LARGE,
    DATASET_REFUSED,
    DATASET_TIMEOUT,
    DATASET_FAILED,
    RUNS_LIMITED,
    QUERY_QUOTA_EXHAUSTED,
];

/// The telemetry-query keys a dataset run passes on as they are, with their status and
/// details; every other error of telemetry-query becomes [`DATASET_FAILED`] (or
/// [`DATASET_REFUSED`], [`DATASET_TIMEOUT`], [`QUERY_QUOTA_EXHAUSTED`] by its code), and no
/// text of it is passed on. `telemetry.account_not_ready` stays a 503: in this API a 404 means
/// a dashboard the caller cannot see.
pub const FORWARDED_TELEMETRY_KEYS: [&str; 4] = [
    "telemetry.outside_queryable_period",
    "telemetry.query_too_heavy",
    "telemetry.too_many_points",
    "telemetry.account_not_ready",
];

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn requests_refuse_content_fields() {
        for (body, field) in [
            (json!({"version": 3, "page": "<p></p>"}), "page"),
            (json!({"version": 3, "spec": {}}), "spec"),
            (json!({"version": 3, "sql": "SELECT 1"}), "sql"),
        ] {
            assert!(
                serde_json::from_value::<PublishRequest>(body).is_err(),
                "publish accepted {field}"
            );
        }
        assert!(
            serde_json::from_value::<RestoreRequest>(
                json!({"from_version": 1, "base_version": 3, "page": "x"})
            )
            .is_err()
        );
        for extra in [
            json!({"sql": "SELECT 1"}),
            json!({"request": {"series": []}}),
            json!({"source": {"kind": "sql", "sql": "SELECT 1"}}),
            json!({"max_rows": 10}),
        ] {
            let mut body = json!({"version": 1, "dataset": "daily"});
            if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
                body.extend(extra.clone());
            }
            assert!(
                serde_json::from_value::<DatasetRunRequest>(body.clone()).is_err(),
                "{body}"
            );
        }
        assert_eq!(
            serde_json::from_value::<DatasetRunRequest>(json!({"version": 1, "dataset": "daily"}))
                .expect("request"),
            DatasetRunRequest {
                version: 1,
                dataset: "daily".to_owned()
            }
        );
    }

    #[test]
    fn keys_are_distinct_and_namespaced() {
        let all = MESSAGE_KEYS;
        let keys: std::collections::BTreeSet<&str> = all.iter().map(MessageKey::as_str).collect();
        assert_eq!(keys.len(), MESSAGE_KEYS.len());
        assert!(keys.iter().all(|key| key.starts_with("dashboard.")));
    }

    #[test]
    fn optional_fields_are_left_out() {
        let summary = DashboardSummary {
            dashboard_id: DashboardId::from_uuid(Uuid::nil()),
            kind: DashboardKind::Saved,
            title: "Energy".to_owned(),
            latest_version: 2,
            created_by: UserId::from_uuid(Uuid::nil()),
            created_at: DateTime::<Utc>::MIN_UTC,
            updated_at: DateTime::<Utc>::MIN_UTC,
            archived_at: None,
            conversation_id: None,
            saved: None,
        };
        let value = serde_json::to_value(&summary).expect("serialize");
        for field in ["archived_at", "conversation_id", "saved"] {
            assert!(value.get(field).is_none(), "{field}");
        }
        assert_eq!(
            serde_json::from_value::<DashboardSummary>(value).expect("deserialize"),
            summary
        );
    }
}
