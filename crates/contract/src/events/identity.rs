//! Events the identity service produces.

use serde::{Deserialize, Serialize};

use crate::event::Event;
use crate::id::UserId;
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

/// The aggregate kind of user events; the aggregate ID is the user ID.
pub const USER_AGGREGATE: &str = "user";

/// A user became a platform administrator (A2, Laysi/trustiot-v2#582).
///
/// A platform-wide event: the envelope's `tenant_id` is null, since a platform administrator
/// belongs to no tenant (and gets no tenant's data by being one). The aggregate is
/// [`USER_AGGREGATE`] with the user ID. The payload names the user by ID only, never by email
/// address: the operation log keeps it for good.
///
/// Today one path grants it: `migrate bootstrap-platform-admin`, which an operator runs once to
/// create the first platform administrator ([`PlatformAdminGrant::Bootstrap`]). It writes the
/// event as the identity schema's owner, in the transaction that sets the user's flag, without
/// an `actor`: the operator who ran it is not a user of the platform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformAdminGranted {
    /// The user who is now a platform administrator.
    pub user_id: UserId,
    /// How the grant was made.
    pub via: PlatformAdminGrant,
}

impl Event for PlatformAdminGranted {
    const TYPE: &'static str = "identity.platform_admin_granted";
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Identity;
}

/// How a user became a platform administrator ([`PlatformAdminGranted::via`]). Only add
/// variants, never rename one: a consumer built before a variant existed decodes it as
/// [`PlatformAdminGrant::Unrecognized`] instead of failing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformAdminGrant {
    /// The one-time creation of the first platform administrator (`migrate
    /// bootstrap-platform-admin`, tools/migrate/README.md).
    Bootstrap,
    /// A way this build does not know, written by a newer producer. Decode only: serializing it
    /// fails, so it never reaches an outbox.
    #[serde(other, skip_serializing)]
    Unrecognized,
}
