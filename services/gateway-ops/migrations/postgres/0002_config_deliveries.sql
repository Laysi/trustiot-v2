-- The delivery of every configuration version and the gateways' apply reports
-- (Laysi/trustiot-v2#720; decision 7). Run by `gateway_ops_owner` like 0001; the table is tenant
-- data and follows crates/service-kit/sql/tenant-table.sql.
--
-- One row per configuration version that was published or reported on, in the tenant of that
-- version (`desired_configs`), so a gateway's reports about one tenant's configuration stay that
-- tenant's data. The delivery and the report handling write it as `gateway_ops_app` in the
-- version's tenant transaction (services/gateway-ops/README.md, "下發", "套用回報").

CREATE TABLE gateway_ops.config_deliveries (
    tenant_id uuid NOT NULL,
    hardware_id text NOT NULL CHECK (hardware_id COLLATE "C" ~ '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'),
    version bigint NOT NULL CHECK (version >= 1),
    -- The first and the latest retained publish of this version the broker acknowledged
    -- (PUBACK); NULL while none was.
    first_published_at timestamptz,
    last_published_at timestamptz,
    -- The gateway's latest report about this version (crates/protocol's `ConfigReport`): what it
    -- did with it, the version it polls with afterwards, why it rejected it, and when the
    -- service received the report (the service's clock); all NULL while it sent none.
    outcome text CHECK (outcome IN ('applied', 'rejected')),
    active_version bigint CHECK (active_version >= 1),
    issues jsonb CHECK (jsonb_typeof(issues) = 'array'),
    reported_at timestamptz,
    PRIMARY KEY (tenant_id, hardware_id, version),
    -- Only versions that exist, in their own tenant.
    FOREIGN KEY (tenant_id, hardware_id, version)
        REFERENCES gateway_ops.desired_configs (tenant_id, hardware_id, version),
    CHECK ((first_published_at IS NULL) = (last_published_at IS NULL)),
    CHECK ((outcome IS NULL) = (reported_at IS NULL)),
    CHECK (outcome IS NOT NULL OR (active_version IS NULL AND issues IS NULL))
);

ALTER TABLE gateway_ops.config_deliveries ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON gateway_ops.config_deliveries
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- The service role records publishes and reports; the key never changes. The platform role has
-- no access: the delivery and the report handling find a version's tenant in `desired_configs`.
GRANT SELECT, INSERT ON gateway_ops.config_deliveries TO gateway_ops_app;
GRANT UPDATE (first_published_at, last_published_at, outcome, active_version, issues, reported_at)
    ON gateway_ops.config_deliveries TO gateway_ops_app;
