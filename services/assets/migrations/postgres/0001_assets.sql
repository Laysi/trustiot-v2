-- Assets service schema, MS0 skeleton (Laysi/trustiot-v2#65; decisions 4 and 29). Run by the
-- migration role `assets_owner`, which owns everything here; the service role `assets_app` and
-- the platform role `assets_platform` own nothing (crates/service-kit/README.md, "角色與 policy
-- 範本"). The migration Job (tools/migrate) creates the roles and the schema `assets` owned by
-- `assets_owner`, creates the outbox consumer tables (`outbox_subscription`, `outbox_inbox`,
-- `outbox_dead_letter`, `outbox_version`) because the registry lists assets as a consumer of
-- identity's events, makes `assets_platform` a reader of `identity.outbox_event`, and applies
-- these files in order as `assets_owner`, each in one transaction.

GRANT USAGE ON SCHEMA assets TO assets_app;

-- The tenants the service knows: one row per tenant, created by the consumer of
-- `identity.tenant_created` (services/assets/README.md). Tenant data, so it follows
-- crates/service-kit/sql/tenant-table.sql, with the tenant ID as the whole primary key: a
-- replayed or repeated event cannot add a second row.
CREATE TABLE assets.tenants (
    tenant_id uuid PRIMARY KEY,
    -- When the service saw the tenant, by the database clock.
    created_at timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE assets.tenants ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON assets.tenants
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- The consumer inserts (`ON CONFLICT DO NOTHING` needs only INSERT) and the API reads. No
-- platform access: nothing reads this table across tenants yet, so `assets_platform` gets no
-- grant on it (crates/service-kit/sql/tenant-table.sql, step 4).
GRANT SELECT, INSERT ON assets.tenants TO assets_app;
