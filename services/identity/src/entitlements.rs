//! A tenant's plan entitlements (Laysi/trustiot-v2#111, checklist B3; services/identity/README.md,
//! "方案權益"; docs/product/plans.md, 「權益清單」).
//!
//! Every tenant has a [`Plan`], the [`EntitlementOverrides`] of its contract (Enterprise plans
//! only) and the resulting [`Entitlements`]: the plan's values ([`Plan::defaults`], the example
//! and provisional values of docs/product/plans.md) with the overrides applied. The service works
//! the snapshot out when it writes it, stores it with a version, and writes the
//! `identity.tenant_entitlements_changed` event (`contract::events::identity::
//! TenantEntitlementsChanged`, the snapshot and its version) in the same transaction of that
//! tenant. Other services keep the snapshot with the larger version (crates/outbox, "通知模式")
//! and check it on their own side; they never call this service on a request path.
//!
//! - A new tenant gets Starter at version 1, with its event, in the transaction that creates it
//!   ([`initialize`], called by `crate::tenants`).
//! - A tenant created before entitlements existed (or by an older replica during a rollout) is
//!   at version 0 with no snapshot; [`backfill`], which every replica runs at start and every
//!   [`BACKFILL_PERIOD`], gives it its plan's snapshot at version 1 with an event whose actor is
//!   the service itself. Until then the APIs show the plan's current values at version 0.
//! - A platform administrator reads and sets a tenant's plan and overrides with
//!   `GET`/`PUT /api/identity/v1/platform/tenants/{tenant_id}/entitlements`. A `PUT` replaces
//!   both, and only while the tenant is still at the `entitlements_version` the administrator
//!   read (`expected_version`): two administrators who opened the same version cannot silently
//!   undo each other's change. The snapshot is worked out from the plan's values of this build,
//!   so putting the same plan again applies values a later release changed. A request that
//!   changes nothing writes nothing.
//! - A tenant's members read their own tenant's with `GET /api/identity/v1/entitlements`, for the
//!   upgrade walls the frontend shows.
//!
//! A plan's values changing in a release do not change existing tenants by themselves: their
//! snapshot stays what the last event carried until an administrator puts the plan again
//! (services/identity/README.md, "方案權益"). So no tenant's entitlements ever change without an
//! event, and consumers never see two values for one version.
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | Platform routes: not a platform administrator, or a token another service exchanged (`act`) | 403 `permission_denied`, decided from the token alone, before the body is read or the database touched |
//! | `GET /api/identity/v1/entitlements`: no tenant, or a role this build does not know | 403 `permission_denied`; a suspended or deleted tenant 403 `tenant_suspended` |
//! | A tenant ID that is not a UUID | 400 `invalid_request` |
//! | `PUT`: a `Content-Type` other than `application/json`, a body over [`MAX_BODY_BYTES`], or one that is not a JSON object with a string `plan`, an optional object or null `overrides`, an integer `expected_version` of at least 0 and no other field | 400 `invalid_request` |
//! | `PUT`: an unknown plan | 422 `validation_failed`, field `/plan`, key `entitlements.plan_invalid` |
//! | `PUT`: overrides that are not an object of known entitlements with values of their type | 422, field `/overrides`, key `entitlements.overrides_invalid` |
//! | `PUT`: overrides with a plan that is not an Enterprise plan | 422, field `/overrides`, key `entitlements.overrides_not_allowed` |
//! | `PUT`: an override out of range | 422, field `/overrides/<name>`, key `entitlements.override_out_of_range`, `{"min", "max"}` |
//! | No tenant has the ID (the tenant route: the token's tenant has no row) | 404 `not_found`; nothing was written |
//! | `PUT`: the tenant is no longer at `expected_version` | 409 `conflict`, key `entitlements.version_conflict`, `details.current_version`; nothing was written |
//! | The database failed or refused the transaction | 503 `unavailable`; nothing was written |
//! | `GET` | 200 |
//! | `PUT` that changes the plan, the overrides or the snapshot | 200 with the new version, and the event |
//! | `PUT` that changes nothing | 200 as it was; nothing was written, no event |
//!
//! Every response of the handlers (all but the identity extractor's 401) carries
//! `cache-control: no-store`. Concurrent requests for one tenant wait for each other on the row's
//! lock (`SELECT ... FOR NO KEY UPDATE`, at most [`LOCK_TIMEOUT`]).

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{FromRef, Path, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Utc};
use contract::entitlements::{
    Entitlements, Limit, MAX_QUERYABLE_MONTHS, MAX_RAW_QUERYABLE_DAYS, MAX_SAMPLING_INTERVAL_MS,
    MIN_SAMPLING_INTERVAL_MS,
};
use contract::event::Actor;
use contract::events::identity::{ENTITLEMENTS_AGGREGATE, TenantEntitlementsChanged};
use contract::{ErrorBody, ErrorCode, FieldError, MessageKey, ServiceName, TenantId, TenantRole};
use outbox::Aggregate;
use serde::{Deserialize, Serialize};
use service_kit::identity::{IdentityVerifier, Requirement, UserIdentity};
use service_kit::tenant::{
    BackgroundTenantDb, PlatformReader, TenantDb, TenantDbError, TenantTransaction,
};
use utoipa::ToSchema;

use crate::tenants::{error, is_json};

/// The platform administrators' route, as the ingress gateway forwards it.
pub const PLATFORM_ENTITLEMENTS_PATH: &str =
    "/api/identity/v1/platform/tenants/{tenant_id}/entitlements";

/// The tenant members' route: the token's tenant.
pub const ENTITLEMENTS_PATH: &str = "/api/identity/v1/entitlements";

/// The message key of a plan this API does not know.
pub const PLAN_INVALID: MessageKey = MessageKey::new("entitlements.plan_invalid");

/// The message key of overrides that are not an object of known entitlements with values of
/// their type.
pub const OVERRIDES_INVALID: MessageKey = MessageKey::new("entitlements.overrides_invalid");

/// The message key of overrides with a plan that takes none.
pub const OVERRIDES_NOT_ALLOWED: MessageKey = MessageKey::new("entitlements.overrides_not_allowed");

/// The message key of an override out of range; `details.min` and `details.max` are the range.
pub const OVERRIDE_OUT_OF_RANGE: MessageKey = MessageKey::new("entitlements.override_out_of_range");

/// The message key of a change that expected another version than the tenant has now;
/// `details.current_version` is the tenant's version.
pub const VERSION_CONFLICT: MessageKey = MessageKey::new("entitlements.version_conflict");

/// The largest body read. The body is read only after the permission check.
pub const MAX_BODY_BYTES: usize = 8192;

/// The longest a change waits for the tenant's row lock; past it the request answers 503.
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// How often every replica looks for tenants without a snapshot ([`backfill`]). A tenant an older
/// replica creates during a rollout gets its first snapshot within this period.
pub const BACKFILL_PERIOD: Duration = Duration::from_secs(600);

/// The most tenants one [`backfill`] pass reads; a later pass takes the rest.
pub const BACKFILL_BATCH: i64 = 500;

/// Names the platform role's reads of [`backfill`] in the kit's INFO line.
pub const BACKFILL_PURPOSE: &str = "entitlements-backfill";

/// Sets [`LOCK_TIMEOUT`] for the rest of the transaction, like `SET LOCAL`.
const SET_LOCK_TIMEOUT: &str = "SELECT set_config('lock_timeout', $1, true)";

/// Names the work in the kit's DEBUG log of the tenant transaction.
const READ_PURPOSE: &str = "read-tenant-entitlements";
const SET_PURPOSE: &str = "set-tenant-entitlements";
const BACKFILL_TENANT_PURPOSE: &str = "backfill-tenant-entitlements";

/// The tenant's entitlement columns; row-level security shows no other tenant's row.
const READ: &str = "SELECT plan, entitlement_overrides::text, entitlements::text,
    entitlements_version, entitlements_changed_at
FROM identity.tenants
WHERE tenant_id = $1::uuid";

/// The same, locked until the transaction ends. `NO KEY`: the update changes no key.
const LOCK: &str = "SELECT plan, entitlement_overrides::text, entitlements::text,
    entitlements_version, entitlements_changed_at
FROM identity.tenants
WHERE tenant_id = $1::uuid
FOR NO KEY UPDATE";

/// The new plan, overrides and snapshot of the locked row: the version goes up by exactly one
/// (the migration's trigger refuses anything else, and sets the time).
const UPDATE: &str = "UPDATE identity.tenants
SET plan = $2, entitlement_overrides = $3::jsonb, entitlements = $4::jsonb,
    entitlements_version = entitlements_version + 1
WHERE tenant_id = $1::uuid
RETURNING entitlements_version, entitlements_changed_at";

/// Tenants without a snapshot, for [`backfill`]; the platform role reads every tenant's row
/// (`platform_access`, 0007).
const WITHOUT_SNAPSHOT: &str = "SELECT tenant_id::text FROM identity.tenants
WHERE entitlements_version = 0
ORDER BY tenant_id
LIMIT $1";

/// A tenant's plan (docs/product/plans.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Plan {
    /// Small plants, one production line: one gateway, no alerts, short queryable periods.
    Starter,
    /// Mid-sized plants, several lines.
    Business,
    /// Groups with several plants, on the SaaS; limits by contract.
    Enterprise,
    /// Groups running the platform in their own data center: everything on by default, limits
    /// by contract, no technical licence check.
    EnterpriseOnPrem,
}

impl Plan {
    /// Every plan.
    pub const ALL: [Self; 4] = [
        Self::Starter,
        Self::Business,
        Self::Enterprise,
        Self::EnterpriseOnPrem,
    ];

    /// The plan `text` names exactly; `None` otherwise.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|plan| plan.as_str() == text)
    }

    /// The name, as the API, the event and the `plan` column write it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starter => "starter",
            Self::Business => "business",
            Self::Enterprise => "enterprise",
            Self::EnterpriseOnPrem => "enterprise_on_prem",
        }
    }

    /// The same plan in the event.
    #[must_use]
    pub const fn event(self) -> contract::entitlements::Plan {
        match self {
            Self::Starter => contract::entitlements::Plan::Starter,
            Self::Business => contract::entitlements::Plan::Business,
            Self::Enterprise => contract::entitlements::Plan::Enterprise,
            Self::EnterpriseOnPrem => contract::entitlements::Plan::EnterpriseOnPrem,
        }
    }

    /// Whether a tenant on this plan may have overrides: the Enterprise plans', set by contract.
    #[must_use]
    pub const fn takes_overrides(self) -> bool {
        matches!(self, Self::Enterprise | Self::EnterpriseOnPrem)
    }

    /// The plan's values: the example and provisional values of docs/product/plans.md
    /// (「權益清單」), which MS6 replaces. The document's table states these numbers and a unit
    /// test holds the two together.
    #[must_use]
    pub const fn defaults(self) -> Entitlements {
        match self {
            Self::Starter => Entitlements {
                max_gateways: Limit::AtMost(1),
                max_members: Limit::AtMost(3),
                max_alert_rules: Limit::AtMost(0),
                raw_queryable_days: 7,
                minute_queryable_months: 3,
                hour_queryable_months: 3,
                min_sampling_interval_ms: 60_000,
                realtime_charts: false,
                email_notifications: false,
                webhook_notifications: false,
                ai_alert_rules: false,
                field_context: false,
                document_rag: false,
                reports: false,
                api_and_mcp: false,
                sso: false,
            },
            Self::Business => Entitlements {
                max_gateways: Limit::AtMost(5),
                max_members: Limit::AtMost(20),
                max_alert_rules: Limit::AtMost(50),
                raw_queryable_days: 90,
                minute_queryable_months: 13,
                hour_queryable_months: 36,
                min_sampling_interval_ms: 10_000,
                realtime_charts: true,
                email_notifications: true,
                webhook_notifications: true,
                ai_alert_rules: true,
                field_context: true,
                document_rag: true,
                reports: true,
                api_and_mcp: false,
                sso: false,
            },
            Self::Enterprise => Entitlements {
                max_gateways: Limit::AtMost(20),
                max_members: Limit::AtMost(100),
                max_alert_rules: Limit::Unlimited,
                raw_queryable_days: 180,
                minute_queryable_months: 13,
                hour_queryable_months: 48,
                min_sampling_interval_ms: 1_000,
                realtime_charts: true,
                email_notifications: true,
                webhook_notifications: true,
                ai_alert_rules: true,
                field_context: true,
                document_rag: true,
                reports: true,
                api_and_mcp: true,
                sso: true,
            },
            Self::EnterpriseOnPrem => Entitlements {
                max_gateways: Limit::Unlimited,
                max_members: Limit::Unlimited,
                max_alert_rules: Limit::Unlimited,
                raw_queryable_days: MAX_RAW_QUERYABLE_DAYS,
                minute_queryable_months: MAX_QUERYABLE_MONTHS,
                hour_queryable_months: MAX_QUERYABLE_MONTHS,
                min_sampling_interval_ms: 1_000,
                realtime_charts: true,
                email_notifications: true,
                webhook_notifications: true,
                ai_alert_rules: true,
                field_context: true,
                document_rag: true,
                reports: true,
                api_and_mcp: true,
                sso: true,
            },
        }
    }
}

/// The entitlements a tenant's contract sets apart from its plan's values; only an Enterprise
/// plan takes any. Absent (or null) means the plan's value; unknown names are refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EntitlementOverrides {
    /// Gateways the tenant may have bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_gateways: Option<Limit>,
    /// Members the tenant may have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_members: Option<Limit>,
    /// Alert rules the tenant may have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_alert_rules: Option<Limit>,
    /// Raw readings' queryable period, in days (1 to 36500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_queryable_days: Option<u32>,
    /// One-minute aggregates' queryable period, in months (1 to 1200).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minute_queryable_months: Option<u32>,
    /// One-hour aggregates' queryable period, in months (1 to 1200).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hour_queryable_months: Option<u32>,
    /// The shortest sampling interval, in milliseconds (100 to 86400000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_sampling_interval_ms: Option<u32>,
    /// Real-time charts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realtime_charts: Option<bool>,
    /// Alert notifications by email.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email_notifications: Option<bool>,
    /// Alert notifications by webhook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_notifications: Option<bool>,
    /// The AI writes alert rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_alert_rules: Option<bool>,
    /// Field context and energy per piece.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_context: Option<bool>,
    /// Questions over the tenant's documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_rag: Option<bool>,
    /// Reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reports: Option<bool>,
    /// The public API and MCP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_and_mcp: Option<bool>,
    /// Single sign-on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sso: Option<bool>,
}

impl EntitlementOverrides {
    /// Whether no entitlement is overridden.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// `base` with these overrides applied: an override takes precedence over the plan's value.
    #[must_use]
    pub fn apply(&self, base: Entitlements) -> Entitlements {
        Entitlements {
            max_gateways: self.max_gateways.unwrap_or(base.max_gateways),
            max_members: self.max_members.unwrap_or(base.max_members),
            max_alert_rules: self.max_alert_rules.unwrap_or(base.max_alert_rules),
            raw_queryable_days: self.raw_queryable_days.unwrap_or(base.raw_queryable_days),
            minute_queryable_months: self
                .minute_queryable_months
                .unwrap_or(base.minute_queryable_months),
            hour_queryable_months: self
                .hour_queryable_months
                .unwrap_or(base.hour_queryable_months),
            min_sampling_interval_ms: self
                .min_sampling_interval_ms
                .unwrap_or(base.min_sampling_interval_ms),
            realtime_charts: self.realtime_charts.unwrap_or(base.realtime_charts),
            email_notifications: self.email_notifications.unwrap_or(base.email_notifications),
            webhook_notifications: self
                .webhook_notifications
                .unwrap_or(base.webhook_notifications),
            ai_alert_rules: self.ai_alert_rules.unwrap_or(base.ai_alert_rules),
            field_context: self.field_context.unwrap_or(base.field_context),
            document_rag: self.document_rag.unwrap_or(base.document_rag),
            reports: self.reports.unwrap_or(base.reports),
            api_and_mcp: self.api_and_mcp.unwrap_or(base.api_and_mcp),
            sso: self.sso.unwrap_or(base.sso),
        }
    }
}

/// The range of an entitlement that has one, by its name, as a refusal's `min` and `max`.
fn range_of(name: &str) -> Option<(u32, u32)> {
    match name {
        "raw_queryable_days" => Some((1, MAX_RAW_QUERYABLE_DAYS)),
        "minute_queryable_months" | "hour_queryable_months" => Some((1, MAX_QUERYABLE_MONTHS)),
        "min_sampling_interval_ms" => Some((MIN_SAMPLING_INTERVAL_MS, MAX_SAMPLING_INTERVAL_MS)),
        _ => None,
    }
}

/// The body of `PUT /api/identity/v1/platform/tenants/{tenant_id}/entitlements`: the plan and the
/// overrides, both replaced. A field this API does not know is refused, so a misspelt
/// `overrides` cannot clear the contract's overrides.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SetEntitlements {
    /// The plan.
    #[schema(value_type = Plan)]
    pub plan: String,
    /// The entitlements the contract sets apart from the plan's values; absent, null or `{}` for
    /// none. Only `enterprise` and `enterprise_on_prem` take any.
    #[serde(default)]
    #[schema(value_type = Option<EntitlementOverrides>)]
    pub overrides: Option<serde_json::Value>,
    /// The tenant's `entitlements_version` as the caller read it; the change is refused with 409
    /// when the tenant has another version by then.
    #[schema(minimum = 0)]
    pub expected_version: i64,
}

/// A tenant's plan, overrides and entitlements, for the back office.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TenantEntitlements {
    /// The tenant.
    pub tenant_id: TenantId,
    /// The plan.
    pub plan: Plan,
    /// The overrides of the tenant's contract (`{}` for none).
    pub overrides: EntitlementOverrides,
    /// What the tenant is entitled to: the plan's values with the overrides applied, as the last
    /// `identity.tenant_entitlements_changed` carried them. At version 0 (a tenant created before
    /// entitlements existed, not yet given its first snapshot) the plan's values of this build.
    pub entitlements: Entitlements,
    /// The snapshot's version: 1 for the first, one more with every change; the event of the
    /// change carries the same number. 0: no snapshot or event yet.
    pub entitlements_version: i64,
    /// When this version was written, by the database clock; null at version 0.
    pub entitlements_changed_at: Option<DateTime<Utc>>,
}

/// The entitlements of the caller's tenant, for its members.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CurrentEntitlements {
    /// The tenant: the token's.
    pub tenant_id: TenantId,
    /// The plan.
    pub plan: Plan,
    /// What the tenant is entitled to: the plan's values with the contract's overrides applied,
    /// as the last `identity.tenant_entitlements_changed` carried them.
    pub entitlements: Entitlements,
    /// The snapshot's version: 1 for the first, one more with every change. 0: the tenant has no
    /// snapshot yet, and `entitlements` are its plan's values.
    pub entitlements_version: i64,
}

/// What a request did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A new version and its event.
    Changed,
    /// The plan, the overrides and the snapshot were already these: nothing written.
    Unchanged,
}

/// Why entitlements were not read or set.
#[derive(Debug, thiserror::Error)]
pub enum EntitlementsError {
    /// The caller is not a platform administrator, or another service exchanged the token.
    #[error("only a platform administrator's own request may read or change a tenant's plan")]
    NotAllowed,
    /// No tenant has the ID.
    #[error("no tenant has this ID")]
    UnknownTenant,
    /// The tenant is no longer at the version the caller expected; nothing was written.
    #[error("the tenant's entitlements are at version {current}, not the expected one")]
    VersionConflict {
        /// The tenant's version now.
        current: i64,
    },
    /// A stored row breaks a rule of its table, or holds what this build cannot read.
    #[error("inconsistent tenant entitlements: {0}")]
    Inconsistent(&'static str),
    /// The transaction could not start, or a statement in it failed.
    #[error(transparent)]
    Database(#[from] TenantDbError),
    /// The event could not be written; the transaction was rolled back.
    #[error(transparent)]
    Publish(#[from] outbox::PublishError),
    /// The event's aggregate is not valid; the kind and a tenant ID always are.
    #[error(transparent)]
    Aggregate(#[from] outbox::InvalidAggregate),
}

impl From<sqlx::Error> for EntitlementsError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(TenantDbError::Database(error))
    }
}

impl EntitlementsError {
    /// A short description for the log: the SQLSTATE or the kind, never data.
    pub(crate) fn log_kind(&self) -> String {
        match self {
            Self::Database(TenantDbError::Database(error)) => crate::session::error_kind(error),
            Self::Database(error) => error.to_string(),
            Self::Publish(error) => error.to_string(),
            Self::Aggregate(error) => error.to_string(),
            Self::NotAllowed
            | Self::UnknownTenant
            | Self::VersionConflict { .. }
            | Self::Inconsistent(_) => self.to_string(),
        }
    }
}

/// A row of [`READ`] or [`LOCK`].
type Row = (String, String, Option<String>, i64, Option<DateTime<Utc>>);

/// A tenant's stored entitlement columns.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stored {
    plan: Plan,
    overrides: EntitlementOverrides,
    entitlements: Option<Entitlements>,
    version: i64,
    changed_at: Option<DateTime<Utc>>,
}

impl Stored {
    fn parse(row: Row) -> Result<Self, EntitlementsError> {
        let (plan, overrides, entitlements, version, changed_at) = row;
        let plan = Plan::parse(&plan).ok_or(EntitlementsError::Inconsistent("plan"))?;
        let overrides = serde_json::from_str(&overrides)
            .map_err(|_| EntitlementsError::Inconsistent("overrides"))?;
        let entitlements = entitlements
            .map(|text| serde_json::from_str::<Entitlements>(&text))
            .transpose()
            .map_err(|_| EntitlementsError::Inconsistent("entitlements"))?;
        if entitlements.is_none() != (version == 0) {
            return Err(EntitlementsError::Inconsistent("version"));
        }
        Ok(Self {
            plan,
            overrides,
            entitlements,
            version,
            changed_at,
        })
    }

    /// The snapshot, or at version 0 the plan's current values with the overrides.
    fn entitlements(&self) -> Entitlements {
        self.entitlements
            .unwrap_or_else(|| self.overrides.apply(self.plan.defaults()))
    }

    fn view(&self, tenant: TenantId) -> TenantEntitlements {
        TenantEntitlements {
            tenant_id: tenant,
            plan: self.plan,
            overrides: self.overrides,
            entitlements: self.entitlements(),
            entitlements_version: self.version,
            entitlements_changed_at: self.changed_at,
        }
    }
}

/// The plan and overrides a request asks for, checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    plan: Plan,
    overrides: EntitlementOverrides,
}

impl Target {
    /// The plan with the overrides, when the plan takes them and each value is in range;
    /// otherwise the refusal.
    ///
    /// # Errors
    ///
    /// The 422 body, as [`parse_body`] answers it.
    pub fn new(plan: Plan, overrides: EntitlementOverrides) -> Result<Self, ErrorBody> {
        if !overrides.is_empty() && !plan.takes_overrides() {
            return Err(ErrorBody::new(ErrorCode::ValidationFailed)
                .with_field_error(FieldError::new("/overrides", OVERRIDES_NOT_ALLOWED)));
        }
        // The plan's values are in range (a unit test holds that), so a value out of range is
        // an override.
        if let Some(name) = overrides.apply(plan.defaults()).out_of_range() {
            let (min, max) = range_of(name).unwrap_or((0, u32::MAX));
            return Err(
                ErrorBody::new(ErrorCode::ValidationFailed).with_field_error(
                    FieldError::new(format!("/overrides/{name}"), OVERRIDE_OUT_OF_RANGE)
                        .with_detail("min", min)
                        .with_detail("max", max),
                ),
            );
        }
        Ok(Self { plan, overrides })
    }

    fn entitlements(&self) -> Entitlements {
        self.overrides.apply(self.plan.defaults())
    }
}

/// Reads and sets tenants' entitlements for platform administrators, and gives tenants without
/// a snapshot their first: like [`crate::tenant_state::TenantStateStore`], code that opens a
/// transaction for a tenant a request names but its token does not. It holds the background
/// handle privately, and refuses every caller but a platform administrator's own request before
/// it opens a transaction for one (services/identity/README.md, "資料庫").
#[derive(Clone, Debug)]
pub struct EntitlementStore {
    db: BackgroundTenantDb,
}

impl EntitlementStore {
    /// A store on `db`, the background handle of the tenant store's pool.
    pub(crate) fn new(db: BackgroundTenantDb) -> Self {
        Self { db }
    }

    /// The entitlements of `tenant`, for the platform administrator `admin`.
    ///
    /// # Errors
    ///
    /// [`EntitlementsError::NotAllowed`] unless `admin` is a platform administrator's own
    /// request, before anything is read; [`EntitlementsError::UnknownTenant`]; otherwise when
    /// the database fails.
    pub async fn read(
        &self,
        admin: &UserIdentity,
        tenant: TenantId,
    ) -> Result<TenantEntitlements, EntitlementsError> {
        if admin.meets(Requirement::OwnPlatformAdmin).is_err() {
            return Err(EntitlementsError::NotAllowed);
        }
        let mut tx = self.db.begin(tenant, READ_PURPOSE).await?;
        let row: Option<Row> = sqlx::query_as(READ)
            .bind(tenant.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        tx.rollback().await?;
        Ok(Stored::parse(row.ok_or(EntitlementsError::UnknownTenant)?)?.view(tenant))
    }

    /// Gives `tenant` the plan and overrides of `target`, for the platform administrator
    /// `admin`, when the tenant is still at `expected_version` (the version `admin` read): in one
    /// transaction of `tenant`, the new snapshot with the next version and its event naming
    /// `admin`, unless the plan, the overrides and the snapshot were already these.
    ///
    /// # Errors
    ///
    /// [`EntitlementsError::NotAllowed`] unless `admin` is a platform administrator's own
    /// request, before anything is read; [`EntitlementsError::UnknownTenant`];
    /// [`EntitlementsError::VersionConflict`] when the locked row has another version, even when
    /// the change would write nothing; otherwise when the database fails. Nothing was written
    /// then.
    pub async fn set(
        &self,
        admin: &UserIdentity,
        tenant: TenantId,
        target: Target,
        expected_version: i64,
    ) -> Result<(TenantEntitlements, Outcome), EntitlementsError> {
        if admin.meets(Requirement::OwnPlatformAdmin).is_err() {
            return Err(EntitlementsError::NotAllowed);
        }
        let mut tx = self.db.begin(tenant, SET_PURPOSE).await?;
        let current = lock(&mut tx, tenant).await?;
        // Under the row's lock, so no other change can land between this check and the write.
        if current.version != expected_version {
            tx.rollback().await?;
            return Err(EntitlementsError::VersionConflict {
                current: current.version,
            });
        }
        let entitlements = target.entitlements();
        if current.plan == target.plan
            && current.overrides == target.overrides
            && current.entitlements == Some(entitlements)
        {
            tx.rollback().await?;
            return Ok((current.view(tenant), Outcome::Unchanged));
        }
        let changed = write(
            &mut tx,
            tenant,
            target,
            Actor::User {
                user_id: admin.user_id(),
            },
        )
        .await?;
        tx.commit().await?;
        tracing::info!(
            user_id = %admin.user_id(),
            tenant_id = %tenant,
            previous_plan = current.plan.as_str(),
            plan = changed.plan.as_str(),
            overridden = !changed.overrides.is_empty(),
            entitlements_version = changed.entitlements_version,
            "changed a tenant's entitlements"
        );
        Ok((changed, Outcome::Changed))
    }

    /// Gives `tenant` its first snapshot when it has none yet: its plan's values with its
    /// overrides at version 1, and the event, the service itself the actor. `false` when the
    /// tenant has a snapshot already (another replica's pass, or an administrator, was first) or
    /// no longer exists.
    ///
    /// # Errors
    ///
    /// When the database fails; nothing was written then.
    pub async fn backfill_tenant(&self, tenant: TenantId) -> Result<bool, EntitlementsError> {
        let mut tx = self.db.begin(tenant, BACKFILL_TENANT_PURPOSE).await?;
        let current = match lock(&mut tx, tenant).await {
            Ok(current) => current,
            Err(EntitlementsError::UnknownTenant) => {
                tx.rollback().await?;
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        if current.version != 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        let target = Target {
            plan: current.plan,
            overrides: current.overrides,
        };
        let changed = write(
            &mut tx,
            tenant,
            target,
            Actor::Service {
                service: ServiceName::Identity,
            },
        )
        .await?;
        tx.commit().await?;
        tracing::info!(
            tenant_id = %tenant,
            plan = changed.plan.as_str(),
            entitlements_version = changed.entitlements_version,
            "gave a tenant its first entitlements"
        );
        Ok(true)
    }
}

/// Sets the lock timeout and reads the tenant's locked row.
async fn lock(tx: &mut TenantTransaction, tenant: TenantId) -> Result<Stored, EntitlementsError> {
    sqlx::query(SET_LOCK_TIMEOUT)
        .bind(format!("{}ms", LOCK_TIMEOUT.as_millis()))
        .execute(&mut **tx)
        .await?;
    let row: Option<Row> = sqlx::query_as(LOCK)
        .bind(tenant.to_string())
        .fetch_optional(&mut **tx)
        .await?;
    Stored::parse(row.ok_or(EntitlementsError::UnknownTenant)?)
}

/// Writes `target` with the next version over the tenant's row and the event, in `tx`.
async fn write(
    tx: &mut TenantTransaction,
    tenant: TenantId,
    target: Target,
    actor: Actor,
) -> Result<TenantEntitlements, EntitlementsError> {
    let entitlements = target.entitlements();
    let overrides = serde_json::to_string(&target.overrides)
        .map_err(|_| EntitlementsError::Inconsistent("overrides"))?;
    let snapshot = serde_json::to_string(&entitlements)
        .map_err(|_| EntitlementsError::Inconsistent("entitlements"))?;
    let (version, changed_at): (i64, DateTime<Utc>) = sqlx::query_as(UPDATE)
        .bind(tenant.to_string())
        .bind(target.plan.as_str())
        .bind(overrides)
        .bind(snapshot)
        .fetch_one(&mut **tx)
        .await?;
    let aggregate = Aggregate::new(ENTITLEMENTS_AGGREGATE, tenant.to_string())?;
    outbox::publish_as(
        tx.transaction(),
        &aggregate,
        &TenantEntitlementsChanged {
            plan: target.plan.event(),
            entitlements,
            entitlements_version: version,
        },
        Some(tenant),
        actor,
    )
    .await?;
    Ok(TenantEntitlements {
        tenant_id: tenant,
        plan: target.plan,
        overrides: target.overrides,
        entitlements,
        entitlements_version: version,
        entitlements_changed_at: Some(changed_at),
    })
}

/// Gives a tenant just inserted in `tx` its first snapshot, Starter at version 1, and the event
/// naming `actor`: the tenant creation's part (`crate::tenants`), so a tenant never exists
/// without its entitlements.
///
/// # Errors
///
/// When the database fails; the caller rolls the transaction back.
pub(crate) async fn initialize(
    tx: &mut TenantTransaction,
    tenant: TenantId,
    actor: Actor,
) -> Result<TenantEntitlements, EntitlementsError> {
    let target = Target {
        plan: Plan::Starter,
        overrides: EntitlementOverrides::default(),
    };
    write(tx, tenant, target, actor).await
}

/// One pass of the backfill: reads, with the platform role, up to [`BACKFILL_BATCH`] tenants
/// without a snapshot and gives each its first ([`EntitlementStore::backfill_tenant`], in that
/// tenant's own transaction with the service role). Returns how many it gave one. Replicas may
/// run passes at the same time: the row lock and the version check make each tenant get exactly
/// one first snapshot.
///
/// # Errors
///
/// When the database fails; a later pass tries again.
pub async fn backfill(
    reader: &PlatformReader,
    store: &EntitlementStore,
) -> Result<usize, EntitlementsError> {
    let mut tx = reader.begin().await?;
    let tenants: Vec<String> = sqlx::query_scalar(WITHOUT_SNAPSHOT)
        .bind(BACKFILL_BATCH)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    let mut given = 0;
    for tenant in tenants {
        let tenant: TenantId = tenant
            .parse()
            .map_err(|_| EntitlementsError::Inconsistent("tenant id"))?;
        if store.backfill_tenant(tenant).await? {
            given += 1;
        }
    }
    Ok(given)
}

/// Names the periodic job in the kit's log (`service_kit::periodic`).
pub const BACKFILL_JOB: &str = "entitlements-backfill";

/// One logged [`backfill`] pass: INFO when it gave tenants their first snapshot, WARN when it
/// failed (before the migration Job has run, for one; the next pass tries again).
pub async fn backfill_once(reader: &PlatformReader, store: &EntitlementStore) {
    match backfill(reader, store).await {
        Ok(0) => {}
        Ok(given) => tracing::info!(tenants = given, "gave tenants their first entitlements"),
        Err(failure) => tracing::warn!(
            error_kind = failure.log_kind(),
            "could not give tenants their first entitlements; the next pass tries again"
        ),
    }
}

/// The state of the entitlement routes.
#[derive(Clone)]
pub struct EntitlementRoutes {
    verifier: IdentityVerifier,
    store: Arc<EntitlementStore>,
    tenant_db: TenantDb,
}

impl FromRef<EntitlementRoutes> for IdentityVerifier {
    fn from_ref(state: &EntitlementRoutes) -> Self {
        state.verifier.clone()
    }
}

/// The entitlement routes: [`PLATFORM_ENTITLEMENTS_PATH`] on `store`, and [`ENTITLEMENTS_PATH`],
/// which reads the token's tenant through `tenant_db`.
pub fn routes(verifier: IdentityVerifier, store: EntitlementStore, tenant_db: TenantDb) -> Router {
    Router::new()
        .route(
            PLATFORM_ENTITLEMENTS_PATH,
            get(get_tenant_entitlements).put(set_tenant_entitlements),
        )
        .route(ENTITLEMENTS_PATH, get(get_current_entitlements))
        .with_state(EntitlementRoutes {
            verifier,
            store: Arc::new(store),
            tenant_db,
        })
}

/// The checked body: the plan and the overrides, or the 422 body.
///
/// # Errors
///
/// The 422 body: an unknown plan (`/plan`), overrides that are not an object of known
/// entitlements of their types (`/overrides`), overrides with a plan that takes none
/// (`/overrides`), or an override out of range (`/overrides/<name>`).
pub fn parse_body(request: &SetEntitlements) -> Result<Target, ErrorBody> {
    let plan = Plan::parse(&request.plan);
    let overrides = match &request.overrides {
        None | Some(serde_json::Value::Null) => Some(EntitlementOverrides::default()),
        Some(value) => serde_json::from_value::<EntitlementOverrides>(value.clone()).ok(),
    };
    match (plan, overrides) {
        (Some(plan), Some(overrides)) => Target::new(plan, overrides),
        (plan, overrides) => {
            let mut body = ErrorBody::new(ErrorCode::ValidationFailed);
            if plan.is_none() {
                body = body.with_field_error(FieldError::new("/plan", PLAN_INVALID));
            }
            if overrides.is_none() {
                body = body.with_field_error(FieldError::new("/overrides", OVERRIDES_INVALID));
            }
            Err(body)
        }
    }
}

/// The tenant ID of the path, or `None` when it is not a UUID.
fn tenant_of(path: Result<Path<String>, PathRejection>) -> Option<TenantId> {
    path.ok()
        .and_then(|Path(tenant)| tenant.parse::<TenantId>().ok())
}

/// A body, `200` and never cached.
fn ok<T: Serialize>(body: T) -> Response {
    (
        StatusCode::OK,
        [(CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}

/// The answer to a failed read or change, logging what the database did.
fn failure_response(
    user: &UserIdentity,
    tenant: Option<TenantId>,
    failure: &EntitlementsError,
) -> Response {
    match failure {
        EntitlementsError::NotAllowed | EntitlementsError::Database(TenantDbError::NoTenant) => {
            error(ErrorBody::new(ErrorCode::PermissionDenied))
        }
        EntitlementsError::UnknownTenant => error(ErrorBody::new(ErrorCode::NotFound)),
        EntitlementsError::VersionConflict { current } => error(
            ErrorBody::new(ErrorCode::Conflict)
                .with_message_key(VERSION_CONFLICT)
                .with_detail("current_version", *current),
        ),
        EntitlementsError::Inconsistent(_)
        | EntitlementsError::Database(_)
        | EntitlementsError::Publish(_)
        | EntitlementsError::Aggregate(_) => {
            tracing::error!(
                user_id = %user.user_id(),
                tenant_id = ?tenant.map(|tenant| tenant.to_string()),
                error_kind = failure.log_kind(),
                "could not read or change a tenant's entitlements"
            );
            error(ErrorBody::new(ErrorCode::Unavailable))
        }
    }
}

/// Reads the entitlements of the caller's tenant: its members' (and services acting for them).
#[utoipa::path(
    get,
    path = "/api/identity/v1/entitlements",
    tag = "identity",
    operation_id = "getEntitlements",
    responses(
        (status = 200, description = "The plan and entitlements of the token's tenant.", body = CurrentEntitlements),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "The user has no tenant or a role this build does not know (`permission_denied`), or the tenant is suspended or deleted (`tenant_suspended`).", body = ErrorBody),
        (status = 404, description = "The token's tenant has no record (`not_found`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn get_current_entitlements(
    State(state): State<EntitlementRoutes>,
    user: UserIdentity,
) -> Response {
    if let Err(refused) = user.require(
        Requirement::Tenant(TenantRole::Viewer),
        "read the tenant's entitlements",
    ) {
        return refused.into_response();
    }
    let tenant = user.tenant_id();
    let result = async {
        let mut tx = state.tenant_db.begin(&user).await?;
        let row: Option<Row> = sqlx::query_as(READ)
            .bind(tenant.map(|tenant| tenant.to_string()))
            .fetch_optional(&mut *tx)
            .await?;
        tx.rollback().await?;
        Stored::parse(row.ok_or(EntitlementsError::UnknownTenant)?)
    }
    .await;
    match (result, tenant) {
        (Ok(stored), Some(tenant)) => ok(CurrentEntitlements {
            tenant_id: tenant,
            plan: stored.plan,
            entitlements: stored.entitlements(),
            entitlements_version: stored.version,
        }),
        // `require` passed, so the token has a tenant.
        (Ok(_), None) => error(ErrorBody::new(ErrorCode::PermissionDenied)),
        (Err(failure), _) => failure_response(&user, tenant, &failure),
    }
}

/// Reads a tenant's plan, overrides and entitlements. Platform administrators only, in their own
/// request.
#[utoipa::path(
    get,
    path = "/api/identity/v1/platform/tenants/{tenant_id}/entitlements",
    tag = "identity",
    operation_id = "getPlatformTenantEntitlements",
    params(
        ("tenant_id" = String, Path, description = "The tenant's ID."),
    ),
    responses(
        (status = 200, description = "The tenant's plan, overrides and entitlements.", body = TenantEntitlements),
        (status = 400, description = "The tenant ID is not a UUID (`invalid_request`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "Not a platform administrator, or not the administrator's own request: another service exchanged the token (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No tenant has the ID (`not_found`).", body = ErrorBody),
        (status = 503, description = "The database failed (`unavailable`).", body = ErrorBody),
    )
)]
async fn get_tenant_entitlements(
    State(state): State<EntitlementRoutes>,
    user: UserIdentity,
    tenant: Result<Path<String>, PathRejection>,
) -> Response {
    // Decided from the token alone, before the database is read.
    if let Err(refused) = user.require(
        Requirement::OwnPlatformAdmin,
        "read a tenant's entitlements",
    ) {
        return refused.into_response();
    }
    let Some(tenant) = tenant_of(tenant) else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    match state.store.read(&user, tenant).await {
        Ok(view) => ok(view),
        Err(failure) => failure_response(&user, Some(tenant), &failure),
    }
}

/// Sets a tenant's plan and overrides, both replaced; the entitlements are the plan's values with
/// the overrides applied. Platform administrators only, in their own request.
#[utoipa::path(
    put,
    path = "/api/identity/v1/platform/tenants/{tenant_id}/entitlements",
    tag = "identity",
    operation_id = "setPlatformTenantEntitlements",
    params(
        ("tenant_id" = String, Path, description = "The tenant's ID."),
    ),
    request_body = SetEntitlements,
    responses(
        (status = 200, description = "The tenant has the plan and overrides: when the plan, the overrides or the entitlements changed, the version went up by one and `identity.tenant_entitlements_changed` was written in the same transaction; otherwise nothing was written.", body = TenantEntitlements),
        (status = 400, description = "The tenant ID is not a UUID, the `Content-Type` is not `application/json`, or the body is over 8192 bytes or not a JSON object with a string `plan`, an optional `overrides`, an integer `expected_version` of at least 0 and no other field (`invalid_request`); nothing was written.", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "Not a platform administrator, or not the administrator's own request: another service exchanged the token (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No tenant has the ID; nothing was written (`not_found`).", body = ErrorBody),
        (status = 409, description = "The tenant is no longer at `expected_version` (`conflict`, message key `entitlements.version_conflict`, `details.current_version`); nothing was written.", body = ErrorBody),
        (status = 422, description = "An unknown plan (`/plan`, `entitlements.plan_invalid`); overrides that are not an object of known entitlements with values of their type (`/overrides`, `entitlements.overrides_invalid`) or with a plan other than `enterprise` and `enterprise_on_prem` (`/overrides`, `entitlements.overrides_not_allowed`); an override out of range (`/overrides/<name>`, `entitlements.override_out_of_range`, `min`, `max`) (`validation_failed`). Nothing was written.", body = ErrorBody),
        (status = 503, description = "The database failed; nothing was written (`unavailable`).", body = ErrorBody),
    )
)]
async fn set_tenant_entitlements(
    State(state): State<EntitlementRoutes>,
    user: UserIdentity,
    tenant: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // Decided from the token alone, before the body is read or the database touched.
    if let Err(refused) = user.require(
        Requirement::OwnPlatformAdmin,
        "change a tenant's entitlements",
    ) {
        return refused.into_response();
    }
    let Some(tenant) = tenant_of(tenant) else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    if !is_json(&headers) {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    }
    let Ok(body) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    let Ok(request) = serde_json::from_slice::<SetEntitlements>(&body) else {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    };
    if !matches!(
        request.overrides,
        None | Some(serde_json::Value::Null | serde_json::Value::Object(_))
    ) {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    }
    if request.expected_version < 0 {
        return error(ErrorBody::new(ErrorCode::InvalidRequest));
    }
    let target = match parse_body(&request) {
        Ok(target) => target,
        Err(refused) => return error(refused),
    };
    match state
        .store
        .set(&user, tenant, target, request.expected_version)
        .await
    {
        Ok((view, _)) => ok(view),
        Err(failure) => failure_response(&user, Some(tenant), &failure),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const PLANS_DOC: &str = include_str!("../../../docs/product/plans.md");

    #[test]
    fn plans_match_the_event_and_the_column() {
        for plan in Plan::ALL {
            assert_eq!(Plan::parse(plan.as_str()), Some(plan));
            assert_eq!(
                serde_json::to_value(plan).expect("serialize"),
                json!(plan.as_str())
            );
            assert_eq!(
                serde_json::to_value(plan.event()).expect("serialize"),
                json!(plan.as_str())
            );
        }
        for text in ["", "Starter", "free", "enterprise-on-prem", "unrecognized"] {
            assert_eq!(Plan::parse(text), None, "{text:?}");
        }
        let migration = include_str!("../migrations/postgres/0010_entitlements.sql");
        assert!(migration.contains(
            "CHECK (plan IN ('starter', 'business', 'enterprise', 'enterprise_on_prem'))"
        ));
        assert!(migration.contains("plan IN ('enterprise', 'enterprise_on_prem')"));
        for plan in Plan::ALL {
            assert_eq!(
                plan.takes_overrides(),
                matches!(plan, Plan::Enterprise | Plan::EnterpriseOnPrem)
            );
        }
    }

    #[test]
    fn every_plan_is_in_range_and_ordered() {
        for plan in Plan::ALL {
            assert_eq!(plan.defaults().out_of_range(), None, "{plan:?}");
        }
        // Each plan gives at least what the one below it does (an upgrade never takes anything
        // away), and the SaaS plans read no further back than the platform keeps data
        // (docs/architecture/data-retention.md: raw 180 days, 1-minute 13 months, 1-hour 4 years).
        let order = [
            Plan::Starter,
            Plan::Business,
            Plan::Enterprise,
            Plan::EnterpriseOnPrem,
        ];
        let limit = |limit: Limit| limit.max().map_or(u64::MAX, u64::from);
        for pair in order.windows(2) {
            let (lower, higher) = (pair[0].defaults(), pair[1].defaults());
            for quantity in contract::entitlements::Quantity::ALL {
                assert!(limit(lower.limit(quantity)) <= limit(higher.limit(quantity)));
            }
            for feature in contract::entitlements::Feature::ALL {
                assert!(!lower.has(feature) || higher.has(feature), "{feature:?}");
            }
            assert!(lower.raw_queryable_days <= higher.raw_queryable_days);
            assert!(lower.minute_queryable_months <= higher.minute_queryable_months);
            assert!(lower.hour_queryable_months <= higher.hour_queryable_months);
            assert!(lower.min_sampling_interval_ms >= higher.min_sampling_interval_ms);
        }
        for plan in [Plan::Starter, Plan::Business, Plan::Enterprise] {
            let values = plan.defaults();
            assert!(values.raw_queryable_days <= 180, "{plan:?}");
            assert!(values.minute_queryable_months <= 13, "{plan:?}");
            assert!(values.hour_queryable_months <= 48, "{plan:?}");
        }
    }

    /// docs/product/plans.md, 「權益清單」, states every plan's values; a change to one side alone
    /// fails here.
    #[test]
    fn the_plans_document_states_these_values() {
        fn shown(limit: Limit) -> String {
            match limit {
                Limit::AtMost(max) => max.to_string(),
                Limit::Unlimited => "不限".to_owned(),
            }
        }
        fn flag(on: bool) -> String {
            if on { "✓" } else { "—" }.to_owned()
        }
        type Shown = fn(&Entitlements) -> String;
        let rows: [(&str, Shown); 16] = [
            ("`max_gateways`", |e| shown(e.max_gateways)),
            ("`max_members`", |e| shown(e.max_members)),
            ("`max_alert_rules`", |e| shown(e.max_alert_rules)),
            ("`raw_queryable_days`", |e| e.raw_queryable_days.to_string()),
            ("`minute_queryable_months`", |e| {
                e.minute_queryable_months.to_string()
            }),
            ("`hour_queryable_months`", |e| {
                e.hour_queryable_months.to_string()
            }),
            ("`min_sampling_interval_ms`", |e| {
                e.min_sampling_interval_ms.to_string()
            }),
            ("`realtime_charts`", |e| flag(e.realtime_charts)),
            ("`email_notifications`", |e| flag(e.email_notifications)),
            ("`webhook_notifications`", |e| flag(e.webhook_notifications)),
            ("`ai_alert_rules`", |e| flag(e.ai_alert_rules)),
            ("`field_context`", |e| flag(e.field_context)),
            ("`document_rag`", |e| flag(e.document_rag)),
            ("`reports`", |e| flag(e.reports)),
            ("`api_and_mcp`", |e| flag(e.api_and_mcp)),
            ("`sso`", |e| flag(e.sso)),
        ];
        for (name, value) in rows {
            let row = PLANS_DOC
                .lines()
                .find(|line| line.starts_with(&format!("| {name} |")))
                .unwrap_or_else(|| panic!("docs/product/plans.md has no row for {name}"));
            let cells: Vec<&str> = row.split('|').map(str::trim).collect();
            // `| name | meaning | starter | business | enterprise | on-prem | ... |`
            for (index, plan) in Plan::ALL.into_iter().enumerate() {
                let cell = cells.get(3 + index).copied().unwrap_or_default();
                let expected = value(&plan.defaults());
                assert!(
                    cell == expected || cell.starts_with(&format!("{expected}（")),
                    "{name}, {plan:?}: the document says {cell:?}, the code {expected:?}"
                );
            }
        }
    }

    fn body(plan: &str, overrides: Option<serde_json::Value>) -> SetEntitlements {
        SetEntitlements {
            plan: plan.to_owned(),
            overrides,
            expected_version: 0,
        }
    }

    fn fields(body: ErrorBody) -> Vec<(String, String)> {
        let value = serde_json::to_value(body).expect("serialize");
        assert_eq!(value["code"], "validation_failed");
        value["field_errors"]
            .as_array()
            .expect("field errors")
            .iter()
            .map(|error| {
                (
                    error["field"].as_str().expect("field").to_owned(),
                    error["message_key"].as_str().expect("key").to_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn the_body_needs_the_expected_version_and_no_other_field() {
        let parsed = |value: serde_json::Value| serde_json::from_value::<SetEntitlements>(value);
        let request = parsed(json!({"plan": "enterprise", "expected_version": 5}))
            .expect("a plan and the version");
        assert_eq!(
            (request.plan.as_str(), request.expected_version),
            ("enterprise", 5)
        );
        assert!(request.overrides.is_none());
        for refused in [
            json!({"plan": "enterprise"}),
            json!({"plan": "enterprise", "expected_version": "5"}),
            json!({"plan": "enterprise", "expected_version": 5.5}),
            json!({"plan": "enterprise", "expected_version": null}),
            // A misspelt `overrides` would otherwise clear the contract's overrides.
            json!({"plan": "enterprise", "override": {"max_gateways": 80}, "expected_version": 5}),
            json!({"plan": "enterprise", "expected_version": 5, "tenant_id": "x"}),
        ] {
            assert!(parsed(refused.clone()).is_err(), "{refused}");
        }
    }

    #[test]
    fn overrides_take_precedence_over_the_plan() {
        let target = parse_body(&body(
            "enterprise",
            Some(json!({
                "max_gateways": 50,
                "max_members": "unlimited",
                "raw_queryable_days": 30,
                "sso": false,
            })),
        ))
        .expect("valid");
        let entitlements = target.entitlements();
        let plan = Plan::Enterprise.defaults();
        assert_eq!(entitlements.max_gateways, Limit::AtMost(50));
        assert_eq!(entitlements.max_members, Limit::Unlimited);
        assert_eq!(entitlements.raw_queryable_days, 30);
        assert!(!entitlements.sso);
        // Everything else is the plan's.
        assert_eq!(
            Entitlements {
                max_gateways: plan.max_gateways,
                max_members: plan.max_members,
                raw_queryable_days: plan.raw_queryable_days,
                sso: plan.sso,
                ..entitlements
            },
            plan
        );
        // No overrides, null or `{}`: the plan's values.
        for overrides in [None, Some(json!(null)), Some(json!({}))] {
            for plan in Plan::ALL {
                assert_eq!(
                    parse_body(&body(plan.as_str(), overrides.clone()))
                        .expect("valid")
                        .entitlements(),
                    plan.defaults()
                );
            }
        }
        // On-prem takes overrides too (limits by contract).
        assert_eq!(
            parse_body(&body(
                "enterprise_on_prem",
                Some(json!({"max_gateways": 10}))
            ))
            .expect("valid")
            .entitlements()
            .max_gateways,
            Limit::AtMost(10)
        );
        // Stored as only what was set.
        assert_eq!(
            serde_json::to_value(target.overrides).expect("serialize"),
            json!({"max_gateways": 50, "max_members": "unlimited", "raw_queryable_days": 30, "sso": false})
        );
        assert_eq!(
            serde_json::to_value(EntitlementOverrides::default()).expect("serialize"),
            json!({})
        );
    }

    #[test]
    fn bad_plans_and_overrides_are_refused() {
        let plan = ("/plan".to_owned(), "entitlements.plan_invalid".to_owned());
        let invalid = (
            "/overrides".to_owned(),
            "entitlements.overrides_invalid".to_owned(),
        );
        let not_allowed = (
            "/overrides".to_owned(),
            "entitlements.overrides_not_allowed".to_owned(),
        );
        let range = |name: &str| {
            (
                format!("/overrides/{name}"),
                "entitlements.override_out_of_range".to_owned(),
            )
        };
        for (request, expected) in [
            (body("free", None), vec![plan.clone()]),
            (body("Enterprise", None), vec![plan.clone()]),
            (
                body("platinum", Some(json!({"bogus": 1}))),
                vec![plan.clone(), invalid.clone()],
            ),
            (
                body("enterprise", Some(json!({"bogus": 1}))),
                vec![invalid.clone()],
            ),
            (
                body("enterprise", Some(json!({"max_gateways": -1}))),
                vec![invalid.clone()],
            ),
            (
                body("enterprise", Some(json!({"max_gateways": "lots"}))),
                vec![invalid.clone()],
            ),
            (
                body("enterprise", Some(json!({"sso": "yes"}))),
                vec![invalid.clone()],
            ),
            (
                body(
                    "enterprise",
                    Some(json!({"raw_queryable_days": 4_294_967_296_u64})),
                ),
                vec![invalid.clone()],
            ),
            // Only the Enterprise plans take overrides.
            (
                body("starter", Some(json!({"max_gateways": 5}))),
                vec![not_allowed.clone()],
            ),
            (
                body("business", Some(json!({"realtime_charts": false}))),
                vec![not_allowed.clone()],
            ),
            (
                body("enterprise", Some(json!({"raw_queryable_days": 0}))),
                vec![range("raw_queryable_days")],
            ),
            (
                body("enterprise", Some(json!({"hour_queryable_months": 1201}))),
                vec![range("hour_queryable_months")],
            ),
            (
                body("enterprise", Some(json!({"min_sampling_interval_ms": 99}))),
                vec![range("min_sampling_interval_ms")],
            ),
        ] {
            let shown = format!("{request:?}");
            assert_eq!(
                fields(parse_body(&request).expect_err(&shown)),
                expected,
                "{shown}"
            );
        }
        let out_of_range = serde_json::to_value(
            parse_body(&body("enterprise", Some(json!({"raw_queryable_days": 0}))))
                .expect_err("out of range"),
        )
        .expect("serialize");
        assert_eq!(
            out_of_range["field_errors"][0]["details"],
            json!({"min": 1, "max": 36_500})
        );
    }

    #[test]
    fn stored_rows_are_read_back_or_refused() {
        let snapshot = serde_json::to_string(&Plan::Business.defaults()).expect("serialize");
        let stored = Stored::parse((
            "business".to_owned(),
            "{}".to_owned(),
            Some(snapshot.clone()),
            2,
            Some(DateTime::UNIX_EPOCH),
        ))
        .expect("a row");
        assert_eq!(stored.entitlements(), Plan::Business.defaults());
        // Version 0: no snapshot yet, the plan's values shown.
        let fresh =
            Stored::parse(("starter".to_owned(), "{}".to_owned(), None, 0, None)).expect("a row");
        assert_eq!(fresh.entitlements(), Plan::Starter.defaults());
        for row in [
            ("gold".to_owned(), "{}".to_owned(), None, 0, None),
            (
                "starter".to_owned(),
                "{\"bogus\":1}".to_owned(),
                None,
                0,
                None,
            ),
            (
                "starter".to_owned(),
                "{}".to_owned(),
                Some("{}".to_owned()),
                1,
                None,
            ),
            ("starter".to_owned(), "{}".to_owned(), None, 1, None),
            (
                "starter".to_owned(),
                "{}".to_owned(),
                Some(snapshot.clone()),
                0,
                None,
            ),
        ] {
            assert!(Stored::parse(row.clone()).is_err(), "{row:?}");
        }
    }

    #[test]
    fn the_spec_lists_the_routes_and_their_answers() {
        let spec = serde_json::to_value(crate::openapi()).expect("the spec");
        let read = &spec["paths"][PLATFORM_ENTITLEMENTS_PATH]["get"];
        assert_eq!(read["operationId"], "getPlatformTenantEntitlements");
        for status in ["200", "400", "401", "403", "404", "503"] {
            assert!(read["responses"][status].is_object(), "GET {status}");
        }
        let set = &spec["paths"][PLATFORM_ENTITLEMENTS_PATH]["put"];
        assert_eq!(set["operationId"], "setPlatformTenantEntitlements");
        for status in ["200", "400", "401", "403", "404", "409", "422", "503"] {
            assert!(set["responses"][status].is_object(), "PUT {status}");
        }
        let own = &spec["paths"][ENTITLEMENTS_PATH]["get"];
        assert_eq!(own["operationId"], "getEntitlements");
        for status in ["200", "401", "403", "404", "503"] {
            assert!(own["responses"][status].is_object(), "GET own {status}");
        }
        let schemas = &spec["components"]["schemas"];
        assert_eq!(
            schemas["Plan"]["enum"],
            json!(["starter", "business", "enterprise", "enterprise_on_prem"])
        );
        assert_eq!(
            schemas["Entitlements"]["required"]
                .as_array()
                .expect("required")
                .len(),
            16
        );
    }
}
