//! Events the identity service produces.

use serde::{Deserialize, Serialize};

use crate::event::Event;
use crate::id::{TenantId, UserId};
use crate::identity::TenantRole;
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

/// The aggregate kind of membership events; the aggregate ID is [`membership_aggregate_id`]: the
/// tenant ID and the user ID, so one user's memberships in different tenants are different
/// aggregates (an aggregate belongs to the tenant of its first event, crates/outbox).
pub const MEMBERSHIP_AGGREGATE: &str = "membership";

/// The aggregate ID of the membership of `user` in `tenant`: `<tenant ID>:<user ID>`.
#[must_use]
pub fn membership_aggregate_id(tenant: TenantId, user: UserId) -> String {
    format!("{tenant}:{user}")
}

/// A member's role in a tenant was set: a new membership, or a different role for an existing
/// one (Laysi/trustiot-v2#583). Setting the role a member already has writes no event.
///
/// The identity service writes it in the transaction that changes the membership, in that
/// tenant's row-level security context, so the envelope's `tenant_id` is the membership's tenant
/// and consumers take the tenant from there, never from the payload. The envelope's `actor` is
/// who set the role: a platform administrator through the back office (Laysi/trustiot-v2#583),
/// later also a tenant administrator (Laysi/trustiot-v2#105). The aggregate is
/// [`MEMBERSHIP_AGGREGATE`] with [`membership_aggregate_id`], so a membership's events arrive in
/// the order they were written.
///
/// The payload names the user by ID only: the email address is personal data and stays with the
/// identity service, as it does in the operation log that records this event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberRoleChanged {
    /// The member.
    pub user_id: UserId,
    /// The role before the change; `None` when the user was not a member.
    #[serde(default)]
    pub previous_role: Option<TenantRole>,
    /// The role now: `viewer`, `editor` or `admin`.
    pub role: TenantRole,
}

impl Event for MemberRoleChanged {
    const TYPE: &'static str = "identity.member_role_changed";
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
