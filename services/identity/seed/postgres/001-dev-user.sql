-- Development user and tenant membership for the local environment (scripts/dev.ts seed,
-- Laysi/trustiot-v2#336). The identity service has no login yet (Laysi/trustiot-v2#103), so the
-- local environment makes the rows an ext_authz check needs; `seed` adds the session (its cookie
-- is random, so it is not a static file). Written as the schema owner, the way
-- services/identity/README.md says a session is made, so a missing privilege fails here and not
-- in a service. Idempotent. The ids are the same as DEV_USER_ID and DEV_TENANT_ID in
-- scripts/lib/identity.ts.
SET ROLE identity_owner;

INSERT INTO identity.users (id, platform_admin)
VALUES ('00000000-0000-4000-8000-000000000a01', false)
ON CONFLICT (id) DO NOTHING;

INSERT INTO identity.memberships (tenant_id, user_id, tenant_role)
VALUES ('00000000-0000-4000-8000-000000000b01', '00000000-0000-4000-8000-000000000a01', 'admin')
ON CONFLICT (tenant_id, user_id) DO UPDATE SET tenant_role = excluded.tenant_role;
