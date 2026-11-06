//! The conversations ([`assistant::api::conversations`]).

use assistant::api::conversations::{
    CONVERSATION_PATH, CONVERSATIONS_PATH, DELETED_CONVERSATIONS_PATH, RESTORE_PATH,
};
use serde_json::json;
use testkit::isolation::api::{Endpoint, Method};
use testkit::isolation::{self, Store};
use testkit::permissions;

use super::{CONVERSATIONS, viewer};

pub(super) fn permissions(suite: permissions::Suite<'_>) -> permissions::Suite<'_> {
    suite
        .operation(viewer("GET", CONVERSATIONS_PATH))
        .operation(viewer("POST", CONVERSATIONS_PATH))
        .operation(viewer("GET", CONVERSATION_PATH))
        .operation(viewer("PATCH", CONVERSATION_PATH))
        .operation(viewer("DELETE", CONVERSATION_PATH))
        .operation(viewer("POST", RESTORE_PATH))
        .operation(viewer("GET", DELETED_CONVERSATIONS_PATH))
}

pub(super) fn cross_tenant<S: Store>(suite: isolation::Suite<'_, S>) -> isolation::Suite<'_, S> {
    suite
        .endpoint(Endpoint::list(CONVERSATIONS_PATH, CONVERSATIONS))
        .endpoint(Endpoint::get(CONVERSATION_PATH, CONVERSATIONS))
        .endpoint(
            Endpoint::item(Method::PATCH, CONVERSATION_PATH, CONVERSATIONS)
                .body(json!({"title": "{canary}", "archived": true})),
        )
        .endpoint(Endpoint::item(
            Method::DELETE,
            CONVERSATION_PATH,
            CONVERSATIONS,
        ))
        // Starting a conversation takes no tenant from the request: the token's tenant is the
        // only one it can write.
        .endpoint(Endpoint::create(Method::POST, CONVERSATIONS_PATH, CONVERSATIONS).body(json!({})))
        .exempt_endpoint(
            &format!("GET {DELETED_CONVERSATIONS_PATH}"),
            "lists only deleted conversations, which the suite's rows are not; \
             deleted_conversations_stay_in_their_tenant in tests/cross_tenant.rs probes it \
             across tenants",
        )
        .exempt_endpoint(
            &format!("POST {RESTORE_PATH}"),
            "restores only deleted conversations, which the suite's rows are not; \
             deleted_conversations_stay_in_their_tenant in tests/cross_tenant.rs probes it \
             across tenants",
        )
}
