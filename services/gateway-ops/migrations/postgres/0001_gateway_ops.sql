-- Gateway operations service schema (Laysi/trustiot-v2#719; decisions 4, 7 and 29). Run by the
-- migration role `gateway_ops_owner`, which owns everything here; the service role
-- `gateway_ops_app` and the platform role `gateway_ops_platform` own nothing
-- (crates/service-kit/README.md, "角色與 policy 範本"). The migration Job (tools/migrate) creates
-- the roles and the schema `gateway_ops` owned by `gateway_ops_owner`, creates the outbox consumer
-- tables because the registry lists gateway-ops as a consumer of the assets service's events,
-- makes `gateway_ops_platform` a reader of `assets.outbox_event`, and applies these files in
-- order as `gateway_ops_owner`, each in one transaction.
--
-- Every table here is tenant data and follows crates/service-kit/sql/tenant-table.sql:
-- `tenant_id` leads the primary key, and `tenant_isolation` limits every role to the
-- transaction's tenant. The consumer writes each event's rows in the event's tenant transaction
-- (services/gateway-ops/README.md, "資料表").

GRANT USAGE ON SCHEMA gateway_ops TO gateway_ops_app;
GRANT USAGE ON SCHEMA gateway_ops TO gateway_ops_platform;

-- Which tenant each gateway is bound to, projected from `assets.gateway_bound`, as ingest
-- (services/ingest/migrations/postgres/0001_ingest.sql) and certs keep it: one row per binding,
-- in the binding's tenant. A gateway's current binding is the one whose event comes last in the
-- assets service's log (`position_tx`, `position_id`), and only while `unbound_at` is NULL.
CREATE TABLE gateway_ops.gateway_bindings (
    tenant_id uuid NOT NULL,
    binding_id uuid NOT NULL,
    -- crates/protocol's `GatewayId`.
    hardware_id text NOT NULL CHECK (hardware_id COLLATE "C" ~ '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'),
    -- The event's `occurred_at`, for reading only: it never orders bindings.
    bound_at timestamptz NOT NULL,
    -- The event's position in `assets.outbox_event`.
    position_tx bigint NOT NULL CHECK (position_tx >= 0),
    position_id bigint NOT NULL CHECK (position_id >= 0),
    -- Set when the binding ends (`projection::apply_unbinding`, called by the unbinding event's
    -- handler of Laysi/trustiot-v2#115); NULL while it holds. Not compared with `bound_at`.
    unbound_at timestamptz,
    recorded_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, binding_id)
);

-- A gateway's latest binding across tenants (the platform role). The one deliberate exception to
-- "tenant_id leads every index": which tenant holds a gateway is a question across tenants.
CREATE INDEX gateway_bindings_by_gateway
    ON gateway_ops.gateway_bindings (hardware_id, position_tx DESC, position_id DESC);

ALTER TABLE gateway_ops.gateway_bindings ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON gateway_ops.gateway_bindings
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- The consumer asks, before it writes a gateway's configuration, which tenant holds the gateway
-- now; it is the platform role's read-only question.
CREATE POLICY platform_access ON gateway_ops.gateway_bindings
    FOR SELECT
    TO gateway_ops_platform
    USING (true);

-- `gateway_ops_app` records bindings (INSERT, and SELECT to compare a repeated event with the row
-- it finds) and ends them: only `unbound_at` may change.
GRANT SELECT, INSERT ON gateway_ops.gateway_bindings TO gateway_ops_app;
GRANT UPDATE (unbound_at) ON gateway_ops.gateway_bindings TO gateway_ops_app;
GRANT SELECT ON gateway_ops.gateway_bindings TO gateway_ops_platform;

-- The tenant's device models, projected from `assets.device_model_*`. A deleted model stays as a
-- tombstone (`deleted`, no `model`) with its last revision, so an older event delivered late (a
-- dead letter retried) cannot bring it back.
CREATE TABLE gateway_ops.device_models (
    tenant_id uuid NOT NULL,
    model_id uuid NOT NULL,
    -- The model's name as the configuration carries it (crates/protocol's `ModelName`).
    name text NOT NULL,
    -- The assets service's revision of the model; only a higher one replaces the row.
    revision bigint NOT NULL CHECK (revision >= 1),
    -- `protocol::config::DeviceModel` as JSON; NULL once the model is deleted.
    model jsonb,
    deleted boolean NOT NULL DEFAULT false,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, model_id),
    CHECK (deleted = (model IS NULL))
);

ALTER TABLE gateway_ops.device_models ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON gateway_ops.device_models
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

GRANT SELECT, INSERT, UPDATE ON gateway_ops.device_models TO gateway_ops_app;

-- The tenant's devices, projected from `assets.device_*`, with the same tombstones as the models.
CREATE TABLE gateway_ops.devices (
    tenant_id uuid NOT NULL,
    device_id uuid NOT NULL,
    -- The gateway that polls it, never changed (crates/protocol's `GatewayId`).
    hardware_id text NOT NULL CHECK (hardware_id COLLATE "C" ~ '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'),
    model_id uuid NOT NULL,
    -- The binding the device belongs to (the events' optional `binding_id`); NULL when only events
    -- written before 2026-10-29 named the device (README.md, "綁定與租戶").
    binding_id uuid,
    revision bigint NOT NULL CHECK (revision >= 1),
    -- `protocol::config::Device` as JSON; NULL once the device is deleted.
    device jsonb,
    deleted boolean NOT NULL DEFAULT false,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, device_id),
    CHECK (deleted = (device IS NULL))
);

-- A gateway's devices, and the devices of a model, within the tenant.
CREATE INDEX devices_by_gateway ON gateway_ops.devices (tenant_id, hardware_id);
CREATE INDEX devices_by_model ON gateway_ops.devices (tenant_id, model_id);

ALTER TABLE gateway_ops.devices ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON gateway_ops.devices
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

GRANT SELECT, INSERT, UPDATE ON gateway_ops.devices TO gateway_ops_app;

-- Every version of every gateway's desired configuration (decision 7: Postgres is the source of
-- truth; the retained MQTT message of Laysi/trustiot-v2#720 is only the delivery). Only appended:
-- a version is never changed or deleted by the service. A configuration is in the tenant of the
-- binding it was made for, so the devices it lists are that tenant's data under that tenant's
-- row-level security; a gateway bound to another tenant later gets its next version in the new
-- tenant.
CREATE TABLE gateway_ops.desired_configs (
    tenant_id uuid NOT NULL,
    hardware_id text NOT NULL CHECK (hardware_id COLLATE "C" ~ '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'),
    -- crates/protocol's `ConfigVersion`: increases by one per gateway, across tenants.
    version bigint NOT NULL CHECK (version >= 1),
    -- The binding the configuration was made for.
    binding_id uuid NOT NULL,
    -- false for the configuration without devices that ends a binding (README.md, "解綁").
    bound boolean NOT NULL,
    -- The encoded `DesiredConfig` (CBOR, crates/protocol's codec) exactly as it is delivered,
    -- with this version inside.
    payload bytea NOT NULL CHECK (octet_length(payload) > 0),
    device_count integer NOT NULL CHECK (device_count >= 0),
    -- The event whose processing made this version.
    event_id uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, hardware_id, version),
    CHECK (bound OR device_count = 0)
);

-- A version is used once per gateway whichever tenant holds it: the second deliberate exception
-- to "tenant_id leads every index", since versions increase across the gateway's bindings. It
-- also serves the platform role's reads of each gateway's latest version.
CREATE UNIQUE INDEX desired_configs_gateway_version
    ON gateway_ops.desired_configs (hardware_id, version DESC);

ALTER TABLE gateway_ops.desired_configs ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON gateway_ops.desired_configs
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- The delivery (Laysi/trustiot-v2#720) publishes per gateway, before it knows a tenant, and the
-- consumer reads a gateway's latest version across tenants: the platform role reads every
-- tenant's configurations, read only.
CREATE POLICY platform_access ON gateway_ops.desired_configs
    FOR SELECT
    TO gateway_ops_platform
    USING (true);

GRANT SELECT, INSERT ON gateway_ops.desired_configs TO gateway_ops_app;
GRANT SELECT ON gateway_ops.desired_configs TO gateway_ops_platform;
