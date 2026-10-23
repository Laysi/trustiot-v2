-- Telemetry-query service schema (Laysi/trustiot-v2#119; decisions 14 and 29). Run by the
-- migration role `telemetry_query_owner`, which owns everything here; the service role
-- `telemetry_query_app` and the platform role `telemetry_query_platform` own nothing
-- (crates/service-kit/README.md, "角色與 policy 範本"). The migration Job (tools/migrate) creates
-- the roles and the schema `telemetry_query` owned by `telemetry_query_owner`, creates the outbox
-- consumer tables (`outbox_subscription`, `outbox_inbox`, `outbox_dead_letter`, `outbox_version`)
-- because the registry lists telemetry-query as a consumer of identity's events, makes
-- `telemetry_query_platform` a reader of `identity.outbox_event`, and applies these files in
-- order as `telemetry_query_owner`, each in one transaction.

GRANT USAGE ON SCHEMA telemetry_query TO telemetry_query_app;

-- The tenants whose read-only ClickHouse account exists: one row per tenant, written by the
-- consumer of `identity.tenant_created` once the account logs in with the tenant's setting
-- (services/telemetry-query/README.md). The account's name and password are derived from the
-- tenant ID and are not stored. Tenant data, so it follows crates/service-kit/sql/tenant-table.sql,
-- with the tenant ID as the whole primary key: a replayed or repeated event cannot add a second
-- row.
CREATE TABLE telemetry_query.tenant_accounts (
    tenant_id uuid PRIMARY KEY,
    -- When the account was first found ready, by the database clock.
    provisioned_at timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE telemetry_query.tenant_accounts ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON telemetry_query.tenant_accounts
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- The consumer inserts (`ON CONFLICT DO NOTHING` needs only INSERT) and the API reads. No
-- platform access: nothing reads this table across tenants, so `telemetry_query_platform` gets
-- no grant on it (crates/service-kit/sql/tenant-table.sql, step 4).
GRANT SELECT, INSERT ON telemetry_query.tenant_accounts TO telemetry_query_app;
