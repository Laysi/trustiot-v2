-- The device pool and gateway bindings (Laysi/trustiot-v2#101, rebuild checklist D3;
-- services/assets/README.md, "閘道器設備池與綁定").
--
-- From this release the migration Job also makes `assets_platform` a reader of
-- `certs.outbox_event` (tools/migrate/src/registry.rs), so the service learns every factory
-- registration from `certs.gateway_registered`.

-- Every gateway registered at the factory, one row per hardware ID, whether bound or not. Not
-- tenant data: a gateway belongs to no tenant when it leaves the factory, and which tenant it is
-- bound to is in `gateway_bindings`, under row-level security. So the table has no `tenant_id`
-- and no row-level security, and the roles get only what they need:
--
-- - `assets_app` inserts rows: the consumer of `certs.gateway_registered` runs in a transaction
--   without a tenant (a platform-wide event) and inserts with `ON CONFLICT DO NOTHING`, so a
--   replayed or repeated event adds nothing. It cannot read the table, so no tenant transaction
--   of the API can list the pool; a binding finds its gateway through the foreign key below,
--   which Postgres checks with the owner's rights.
-- - `assets_platform` reads it, for the platform administrators' pool listing.
CREATE TABLE assets.gateway_pool (
    -- crates/protocol's `GatewayId`: the certificates' common name and the MQTT username.
    hardware_id text PRIMARY KEY CHECK (hardware_id COLLATE "C" ~ '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'),
    -- When the certs service registered it: the event's `occurred_at`.
    registered_at timestamptz NOT NULL,
    -- The production workstation that registered it (its certificate's `workstation:<name>`).
    workstation text NOT NULL CHECK (char_length(workstation) BETWEEN 1 AND 255),
    -- When this service put it in the pool, by the database clock.
    pooled_at timestamptz NOT NULL DEFAULT now()
);

-- The bindings of gateways to tenants. Tenant data, so it follows
-- crates/service-kit/sql/tenant-table.sql: `tenant_id` leads the primary key, and
-- `tenant_isolation` limits every role to the transaction's tenant.
--
-- A binding is never deleted or rewritten: unbinding (Laysi/trustiot-v2#115) will set
-- `unbound_at`, and binding the gateway again adds a new row with a new `binding_id`, in
-- whichever tenant it goes to. A gateway has at most one binding without `unbound_at`, across
-- all tenants (`gateway_bindings_one_current`). That index is the one deliberate exception to
-- "tenant_id leads every index": the rule it enforces is across tenants. A tenant transaction
-- that tries to bind a gateway another tenant holds therefore learns that the gateway is bound
-- (the insert conflicts), but not to whom; the rows of other tenants stay invisible.
CREATE TABLE assets.gateway_bindings (
    tenant_id uuid NOT NULL REFERENCES assets.tenants (tenant_id),
    binding_id uuid NOT NULL,
    hardware_id text NOT NULL REFERENCES assets.gateway_pool (hardware_id),
    bound_at timestamptz NOT NULL DEFAULT now(),
    -- Set when the binding ends (Laysi/trustiot-v2#115); NULL while it holds.
    unbound_at timestamptz CHECK (unbound_at >= bound_at),
    PRIMARY KEY (tenant_id, binding_id)
);

CREATE UNIQUE INDEX gateway_bindings_one_current ON assets.gateway_bindings (hardware_id)
    WHERE unbound_at IS NULL;

CREATE INDEX gateway_bindings_by_gateway ON assets.gateway_bindings (tenant_id, hardware_id);

ALTER TABLE assets.gateway_bindings ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON assets.gateway_bindings
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- The platform administrators' pool listing shows which tenant each gateway is bound to, so the
-- platform role reads every tenant's bindings. Read only: bindings are written by the service
-- role in the tenant's transaction, with the event in the same transaction.
CREATE POLICY platform_access ON assets.gateway_bindings
    FOR SELECT
    TO assets_platform
    USING (true);

-- `assets_app` binds (SELECT to find the tenant's current binding, INSERT a new one); nothing
-- updates or deletes a binding yet. `assets_platform` reads the pool and the bindings. The
-- schema's USAGE is granted here too, so the listing does not depend on `assets_platform` also
-- being a reader of the service's own outbox (tools/migrate/src/registry.rs).
GRANT INSERT ON assets.gateway_pool TO assets_app;
GRANT SELECT, INSERT ON assets.gateway_bindings TO assets_app;
GRANT USAGE ON SCHEMA assets TO assets_platform;
GRANT SELECT ON assets.gateway_pool TO assets_platform;
GRANT SELECT ON assets.gateway_bindings TO assets_platform;
