//! The projection of the assets service's models, devices and bindings, and the versions of every
//! gateway's desired configuration made from it (decision 7; services/gateway-ops/README.md,
//! "由事件到設定").
//!
//! [`AssetsEventsHandler`] is the consumer's handler of `assets.device_model_*`, `assets.device_*`
//! and `assets.gateway_bound`. The consumer runs it in a transaction of the service role in the
//! event's tenant, so row-level security keeps every row it reads or writes in that tenant. For
//! each event it:
//!
//! 1. reads, as the platform role and read only, the current binding and the latest
//!    configuration version of each gateway the event affects: which tenant holds a gateway, and
//!    which version it has, are questions across tenants;
//! 2. records the event in the projection, newest revision first (a late, older event never
//!    replaces a newer row; a deleted model or device stays as a tombstone);
//! 3. for every affected gateway bound to the event's tenant, builds the configuration from that
//!    tenant's rows only ([`crate::generate`]) and stores it as the next version when it differs
//!    from the gateway's latest one.
//!
//! A device event whose tenant is not the tenant the gateway is bound to now, or whose binding is
//! not the gateway's current one, is recorded but changes no configuration: generating in the
//! bound tenant's transaction could not even see it. A configuration that cannot be built (a
//! missing model, a rule broken) is logged at ERROR, counted, and not stored, so the gateway
//! keeps its previous version; the event still counts as processed, and the event that repairs
//! the projection (the model arriving, the device changing) regenerates the gateway.
//!
//! Versions: a gateway's next version is its latest version, whichever tenant holds it, plus
//! one, so versions rise across bindings and a gateway never sees a lower one for newer content.
//! Only this consumer writes configurations, and the subscription's lock lets one delivery run at
//! a time across replicas, so the latest version read in step 1 cannot change before the write.
//! The cross-tenant unique index on `(hardware_id, version)` turns any break of that rule into a
//! retried database error instead of two configurations with one version.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use contract::events::assets::{
    DEVICE_AGGREGATE, DEVICE_MODEL_AGGREGATE, DeviceCreated, DeviceDeleted, DeviceModelCreated,
    DeviceModelDeleted, DeviceModelUpdated, DeviceUpdated, GATEWAY_BINDING_AGGREGATE, GatewayBound,
};
use contract::{DeviceId, DeviceModelId, EventId, GatewayBindingId, ServiceName, TenantId};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use outbox::{Delivery, Handler, HandlerError, Position};
use protocol::config::{DesiredConfig, Device, DeviceModel};
use protocol::{ConfigVersion, GatewayId, ModelName};
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{PlatformActor, PlatformDb, PlatformReader};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Postgres, Row, Transaction};

use crate::generate::{self, Content, DeviceRow, GenerationError, ModelRow};

/// The consumer's subscription to the assets service's events; never renamed (a new name starts
/// reading the log from the beginning again).
pub const SUBSCRIPTION: &str = "gateway-ops.assets-events";

/// The producer's schema.
pub const SOURCE_SCHEMA: &str = "assets";

/// The purpose the handler's platform role reads name in the log.
pub const READ_PURPOSE: &str = "gateway-config";

/// SQLSTATE `check_violation`.
const CHECK_VIOLATION: &str = "23514";

/// Each gateway of `$1` (hardware IDs) with its binding whose event comes last in the assets
/// service's log, and its latest configuration, across tenants (the platform role).
const GATEWAY_STATES: &str = "SELECT gateway.hardware_id,
    binding.tenant_id::text AS binding_tenant, binding.binding_id::text AS binding_id,
    binding.position_tx, binding.position_id, binding.unbound_at IS NULL AS binding_current,
    config.version AS config_version, config.tenant_id::text AS config_tenant,
    config.binding_id::text AS config_binding, config.bound AS config_bound
FROM unnest($1::text[]) AS gateway(hardware_id)
LEFT JOIN LATERAL (
    SELECT tenant_id, binding_id, position_tx, position_id, unbound_at
    FROM gateway_ops.gateway_bindings
    WHERE hardware_id = gateway.hardware_id
    ORDER BY position_tx DESC, position_id DESC
    LIMIT 1
) AS binding ON true
LEFT JOIN LATERAL (
    SELECT version, tenant_id, binding_id, bound
    FROM gateway_ops.desired_configs
    WHERE hardware_id = gateway.hardware_id
    ORDER BY version DESC
    LIMIT 1
) AS config ON true";

/// A binding of the transaction's tenant; nothing when the tenant has it already. `$1` tenant,
/// `$2` binding, `$3` hardware ID, `$4` when it was bound, `$5` and `$6` the event's position.
const INSERT_BINDING: &str = "INSERT INTO gateway_ops.gateway_bindings
    (tenant_id, binding_id, hardware_id, bound_at, position_tx, position_id)
VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6)
ON CONFLICT (tenant_id, binding_id) DO NOTHING";

/// The gateway of a recorded binding. `$1` tenant, `$2` binding.
const RECORDED_BINDING: &str = "SELECT hardware_id FROM gateway_ops.gateway_bindings
WHERE tenant_id = $1::uuid AND binding_id = $2::uuid";

/// Ends a binding of the tenant; returns its gateway when it held until now. `$1` tenant, `$2`
/// binding, `$3` when it ended.
const END_BINDING: &str = "UPDATE gateway_ops.gateway_bindings SET unbound_at = $3
WHERE tenant_id = $1::uuid AND binding_id = $2::uuid AND unbound_at IS NULL
RETURNING hardware_id";

/// The recorded name of a model. `$1` tenant, `$2` model.
const RECORDED_MODEL: &str = "SELECT name FROM gateway_ops.device_models
WHERE tenant_id = $1::uuid AND model_id = $2::uuid";

/// A model, replacing the stored one only for a higher revision. `$1` tenant, `$2` model, `$3`
/// name, `$4` revision, `$5` the model as JSON.
const UPSERT_MODEL: &str = "INSERT INTO gateway_ops.device_models AS stored
    (tenant_id, model_id, name, revision, model, deleted)
VALUES ($1::uuid, $2::uuid, $3, $4, $5::jsonb, false)
ON CONFLICT (tenant_id, model_id) DO UPDATE
    SET revision = EXCLUDED.revision, model = EXCLUDED.model, deleted = false,
        updated_at = now()
    WHERE stored.revision < EXCLUDED.revision
RETURNING 1";

/// A model's tombstone, unless a higher revision is stored. `$1` tenant, `$2` model, `$3` name,
/// `$4` its last revision.
const DELETE_MODEL: &str = "INSERT INTO gateway_ops.device_models AS stored
    (tenant_id, model_id, name, revision, model, deleted)
VALUES ($1::uuid, $2::uuid, $3, $4, NULL, true)
ON CONFLICT (tenant_id, model_id) DO UPDATE
    SET revision = EXCLUDED.revision, model = NULL, deleted = true, updated_at = now()
    WHERE stored.revision <= EXCLUDED.revision AND NOT stored.deleted
RETURNING 1";

/// The gateways with a device of a model in the tenant. `$1` tenant, `$2` model.
const GATEWAYS_OF_MODEL: &str = "SELECT DISTINCT hardware_id FROM gateway_ops.devices
WHERE tenant_id = $1::uuid AND model_id = $2::uuid AND NOT deleted
ORDER BY hardware_id";

/// The recorded gateway of a device. `$1` tenant, `$2` device.
const RECORDED_DEVICE: &str = "SELECT hardware_id, binding_id::text AS binding_id
FROM gateway_ops.devices WHERE tenant_id = $1::uuid AND device_id = $2::uuid";

/// A device, replacing the stored one only for a higher revision. `$1` tenant, `$2` device, `$3`
/// hardware ID, `$4` model, `$5` binding (NULL when the event names none), `$6` revision, `$7`
/// the device as JSON.
const UPSERT_DEVICE: &str = "INSERT INTO gateway_ops.devices AS stored
    (tenant_id, device_id, hardware_id, model_id, binding_id, revision, device, deleted)
VALUES ($1::uuid, $2::uuid, $3, $4::uuid, $5::uuid, $6, $7::jsonb, false)
ON CONFLICT (tenant_id, device_id) DO UPDATE
    SET binding_id = COALESCE(EXCLUDED.binding_id, stored.binding_id),
        model_id = EXCLUDED.model_id, revision = EXCLUDED.revision, device = EXCLUDED.device, deleted = false,
        updated_at = now()
    WHERE stored.revision < EXCLUDED.revision
RETURNING 1";

/// A device's tombstone, unless a higher revision is stored. Parameters as [`UPSERT_DEVICE`]
/// without the device.
const DELETE_DEVICE: &str = "INSERT INTO gateway_ops.devices AS stored
    (tenant_id, device_id, hardware_id, model_id, binding_id, revision, device, deleted)
VALUES ($1::uuid, $2::uuid, $3, $4::uuid, $5::uuid, $6, NULL, true)
ON CONFLICT (tenant_id, device_id) DO UPDATE
    SET binding_id = COALESCE(stored.binding_id, EXCLUDED.binding_id),
        revision = EXCLUDED.revision, device = NULL, deleted = true, updated_at = now()
    WHERE stored.revision <= EXCLUDED.revision AND NOT stored.deleted
RETURNING 1";

/// The devices a gateway's configuration for a binding holds: the tenant's live devices on the
/// gateway that belong to the binding, or name none. `$1` tenant, `$2` hardware ID, `$3` binding.
const GATEWAY_DEVICES: &str = "SELECT device_id::text AS device_id, model_id::text AS model_id,
    device::text AS device
FROM gateway_ops.devices
WHERE tenant_id = $1::uuid AND hardware_id = $2 AND NOT deleted
    AND (binding_id IS NULL OR binding_id = $3::uuid)
ORDER BY device_id";

/// The live models of `$2` (model IDs as text) in tenant `$1`.
const MODELS: &str = "SELECT model_id::text AS model_id, model::text AS model
FROM gateway_ops.device_models
WHERE tenant_id = $1::uuid AND model_id = ANY($2::text[]::uuid[]) AND NOT deleted";

/// A stored configuration's payload. `$1` tenant, `$2` hardware ID, `$3` version.
const CONFIG_PAYLOAD: &str = "SELECT payload FROM gateway_ops.desired_configs
WHERE tenant_id = $1::uuid AND hardware_id = $2 AND version = $3";

/// A new configuration version. `$1` tenant, `$2` hardware ID, `$3` version, `$4` binding, `$5`
/// bound, `$6` payload, `$7` device count, `$8` the event.
const INSERT_CONFIG: &str = "INSERT INTO gateway_ops.desired_configs
    (tenant_id, hardware_id, version, binding_id, bound, payload, device_count, event_id)
VALUES ($1::uuid, $2, $3, $4::uuid, $5, $6, $7, $8::uuid)";

/// Why a device event changed no configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ignored {
    /// The gateway has no current binding this service knows of.
    Unbound,
    /// The gateway is bound to another tenant than the event's.
    OtherTenant,
    /// The device belongs to an earlier binding of the gateway in the same tenant.
    EndedBinding,
}

impl Ignored {
    /// The reason for the logs and the metric's `reason` attribute.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::Unbound => "unbound",
            Self::OtherTenant => "other_tenant",
            Self::EndedBinding => "ended_binding",
        }
    }
}

/// A gateway's latest binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LatestBinding {
    tenant_id: TenantId,
    binding_id: GatewayBindingId,
    position: Position,
    current: bool,
}

/// A gateway's latest configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LatestConfig {
    version: ConfigVersion,
    tenant_id: TenantId,
    binding_id: GatewayBindingId,
    bound: bool,
}

/// What the handler knows of a gateway before it writes.
#[derive(Clone, Debug)]
struct GatewayState {
    hardware_id: GatewayId,
    binding: Option<LatestBinding>,
    config: Option<LatestConfig>,
}

impl GatewayState {
    /// The binding the gateway's configuration is made for in `tenant`, if it is bound there now.
    fn current_in(&self, tenant: TenantId) -> Result<GatewayBindingId, Ignored> {
        match self.binding {
            Some(binding) if binding.current && binding.tenant_id == tenant => {
                Ok(binding.binding_id)
            }
            Some(binding) if binding.current => Err(Ignored::OtherTenant),
            _ => Err(Ignored::Unbound),
        }
    }
}

/// The handler's counters (README.md, "指標").
#[derive(Clone, Debug)]
struct Metrics {
    stored: Counter<u64>,
    failed: Counter<u64>,
    ignored: Counter<u64>,
}

impl Metrics {
    fn new() -> Self {
        let meter = opentelemetry::global::meter("gateway-ops");
        Self {
            stored: meter
                .u64_counter("gateway_ops.config.versions")
                .with_description("Desired configuration versions stored")
                .build(),
            failed: meter
                .u64_counter("gateway_ops.config.failures")
                .with_description(
                    "Desired configurations not generated, by reason; the gateway keeps its \
                     previous version",
                )
                .build(),
            ignored: meter
                .u64_counter("gateway_ops.config.ignored_device_events")
                .with_description(
                    "Device events that changed no configuration, by reason (other_tenant: the \
                     gateway is bound to another tenant)",
                )
                .build(),
        }
    }
}

/// What a device model event says.
struct ModelChange<'a> {
    model_id: DeviceModelId,
    name: &'a ModelName,
    revision: u64,
    /// `None` for a deletion.
    model: Option<&'a DeviceModel>,
}

/// What a device event says.
struct DeviceChange<'a> {
    device_id: DeviceId,
    hardware_id: &'a GatewayId,
    model_id: DeviceModelId,
    /// `None` in the events written before 2026-10-29 (README.md, "綁定與租戶").
    binding_id: Option<GatewayBindingId>,
    revision: u64,
    /// `None` for a deletion.
    device: Option<&'a Device>,
}

/// What making a gateway's configuration came to.
#[derive(Clone, Debug, PartialEq)]
enum Made {
    Stored(ConfigVersion),
    Unchanged,
    Failed(GenerationError),
}

/// The consumer's handler; see the module. Cloning is cheap and shares the pool.
#[derive(Clone, Debug)]
pub struct AssetsEventsHandler {
    reader: PlatformReader,
    metrics: Metrics,
}

impl AssetsEventsHandler {
    /// The handler, reading across tenants through `platform` (the platform role's pool) as the
    /// gateway-ops service, purpose [`READ_PURPOSE`]. Create it after service-kit installed the
    /// meter provider (in `start`), so its counters are exported.
    #[must_use]
    pub fn new(platform: &PlatformDb) -> Self {
        let actor = PlatformActor::service(ServiceIdentity::new(ServiceName::GatewayOps));
        Self {
            reader: platform.reader(actor, READ_PURPOSE),
            metrics: Metrics::new(),
        }
    }

    /// Ends `binding_id` of `tenant` at `unbound_at` and, when it was its gateway's current
    /// binding, stores the configuration without devices for the gateway in `tenant`
    /// (README.md, "解綁"). Idempotent: an ended binding is left as it is, and the empty
    /// configuration is stored once. The unbinding event of Laysi/trustiot-v2#115 calls this
    /// from [`Handler::handle`] in its delivery's transaction (`tx`, in `tenant`); until then
    /// only the tests do.
    ///
    /// # Errors
    ///
    /// A [`HandlerError`] to retry when the database fails.
    pub async fn apply_unbinding(
        &self,
        tx: &mut PgConnection,
        tenant: TenantId,
        binding_id: GatewayBindingId,
        unbound_at: DateTime<Utc>,
        event_id: EventId,
    ) -> Result<(), HandlerError> {
        let recorded: Option<String> = sqlx::query_scalar(RECORDED_BINDING)
            .bind(tenant.to_string())
            .bind(binding_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        let Some(recorded) = recorded else {
            tracing::warn!(
                tenant_id = %tenant,
                binding_id = %binding_id,
                event_id = %event_id,
                "unbinding of a binding this service has not recorded; nothing to end"
            );
            return Ok(());
        };
        let hardware_id = GatewayId::new(recorded)
            .map_err(|_| HandlerError::permanent("a recorded binding has an invalid gateway"))?;
        let state = self.single_state(&hardware_id).await?;
        sqlx::query(END_BINDING)
            .bind(tenant.to_string())
            .bind(binding_id.to_string())
            .bind(unbound_at)
            .fetch_optional(&mut *tx)
            .await
            .map_err(write_error)?;
        let was_current = state
            .binding
            .is_some_and(|binding| binding.binding_id == binding_id && binding.tenant_id == tenant);
        if !was_current {
            return Ok(());
        }
        let already_empty = state.config.is_some_and(|config| {
            config.tenant_id == tenant && config.binding_id == binding_id && !config.bound
        });
        let made = if already_empty {
            Made::Unchanged
        } else {
            store(
                tx,
                tenant,
                &state,
                binding_id,
                false,
                &Content::empty(),
                event_id,
            )
            .await?
        };
        self.report(&state.hardware_id, tenant, binding_id, event_id, &made);
        Ok(())
    }

    /// The states of `gateways`, read as the platform role in one read-only transaction.
    async fn states(&self, gateways: &[GatewayId]) -> Result<Vec<GatewayState>, HandlerError> {
        let ids: Vec<String> = gateways.iter().map(|id| id.as_str().to_owned()).collect();
        let mut read = self
            .reader
            .begin()
            .await
            .map_err(|error| HandlerError::retry("platform read failed").with_source(error))?;
        let rows = sqlx::query(GATEWAY_STATES)
            .bind(ids)
            .fetch_all(&mut *read)
            .await?;
        read.commit().await?;
        rows.iter().map(gateway_state).collect()
    }

    async fn single_state(&self, gateway: &GatewayId) -> Result<GatewayState, HandlerError> {
        self.states(std::slice::from_ref(gateway))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| HandlerError::retry("the gateway's state was not read"))
    }

    async fn model_event(
        &self,
        tx: &mut PgConnection,
        tenant: TenantId,
        delivery: &Delivery,
        change: ModelChange<'_>,
    ) -> Result<(), HandlerError> {
        let ModelChange {
            model_id,
            name,
            revision,
            model,
        } = change;
        in_aggregate(delivery, DEVICE_MODEL_AGGREGATE, &model_id.to_string())?;
        if model.is_some_and(|model| &model.name != name) {
            return Err(HandlerError::permanent(
                "a device model event names two names",
            ));
        }
        let revision = stored_revision(revision)?;
        let recorded: Option<String> = sqlx::query_scalar(RECORDED_MODEL)
            .bind(tenant.to_string())
            .bind(model_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        if recorded.is_some_and(|recorded| recorded != name.as_str()) {
            return Err(HandlerError::permanent(
                "a device model event names another name than the recorded one",
            ));
        }
        let gateways: Vec<String> = sqlx::query_scalar(GATEWAYS_OF_MODEL)
            .bind(tenant.to_string())
            .bind(model_id.to_string())
            .fetch_all(&mut *tx)
            .await?;
        let gateways = gateways
            .into_iter()
            .map(GatewayId::new)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| HandlerError::permanent("a recorded device has an invalid gateway"))?;
        let states = if gateways.is_empty() {
            Vec::new()
        } else {
            self.states(&gateways).await?
        };
        let changed = if let Some(model) = model {
            let json = serde_json::to_string(model).map_err(|error| {
                HandlerError::permanent("the model cannot be stored").with_source(error)
            })?;
            sqlx::query(UPSERT_MODEL)
                .bind(tenant.to_string())
                .bind(model_id.to_string())
                .bind(name.as_str())
                .bind(revision)
                .bind(json)
                .fetch_optional(&mut *tx)
                .await
                .map_err(write_error)?
        } else {
            sqlx::query(DELETE_MODEL)
                .bind(tenant.to_string())
                .bind(model_id.to_string())
                .bind(name.as_str())
                .bind(revision)
                .fetch_optional(&mut *tx)
                .await
                .map_err(write_error)?
        }
        .is_some();
        tracing::info!(
            event_type = delivery.envelope.event_type.as_str(),
            event_id = %delivery.envelope.id,
            tenant_id = %tenant,
            model_id = %model_id,
            changed,
            gateways = states.len(),
            "applied assets device model event"
        );
        for state in &states {
            if let Ok(binding) = state.current_in(tenant) {
                let made = make(tx, tenant, state, binding, delivery.envelope.id).await?;
                self.report(
                    &state.hardware_id,
                    tenant,
                    binding,
                    delivery.envelope.id,
                    &made,
                );
            }
        }
        Ok(())
    }

    async fn device_event(
        &self,
        tx: &mut PgConnection,
        tenant: TenantId,
        delivery: &Delivery,
        change: DeviceChange<'_>,
    ) -> Result<(), HandlerError> {
        let DeviceChange {
            device_id,
            hardware_id,
            model_id,
            binding_id,
            revision,
            device,
        } = change;
        in_aggregate(delivery, DEVICE_AGGREGATE, &device_id.to_string())?;
        if device.is_some_and(|device| device.id.as_str() != device_id.to_string()) {
            return Err(HandlerError::permanent(
                "a device event's device has another ID than the event",
            ));
        }
        let revision = stored_revision(revision)?;
        let recorded = sqlx::query(RECORDED_DEVICE)
            .bind(tenant.to_string())
            .bind(device_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        let mut device_binding = binding_id;
        if let Some(recorded) = recorded {
            let recorded_gateway: String = recorded.try_get("hardware_id")?;
            if recorded_gateway != hardware_id.as_str() {
                return Err(HandlerError::permanent(
                    "a device event names another gateway than the recorded one",
                ));
            }
            let recorded_binding: Option<String> = recorded.try_get("binding_id")?;
            let recorded_binding = recorded_binding
                .map(|text| text.parse::<GatewayBindingId>())
                .transpose()
                .map_err(|_| HandlerError::permanent("a recorded device has an invalid binding"))?;
            match (binding_id, recorded_binding) {
                (Some(sent), Some(stored)) if sent != stored => {
                    return Err(HandlerError::permanent(
                        "a device event names another binding than the recorded one",
                    ));
                }
                (None, stored) => device_binding = stored,
                _ => {}
            }
        }
        let state = self.single_state(hardware_id).await?;
        let binding_text = binding_id.map(|binding| binding.to_string());
        let changed = if let Some(device) = device {
            let json = serde_json::to_string(device).map_err(|error| {
                HandlerError::permanent("the device cannot be stored").with_source(error)
            })?;
            sqlx::query(UPSERT_DEVICE)
                .bind(tenant.to_string())
                .bind(device_id.to_string())
                .bind(hardware_id.as_str())
                .bind(model_id.to_string())
                .bind(binding_text)
                .bind(revision)
                .bind(json)
                .fetch_optional(&mut *tx)
                .await
                .map_err(write_error)?
        } else {
            sqlx::query(DELETE_DEVICE)
                .bind(tenant.to_string())
                .bind(device_id.to_string())
                .bind(hardware_id.as_str())
                .bind(model_id.to_string())
                .bind(binding_text)
                .bind(revision)
                .fetch_optional(&mut *tx)
                .await
                .map_err(write_error)?
        }
        .is_some();
        tracing::info!(
            event_type = delivery.envelope.event_type.as_str(),
            event_id = %delivery.envelope.id,
            tenant_id = %tenant,
            device_id = %device_id,
            hardware_id = hardware_id.as_str(),
            changed,
            "applied assets device event"
        );
        let relevant = state
            .current_in(tenant)
            .and_then(|current| match device_binding {
                Some(binding) if binding != current => Err(Ignored::EndedBinding),
                _ => Ok(current),
            });
        match relevant {
            Ok(binding) => {
                let made = make(tx, tenant, &state, binding, delivery.envelope.id).await?;
                self.report(hardware_id, tenant, binding, delivery.envelope.id, &made);
            }
            Err(ignored) => {
                self.metrics
                    .ignored
                    .add(1, &[KeyValue::new("reason", ignored.reason())]);
                if ignored == Ignored::OtherTenant {
                    tracing::warn!(
                        event_type = delivery.envelope.event_type.as_str(),
                        event_id = %delivery.envelope.id,
                        tenant_id = %tenant,
                        device_id = %device_id,
                        hardware_id = hardware_id.as_str(),
                        reason = ignored.reason(),
                        "device event ignored for the gateway configuration"
                    );
                } else {
                    tracing::info!(
                        event_type = delivery.envelope.event_type.as_str(),
                        event_id = %delivery.envelope.id,
                        tenant_id = %tenant,
                        device_id = %device_id,
                        hardware_id = hardware_id.as_str(),
                        reason = ignored.reason(),
                        "device event ignored for the gateway configuration"
                    );
                }
            }
        }
        Ok(())
    }

    async fn bound_event(
        &self,
        tx: &mut PgConnection,
        tenant: TenantId,
        delivery: &Delivery,
        bound: &GatewayBound,
    ) -> Result<(), HandlerError> {
        in_aggregate(
            delivery,
            GATEWAY_BINDING_AGGREGATE,
            &bound.binding_id.to_string(),
        )?;
        let state = self.single_state(&bound.hardware_id).await?;
        let inserted = sqlx::query(INSERT_BINDING)
            .bind(tenant.to_string())
            .bind(bound.binding_id.to_string())
            .bind(bound.hardware_id.as_str())
            .bind(delivery.envelope.occurred_at)
            .bind(delivery.position.tx())
            .bind(delivery.position.id())
            .execute(&mut *tx)
            .await
            .map_err(write_error)?;
        let created = inserted.rows_affected() == 1;
        if !created {
            let recorded: String = sqlx::query_scalar(RECORDED_BINDING)
                .bind(tenant.to_string())
                .bind(bound.binding_id.to_string())
                .fetch_one(&mut *tx)
                .await?;
            if recorded != bound.hardware_id.as_str() {
                return Err(HandlerError::permanent(
                    "assets.gateway_bound names a recorded binding with another gateway",
                ));
            }
        }
        // The binding is the gateway's current one when its event is the latest in the log (the
        // platform read cannot see this transaction's insert), or when it is the latest recorded
        // one and has not ended (the same event again).
        let current = match state.binding {
            None => true,
            Some(latest) if latest.binding_id == bound.binding_id => latest.current,
            Some(latest) => delivery.position > latest.position,
        };
        tracing::info!(
            hardware_id = bound.hardware_id.as_str(),
            binding_id = %bound.binding_id,
            tenant_id = %tenant,
            event_id = %delivery.envelope.id,
            created,
            current,
            "applied assets.gateway_bound"
        );
        if current {
            let made = make(tx, tenant, &state, bound.binding_id, delivery.envelope.id).await?;
            self.report(
                &bound.hardware_id,
                tenant,
                bound.binding_id,
                delivery.envelope.id,
                &made,
            );
        }
        Ok(())
    }

    /// Logs and counts what making a configuration came to.
    fn report(
        &self,
        hardware_id: &GatewayId,
        tenant: TenantId,
        binding: GatewayBindingId,
        event_id: EventId,
        made: &Made,
    ) {
        match made {
            Made::Stored(version) => {
                self.metrics.stored.add(1, &[]);
                tracing::info!(
                    hardware_id = hardware_id.as_str(),
                    tenant_id = %tenant,
                    binding_id = %binding,
                    event_id = %event_id,
                    version = version.get(),
                    "gateway configuration stored"
                );
            }
            Made::Unchanged => tracing::info!(
                hardware_id = hardware_id.as_str(),
                tenant_id = %tenant,
                binding_id = %binding,
                event_id = %event_id,
                "gateway configuration unchanged"
            ),
            Made::Failed(error) => {
                self.metrics
                    .failed
                    .add(1, &[KeyValue::new("reason", error.reason())]);
                tracing::error!(
                    hardware_id = hardware_id.as_str(),
                    tenant_id = %tenant,
                    binding_id = %binding,
                    event_id = %event_id,
                    reason = error.reason(),
                    error = %error,
                    "gateway configuration not generated; the gateway keeps its previous version"
                );
            }
        }
    }
}

impl Handler for AssetsEventsHandler {
    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        let envelope = &delivery.envelope;
        let followed = [
            DEVICE_MODEL_AGGREGATE,
            DEVICE_AGGREGATE,
            GATEWAY_BINDING_AGGREGATE,
        ]
        .contains(&delivery.aggregate.kind());
        let known = envelope.is::<DeviceModelCreated>()
            || envelope.is::<DeviceModelUpdated>()
            || envelope.is::<DeviceModelDeleted>()
            || envelope.is::<DeviceCreated>()
            || envelope.is::<DeviceUpdated>()
            || envelope.is::<DeviceDeleted>()
            || envelope.is::<GatewayBound>();
        if !known {
            // An event of a followed aggregate this build does not know (the unbinding of
            // Laysi/trustiot-v2#115, when assets ships first) changes a configuration:
            // acknowledging it would lose it for good. Retrying holds the subscription on it,
            // where the consumer's lag shows, until a build that knows it runs.
            if followed {
                return Err(HandlerError::retry(
                    "an assets model, device or binding event this build does not know",
                ));
            }
            return Ok(());
        }
        // Models, devices and bindings are tenant data, written in the tenant's transaction; an
        // event without a tenant is not what the assets service writes.
        let Some(tenant) = delivery.tenant() else {
            return Err(HandlerError::permanent(
                "an assets model, device or binding event without a tenant",
            ));
        };
        let tx: &mut PgConnection = tx;
        if envelope.is::<DeviceModelCreated>() {
            let event: DeviceModelCreated = envelope.decode()?;
            let name = event.model.name.clone();
            self.model_event(
                tx,
                tenant,
                delivery,
                ModelChange {
                    model_id: event.model_id,
                    name: &name,
                    revision: event.revision,
                    model: Some(&event.model),
                },
            )
            .await
        } else if envelope.is::<DeviceModelUpdated>() {
            let event: DeviceModelUpdated = envelope.decode()?;
            let name = event.model.name.clone();
            self.model_event(
                tx,
                tenant,
                delivery,
                ModelChange {
                    model_id: event.model_id,
                    name: &name,
                    revision: event.revision,
                    model: Some(&event.model),
                },
            )
            .await
        } else if envelope.is::<DeviceModelDeleted>() {
            let event: DeviceModelDeleted = envelope.decode()?;
            self.model_event(
                tx,
                tenant,
                delivery,
                ModelChange {
                    model_id: event.model_id,
                    name: &event.name,
                    revision: event.revision,
                    model: None,
                },
            )
            .await
        } else if envelope.is::<DeviceCreated>() {
            let event: DeviceCreated = envelope.decode()?;
            self.device_event(
                tx,
                tenant,
                delivery,
                DeviceChange {
                    device_id: event.device_id,
                    hardware_id: &event.hardware_id,
                    model_id: event.model_id,
                    binding_id: event.binding_id,
                    revision: event.revision,
                    device: Some(&event.device),
                },
            )
            .await
        } else if envelope.is::<DeviceUpdated>() {
            let event: DeviceUpdated = envelope.decode()?;
            self.device_event(
                tx,
                tenant,
                delivery,
                DeviceChange {
                    device_id: event.device_id,
                    hardware_id: &event.hardware_id,
                    model_id: event.model_id,
                    binding_id: event.binding_id,
                    revision: event.revision,
                    device: Some(&event.device),
                },
            )
            .await
        } else if envelope.is::<DeviceDeleted>() {
            let event: DeviceDeleted = envelope.decode()?;
            self.device_event(
                tx,
                tenant,
                delivery,
                DeviceChange {
                    device_id: event.device_id,
                    hardware_id: &event.hardware_id,
                    model_id: event.model_id,
                    binding_id: event.binding_id,
                    revision: event.revision,
                    device: None,
                },
            )
            .await
        } else {
            let event: GatewayBound = envelope.decode()?;
            self.bound_event(tx, tenant, delivery, &event).await
        }
    }
}

/// Refuses an event outside its own aggregate: the aggregate is what orders an entity's events.
fn in_aggregate(delivery: &Delivery, kind: &str, id: &str) -> Result<(), HandlerError> {
    if delivery.aggregate.kind() == kind && delivery.aggregate.id() == id {
        Ok(())
    } else {
        Err(HandlerError::permanent(
            "an assets event outside its entity's aggregate",
        ))
    }
}

/// A revision as the tables store it: 1 to `i64::MAX`.
fn stored_revision(revision: u64) -> Result<i64, HandlerError> {
    i64::try_from(revision)
        .ok()
        .filter(|revision| *revision >= 1)
        .ok_or_else(|| HandlerError::permanent("an assets event's revision is out of range"))
}

fn gateway_state(row: &PgRow) -> Result<GatewayState, HandlerError> {
    fn corrupt<E>(_: E) -> HandlerError {
        HandlerError::permanent("a stored binding or configuration breaks the rules")
    }
    let hardware_id: String = row.try_get("hardware_id")?;
    let hardware_id = GatewayId::new(hardware_id)
        .map_err(|_| HandlerError::permanent("an invalid gateway was asked for"))?;
    let binding = match row.try_get::<Option<String>, _>("binding_tenant")? {
        None => None,
        Some(tenant) => Some(LatestBinding {
            tenant_id: tenant.parse().map_err(corrupt)?,
            binding_id: row
                .try_get::<String, _>("binding_id")?
                .parse()
                .map_err(corrupt)?,
            position: Position::new(row.try_get("position_tx")?, row.try_get("position_id")?),
            current: row.try_get("binding_current")?,
        }),
    };
    let config = match row.try_get::<Option<i64>, _>("config_version")? {
        None => None,
        Some(version) => Some(LatestConfig {
            version: u64::try_from(version)
                .ok()
                .and_then(ConfigVersion::new)
                .ok_or_else(|| corrupt(()))?,
            tenant_id: row
                .try_get::<String, _>("config_tenant")?
                .parse()
                .map_err(corrupt)?,
            binding_id: row
                .try_get::<String, _>("config_binding")?
                .parse()
                .map_err(corrupt)?,
            bound: row.try_get("config_bound")?,
        }),
    };
    Ok(GatewayState {
        hardware_id,
        binding,
        config,
    })
}

/// Makes the gateway's configuration for `binding` of `tenant` from the tenant's rows and stores
/// it when it differs from the latest version.
async fn make(
    tx: &mut PgConnection,
    tenant: TenantId,
    state: &GatewayState,
    binding: GatewayBindingId,
    event_id: EventId,
) -> Result<Made, HandlerError> {
    let content = match read_content(tx, tenant, &state.hardware_id, binding).await? {
        Ok(content) => content,
        Err(error) => return Ok(Made::Failed(error)),
    };
    if let Some(latest) = state.config
        && latest.tenant_id == tenant
        && latest.binding_id == binding
        && latest.bound
    {
        let payload: Option<Vec<u8>> = sqlx::query_scalar(CONFIG_PAYLOAD)
            .bind(tenant.to_string())
            .bind(state.hardware_id.as_str())
            .bind(version_column(latest.version)?)
            .fetch_optional(&mut *tx)
            .await?;
        let unchanged = payload
            .and_then(|payload| DesiredConfig::decode(&payload).ok())
            .is_some_and(|stored| content.same_as(&stored));
        if unchanged {
            return Ok(Made::Unchanged);
        }
    }
    store(tx, tenant, state, binding, true, &content, event_id).await
}

/// Stores `content` as the gateway's next version.
async fn store(
    tx: &mut PgConnection,
    tenant: TenantId,
    state: &GatewayState,
    binding: GatewayBindingId,
    bound: bool,
    content: &Content,
    event_id: EventId,
) -> Result<Made, HandlerError> {
    let next = state
        .config
        .map_or(Some(1), |latest| latest.version.get().checked_add(1))
        .and_then(ConfigVersion::new)
        .ok_or_else(|| {
            HandlerError::permanent("the gateway's configuration version is exhausted")
        })?;
    let payload = match generate::encode(content, next) {
        Ok(payload) => payload,
        Err(error) => return Ok(Made::Failed(error)),
    };
    let devices = i32::try_from(content.devices.len())
        .map_err(|_| HandlerError::permanent("too many devices"))?;
    sqlx::query(INSERT_CONFIG)
        .bind(tenant.to_string())
        .bind(state.hardware_id.as_str())
        .bind(version_column(next)?)
        .bind(binding.to_string())
        .bind(bound)
        .bind(payload)
        .bind(devices)
        .bind(event_id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(write_error)?;
    Ok(Made::Stored(next))
}

fn version_column(version: ConfigVersion) -> Result<i64, HandlerError> {
    i64::try_from(version.get())
        .map_err(|_| HandlerError::permanent("the gateway's configuration version is exhausted"))
}

/// The tenant's devices of `binding` on the gateway, with their models, as one configuration's
/// content; the inner error when it cannot be built.
async fn read_content(
    tx: &mut PgConnection,
    tenant: TenantId,
    hardware_id: &GatewayId,
    binding: GatewayBindingId,
) -> Result<Result<Content, GenerationError>, HandlerError> {
    let rows = sqlx::query(GATEWAY_DEVICES)
        .bind(tenant.to_string())
        .bind(hardware_id.as_str())
        .bind(binding.to_string())
        .fetch_all(&mut *tx)
        .await?;
    let mut devices = Vec::with_capacity(rows.len());
    for row in &rows {
        let Ok(device) = device_row(row) else {
            return Ok(Err(GenerationError::Corrupt("device")));
        };
        devices.push(device);
    }
    let mut model_ids: Vec<String> = devices.iter().map(|d| d.model_id.to_string()).collect();
    model_ids.sort();
    model_ids.dedup();
    let rows = sqlx::query(MODELS)
        .bind(tenant.to_string())
        .bind(model_ids)
        .fetch_all(&mut *tx)
        .await?;
    let mut models = HashMap::with_capacity(rows.len());
    for row in &rows {
        let Ok(model) = model_row(row) else {
            return Ok(Err(GenerationError::Corrupt("device model")));
        };
        models.insert(model.model_id, model);
    }
    Ok(generate::build(devices, &models))
}

fn device_row(row: &PgRow) -> Result<DeviceRow, Box<dyn std::error::Error>> {
    Ok(DeviceRow {
        device_id: row.try_get::<String, _>("device_id")?.parse()?,
        model_id: row.try_get::<String, _>("model_id")?.parse()?,
        device: serde_json::from_str(&row.try_get::<String, _>("device")?)?,
    })
}

fn model_row(row: &PgRow) -> Result<ModelRow, Box<dyn std::error::Error>> {
    Ok(ModelRow {
        model_id: row.try_get::<String, _>("model_id")?.parse()?,
        model: serde_json::from_str(&row.try_get::<String, _>("model")?)?,
    })
}

/// The error for a failed write. A value a table refuses (SQLSTATE class 22, data exception, or
/// 23514, a CHECK) fails the same way on every attempt, so the event is dead-lettered and the
/// subscription moves on; every other database error is retried (`outbox`'s rule), a version
/// taken twice (23505) included, which the next attempt reads again.
fn write_error(error: sqlx::Error) -> HandlerError {
    let refused = error
        .as_database_error()
        .and_then(|database| database.code())
        .is_some_and(|code| code.starts_with("22") || code == CHECK_VIOLATION);
    if refused {
        HandlerError::permanent("the projection refuses the event's values").with_source(error)
    } else {
        HandlerError::from(error)
    }
}
