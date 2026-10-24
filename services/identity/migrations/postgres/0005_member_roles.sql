-- Platform administrators set users' roles in tenants (Laysi/trustiot-v2#583;
-- services/identity/README.md, "成員"): `PUT /api/identity/v1/platform/tenants/{tenant_id}/members`
-- creates a membership or changes its role, in a transaction of that tenant, and writes
-- `identity.member_role_changed` to the outbox in the same transaction. Run by `identity_owner`
-- like 0001_identity.sql.
--
-- The service role gets exactly those writes. It may insert a membership (the tenant, the user
-- and the role; `created_at` always takes its default) and change a membership's role; it still
-- cannot move a membership to another tenant or user, change when it started, or delete it
-- (removing members is Laysi/trustiot-v2#105). Row-level security (`tenant_isolation`, 0001)
-- keeps both writes to the transaction's tenant: a row of another tenant can be neither inserted
-- nor seen, so not updated either. `SELECT ... FOR UPDATE`, which serializes two changes of one
-- membership, needs the UPDATE grant on a column, which `tenant_role` is.

-- Fail fast, not queue every ext_authz read behind the grant's lock; the Job fails and retries.
SET LOCAL lock_timeout = '5s';

GRANT INSERT (tenant_id, user_id, tenant_role) ON identity.memberships TO identity_app;
GRANT UPDATE (tenant_role) ON identity.memberships TO identity_app;
