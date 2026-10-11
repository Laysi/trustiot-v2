-- The tenant read path: a DEFINER view over the telemetry table (decision 14,
-- Laysi/trustiot-v2#260).
--
-- Why a view. ClickHouse 26.8 has table functions that analyse a MergeTree table's index
-- (mergeTreeAnalyzeIndexes, mergeTreeAnalyzeIndexesUUID) and need only SELECT on the table,
-- but do not apply row policies, so a tenant account with SELECT on trustiot.telemetry could
-- learn which key ranges of other tenants exist. Tenant accounts therefore get no grant on any
-- table: they read trustiot.tenant_telemetry, a view that runs as its definer. Every table
-- function that names a table checks the caller's grant on that table, so on the tables they
-- are refused, and on the view they find no MergeTree table to inspect.
--
-- The definer is a user that cannot log in (no password, HOST NONE): it holds no credential to
-- rotate, and ClickHouse refuses to drop it while the view depends on it. To replace it, a new
-- migration runs ALTER TABLE trustiot.tenant_telemetry MODIFY SQL SECURITY DEFINER DEFINER =
-- <user>. CREATE VIEW IF NOT EXISTS never changes an existing view, so a column change is a new
-- migration with ALTER TABLE trustiot.tenant_telemetry MODIFY QUERY.
--
-- Where the tenant filter sits. The definer's row policy on the table carries the tenant filter
-- (tenant_view_definer below). Inside a DEFINER view the query keeps the caller's settings, so
-- getSetting('SQL_tenant_id') is the calling account's constant tenant; a caller without that
-- setting gets an error. ClickHouse applies a table's row policy as a row-level filter before
-- PREWHERE and the caller's WHERE, and uses it for primary-key analysis: a predicate the caller
-- writes never sees another tenant's rows, and a tenant's query reads the same granules as on the
-- table itself. A filter that only sits on the view, as a row policy of the view or a WHERE in
-- its query, is merged into PREWHERE with the caller's conditions and gives no such ordering:
-- on 2026-10-11 a throwIf() in a tenant's WHERE then fired on another tenant's row in a shared
-- granule (docs/test-reports/2026-10-11-260-introspection.md). The view's own policies are a
-- second layer and keep deny-by-default for any other account that is granted the view.
--
-- Per-query limits and the quota keep applying: the settings profile is the caller's, the
-- outer query's read and memory limits cover the rows read inside the view, and the quota
-- counts the view's reads for the calling account (tests/clickhouse.rs).
--
-- The view leaves out stream_id and batch_seq, which only the ingest service uses.

-- Servers that applied 0004_access.sql before 2026-10-11 granted the tenant role SELECT on the
-- table itself.
REVOKE SELECT ON trustiot.telemetry FROM trustiot_tenant_reader;

CREATE USER IF NOT EXISTS trustiot_tenant_view_definer IDENTIFIED WITH no_password HOST NONE;

GRANT SELECT ON trustiot.telemetry TO trustiot_tenant_view_definer;

CREATE ROW POLICY IF NOT EXISTS tenant_view_definer ON trustiot.telemetry
AS PERMISSIVE FOR SELECT USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_view_definer;

CREATE VIEW IF NOT EXISTS trustiot.tenant_telemetry
DEFINER = trustiot_tenant_view_definer SQL SECURITY DEFINER
AS SELECT
    tenant_id,
    gateway_id,
    device_id,
    point,
    ts,
    received_at,
    value_type,
    value_float,
    value_int,
    value_bool,
    value_string
FROM trustiot.telemetry
COMMENT 'Telemetry readings of the calling tenant account, the only relation tenant accounts may read (0005_tenant_view.sql)';

CREATE ROW POLICY IF NOT EXISTS deny_by_default ON trustiot.tenant_telemetry
AS PERMISSIVE FOR SELECT USING 0 TO ALL;

CREATE ROW POLICY IF NOT EXISTS tenant_isolation ON trustiot.tenant_telemetry
AS PERMISSIVE FOR SELECT USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_reader;

GRANT SELECT ON trustiot.tenant_telemetry TO trustiot_tenant_reader;
