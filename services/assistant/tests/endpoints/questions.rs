//! Asking, stopping and regenerating ([`assistant::api::questions`]).

use assistant::api::questions::{QUESTIONS_PATH, REGENERATE_PATH, STOP_PATH};
use testkit::isolation::{self, Store};
use testkit::permissions;

use super::viewer;

pub(super) fn permissions(suite: permissions::Suite<'_>) -> permissions::Suite<'_> {
    suite
        .operation(viewer("POST", QUESTIONS_PATH))
        .operation(viewer("POST", STOP_PATH))
        .operation(viewer("POST", REGENERATE_PATH))
}

pub(super) fn cross_tenant<S: Store>(suite: isolation::Suite<'_, S>) -> isolation::Suite<'_, S> {
    suite
        .exempt_endpoint(
            &format!("POST {QUESTIONS_PATH}"),
            "starts a question in the background, which calls the identity service and the AI \
             gateway the suite does not stand in for; questions_and_stops_stay_in_their_tenant \
             in tests/questions.rs probes it across tenants",
        )
        .exempt_endpoint(
            &format!("POST {STOP_PATH}"),
            "two path parameters, a conversation's and its run's, which the suite cannot \
             address; questions_and_stops_stay_in_their_tenant in tests/questions.rs probes it \
             across tenants",
        )
        .exempt_endpoint(
            &format!("POST {REGENERATE_PATH}"),
            "discards the last round and asks its question again in the background, which calls \
             the identity service and the AI gateway the suite does not stand in for; \
             regenerations_stay_in_their_tenant in tests/regenerate.rs probes it across tenants",
        )
}
