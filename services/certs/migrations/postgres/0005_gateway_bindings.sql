-- The certs service's copy of which tenant each gateway is bound to, and of each tenant's status
-- (Laysi/trustiot-v2#632; services/certs/README.md, "閘道器的綁定與租戶狀態"). Run by
-- `certs_owner` like the files before it, in one transaction.
--
-- A gateway certificate carries the hardware ID only, never a tenant: a gateway's tenant is its
-- current binding in the assets service, and each service keeps its own copy from the events
-- (2026-10-24). The renewal (Laysi/trustiot-v2#81) and the broker's permissions
-- (Laysi/trustiot-v2#633) read these tables, so neither calls another service on its request
-- path; suspension (Laysi/trustiot-v2#107, Laysi/trustiot-v2#108) will write the tenant status.
--
-- From this release the migration Job also makes `certs_platform` a reader of
-- `assets.outbox_event` and creates the outbox consumer's tables in this schema
-- (tools/migrate/src/registry.rs): the consumer of `assets.gateway_bound` fills
-- `gateway_bindings`.
--
-- Both tables hold tenant ids, so they are tenant tables under decision 29 and follow
-- crates/service-kit/sql/tenant-table.sql: `tenant_id` leads the primary key, and
-- `tenant_isolation` limits every role to the transaction's tenant. The consumer writes in the
-- event's tenant transaction as `certs_app`; the reads that need every tenant (the renewal has
-- no tenant identity, the broker sync compares all gateways) are the platform role's, read only.

-- One row per binding the assets service made: `assets.gateway_bound` carries a new binding ID
-- for every binding, unique across tenants. A row is never rewritten: a replayed or repeated event
-- inserts nothing (ON CONFLICT DO NOTHING on the primary key). A gateway's current binding is its
-- binding whose event comes last in the assets service's log (`position_tx`, `position_id`, the
-- event's crates/outbox position) and has no `unbound_at`: a binding is made only after the
-- gateway's previous binding ended, in a later transaction, so its event comes later in the log,
-- whatever the clocks say. An older binding that arrives late (a replayed dead letter) is stored
-- but never becomes current.
--
-- Unbinding (Laysi/trustiot-v2#115) will set `unbound_at` on the binding's row from its own event,
-- in the same tenant (the binding's aggregate); no data needs migrating for that. Nothing sets
-- the column yet, so `certs_app` gets no UPDATE: that issue grants it.
CREATE TABLE certs.gateway_bindings (
    tenant_id uuid NOT NULL,
    -- The assets service's binding ID (`contract::GatewayBindingId`).
    binding_id uuid NOT NULL,
    -- protocol::GatewayId: the certificates' common name and the MQTT username.
    hardware_id text NOT NULL
        CHECK (hardware_id COLLATE "C" ~ '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'),
    -- When the assets service bound it: the event's `occurred_at`. For reading only; the order
    -- of bindings is the log's.
    bound_at timestamptz NOT NULL,
    -- The event's position in `assets.outbox_event` (`tx_order`, `id`).
    position_tx bigint NOT NULL CHECK (position_tx >= 0),
    position_id bigint NOT NULL CHECK (position_id >= 0),
    -- Set when the binding ends (Laysi/trustiot-v2#115); NULL while it holds. Not compared with
    -- `bound_at`: the two come from different replicas' clocks, and the order is the log's, so a
    -- skewed clock must not refuse (and dead-letter) an unbinding.
    unbound_at timestamptz,
    -- When this service stored it, by the database clock.
    recorded_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, binding_id)
);

-- The platform role's reads look a gateway up by hardware ID across tenants and take its latest
-- binding. The one deliberate exception to "tenant_id leads every index": the lookup is across
-- tenants by design, and only the platform role makes it (row-level security limits a tenant
-- transaction to its own rows whatever the index).
CREATE INDEX gateway_bindings_latest
    ON certs.gateway_bindings (hardware_id, position_tx DESC, position_id DESC);

ALTER TABLE certs.gateway_bindings ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON certs.gateway_bindings
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- Every tenant's bindings for the platform role, read only.
CREATE POLICY platform_access ON certs.gateway_bindings
    FOR SELECT
    TO certs_platform
    USING (true);

-- Each tenant's status as this service knows it: active or suspended, with the version of the
-- state it was read from (rebuild checklist B1, notification mode: only a newer version replaces
-- the stored one, so a late or replayed older state never overwrites a newer one). A tenant
-- without a row is active. Nothing writes it yet: Laysi/trustiot-v2#107 and
-- Laysi/trustiot-v2#108 connect the tenant's real status; until then only tests write it.
CREATE TABLE certs.tenant_statuses (
    tenant_id uuid PRIMARY KEY,
    status text NOT NULL CHECK (status IN ('active', 'suspended')),
    version bigint NOT NULL CHECK (version >= 0),
    -- When this service stored the current version, by the database clock.
    updated_at timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE certs.tenant_statuses ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON certs.tenant_statuses
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- Every tenant's status for the platform role, read only.
CREATE POLICY platform_access ON certs.tenant_statuses
    FOR SELECT
    TO certs_platform
    USING (true);

-- `certs_app` inserts bindings in the event's tenant (no SELECT: ON CONFLICT DO NOTHING without
-- a conflict target needs none, and nothing of the service reads bindings in a tenant's
-- transaction), and stores its tenant's status with the version check, which reads the stored
-- version (SELECT) and replaces it (UPDATE); it deletes nothing. `certs_platform` only reads.
GRANT INSERT ON certs.gateway_bindings TO certs_app;
GRANT SELECT, INSERT, UPDATE ON certs.tenant_statuses TO certs_app;
GRANT SELECT ON certs.gateway_bindings TO certs_platform;
GRANT SELECT ON certs.tenant_statuses TO certs_platform;
