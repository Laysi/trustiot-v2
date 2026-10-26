-- The queryable period in ClickHouse itself (rebuild checklist B3, J3; Laysi/trustiot-v2#143,
-- owner decision of 2026-10-26): every tenant read, the history API, the semantic tools and the
-- AI's SQL alike, sees only the rows of its plan's period, whatever its query says. The
-- reasoning and the feasibility evidence are in services/ingest/README.md, section
-- 「可查期間」, and docs/architecture/data-retention.md.
--
-- Settings. A tenant account carries, next to SQL_tenant_id and like it as CONST settings that
-- the account cannot change (readonly = 1, CONST), one window per table:
--   SQL_queryable_raw_days   raw readings (trustiot.telemetry), whole days
--   SQL_queryable_1m_months  one-minute aggregates (trustiot.telemetry_1m), calendar months
--   SQL_queryable_1h_months  one-hour aggregates (trustiot.telemetry_1h), calendar months
-- The telemetry-query service writes them when it creates the account and sets them again when
-- they are missing or differ from the tenant's plan (services/telemetry-query/src/accounts.rs),
-- so a plan change applies from the account's next query.
--
-- Boundary. A raw row is visible when ts >= now64(3, 'UTC') - toIntervalDay(<days>), a cell of
-- an aggregate when bucket >= now64(3, 'UTC') - toIntervalMonth(<months>): inclusive, no
-- rounding, no upper bound, on the reading time rather than the receive time, and now64 is the
-- server's clock at the start of the query. Subtracting months keeps the day of the month or,
-- when the month is shorter, takes its last day.
--
-- Where the filter sits. As the tenant filter (0005_tenant_view.sql): in the definer's row
-- policies on the tables, which ClickHouse applies before PREWHERE and the caller's WHERE, so no
-- predicate of the caller ever sees a row outside the window; and again in the tenant role's
-- policies on the tables and on the views, a second layer. The policies of the services, the
-- rollup definer and the migration account do not change: they read every row.
--
-- Values. accurateCast(..., 'UInt16') refuses a negative, too large or non-numeric value
-- (CANNOT_CONVERT_TYPE, CANNOT_PARSE_TEXT), and an account without the settings gets
-- UNKNOWN_SETTING on every read: a missing or broken window fails closed. least() only keeps
-- the date arithmetic from wrapping; the service writes values within the retention.
--
-- Rolling back means ALTER ROW POLICY ... USING with the conditions of 0004, 0005 and 0010
-- (the tenant condition alone), in a new file; services/ingest/README.md has the statements.

ALTER ROW POLICY tenant_view_definer ON trustiot.telemetry
AS PERMISSIVE FOR SELECT
USING tenant_id = toUUID(getSetting('SQL_tenant_id'))
    AND ts >= now64(3, 'UTC') - toIntervalDay(least(accurateCast(getSetting('SQL_queryable_raw_days'), 'UInt16'), 36500))
TO trustiot_tenant_view_definer;

ALTER ROW POLICY tenant_isolation ON trustiot.telemetry
AS PERMISSIVE FOR SELECT
USING tenant_id = toUUID(getSetting('SQL_tenant_id'))
    AND ts >= now64(3, 'UTC') - toIntervalDay(least(accurateCast(getSetting('SQL_queryable_raw_days'), 'UInt16'), 36500))
TO trustiot_tenant_reader;

ALTER ROW POLICY tenant_isolation ON trustiot.tenant_telemetry
AS PERMISSIVE FOR SELECT
USING tenant_id = toUUID(getSetting('SQL_tenant_id'))
    AND ts >= now64(3, 'UTC') - toIntervalDay(least(accurateCast(getSetting('SQL_queryable_raw_days'), 'UInt16'), 36500))
TO trustiot_tenant_reader;

ALTER ROW POLICY tenant_view_definer ON trustiot.telemetry_1m
AS PERMISSIVE FOR SELECT
USING tenant_id = toUUID(getSetting('SQL_tenant_id'))
    AND bucket >= now64(3, 'UTC') - toIntervalMonth(least(accurateCast(getSetting('SQL_queryable_1m_months'), 'UInt16'), 1200))
TO trustiot_tenant_view_definer;

ALTER ROW POLICY tenant_isolation ON trustiot.telemetry_1m
AS PERMISSIVE FOR SELECT
USING tenant_id = toUUID(getSetting('SQL_tenant_id'))
    AND bucket >= now64(3, 'UTC') - toIntervalMonth(least(accurateCast(getSetting('SQL_queryable_1m_months'), 'UInt16'), 1200))
TO trustiot_tenant_reader;

ALTER ROW POLICY tenant_isolation ON trustiot.tenant_telemetry_1m
AS PERMISSIVE FOR SELECT
USING tenant_id = toUUID(getSetting('SQL_tenant_id'))
    AND bucket >= now64(3, 'UTC') - toIntervalMonth(least(accurateCast(getSetting('SQL_queryable_1m_months'), 'UInt16'), 1200))
TO trustiot_tenant_reader;

ALTER ROW POLICY tenant_view_definer ON trustiot.telemetry_1h
AS PERMISSIVE FOR SELECT
USING tenant_id = toUUID(getSetting('SQL_tenant_id'))
    AND bucket >= now64(3, 'UTC') - toIntervalMonth(least(accurateCast(getSetting('SQL_queryable_1h_months'), 'UInt16'), 1200))
TO trustiot_tenant_view_definer;

ALTER ROW POLICY tenant_isolation ON trustiot.telemetry_1h
AS PERMISSIVE FOR SELECT
USING tenant_id = toUUID(getSetting('SQL_tenant_id'))
    AND bucket >= now64(3, 'UTC') - toIntervalMonth(least(accurateCast(getSetting('SQL_queryable_1h_months'), 'UInt16'), 1200))
TO trustiot_tenant_reader;

ALTER ROW POLICY tenant_isolation ON trustiot.tenant_telemetry_1h
AS PERMISSIVE FOR SELECT
USING tenant_id = toUUID(getSetting('SQL_tenant_id'))
    AND bucket >= now64(3, 'UTC') - toIntervalMonth(least(accurateCast(getSetting('SQL_queryable_1h_months'), 'UInt16'), 1200))
TO trustiot_tenant_reader;
