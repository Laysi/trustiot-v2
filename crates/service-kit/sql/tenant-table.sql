-- Template for a table with tenant data (decision 29). Copy it into the service's migration and
-- replace `example_items` with the table, `example_app` with the service role, and
-- `example_platform` with the service's platform role. The migration role runs it and owns the
-- table; the service role and the platform role own nothing.
--
-- Run sql/database.sql once per database first: it revokes temporary tables.
--
-- tests/tenant_db.rs runs this file as it is, so the template is the tested one.

-- 1. The tenant column is named `tenant_id`, is a NOT NULL uuid, and leads the primary key and
--    every index, so tenant-scoped queries use them.
CREATE TABLE example_items (
    tenant_id uuid NOT NULL,
    id uuid NOT NULL,
    name text NOT NULL,
    PRIMARY KEY (tenant_id, id)
);

-- 2. Row-level security on. The owner (the migration role) stays exempt, so migrations can
--    backfill across tenants; no other role is.
ALTER TABLE example_items ENABLE ROW LEVEL SECURITY;

-- 3. Every role sees and writes only the rows of the transaction's tenant. Without a tenant set,
--    the right-hand side is NULL and no row matches; WITH CHECK stops inserting or moving a row
--    into another tenant. The policy applies to PUBLIC, so a role added later is isolated too.
CREATE POLICY tenant_isolation ON example_items
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- 4. Only for tables the platform back office or cross-tenant jobs need: the platform role sees
--    every tenant. Leave it out, and the platform role sees no rows of the table.
CREATE POLICY platform_access ON example_items
    TO example_platform
    USING (true)
    WITH CHECK (true);

-- 5. Grant only what each role needs. Never grant TRUNCATE (row-level security does not apply to
--    it) and never make either role the owner; the service refuses such sessions.
GRANT SELECT, INSERT, UPDATE, DELETE ON example_items TO example_app;
GRANT SELECT, UPDATE ON example_items TO example_platform;

-- Views over tenant tables must be created WITH (security_invoker = true): a view otherwise runs
-- with its owner's rights, and the owner is exempt from row-level security. Materialized views
-- and foreign tables cannot be protected, so the service roles must not be able to read tenant
-- data through them. SECURITY DEFINER functions that read tenant tables have the same problem
-- as views and are not allowed.
