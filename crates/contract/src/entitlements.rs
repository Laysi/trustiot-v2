//! A tenant's plan entitlements (rebuild checklist B3, Laysi/trustiot-v2#111;
//! docs/product/plans.md, 「權益清單」): what the tenant's plan lets it add, read and use, as
//! one snapshot.
//!
//! The identity service owns them: it keeps every tenant's plan, the per-tenant overrides of an
//! Enterprise contract, and the resulting [`Entitlements`], and announces each change with
//! [`crate::events::identity::TenantEntitlementsChanged`], the whole snapshot and its version.
//! A service that checks an entitlement keeps the snapshot with the larger version from those
//! events (crates/outbox, 「通知模式」) and checks it on its own side, on the server; it never
//! works the values out from the plan, so a plan's values change in one place.
//!
//! The floor of checklist B3 holds for every check built on these types: an entitlement never
//! refuses a data write, never disables an alert that is already on and never deletes data. A
//! [`Limit`] only refuses an addition ([`Limit::admits_one_more`]): what already exists keeps
//! working when a plan goes down. The queryable periods only narrow reads.
//!
//! A refused check answers with the M1 code `quota_exceeded` (403), built by
//! [`feature_refusal`] or [`limit_refusal`], so the frontend offers an upgrade the same way for
//! every service.

use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::{ObjectBuilder, OneOfBuilder, Schema, Type};
use utoipa::{PartialSchema, ToSchema};

use crate::error::{ErrorBody, ErrorCode, MessageKey};

/// The longest raw-data window a tenant's ClickHouse account accepts, in days
/// (services/ingest/migrations/clickhouse/0011_queryable_period.sql caps the arithmetic at it).
pub const MAX_RAW_QUERYABLE_DAYS: u32 = 36_500;

/// The longest aggregate window a tenant's ClickHouse account accepts, in calendar months.
pub const MAX_QUERYABLE_MONTHS: u32 = 1_200;

/// The shortest sampling interval an entitlement may allow, in milliseconds: the shortest poll
/// interval a device may have at all.
pub const MIN_SAMPLING_INTERVAL_MS: u32 = protocol::config::MIN_POLL_INTERVAL_MS;

/// The longest sampling interval an entitlement may require, in milliseconds: the longest poll
/// interval a device may have.
pub const MAX_SAMPLING_INTERVAL_MS: u32 = protocol::config::MAX_POLL_INTERVAL_MS;

/// The message key of a refusal because the plan does not include a feature;
/// `details.entitlement` names it ([`Feature::as_str`]).
pub const FEATURE_NOT_IN_PLAN: MessageKey = MessageKey::new("entitlement.feature_not_in_plan");

/// The message key of a refusal because adding one more would pass a plan limit;
/// `details.entitlement` names it ([`Quantity::as_str`]) and `details.max` is the limit.
pub const LIMIT_REACHED: MessageKey = MessageKey::new("entitlement.limit_reached");

/// How many of something a tenant may have: at most a number, or no limit.
///
/// On the wire it is a non-negative integer, or the string `"unlimited"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Limit {
    /// At most this many (0: none at all).
    AtMost(u32),
    /// No limit.
    Unlimited,
}

impl Limit {
    /// Whether one more may be added while `existing` already exist. This is the only check a
    /// limit makes (checklist B3): it refuses additions, never what exists, so a tenant above a
    /// lowered limit keeps everything it has and only cannot add more.
    #[must_use]
    pub const fn admits_one_more(self, existing: u64) -> bool {
        match self {
            Self::AtMost(max) => existing < max as u64,
            Self::Unlimited => true,
        }
    }

    /// The number, or `None` for no limit.
    #[must_use]
    pub const fn max(self) -> Option<u32> {
        match self {
            Self::AtMost(max) => Some(max),
            Self::Unlimited => None,
        }
    }
}

/// The wire spelling of [`Limit::Unlimited`].
const UNLIMITED: &str = "unlimited";

impl Serialize for Limit {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::AtMost(max) => serializer.serialize_u32(*max),
            Self::Unlimited => serializer.serialize_str(UNLIMITED),
        }
    }
}

impl<'de> Deserialize<'de> for Limit {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct LimitVisitor;

        impl Visitor<'_> for LimitVisitor {
            type Value = Limit;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a non-negative 32-bit integer or \"unlimited\"")
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Limit, E> {
                u32::try_from(value)
                    .map(Limit::AtMost)
                    .map_err(|_| E::invalid_value(de::Unexpected::Unsigned(value), &self))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Limit, E> {
                u32::try_from(value)
                    .map(Limit::AtMost)
                    .map_err(|_| E::invalid_value(de::Unexpected::Signed(value), &self))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Limit, E> {
                if value == UNLIMITED {
                    Ok(Limit::Unlimited)
                } else {
                    Err(E::invalid_value(de::Unexpected::Str(value), &self))
                }
            }
        }

        deserializer.deserialize_any(LimitVisitor)
    }
}

impl PartialSchema for Limit {
    fn schema() -> RefOr<Schema> {
        OneOfBuilder::new()
            .item(
                ObjectBuilder::new()
                    .schema_type(Type::Integer)
                    .format(Some(utoipa::openapi::SchemaFormat::KnownFormat(
                        utoipa::openapi::KnownFormat::Int64,
                    )))
                    .minimum(Some(0))
                    .maximum(Some(u32::MAX))
                    .description(Some("At most this many (0: none).")),
            )
            .item(
                ObjectBuilder::new()
                    .schema_type(Type::String)
                    .enum_values(Some([UNLIMITED]))
                    .description(Some("No limit.")),
            )
            .description(Some(
                "How many a tenant may have: a non-negative integer, or `unlimited`. A limit only \
                 refuses additions; what exists keeps working.",
            ))
            .into()
    }
}

impl ToSchema for Limit {}

/// What a tenant's plan entitles it to (docs/product/plans.md, 「權益清單」). Every field is
/// required on the wire: a field added later is optional and decodes, when missing, to its most
/// restrictive value (crates/contract/README.md, 「方案權益」).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Entitlements {
    /// Gateways the tenant may have bound (B2). Checked when a gateway is bound.
    pub max_gateways: Limit,
    /// Members the tenant may have (B2). Checked when a member is added.
    pub max_members: Limit,
    /// Alert rules the tenant may have (B2); 0 means the plan has no alerts. Checked when a rule
    /// is created; rules that exist keep running.
    pub max_alert_rules: Limit,
    /// How far back raw readings can be read, in whole days (1 to 36500).
    #[schema(minimum = 1, maximum = 36_500)]
    pub raw_queryable_days: u32,
    /// How far back one-minute aggregates can be read, in calendar months (1 to 1200).
    #[schema(minimum = 1, maximum = 1_200)]
    pub minute_queryable_months: u32,
    /// How far back one-hour aggregates can be read, in calendar months (1 to 1200).
    #[schema(minimum = 1, maximum = 1_200)]
    pub hour_queryable_months: u32,
    /// The shortest sampling interval a device may be set to, in milliseconds (100 to
    /// 86400000); also the rate of real-time values.
    #[schema(minimum = 100, maximum = 86_400_000)]
    pub min_sampling_interval_ms: u32,
    /// Real-time charts (checklist I3).
    pub realtime_charts: bool,
    /// Alert notifications by email.
    pub email_notifications: bool,
    /// Alert notifications by webhook.
    pub webhook_notifications: bool,
    /// The AI writes alert rules.
    pub ai_alert_rules: bool,
    /// Field context and energy per piece.
    pub field_context: bool,
    /// Questions over the tenant's documents (RAG).
    pub document_rag: bool,
    /// Reports: the monthly carbon report and scheduled reports.
    pub reports: bool,
    /// The public API and MCP.
    pub api_and_mcp: bool,
    /// Single sign-on (OIDC, SAML).
    pub sso: bool,
}

impl Entitlements {
    /// Whether the plan includes `feature`.
    #[must_use]
    pub const fn has(&self, feature: Feature) -> bool {
        match feature {
            Feature::RealtimeCharts => self.realtime_charts,
            Feature::EmailNotifications => self.email_notifications,
            Feature::WebhookNotifications => self.webhook_notifications,
            Feature::AiAlertRules => self.ai_alert_rules,
            Feature::FieldContext => self.field_context,
            Feature::DocumentRag => self.document_rag,
            Feature::Reports => self.reports,
            Feature::ApiAndMcp => self.api_and_mcp,
            Feature::Sso => self.sso,
        }
    }

    /// The plan's limit on `quantity`.
    #[must_use]
    pub const fn limit(&self, quantity: Quantity) -> Limit {
        match quantity {
            Quantity::Gateways => self.max_gateways,
            Quantity::Members => self.max_members,
            Quantity::AlertRules => self.max_alert_rules,
        }
    }

    /// The first value outside what the services can apply, by its field name; `None` when
    /// every value is in range. The windows must be at least 1 (a ClickHouse account refuses
    /// 0) and at most what the accounts accept; the sampling interval must be one a device may
    /// have.
    #[must_use]
    pub const fn out_of_range(&self) -> Option<&'static str> {
        if self.raw_queryable_days < 1 || self.raw_queryable_days > MAX_RAW_QUERYABLE_DAYS {
            Some("raw_queryable_days")
        } else if self.minute_queryable_months < 1
            || self.minute_queryable_months > MAX_QUERYABLE_MONTHS
        {
            Some("minute_queryable_months")
        } else if self.hour_queryable_months < 1
            || self.hour_queryable_months > MAX_QUERYABLE_MONTHS
        {
            Some("hour_queryable_months")
        } else if self.min_sampling_interval_ms < MIN_SAMPLING_INTERVAL_MS
            || self.min_sampling_interval_ms > MAX_SAMPLING_INTERVAL_MS
        {
            Some("min_sampling_interval_ms")
        } else {
            None
        }
    }
}

/// A feature a plan includes or not, named as its [`Entitlements`] field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Feature {
    /// [`Entitlements::realtime_charts`].
    RealtimeCharts,
    /// [`Entitlements::email_notifications`].
    EmailNotifications,
    /// [`Entitlements::webhook_notifications`].
    WebhookNotifications,
    /// [`Entitlements::ai_alert_rules`].
    AiAlertRules,
    /// [`Entitlements::field_context`].
    FieldContext,
    /// [`Entitlements::document_rag`].
    DocumentRag,
    /// [`Entitlements::reports`].
    Reports,
    /// [`Entitlements::api_and_mcp`].
    ApiAndMcp,
    /// [`Entitlements::sso`].
    Sso,
}

impl Feature {
    /// Every feature.
    pub const ALL: [Self; 9] = [
        Self::RealtimeCharts,
        Self::EmailNotifications,
        Self::WebhookNotifications,
        Self::AiAlertRules,
        Self::FieldContext,
        Self::DocumentRag,
        Self::Reports,
        Self::ApiAndMcp,
        Self::Sso,
    ];

    /// The field's name, which a refusal's `details.entitlement` carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RealtimeCharts => "realtime_charts",
            Self::EmailNotifications => "email_notifications",
            Self::WebhookNotifications => "webhook_notifications",
            Self::AiAlertRules => "ai_alert_rules",
            Self::FieldContext => "field_context",
            Self::DocumentRag => "document_rag",
            Self::Reports => "reports",
            Self::ApiAndMcp => "api_and_mcp",
            Self::Sso => "sso",
        }
    }
}

/// Something a plan limits the number of, named as its [`Entitlements`] field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Quantity {
    /// [`Entitlements::max_gateways`].
    Gateways,
    /// [`Entitlements::max_members`].
    Members,
    /// [`Entitlements::max_alert_rules`].
    AlertRules,
}

impl Quantity {
    /// Every quantity.
    pub const ALL: [Self; 3] = [Self::Gateways, Self::Members, Self::AlertRules];

    /// The field's name, which a refusal's `details.entitlement` carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gateways => "max_gateways",
            Self::Members => "max_members",
            Self::AlertRules => "max_alert_rules",
        }
    }
}

/// The answer to a request that needs `feature` when the tenant's plan does not include it:
/// 403 `quota_exceeded`, key [`FEATURE_NOT_IN_PLAN`], `details.entitlement`.
#[must_use]
pub fn feature_refusal(feature: Feature) -> ErrorBody {
    ErrorBody::new(ErrorCode::QuotaExceeded)
        .with_message_key(FEATURE_NOT_IN_PLAN)
        .with_detail("entitlement", feature.as_str())
}

/// The answer to a request that would add one more of `quantity` past the plan's limit `max`:
/// 403 `quota_exceeded`, key [`LIMIT_REACHED`], `details.entitlement` and `details.max`.
#[must_use]
pub fn limit_refusal(quantity: Quantity, max: u32) -> ErrorBody {
    ErrorBody::new(ErrorCode::QuotaExceeded)
        .with_message_key(LIMIT_REACHED)
        .with_detail("entitlement", quantity.as_str())
        .with_detail("max", max)
}

/// A tenant's plan, as [`crate::events::identity::TenantEntitlementsChanged`] names it. Only add
/// variants, never rename one: a consumer built before a variant existed decodes it as
/// [`Plan::Unrecognized`]. The plan is for display and the operation log; checks read the
/// snapshot's [`Entitlements`], never the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Plan {
    /// Small plants, one production line.
    Starter,
    /// Mid-sized plants, several lines.
    Business,
    /// Groups with several plants, on the SaaS.
    Enterprise,
    /// Groups running the platform in their own data center: no technical licence check.
    EnterpriseOnPrem,
    /// A plan this build does not know, written by a newer producer. Decode only: serializing
    /// it fails, so it never reaches an outbox.
    #[serde(other, skip_serializing)]
    Unrecognized,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn starter_like() -> Entitlements {
        Entitlements {
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
        }
    }

    #[test]
    fn a_limit_is_a_number_or_unlimited_on_the_wire() {
        assert_eq!(
            serde_json::to_value(Limit::AtMost(5)).expect("encode"),
            json!(5)
        );
        assert_eq!(
            serde_json::to_value(Limit::AtMost(0)).expect("encode"),
            json!(0)
        );
        assert_eq!(
            serde_json::to_value(Limit::Unlimited).expect("encode"),
            json!("unlimited")
        );
        for (wire, limit) in [
            (json!(0), Limit::AtMost(0)),
            (json!(7), Limit::AtMost(7)),
            (json!(u32::MAX), Limit::AtMost(u32::MAX)),
            (json!("unlimited"), Limit::Unlimited),
        ] {
            assert_eq!(
                serde_json::from_value::<Limit>(wire.clone()).expect("decode"),
                limit,
                "{wire}"
            );
        }
        // Anything else is not a limit: a missing limit must never read as "no limit".
        for wire in [
            json!(-1),
            json!(u64::from(u32::MAX) + 1),
            json!(1.5),
            json!("Unlimited"),
            json!("10"),
            json!(null),
            json!(true),
            json!({}),
        ] {
            assert!(
                serde_json::from_value::<Limit>(wire.clone()).is_err(),
                "{wire} is not a limit"
            );
        }
    }

    #[test]
    fn a_limit_refuses_only_additions() {
        // At the limit or above it (after a downgrade): nothing more, and nothing taken away.
        assert!(Limit::AtMost(5).admits_one_more(4));
        assert!(!Limit::AtMost(5).admits_one_more(5));
        assert!(!Limit::AtMost(5).admits_one_more(12));
        assert!(!Limit::AtMost(0).admits_one_more(0));
        assert!(Limit::Unlimited.admits_one_more(u64::MAX));
        assert_eq!(Limit::AtMost(5).max(), Some(5));
        assert_eq!(Limit::Unlimited.max(), None);
    }

    #[test]
    fn every_field_is_required_on_the_wire() {
        let full = serde_json::to_value(starter_like()).expect("encode");
        let fields = full.as_object().expect("an object");
        assert_eq!(fields.len(), 16);
        for name in fields.keys() {
            let mut without = fields.clone();
            without.remove(name);
            assert!(
                serde_json::from_value::<Entitlements>(serde_json::Value::Object(without)).is_err(),
                "{name} must be required: a missing one never defaults to a looser value"
            );
        }
        // Fields a newer producer adds are ignored.
        let mut newer = fields.clone();
        newer.insert("max_dashboards".to_owned(), json!(3));
        assert_eq!(
            serde_json::from_value::<Entitlements>(serde_json::Value::Object(newer))
                .expect("decode"),
            starter_like()
        );
    }

    #[test]
    fn features_and_quantities_name_their_fields() {
        let mut entitlements = starter_like();
        let encoded = serde_json::to_value(entitlements).expect("encode");
        for feature in Feature::ALL {
            assert_eq!(encoded[feature.as_str()], json!(false), "{feature:?}");
            assert!(!entitlements.has(feature));
        }
        entitlements.realtime_charts = true;
        entitlements.sso = true;
        assert!(entitlements.has(Feature::RealtimeCharts));
        assert!(entitlements.has(Feature::Sso));
        assert!(!entitlements.has(Feature::Reports));
        for quantity in Quantity::ALL {
            assert_eq!(
                encoded[quantity.as_str()],
                serde_json::to_value(entitlements.limit(quantity)).expect("encode"),
                "{quantity:?}"
            );
        }
    }

    #[test]
    fn values_out_of_range_are_named() {
        assert_eq!(starter_like().out_of_range(), None);
        let max = Entitlements {
            raw_queryable_days: MAX_RAW_QUERYABLE_DAYS,
            minute_queryable_months: MAX_QUERYABLE_MONTHS,
            hour_queryable_months: MAX_QUERYABLE_MONTHS,
            min_sampling_interval_ms: MIN_SAMPLING_INTERVAL_MS,
            ..starter_like()
        };
        assert_eq!(max.out_of_range(), None);
        for (bad, field) in [
            (
                Entitlements {
                    raw_queryable_days: 0,
                    ..starter_like()
                },
                "raw_queryable_days",
            ),
            (
                Entitlements {
                    raw_queryable_days: MAX_RAW_QUERYABLE_DAYS + 1,
                    ..starter_like()
                },
                "raw_queryable_days",
            ),
            (
                Entitlements {
                    minute_queryable_months: 0,
                    ..starter_like()
                },
                "minute_queryable_months",
            ),
            (
                Entitlements {
                    hour_queryable_months: MAX_QUERYABLE_MONTHS + 1,
                    ..starter_like()
                },
                "hour_queryable_months",
            ),
            (
                Entitlements {
                    min_sampling_interval_ms: MIN_SAMPLING_INTERVAL_MS - 1,
                    ..starter_like()
                },
                "min_sampling_interval_ms",
            ),
            (
                Entitlements {
                    min_sampling_interval_ms: MAX_SAMPLING_INTERVAL_MS + 1,
                    ..starter_like()
                },
                "min_sampling_interval_ms",
            ),
        ] {
            assert_eq!(bad.out_of_range(), Some(field));
        }
    }

    #[test]
    fn refusals_are_quota_exceeded_with_the_entitlement() {
        let feature =
            serde_json::to_value(feature_refusal(Feature::RealtimeCharts)).expect("encode");
        assert_eq!(feature["code"], "quota_exceeded");
        assert_eq!(feature["message_key"], "entitlement.feature_not_in_plan");
        assert_eq!(
            feature["details"],
            json!({"entitlement": "realtime_charts"})
        );
        let limit = limit_refusal(Quantity::Gateways, 1);
        assert_eq!(limit.http_status(), http::StatusCode::FORBIDDEN);
        let limit = serde_json::to_value(limit).expect("encode");
        assert_eq!(limit["code"], "quota_exceeded");
        assert_eq!(limit["message_key"], "entitlement.limit_reached");
        assert_eq!(
            limit["details"],
            json!({"entitlement": "max_gateways", "max": 1})
        );
    }

    #[test]
    fn plans_keep_their_wire_names_and_unknown_ones_decode_only() {
        for (plan, name) in [
            (Plan::Starter, "starter"),
            (Plan::Business, "business"),
            (Plan::Enterprise, "enterprise"),
            (Plan::EnterpriseOnPrem, "enterprise_on_prem"),
        ] {
            assert_eq!(serde_json::to_value(plan).expect("encode"), json!(name));
            assert_eq!(
                serde_json::from_value::<Plan>(json!(name)).expect("decode"),
                plan
            );
        }
        assert_eq!(
            serde_json::from_value::<Plan>(json!("platinum")).expect("decode"),
            Plan::Unrecognized
        );
        assert!(serde_json::to_value(Plan::Unrecognized).is_err());
    }
}
