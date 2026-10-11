-- Roles, row policies, settings profile and quota for tenant isolation (decision 14).
--
-- Every access entity is created with SQL and stored in ClickHouse's own access storage, never
-- in server configuration files, so the same statements also work on ClickHouse Cloud. The
-- migration Job (Laysi/trustiot-v2#60) applies this file with the bootstrap admin account.
--
-- Users are not created here: service accounts come from the migration Job with their
-- passwords from Kubernetes Secrets, and each tenant's read-only account is created with the
-- tenant (Laysi/trustiot-v2#119) by two statements:
--
--   CREATE USER <name> IDENTIFIED WITH sha256_password BY '<password>'
--       SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = '<tenant UUID>' CONST;
--   GRANT trustiot_tenant_reader TO <name>;
--
-- The profile is attached to the user as well as to the role, so the per-query limits stay in
-- force after SET ROLE NONE (which also drops the SELECT grant). tests/clickhouse.rs creates its
-- tenant accounts with exactly these statements.
--
-- Tenant accounts read telemetry only through the view trustiot.tenant_telemetry
-- (0005_tenant_view.sql), never the tables below: some MergeTree introspection table functions
-- read a table's index without applying row policies (Laysi/trustiot-v2#260), so the tenant
-- role has no grant on any table of this file.
--
-- Row policies. ClickHouse ORs the permissive policies that apply to a user, and by default a
-- user with no policy on a table reads every row (users_without_row_policies_can_read_rows).
-- deny_by_default applies to every user, so a user or role that no other policy names reads
-- nothing, without changing server configuration. tenant_isolation compares tenant_id with the
-- account's constant SQL_tenant_id setting; an account without that setting gets an error
-- instead of rows. Service roles get a USING 1 policy of their own. The tenant role has no grant
-- on the table, so its tenant_isolation policy here is a second layer only: it would still
-- filter the rows if a grant on the table were ever added by mistake.
--
-- Settings profile and quota values are examples until tenant onboarding sets the real limits
-- per plan (Laysi/trustiot-v2#119). The quota is keyed by user name: without KEYED BY every
-- tenant account would share one counter, and one tenant could exhaust it for all.

CREATE ROLE IF NOT EXISTS trustiot_ingest;

CREATE ROLE IF NOT EXISTS trustiot_tenant_reader;

-- ALTER DELETE is the privilege ClickHouse 26.8 checks for a lightweight DELETE FROM (there is
-- no separate one), which the ingest service needs to remove the rows of an incomplete batch.
-- It also allows ALTER TABLE ... DELETE and the partition statements DETACH, DROP, ATTACH and
-- REPLACE PARTITION and DROP DETACHED PARTITION (docs/test-reports/2026-10-11-59-verify.md,
-- finding 6), but no column, TTL or setting change. Runtime code uses DELETE FROM only;
-- tools/repo-check rejects ALTER TABLE in services/*/src, but that scan only matches text, so
-- the grant is what actually limits the account.
GRANT SELECT, INSERT, ALTER DELETE ON trustiot.telemetry TO trustiot_ingest;

GRANT SELECT, INSERT ON trustiot.batch_commits TO trustiot_ingest;

CREATE ROW POLICY IF NOT EXISTS deny_by_default ON trustiot.telemetry
AS PERMISSIVE FOR SELECT USING 0 TO ALL;

CREATE ROW POLICY IF NOT EXISTS deny_by_default ON trustiot.batch_commits
AS PERMISSIVE FOR SELECT USING 0 TO ALL;

CREATE ROW POLICY IF NOT EXISTS tenant_isolation ON trustiot.telemetry
AS PERMISSIVE FOR SELECT USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_reader;

CREATE ROW POLICY IF NOT EXISTS service_all_rows ON trustiot.telemetry
AS PERMISSIVE FOR SELECT USING 1 TO trustiot_ingest;

CREATE ROW POLICY IF NOT EXISTS service_all_rows ON trustiot.batch_commits
AS PERMISSIVE FOR SELECT USING 1 TO trustiot_ingest;

CREATE SETTINGS PROFILE IF NOT EXISTS trustiot_tenant_reader
SETTINGS
    readonly = 1 CONST,
    max_execution_time = 30 CONST,
    max_memory_usage = 536870912 CONST,
    max_threads = 2 CONST,
    max_concurrent_queries_for_user = 4 CONST,
    max_rows_to_read = 1000000000 CONST,
    max_result_rows = 1000000 CONST,
    result_overflow_mode = 'throw' CONST
TO trustiot_tenant_reader;

CREATE QUOTA IF NOT EXISTS trustiot_tenant_reader
KEYED BY user_name
FOR INTERVAL 1 minute MAX queries = 120, errors = 60,
FOR INTERVAL 1 hour MAX queries = 3000, execution_time = 1800, read_rows = 20000000000
TO trustiot_tenant_reader;
