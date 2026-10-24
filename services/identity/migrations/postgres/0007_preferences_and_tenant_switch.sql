-- User preferences and switching a session's tenant (Laysi/trustiot-v2#497;
-- services/identity/README.md, "目前的 session 與切換租戶" and "偏好設定"). Run by
-- `identity_owner` like 0001_identity.sql.
--
-- 1. Preferences: one row per user who changed one, holding the four settings. A user without a
--    row has the defaults below, which the service also knows (src/preferences.rs, whose test
--    compares them with this file).
-- 2. The service role may change which tenant a session acts in. That column is only a pointer:
--    the role is still read from `memberships` under row-level security on every request, so a
--    session pointed at a tenant the user is not a member of acts in no tenant at all.
-- 3. The platform role may read memberships and tenant names across tenants, so the service can
--    list the tenants a user belongs to (the service role sees one tenant's rows at a time).

-- Fail fast, not queue every ext_authz read behind the policies' locks; the Job fails and
-- retries.
SET LOCAL lock_timeout = '5s';

-- A user's own settings, not tenant data: no `tenant_id`, no row-level security. The checks
-- repeat the service's allow lists (the service answers 422 before the database would refuse);
-- whether a time zone exists in the IANA Time Zone Database only the service checks.
CREATE TABLE identity.preferences (
    user_id uuid PRIMARY KEY REFERENCES identity.users (id) ON DELETE CASCADE,
    -- The interface's language; 'auto' follows the browser.
    language text NOT NULL DEFAULT 'auto' CHECK (language IN ('auto', 'zh-TW', 'en')),
    -- The language AI answers are written in; 'auto' answers in the language of the question.
    ai_language text NOT NULL DEFAULT 'auto' CHECK (ai_language IN ('auto', 'zh-TW', 'en')),
    -- An IANA time zone name, or 'auto' to follow the browser.
    time_zone text NOT NULL DEFAULT 'auto'
        CHECK (
            time_zone = 'auto'
            OR (
                octet_length(time_zone) BETWEEN 1 AND 64
                AND time_zone ~ '^[A-Za-z0-9_+-]+(/[A-Za-z0-9_+-]+)*$'
            )
        ),
    -- 'system' follows the operating system's light or dark setting.
    theme text NOT NULL DEFAULT 'system' CHECK (theme IN ('system', 'light', 'dark'))
);

-- Reading; creating the row (its settings take the defaults); changing the settings. The user a
-- row belongs to never changes, and no row is deleted but with its user.
GRANT SELECT ON identity.preferences TO identity_app;
GRANT INSERT (user_id) ON identity.preferences TO identity_app;
GRANT UPDATE (language, ai_language, time_zone, theme) ON identity.preferences TO identity_app;

-- Switching the session's tenant. The trigger of 0004_login.sql does not fire on this column: a
-- revoked or expired session stays so whatever tenant it names.
GRANT UPDATE (active_tenant_id) ON identity.sessions TO identity_app;

-- Read only, and only by the platform role, through PlatformDb's READ ONLY transactions
-- (crates/service-kit/sql/tenant-table.sql, step 4). The service role's access is unchanged:
-- `tenant_isolation` alone applies to it.
CREATE POLICY platform_access ON identity.memberships
    FOR SELECT
    TO identity_platform
    USING (true);

CREATE POLICY platform_access ON identity.tenants
    FOR SELECT
    TO identity_platform
    USING (true);

GRANT SELECT ON identity.memberships, identity.tenants TO identity_platform;
