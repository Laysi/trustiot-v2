-- Identity service schema (Laysi/trustiot-v2#61): users, tenant memberships and server-side
-- sessions (decisions 4, 13, 29). Run by the migration role `identity_owner`, which owns
-- everything here; the service role `identity_app` owns nothing (crates/service-kit/README.md,
-- "角色與 policy 範本"). The migration Job (Laysi/trustiot-v2#60) creates both roles, runs
-- crates/service-kit/sql/database.sql once per database, and applies these files in order.
--
-- MS0 grants the service role only SELECT: it checks sessions but cannot create, extend or
-- revoke them, and nothing in the service creates a session (no fake login, checklist A1). The
-- login flow (Laysi/trustiot-v2#103) adds the writes it needs in a later file.

CREATE SCHEMA identity;
GRANT USAGE ON SCHEMA identity TO identity_app;

-- Users span tenants, so this is not tenant data (no `tenant_id`, no row-level security).
-- Email and the other profile fields arrive with the login flow (Laysi/trustiot-v2#103).
CREATE TABLE identity.users (
    id uuid PRIMARY KEY,
    -- Platform administrator (A2), independent of any tenant role.
    platform_admin boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- A user's role in one tenant: tenant data, so it follows crates/service-kit/sql/tenant-table.sql.
-- The ext_authz check reads it in a transaction for the session's tenant.
CREATE TABLE identity.memberships (
    tenant_id uuid NOT NULL,
    user_id uuid NOT NULL REFERENCES identity.users (id) ON DELETE CASCADE,
    -- Not called role: tools/sql-check rejects every statement that assigns a column of that
    -- name, since it reads like changing the session role.
    tenant_role text NOT NULL CHECK (tenant_role IN ('viewer', 'editor', 'admin')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, user_id)
);

ALTER TABLE identity.memberships ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON identity.memberships
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- Server-side sessions (decision 13). The cookie holds a 256-bit random secret; only its SHA-256
-- is stored, so this table cannot be replayed as cookies. A session belongs to a user, not to a
-- tenant: it must be found from the cookie before any tenant is known, and the user may switch
-- the tenant it acts in. So it has no `tenant_id` and no row-level security; the tenant it acts
-- in is `active_tenant_id`, and the role there is always read from `memberships` under
-- row-level security, so removing a membership or changing a role applies to the next request.
CREATE TABLE identity.sessions (
    id uuid PRIMARY KEY,
    token_hash bytea NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
    user_id uuid NOT NULL REFERENCES identity.users (id) ON DELETE CASCADE,
    active_tenant_id uuid,
    created_at timestamptz NOT NULL DEFAULT now(),
    -- Decided by the database clock; the check compares it with now().
    expires_at timestamptz NOT NULL,
    -- Logout and "log out everywhere" set it; a revoked session never comes back.
    revoked_at timestamptz,
    CHECK (expires_at > created_at)
);

GRANT SELECT ON identity.users, identity.memberships, identity.sessions TO identity_app;
