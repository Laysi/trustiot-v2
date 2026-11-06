//! A question's answer stream ([`assistant::api::answer_stream`]).

use assistant::api::answer_stream::STREAM_PATH;
use testkit::isolation::{self, Store};
use testkit::permissions;

use super::viewer;

pub(super) fn permissions(suite: permissions::Suite<'_>) -> permissions::Suite<'_> {
    suite.operation(viewer("GET", STREAM_PATH))
}

pub(super) fn cross_tenant<S: Store>(suite: isolation::Suite<'_, S>) -> isolation::Suite<'_, S> {
    suite.exempt_endpoint(
        &format!("GET {STREAM_PATH}"),
        "two path parameters, a conversation's and its run's, which the suite cannot address; \
         only_the_owner_streams_and_a_deleted_conversation_is_not_found in \
         tests/answer_stream.rs probes it across tenants",
    )
}
