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
--    every tenant, with the commands step 5 grants it. Policy 3 applies to the platform role
--    too, but adds nothing here: a platform session that chose its own tenant in
--    `trustiot.tenant_id` (which PlatformDb never does and tools/sql-check rejects) still sees
--    every tenant and still has only its grants. Leave the policy out, and grant the platform
--    role nothing on the table: with a grant but no platform_access, policy 3 alone would give
--    such a session the rows of the tenant it chose (Laysi/trustiot-v2#360), so service-kit's
--    connection check refuses the platform role's sessions then (Laysi/trustiot-v2#366). Keep
--    platform policies to built-in functions and the table's own columns: the check counts a
--    policy that calls a function of the service's own, or reads another table or a view, as
--    opening only the session's tenant, since it cannot see what that object does.
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
