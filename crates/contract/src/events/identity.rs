//! Events the identity service produces.

use serde::{Deserialize, Serialize};

use crate::event::Event;
use crate::service::ServiceName;

/// The aggregate kind of tenant events; the aggregate ID is the tenant ID.
pub const TENANT_AGGREGATE: &str = "tenant";

/// A platform administrator created a tenant (Laysi/trustiot-v2#65).
///
/// The identity service writes it in the transaction that inserts the tenant, in that tenant's
/// row-level security context, so the envelope's `tenant_id` is the new tenant: the outbox's
/// policy checked it when the event was written, and consumers take the tenant from there (the
/// delivery's transaction is in it), never from the payload. Services that keep per-tenant
/// state create it when they see this event. The aggregate is [`TENANT_AGGREGATE`] with the
/// tenant ID.
///
/// The payload is empty on purpose: the tenant's name and other details stay with the identity
/// service, their only owner, and a consumer that needs them asks it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantCreated {}

impl Event for TenantCreated {
    const TYPE: &'static str = "identity.tenant_created";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Identity;
}
