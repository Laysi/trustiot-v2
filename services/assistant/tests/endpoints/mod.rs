//! Every operation of the assistant's client API, declared by resource (Laysi/trustiot-v2#1095):
//! who may call it, for the permission matrix (`tests/permissions.rs`,
//! docs/architecture/permissions.md, section `## assistant`), and how the cross-tenant suite
//! probes it or why it does not (`tests/cross_tenant.rs`). Each file mirrors the resource's
//! module under `src/api/`, which lists the same operations in its OpenAPI paths, so a new
//! endpoint changes its resource's module and its file here, nothing shared; the functions below
//! only join the resources, in the order of the matrix document's table. Both suites hold the
//! declarations to the OpenAPI document, so an operation that no resource declares fails them.
#![allow(
    dead_code,
    reason = "each test binary includes this module and uses one of its two declarations"
)]

mod answer_stream;
mod conversations;
mod dashboards;
mod questions;
mod sharing;

use contract::TenantRole;
use testkit::isolation;
use testkit::permissions::{self, Access, Operation};

/// The conversations' table, which the conversation, question and sharing endpoints serve.
const CONVERSATIONS: &str = "assistant.conversations";

/// Every operation's requirement, resource by resource.
pub fn permissions(suite: permissions::Suite<'_>) -> permissions::Suite<'_> {
    let suite = conversations::permissions(suite);
    let suite = questions::permissions(suite);
    let suite = answer_stream::permissions(suite);
    let suite = sharing::permissions(suite);
    dashboards::permissions(suite)
}

/// Every operation's cross-tenant probe or exemption, resource by resource.
pub fn cross_tenant<S: isolation::Store>(
    suite: isolation::Suite<'_, S>,
) -> isolation::Suite<'_, S> {
    let suite = conversations::cross_tenant(suite);
    let suite = questions::cross_tenant(suite);
    let suite = answer_stream::cross_tenant(suite);
    let suite = sharing::cross_tenant(suite);
    dashboards::cross_tenant(suite)
}

/// `METHOD path` for any role of the tenant.
fn viewer(method: &str, path: &str) -> Operation {
    Operation::new(
        &format!("{method} {path}"),
        Access::Tenant(TenantRole::Viewer),
    )
}

/// `METHOD path` for an editor of the tenant or an admin.
fn editor(method: &str, path: &str) -> Operation {
    Operation::new(
        &format!("{method} {path}"),
        Access::Tenant(TenantRole::Editor),
    )
}
