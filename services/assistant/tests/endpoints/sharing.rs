//! Sharing, the "shared with me" list and copies ([`assistant::api::sharing`]).

use assistant::api::sharing::{
    COPY_PATH, SHARE_PATH, SHARED_CONVERSATION_PATH, SHARED_CONVERSATIONS_PATH,
};
use serde_json::json;
use testkit::isolation::api::{Endpoint, Method};
use testkit::isolation::{self, Store};
use testkit::permissions;

use super::{CONVERSATIONS, viewer};

pub(super) fn permissions(suite: permissions::Suite<'_>) -> permissions::Suite<'_> {
    suite
        .operation(viewer("PUT", SHARE_PATH))
        .operation(viewer("DELETE", SHARE_PATH))
        .operation(viewer("POST", COPY_PATH))
        .operation(viewer("GET", SHARED_CONVERSATIONS_PATH))
        .operation(viewer("GET", SHARED_CONVERSATION_PATH))
}

pub(super) fn cross_tenant<S: Store>(suite: isolation::Suite<'_, S>) -> isolation::Suite<'_, S> {
    suite
        // Sharing, unsharing and copying act on the caller's own conversation here (the suite's
        // rows are the one user's): in tenant B's row the same person gets the one 404.
        .endpoint(Endpoint::item(Method::PUT, SHARE_PATH, CONVERSATIONS))
        .endpoint(Endpoint::item(Method::DELETE, SHARE_PATH, CONVERSATIONS))
        .endpoint(Endpoint::item(Method::POST, COPY_PATH, CONVERSATIONS).body(json!({})))
        .exempt_endpoint(
            &format!("GET {SHARED_CONVERSATIONS_PATH}"),
            "lists only conversations that other users of the tenant shared, which the suite's \
             rows (one user's, not shared) are not; shared_conversations_stay_in_their_tenant in \
             tests/cross_tenant.rs probes it across tenants",
        )
        .exempt_endpoint(
            &format!("GET {SHARED_CONVERSATION_PATH}"),
            "serves only conversations that other users of the tenant shared, which the suite's \
             rows (one user's, not shared) are not; shared_conversations_stay_in_their_tenant in \
             tests/cross_tenant.rs probes it across tenants",
        )
}
