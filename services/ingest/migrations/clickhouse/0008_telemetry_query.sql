-- The telemetry-query service's account (decision 14, Laysi/trustiot-v2#119): it creates each
-- tenant's read-only account when the tenant is created, and queries ClickHouse as that account.
--
-- What the account may do, and nothing else:
--
-- - CREATE USER and ALTER USER on user names that start with trustiot_tenant_account_ (a
--   wildcard grant on a name prefix; tests check that 26.8 refuses every other name): it
--   creates a tenant's account and resets its
--   password, but cannot create, change or log in as any other user. The prefix covers no
--   existing user: the view's definer is trustiot_tenant_view_definer, and the service accounts
--   are named after their service. The service names a tenant's account
--   trustiot_tenant_account_ plus the tenant ID's 32 hex digits (services/telemetry-query).
-- - The tenant role trustiot_tenant_reader WITH ADMIN OPTION, so the accounts it creates get the
--   role (ClickHouse requires ADMIN OPTION on a role to grant it). The role is not a default
--   role of the account: at login the account has no role, cannot read the tenant view and has
--   no settings profile. The service enables the role only on the requests that create accounts
--   (the HTTP interface's role parameter); with it enabled, the account could read the tenant
--   view for any tenant it names in SQL_tenant_id, which is no more than the service can do
--   anyway, since it holds every tenant account's password. ClickHouse 26.8 does not limit to
--   whom a holder of ADMIN OPTION grants the role: with the role enabled, the account could also
--   grant it to any other user or role (tools/migrate/tests/clickhouse_telemetry_query.rs shows
--   it). The migration Job therefore checks on every run that only this account (with ADMIN
--   OPTION) and trustiot_tenant_account_* accounts (without it) hold the role, and stops
--   otherwise (tools/migrate/src/clickhouse.rs, READER_GRANTEES).
--
-- The account cannot grant any privilege (it holds none WITH GRANT OPTION) and holds no other
-- role, so an account it creates can only ever get the tenant role: no table, no other view, no
-- service role (services/ingest/README.md, "租戶隔離").
--
-- The account cannot log in until the migration Job sets its password: it is created here
-- without one and HOST NONE, like the view's definer. The Job sets the password from the Secret
-- the chart declares for telemetry-query and lets it log in (HOST ANY) while the service is
-- deployed, and sets HOST NONE again when it is not (tools/migrate, TRUSTIOT__PASSWORDS__
-- CLICKHOUSE_TELEMETRY_QUERY). Server setting allow_no_password = 0 would stop this file, as it
-- would 0005_tenant_view.sql.

CREATE USER IF NOT EXISTS trustiot_telemetry_query IDENTIFIED WITH no_password HOST NONE DEFAULT ROLE NONE;

GRANT CREATE USER, ALTER USER ON trustiot_tenant_account_* TO trustiot_telemetry_query;

GRANT trustiot_tenant_reader TO trustiot_telemetry_query WITH ADMIN OPTION;

-- A grant to a user whose default roles are ALL makes the role a default one; this account's are
-- NONE from its CREATE USER, and this keeps them so on a server where someone changed that.
SET DEFAULT ROLE NONE TO trustiot_telemetry_query;
