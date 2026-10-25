-- One-minute and one-hour aggregates of the telemetry table (rebuild checklist I2, J3,
-- Laysi/trustiot-v2#141). The reasoning, the I1 contract for the ingest service and the queries
-- the consumers run are in services/ingest/README.md, section 「彙總表」.
--
-- Tables. trustiot.telemetry_1m and trustiot.telemetry_1h hold one row per cell: tenant,
-- device, point, bucket (the reading time ts truncated to the UTC minute or hour), gateway,
-- stream and receive day. Each cell carries the count of readings, the count of numeric ones
-- (float, int, bool as 0 or 1), their sum, minimum and maximum, and the first and last reading
-- by ts with its typed value, as aggregate states that merge (AggregatingMergeTree). A cell
-- belongs to one gateway stream, so the ingest service can correct a stream's cells without
-- racing another stream's writes, and to one receive day, so a cell expires as a whole and a
-- correction never touches days whose raw rows may have expired. gateway_id and stream_id are
-- strings, written with toString(), so the tables need no change when the telemetry table's
-- identifiers become strings (Laysi/trustiot-v2#639).
--
-- Filling. Two materialized views add every block inserted into trustiot.telemetry, in the same
-- INSERT, so the aggregates show a reading as soon as the raw table does, uncommitted batches
-- included, as trustiot.tenant_telemetry shows them. A DELETE does not reach them: after the I1
-- rewrite of a batch the ingest service runs the reconcile statement of
-- services/ingest/src/rollup.rs, which inserts the difference between the cells recomputed from
-- the raw rows and the cells as they are. The views run as trustiot_rollup_definer, which needs
-- a row policy of its own on the telemetry table: without one, deny_by_default hides every row
-- of the inserted block from the view, which then writes nothing and reports no error.
--
-- Retention counts from the server's receive time, by the receive day: a cell expires once
-- its whole receive day is past the retention, at most a day later than its newest row would,
-- never earlier. The values here are the platform's (13 months, 4 years, src/retention.rs). The
-- migration Job sets both TTLs from its settings after it applies the files, as it does for the
-- raw table (docs/architecture/data-retention.md).
--
-- Deduplication: a retried INSERT that the telemetry table drops as a duplicate still runs the
-- views, so the aggregate tables have the same non_replicated_deduplication_window and drop the
-- view's block too. Distinct raw blocks whose aggregates happen to be equal are not dropped: the
-- views' blocks are deduplicated by the raw block they came from.
--
-- Tenant reads go through trustiot.tenant_telemetry_1m and trustiot.tenant_telemetry_1h,
-- DEFINER views like trustiot.tenant_telemetry (0005_tenant_view.sql): the definer's row policy
-- on the aggregate tables carries the tenant filter, and the tenant role has no grant on the
-- tables.
--
-- Backfill. The last statements fill the aggregates from the rows the telemetry table held
-- before the views existed. They insert the difference between the recomputation and the
-- current aggregates, so rows the views already counted are not counted again, and applying
-- the file again inserts nothing. The admin account reads no telemetry rows (deny_by_default),
-- so the difference comes from the views trustiot.telemetry_1m_backfill and
-- trustiot.telemetry_1h_backfill, which run as trustiot_rollup_definer and which only the
-- account that applied this file may read. The backfill assumes that no ingest service writes
-- while it runs (a block between its raw part and its view part would be counted twice): true
-- when this file is first applied, since no release before it has an ingest writer.
-- max_execution_time stays below the migration Job's 30-second request timeout, so a statement
-- that runs too long stops on the server too and is not repeated alongside a running one by the
-- next run, which inserts what is still missing. The backfill views also check and repair a
-- range of receive days by hand (docs/runbooks/migrations.md).
-- deduplicate_insert_select = 'disable': ClickHouse 26.8 drops an INSERT SELECT whose block it
-- saw before when the SELECT is stable, and a difference may equal an earlier one; its
-- insert_deduplicate setting no longer turns deduplication off.

CREATE TABLE IF NOT EXISTS trustiot.telemetry_1m
(
    tenant_id UUID COMMENT 'Tenant that owns the readings',
    device_id UUID COMMENT 'Device the readings belong to',
    point LowCardinality(String) COMMENT 'Data point key within the device model (C1)',
    bucket DateTime('UTC') COMMENT 'Reading time ts truncated to the UTC minute',
    gateway_id String COMMENT 'Gateway that sent the readings',
    stream_id String COMMENT 'Gateway batch stream the readings came in',
    received_day Date COMMENT 'Server receive day (UTC), drives partitioning and retention',
    readings SimpleAggregateFunction(sum, Int64) COMMENT 'Readings in the cell',
    numeric_readings SimpleAggregateFunction(sum, Int64) COMMENT 'Readings with a numeric value: float, int, bool as 0 or 1',
    value_sum SimpleAggregateFunction(sum, Float64) COMMENT 'Sum of the numeric values',
    value_min SimpleAggregateFunction(min, Nullable(Float64)) COMMENT 'Smallest numeric value, NULL without one',
    value_max SimpleAggregateFunction(max, Nullable(Float64)) COMMENT 'Largest numeric value, NULL without one',
    first AggregateFunction(argMin, Tuple(ts DateTime64(3, 'UTC'), value_type Enum8('float' = 1, 'int' = 2, 'bool' = 3, 'string' = 4), value_float Float64, value_int Int64, value_bool Bool, value_string String), DateTime64(3, 'UTC')) COMMENT 'Earliest reading by ts with its typed value',
    last AggregateFunction(argMax, Tuple(ts DateTime64(3, 'UTC'), value_type Enum8('float' = 1, 'int' = 2, 'bool' = 3, 'string' = 4), value_float Float64, value_int Int64, value_bool Bool, value_string String), DateTime64(3, 'UTC')) COMMENT 'Latest reading by ts with its typed value'
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(received_day)
ORDER BY (tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day)
TTL received_day + toIntervalMonth(13) + toIntervalDay(1)
SETTINGS
    fsync_after_insert = 1,
    fsync_part_directory = 1,
    non_replicated_deduplication_window = 1000
COMMENT 'One-minute aggregates of trustiot.telemetry, tenant isolation by row policy (0010_telemetry_rollups.sql)';

CREATE TABLE IF NOT EXISTS trustiot.telemetry_1h
(
    tenant_id UUID COMMENT 'Tenant that owns the readings',
    device_id UUID COMMENT 'Device the readings belong to',
    point LowCardinality(String) COMMENT 'Data point key within the device model (C1)',
    bucket DateTime('UTC') COMMENT 'Reading time ts truncated to the UTC hour',
    gateway_id String COMMENT 'Gateway that sent the readings',
    stream_id String COMMENT 'Gateway batch stream the readings came in',
    received_day Date COMMENT 'Server receive day (UTC), drives partitioning and retention',
    readings SimpleAggregateFunction(sum, Int64) COMMENT 'Readings in the cell',
    numeric_readings SimpleAggregateFunction(sum, Int64) COMMENT 'Readings with a numeric value: float, int, bool as 0 or 1',
    value_sum SimpleAggregateFunction(sum, Float64) COMMENT 'Sum of the numeric values',
    value_min SimpleAggregateFunction(min, Nullable(Float64)) COMMENT 'Smallest numeric value, NULL without one',
    value_max SimpleAggregateFunction(max, Nullable(Float64)) COMMENT 'Largest numeric value, NULL without one',
    first AggregateFunction(argMin, Tuple(ts DateTime64(3, 'UTC'), value_type Enum8('float' = 1, 'int' = 2, 'bool' = 3, 'string' = 4), value_float Float64, value_int Int64, value_bool Bool, value_string String), DateTime64(3, 'UTC')) COMMENT 'Earliest reading by ts with its typed value',
    last AggregateFunction(argMax, Tuple(ts DateTime64(3, 'UTC'), value_type Enum8('float' = 1, 'int' = 2, 'bool' = 3, 'string' = 4), value_float Float64, value_int Int64, value_bool Bool, value_string String), DateTime64(3, 'UTC')) COMMENT 'Latest reading by ts with its typed value'
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(received_day)
ORDER BY (tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day)
TTL received_day + toIntervalMonth(48) + toIntervalDay(1)
SETTINGS
    fsync_after_insert = 1,
    fsync_part_directory = 1,
    non_replicated_deduplication_window = 1000
COMMENT 'One-hour aggregates of trustiot.telemetry, tenant isolation by row policy (0010_telemetry_rollups.sql)';

-- The account the materialized views and the backfill views run as: it cannot log in, reads
-- every row of the telemetry and aggregate tables, and writes only the aggregate tables.
CREATE USER IF NOT EXISTS trustiot_rollup_definer IDENTIFIED WITH no_password HOST NONE;

GRANT SELECT ON trustiot.telemetry TO trustiot_rollup_definer;

GRANT SELECT, INSERT ON trustiot.telemetry_1m TO trustiot_rollup_definer;

GRANT SELECT, INSERT ON trustiot.telemetry_1h TO trustiot_rollup_definer;

CREATE ROW POLICY IF NOT EXISTS rollup_definer ON trustiot.telemetry
AS PERMISSIVE FOR SELECT USING 1 TO trustiot_rollup_definer;

CREATE MATERIALIZED VIEW IF NOT EXISTS trustiot.telemetry_1m_mv
TO trustiot.telemetry_1m
DEFINER = trustiot_rollup_definer SQL SECURITY DEFINER
AS SELECT
    tenant_id,
    device_id,
    point,
    bucket,
    gateway_id,
    stream_id,
    received_day,
    toInt64(count()) AS readings,
    toInt64(count(value_number)) AS numeric_readings,
    sum(ifNull(value_number, 0)) AS value_sum,
    min(value_number) AS value_min,
    max(value_number) AS value_max,
    argMinState(reading, ts) AS first,
    argMaxState(reading, ts) AS last
FROM
(
    SELECT
        tenant_id,
        device_id,
        point,
        toStartOfMinute(ts) AS bucket,
        toString(gateway_id) AS gateway_id,
        toString(stream_id) AS stream_id,
        toDate(received_at) AS received_day,
        ts,
        multiIf(value_type = 'float', value_float, value_type = 'int', toFloat64(value_int), value_type = 'bool', toFloat64(value_bool), NULL) AS value_number,
        CAST((ts, value_type, value_float, value_int, value_bool, value_string) AS Tuple(ts DateTime64(3, 'UTC'), value_type Enum8('float' = 1, 'int' = 2, 'bool' = 3, 'string' = 4), value_float Float64, value_int Int64, value_bool Bool, value_string String)) AS reading
    FROM trustiot.telemetry
)
GROUP BY tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day;

CREATE MATERIALIZED VIEW IF NOT EXISTS trustiot.telemetry_1h_mv
TO trustiot.telemetry_1h
DEFINER = trustiot_rollup_definer SQL SECURITY DEFINER
AS SELECT
    tenant_id,
    device_id,
    point,
    bucket,
    gateway_id,
    stream_id,
    received_day,
    toInt64(count()) AS readings,
    toInt64(count(value_number)) AS numeric_readings,
    sum(ifNull(value_number, 0)) AS value_sum,
    min(value_number) AS value_min,
    max(value_number) AS value_max,
    argMinState(reading, ts) AS first,
    argMaxState(reading, ts) AS last
FROM
(
    SELECT
        tenant_id,
        device_id,
        point,
        toStartOfHour(ts) AS bucket,
        toString(gateway_id) AS gateway_id,
        toString(stream_id) AS stream_id,
        toDate(received_at) AS received_day,
        ts,
        multiIf(value_type = 'float', value_float, value_type = 'int', toFloat64(value_int), value_type = 'bool', toFloat64(value_bool), NULL) AS value_number,
        CAST((ts, value_type, value_float, value_int, value_bool, value_string) AS Tuple(ts DateTime64(3, 'UTC'), value_type Enum8('float' = 1, 'int' = 2, 'bool' = 3, 'string' = 4), value_float Float64, value_int Int64, value_bool Bool, value_string String)) AS reading
    FROM trustiot.telemetry
)
GROUP BY tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day;

-- A SELECT from a materialized view reads its target table as the view's definer, whose policy
-- shows every row: without a policy of their own, any account granted SELECT on the database
-- (the admin included) would read every tenant's aggregates through the views. Nothing reads
-- them; the aggregates are read from the tables and the tenant views.
CREATE ROW POLICY IF NOT EXISTS deny_by_default ON trustiot.telemetry_1m_mv
AS PERMISSIVE FOR SELECT USING 0 TO ALL;

CREATE ROW POLICY IF NOT EXISTS deny_by_default ON trustiot.telemetry_1h_mv
AS PERMISSIVE FOR SELECT USING 0 TO ALL;

-- Row policies and grants on the aggregate tables, as 0004_access.sql and 0005_tenant_view.sql
-- set them on the telemetry table: nothing for an account no policy names, the tenant filter
-- for the tenant views' definer, every row for the ingest service (which runs the reconcile)
-- and for the rollup definer, and the tenant filter for the tenant role as a second layer
-- (the role has no grant on the tables).
CREATE ROW POLICY IF NOT EXISTS deny_by_default ON trustiot.telemetry_1m
AS PERMISSIVE FOR SELECT USING 0 TO ALL;

CREATE ROW POLICY IF NOT EXISTS deny_by_default ON trustiot.telemetry_1h
AS PERMISSIVE FOR SELECT USING 0 TO ALL;

CREATE ROW POLICY IF NOT EXISTS tenant_isolation ON trustiot.telemetry_1m
AS PERMISSIVE FOR SELECT USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_reader;

CREATE ROW POLICY IF NOT EXISTS tenant_isolation ON trustiot.telemetry_1h
AS PERMISSIVE FOR SELECT USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_reader;

CREATE ROW POLICY IF NOT EXISTS tenant_view_definer ON trustiot.telemetry_1m
AS PERMISSIVE FOR SELECT USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_view_definer;

CREATE ROW POLICY IF NOT EXISTS tenant_view_definer ON trustiot.telemetry_1h
AS PERMISSIVE FOR SELECT USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_view_definer;

CREATE ROW POLICY IF NOT EXISTS service_all_rows ON trustiot.telemetry_1m
AS PERMISSIVE FOR SELECT USING 1 TO trustiot_ingest;

CREATE ROW POLICY IF NOT EXISTS service_all_rows ON trustiot.telemetry_1h
AS PERMISSIVE FOR SELECT USING 1 TO trustiot_ingest;

CREATE ROW POLICY IF NOT EXISTS rollup_definer ON trustiot.telemetry_1m
AS PERMISSIVE FOR SELECT USING 1 TO trustiot_rollup_definer;

CREATE ROW POLICY IF NOT EXISTS rollup_definer ON trustiot.telemetry_1h
AS PERMISSIVE FOR SELECT USING 1 TO trustiot_rollup_definer;

-- The ingest service reads the aggregates and inserts the reconcile's differences (I1); it
-- cannot delete from them or change them.
GRANT SELECT, INSERT ON trustiot.telemetry_1m TO trustiot_ingest;

GRANT SELECT, INSERT ON trustiot.telemetry_1h TO trustiot_ingest;

GRANT SELECT ON trustiot.telemetry_1m TO trustiot_tenant_view_definer;

GRANT SELECT ON trustiot.telemetry_1h TO trustiot_tenant_view_definer;

-- The tenant views merge a bucket's cells across gateways, streams and receive days. A cell
-- whose readings were all deleted and corrected (I1) has a count of 0 and is left out. The
-- caller's conditions on the key columns reach the table's primary key through the GROUP BY.
CREATE VIEW IF NOT EXISTS trustiot.tenant_telemetry_1m
DEFINER = trustiot_tenant_view_definer SQL SECURITY DEFINER
AS SELECT
    tenant_id,
    device_id,
    point,
    bucket,
    sum(readings) AS readings,
    sum(numeric_readings) AS numeric_readings,
    sum(value_sum) AS value_sum,
    min(value_min) AS value_min,
    max(value_max) AS value_max,
    argMinMerge(first) AS first,
    argMaxMerge(last) AS last
FROM trustiot.telemetry_1m
GROUP BY tenant_id, device_id, point, bucket
HAVING readings > 0
COMMENT 'One-minute aggregates of the calling tenant account (0010_telemetry_rollups.sql)';

CREATE VIEW IF NOT EXISTS trustiot.tenant_telemetry_1h
DEFINER = trustiot_tenant_view_definer SQL SECURITY DEFINER
AS SELECT
    tenant_id,
    device_id,
    point,
    bucket,
    sum(readings) AS readings,
    sum(numeric_readings) AS numeric_readings,
    sum(value_sum) AS value_sum,
    min(value_min) AS value_min,
    max(value_max) AS value_max,
    argMinMerge(first) AS first,
    argMaxMerge(last) AS last
FROM trustiot.telemetry_1h
GROUP BY tenant_id, device_id, point, bucket
HAVING readings > 0
COMMENT 'One-hour aggregates of the calling tenant account (0010_telemetry_rollups.sql)';

CREATE ROW POLICY IF NOT EXISTS deny_by_default ON trustiot.tenant_telemetry_1m
AS PERMISSIVE FOR SELECT USING 0 TO ALL;

CREATE ROW POLICY IF NOT EXISTS deny_by_default ON trustiot.tenant_telemetry_1h
AS PERMISSIVE FOR SELECT USING 0 TO ALL;

CREATE ROW POLICY IF NOT EXISTS tenant_isolation ON trustiot.tenant_telemetry_1m
AS PERMISSIVE FOR SELECT USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_reader;

CREATE ROW POLICY IF NOT EXISTS tenant_isolation ON trustiot.tenant_telemetry_1h
AS PERMISSIVE FOR SELECT USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_reader;

GRANT SELECT ON trustiot.tenant_telemetry_1m TO trustiot_tenant_reader;

GRANT SELECT ON trustiot.tenant_telemetry_1h TO trustiot_tenant_reader;

-- The backfill views: for the cells received from first_day to last_day, the recomputation
-- from the raw rows minus the aggregates as they are, only where the two differ. The
-- differences of the counts and the sum are added, and the raw side's minimum, maximum, first
-- and last are merged in (they are empty states for a cell that has no raw rows left). Only
-- the account that applied this file reads them: deny_by_default hides every row from anyone
-- else, including an account that was granted SELECT on the whole database.
CREATE VIEW IF NOT EXISTS trustiot.telemetry_1m_backfill
DEFINER = trustiot_rollup_definer SQL SECURITY DEFINER
AS SELECT
    tenant_id,
    device_id,
    point,
    bucket,
    gateway_id,
    stream_id,
    received_day,
    r.raw_readings - a.agg_readings AS readings,
    r.raw_numeric_readings - a.agg_numeric_readings AS numeric_readings,
    r.raw_value_sum - a.agg_value_sum AS value_sum,
    r.raw_value_min AS value_min,
    r.raw_value_max AS value_max,
    r.raw_first AS first,
    r.raw_last AS last
FROM
(
    SELECT
        tenant_id,
        device_id,
        point,
        bucket,
        gateway_id,
        stream_id,
        received_day,
        toInt64(count()) AS raw_readings,
        toInt64(count(value_number)) AS raw_numeric_readings,
        sum(ifNull(value_number, 0)) AS raw_value_sum,
        min(value_number) AS raw_value_min,
        max(value_number) AS raw_value_max,
        argMinState(reading, ts) AS raw_first,
        argMaxState(reading, ts) AS raw_last
    FROM
    (
        SELECT
            tenant_id,
            device_id,
            point,
            toStartOfMinute(ts) AS bucket,
            toString(gateway_id) AS gateway_id,
            toString(stream_id) AS stream_id,
            toDate(received_at) AS received_day,
            ts,
            multiIf(value_type = 'float', value_float, value_type = 'int', toFloat64(value_int), value_type = 'bool', toFloat64(value_bool), NULL) AS value_number,
            CAST((ts, value_type, value_float, value_int, value_bool, value_string) AS Tuple(ts DateTime64(3, 'UTC'), value_type Enum8('float' = 1, 'int' = 2, 'bool' = 3, 'string' = 4), value_float Float64, value_int Int64, value_bool Bool, value_string String)) AS reading
        FROM trustiot.telemetry
        WHERE toDate(received_at) BETWEEN {first_day:Date} AND {last_day:Date}
    )
    GROUP BY tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day
) AS r
FULL OUTER JOIN
(
    SELECT
        tenant_id,
        device_id,
        point,
        bucket,
        gateway_id,
        stream_id,
        received_day,
        sum(readings) AS agg_readings,
        sum(numeric_readings) AS agg_numeric_readings,
        sum(value_sum) AS agg_value_sum
    FROM trustiot.telemetry_1m
    WHERE received_day BETWEEN {first_day:Date} AND {last_day:Date}
    GROUP BY tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day
) AS a USING (tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day)
WHERE r.raw_readings != a.agg_readings
    OR r.raw_numeric_readings != a.agg_numeric_readings
    OR abs(r.raw_value_sum - a.agg_value_sum) > 1e-9 * greatest(abs(r.raw_value_sum), abs(a.agg_value_sum), 1)
SETTINGS join_use_nulls = 0;

CREATE VIEW IF NOT EXISTS trustiot.telemetry_1h_backfill
DEFINER = trustiot_rollup_definer SQL SECURITY DEFINER
AS SELECT
    tenant_id,
    device_id,
    point,
    bucket,
    gateway_id,
    stream_id,
    received_day,
    r.raw_readings - a.agg_readings AS readings,
    r.raw_numeric_readings - a.agg_numeric_readings AS numeric_readings,
    r.raw_value_sum - a.agg_value_sum AS value_sum,
    r.raw_value_min AS value_min,
    r.raw_value_max AS value_max,
    r.raw_first AS first,
    r.raw_last AS last
FROM
(
    SELECT
        tenant_id,
        device_id,
        point,
        bucket,
        gateway_id,
        stream_id,
        received_day,
        toInt64(count()) AS raw_readings,
        toInt64(count(value_number)) AS raw_numeric_readings,
        sum(ifNull(value_number, 0)) AS raw_value_sum,
        min(value_number) AS raw_value_min,
        max(value_number) AS raw_value_max,
        argMinState(reading, ts) AS raw_first,
        argMaxState(reading, ts) AS raw_last
    FROM
    (
        SELECT
            tenant_id,
            device_id,
            point,
            toStartOfHour(ts) AS bucket,
            toString(gateway_id) AS gateway_id,
            toString(stream_id) AS stream_id,
            toDate(received_at) AS received_day,
            ts,
            multiIf(value_type = 'float', value_float, value_type = 'int', toFloat64(value_int), value_type = 'bool', toFloat64(value_bool), NULL) AS value_number,
            CAST((ts, value_type, value_float, value_int, value_bool, value_string) AS Tuple(ts DateTime64(3, 'UTC'), value_type Enum8('float' = 1, 'int' = 2, 'bool' = 3, 'string' = 4), value_float Float64, value_int Int64, value_bool Bool, value_string String)) AS reading
        FROM trustiot.telemetry
        WHERE toDate(received_at) BETWEEN {first_day:Date} AND {last_day:Date}
    )
    GROUP BY tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day
) AS r
FULL OUTER JOIN
(
    SELECT
        tenant_id,
        device_id,
        point,
        bucket,
        gateway_id,
        stream_id,
        received_day,
        sum(readings) AS agg_readings,
        sum(numeric_readings) AS agg_numeric_readings,
        sum(value_sum) AS agg_value_sum
    FROM trustiot.telemetry_1h
    WHERE received_day BETWEEN {first_day:Date} AND {last_day:Date}
    GROUP BY tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day
) AS a USING (tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day)
WHERE r.raw_readings != a.agg_readings
    OR r.raw_numeric_readings != a.agg_numeric_readings
    OR abs(r.raw_value_sum - a.agg_value_sum) > 1e-9 * greatest(abs(r.raw_value_sum), abs(a.agg_value_sum), 1)
SETTINGS join_use_nulls = 0;

CREATE ROW POLICY IF NOT EXISTS deny_by_default ON trustiot.telemetry_1m_backfill
AS PERMISSIVE FOR SELECT USING 0 TO ALL;

CREATE ROW POLICY IF NOT EXISTS deny_by_default ON trustiot.telemetry_1h_backfill
AS PERMISSIVE FOR SELECT USING 0 TO ALL;

CREATE ROW POLICY IF NOT EXISTS migration_account ON trustiot.telemetry_1m_backfill
AS PERMISSIVE FOR SELECT USING 1 TO CURRENT_USER;

CREATE ROW POLICY IF NOT EXISTS migration_account ON trustiot.telemetry_1h_backfill
AS PERMISSIVE FOR SELECT USING 1 TO CURRENT_USER;

-- The backfill itself, over every receive day a Date can hold.
INSERT INTO trustiot.telemetry_1m (tenant_id, device_id, point, bucket, gateway_id, stream_id,
    received_day, readings, numeric_readings, value_sum, value_min, value_max, first, last)
SETTINGS deduplicate_insert_select = 'disable', max_execution_time = 25
SELECT tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day, readings,
    numeric_readings, value_sum, value_min, value_max, first, last
FROM trustiot.telemetry_1m_backfill(first_day = '1970-01-01', last_day = '2149-06-06');

INSERT INTO trustiot.telemetry_1h (tenant_id, device_id, point, bucket, gateway_id, stream_id,
    received_day, readings, numeric_readings, value_sum, value_min, value_max, first, last)
SETTINGS deduplicate_insert_select = 'disable', max_execution_time = 25
SELECT tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day, readings,
    numeric_readings, value_sum, value_min, value_max, first, last
FROM trustiot.telemetry_1h_backfill(first_day = '1970-01-01', last_day = '2149-06-06');
