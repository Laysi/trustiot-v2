//! The dashboards, saved and drafts ([`assistant::api::dashboards`]).

use assistant::api::dashboards::{
    ARCHIVE_PATH, CONVERSATION_DASHBOARDS_PATH, DASHBOARD_PATH, DASHBOARDS_PATH, DATASET_RUNS_PATH,
    PUBLISH_PATH, RESTORE_PATH, VERSIONS_PATH,
};
use serde_json::json;
use testkit::isolation::api::{Endpoint, Method};
use testkit::isolation::{self, Store};
use testkit::permissions;

use super::{editor, viewer};

/// The saved dashboards, every member's of the tenant.
const SAVED: &str = "assistant.dashboards";

/// The dashboards' versions; a draft is its conversation's owner's.
const DRAFTS: &str = "assistant.dashboard_versions";

/// Every role may read, restore and run; publishing and archiving need editor.
pub(super) fn permissions(suite: permissions::Suite<'_>) -> permissions::Suite<'_> {
    suite
        .operation(viewer("GET", CONVERSATION_DASHBOARDS_PATH))
        .operation(viewer("GET", DASHBOARDS_PATH))
        .operation(viewer("GET", DASHBOARD_PATH))
        .operation(viewer("GET", VERSIONS_PATH))
        .operation(viewer("POST", DATASET_RUNS_PATH))
        .operation(viewer("POST", RESTORE_PATH))
        .operation(editor("POST", PUBLISH_PATH))
        .operation(editor("PUT", ARCHIVE_PATH))
        .operation(editor("DELETE", ARCHIVE_PATH))
}

pub(super) fn cross_tenant<S: Store>(suite: isolation::Suite<'_, S>) -> isolation::Suite<'_, S> {
    suite
        // The saved dashboards, every member's of the tenant.
        .endpoint(Endpoint::list(DASHBOARDS_PATH, SAVED))
        .endpoint(Endpoint::get(DASHBOARD_PATH, SAVED))
        .endpoint(Endpoint::get(VERSIONS_PATH, SAVED))
        .endpoint(
            Endpoint::item(Method::POST, RESTORE_PATH, SAVED)
                .body(json!({"from_version": 1, "base_version": 2})),
        )
        .endpoint(Endpoint::item(Method::PUT, ARCHIVE_PATH, SAVED))
        .endpoint(Endpoint::item(Method::DELETE, ARCHIVE_PATH, SAVED))
        .endpoint(
            Endpoint::item(Method::POST, DATASET_RUNS_PATH, SAVED)
                .body(json!({"version": 1, "dataset": "testkit"})),
        )
        // A draft, its conversation's owner's.
        .endpoint(Endpoint::item(Method::POST, PUBLISH_PATH, DRAFTS).body(json!({"version": 2})))
        .exempt_endpoint(
            &format!("GET {CONVERSATION_DASHBOARDS_PATH}"),
            "a conversation's drafts: a list whose path parameter is the conversation's ID, which \
             the suite cannot address; a_conversations_drafts_stay_in_their_tenant in \
             tests/cross_tenant.rs probes it across tenants",
        )
}
