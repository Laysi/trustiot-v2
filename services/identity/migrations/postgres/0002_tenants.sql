-- Tenants (Laysi/trustiot-v2#65): the identity service owns the list of tenants. A platform
-- administrator creates one through `POST /api/identity/v1/tenants`, which inserts the row and
-- writes `identity.tenant_created` to the outbox in the same transaction, in the new tenant's
-- row-level security context (services/identity/README.md, "租戶"). Other services create their
-- per-tenant state from that event.
--
-- Run by `identity_owner` like 0001_identity.sql. The outbox tables (`outbox_event`,
-- `outbox_aggregate`) are created by the migration Job itself, because the registry lists
-- identity as an outbox producer (tools/migrate/src/registry.rs).

-- One row per tenant: tenant data, so it follows crates/service-kit/sql/tenant-table.sql, with
-- the tenant ID as the whole primary key. The service role reads and writes only the row of the
-- transaction's tenant; a tenant transaction for a new tenant is the only way to insert it.
CREATE TABLE identity.tenants (
    tenant_id uuid PRIMARY KEY,
    -- Shown to people; never logged (it may be a company name). Checked by the service too,
    -- which answers 422 before the database would refuse it.
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100),
    created_at timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE identity.tenants ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON identity.tenants
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- INSERT for creating a tenant; SELECT because the insert returns the row (RETURNING needs it
-- under row-level security). No UPDATE or DELETE yet: renaming and removing tenants come with the
-- back office (Laysi/trustiot-v2#103 and later).
GRANT SELECT, INSERT ON identity.tenants TO identity_app;
