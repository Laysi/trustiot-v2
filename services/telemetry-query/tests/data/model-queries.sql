-- Queries a model writes over the telemetry views (Laysi/trustiot-v2#143), one per paragraph,
-- separated by lines of %%. The SQL check's unit tests (src/sql/tests.rs) require each to pass
-- and print stably; tests/sql_api.rs runs each through the endpoint on ClickHouse.
%%
SELECT count(*) FROM tenant_telemetry
%%
SELECT device_id, point, max(ts) FROM trustiot.tenant_telemetry GROUP BY device_id, point
%%
SELECT toStartOfDay(bucket, 'Asia/Taipei') AS day, sum(value_sum) / sum(numeric_readings) AS avg
     FROM trustiot.tenant_telemetry_1h
     WHERE point = 'power' AND bucket >= now() - INTERVAL 30 DAY
     GROUP BY day ORDER BY day
%%
SELECT device_id, argMax(last.value_float, last.ts) FROM tenant_telemetry_1m GROUP BY device_id
%%
SELECT t.device_id, t.last.value_float FROM tenant_telemetry_1m t LIMIT 10
%%
WITH daily AS (SELECT toDate(bucket, 'UTC') AS d, sum(readings) AS n FROM tenant_telemetry_1h GROUP BY d)
     SELECT d, n, n - lagInFrame(n) OVER (ORDER BY d) FROM daily ORDER BY d
%%
SELECT quantile(0.95)(value_float), quantiles(0.5, 0.9)(value_float) FROM tenant_telemetry
%%
SELECT countIf(value_float > 10), avgIfOrNull(value_float, value_float > 0) FROM tenant_telemetry
%%
SELECT arrayMap(x -> x * 2, groupArray(10)(value_float)) FROM tenant_telemetry
%%
SELECT arraySort((x, y) -> y, [1, 2], [2, 1])[1] FROM tenant_telemetry_1m
%%
SELECT point FROM tenant_telemetry WHERE device_id IN ('0199a0b0-c0d0-7e0f-8a1b-2c3d4e5f6a7b') AND value_int IN (-1, 2)
%%
SELECT point FROM tenant_telemetry WHERE (point, value_int) IN (('a', 1), ('b', 2))
%%
SELECT a.point FROM tenant_telemetry_1m AS a JOIN tenant_telemetry_1h AS b USING (device_id, point)
%%
SELECT a.point FROM tenant_telemetry_1m AS a LEFT JOIN tenant_telemetry_1h AS b ON a.point = b.point
%%
SELECT a.point FROM tenant_telemetry_1m AS a CROSS JOIN tenant_telemetry_1h AS b
%%
SELECT a.point FROM tenant_telemetry_1m AS a, tenant_telemetry_1h AS b WHERE a.point = b.point
%%
SELECT a.point FROM tenant_telemetry_1m AS a LEFT SEMI JOIN tenant_telemetry_1h AS b ON a.point = b.point
%%
SELECT point FROM tenant_telemetry WHERE EXISTS (SELECT 1 FROM tenant_telemetry_1m)
%%
SELECT point, (SELECT max(bucket) FROM tenant_telemetry_1h) FROM tenant_telemetry
%%
SELECT point FROM tenant_telemetry WHERE device_id IN (SELECT device_id FROM tenant_telemetry_1h)
%%
SELECT * FROM (SELECT point, ts FROM tenant_telemetry ORDER BY ts DESC LIMIT 5) ORDER BY ts
%%
SELECT point, count() FROM tenant_telemetry GROUP BY ROLLUP (point)
%%
SELECT point, count() FROM tenant_telemetry GROUP BY ALL ORDER BY ALL
%%
SELECT point, count() FROM tenant_telemetry GROUP BY point WITH ROLLUP HAVING count() > 1
%%
SELECT DISTINCT point FROM tenant_telemetry ORDER BY point DESC NULLS LAST LIMIT 5 OFFSET 2
%%
SELECT point FROM tenant_telemetry LIMIT 2, 3
%%
SELECT point, ts FROM tenant_telemetry ORDER BY ts DESC LIMIT 1 BY point
%%
SELECT point FROM tenant_telemetry_1m UNION ALL SELECT point FROM tenant_telemetry_1h
%%
SELECT point FROM tenant_telemetry_1m EXCEPT DISTINCT SELECT point FROM tenant_telemetry_1h
%%
SELECT row_number() OVER w, sum(readings) OVER (PARTITION BY point ORDER BY bucket ROWS BETWEEN 2 PRECEDING AND CURRENT ROW)
     FROM tenant_telemetry_1m WINDOW w AS (ORDER BY bucket)
%%
SELECT CASE WHEN value_float > 1 THEN 'high' ELSE 'low' END, if(value_bool, 1, 0), multiIf(value_int > 1, 'a', 'b') FROM tenant_telemetry
%%
SELECT CAST(value_int AS Float64), value_int::String, CAST(value_float AS UInt32) FROM tenant_telemetry
%%
SELECT FLOOR(value_float), ceil(value_float, 2), SUBSTRING(point, 1, 3), round(value_float, 1) FROM tenant_telemetry
%%
SELECT formatDateTime(ts, '%F %T', 'Asia/Taipei'), dateDiff('day', ts, now(), 'UTC') FROM tenant_telemetry
%%
SELECT toStartOfInterval(ts, INTERVAL 15 MINUTE, 'UTC'), toStartOfWeek(ts, 1, 'UTC') FROM tenant_telemetry
%%
SELECT toHour(ts, 'Asia/Taipei'), toDayOfWeek(ts, 0, 'UTC'), toDate('2026-10-01'), toDateTime(ts) FROM tenant_telemetry
%%
SELECT point FROM tenant_telemetry WHERE ts >= toDateTime('2026-10-01 00:00:00', 'Asia/Taipei')
%%
SELECT point FROM tenant_telemetry WHERE point LIKE 'temp%' AND point NOT ILIKE '%x' AND value_float BETWEEN 1 AND 2
%%
SELECT point FROM tenant_telemetry WHERE value_string IS NOT NULL OR NOT value_bool
%%
SELECT 'it''s', concat(point, '-', toString(value_int)), point || '!' FROM tenant_telemetry
%%
SELECT tupleElement(first, 'value_float'), first.ts FROM tenant_telemetry_1h
%%
SELECT count(DISTINCT point), uniqExact(device_id) FROM tenant_telemetry
%%
(SELECT point FROM tenant_telemetry)
%%
SELECT 1 FROM tenant_telemetry; -- one statement, a trailing comment
%%
SELECT 1e3, 2.5E-1, true, NULL FROM tenant_telemetry
