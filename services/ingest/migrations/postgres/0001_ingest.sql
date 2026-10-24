-- Ingest service schema (Laysi/trustiot-v2#640; decisions 4, 8, 12 and 29). Run by the migration
-- role `ingest_owner`, which owns everything here; the service role `ingest_app` and the platform
-- role `ingest_platform` own nothing (crates/service-kit/README.md, "角色與 policy 範本"). The
-- migration Job (tools/migrate) creates the roles and the schema `ingest` owned by
-- `ingest_owner`, creates the outbox consumer tables (`outbox_subscription`, `outbox_inbox`,
-- `outbox_dead_letter`, `outbox_version`) because the registry lists ingest as a consumer of the
-- assets service's events, makes `ingest_platform` a reader of `assets.outbox_event`, and applies
-- these files in order as `ingest_owner`, each in one transaction.

GRANT USAGE ON SCHEMA ingest TO ingest_app;

-- Which tenant each gateway is bound to, projected from `assets.gateway_bound`
-- (services/ingest/README.md, "閘道器綁定對照表"). Telemetry takes its tenant from here and from
-- nothing in an MQTT message (decisions 8 and 12). One row per binding, never per gateway: a
-- binding is the tenant's data and is written in the tenant's transaction (the event's tenant),
-- so a gateway that is unbound and bound to another tenant (Laysi/trustiot-v2#115) gets a second
-- row in that tenant, and the first stays in its own.
--
-- Tenant data, so it follows crates/service-kit/sql/tenant-table.sql: `tenant_id` leads the
-- primary key, and `tenant_isolation` limits every role to the transaction's tenant. The binding
-- ID is unique across tenants at the source; the key repeats the tenant only so that a replayed
-- or repeated event of the tenant finds its own row (`ON CONFLICT DO NOTHING`).
CREATE TABLE ingest.gateway_bindings (
    tenant_id uuid NOT NULL,
    binding_id uuid NOT NULL,
    -- crates/protocol's `GatewayId`: the certificates' common name, the MQTT username and the
    -- `{gateway}` of the telemetry topic.
    hardware_id text NOT NULL CHECK (hardware_id COLLATE "C" ~ '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'),
    -- When the assets service bound it: the event's `occurred_at`. The lookup takes a gateway's
    -- newest binding by this time, so an older binding's event that arrives late (a replay, a
    -- dead letter retried) never replaces a newer one.
    bound_at timestamptz NOT NULL,
    -- Set when the binding ends, by the unbinding event of Laysi/trustiot-v2#115; NULL while it
    -- holds. Nothing writes it yet.
    unbound_at timestamptz CHECK (unbound_at >= bound_at),
    -- When this service recorded the binding, by the database clock.
    recorded_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, binding_id)
);

-- The lookup by hardware ID across tenants (`gateway_tenant`, the platform role). The one
-- deliberate exception to "tenant_id leads every index": the question it answers, which tenant
-- holds this gateway, is across tenants by nature.
CREATE INDEX gateway_bindings_by_gateway ON ingest.gateway_bindings (hardware_id, bound_at DESC, binding_id DESC);

ALTER TABLE ingest.gateway_bindings ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON ingest.gateway_bindings
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- The telemetry path learns a gateway's tenant from the topic's hardware ID, before it knows any
-- tenant, so the platform role reads every tenant's bindings, as the assets service's platform
-- role reads `assets.gateway_bindings`. Read only, and only through `PlatformReader`'s READ ONLY
-- transactions: bindings are written by the service role in the event's tenant.
CREATE POLICY platform_access ON ingest.gateway_bindings
    FOR SELECT
    TO ingest_platform
    USING (true);

-- `ingest_app` records bindings (INSERT, and SELECT to compare a replayed event with the row it
-- finds); nothing updates or deletes one before Laysi/trustiot-v2#115. `ingest_platform` only
-- reads. The schema's USAGE is granted here too, so the lookup does not depend on
-- `ingest_platform` also being a reader of an outbox in this schema (tools/migrate).
GRANT SELECT, INSERT ON ingest.gateway_bindings TO ingest_app;
GRANT USAGE ON SCHEMA ingest TO ingest_platform;
GRANT SELECT ON ingest.gateway_bindings TO ingest_platform;
