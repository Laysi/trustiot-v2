INSERT INTO dev_seed.telemetry
SELECT
    machine_id,
    toDateTime('2026-01-01 00:00:00', 'UTC') + toIntervalMinute(minute) AS ts,
    metric,
    round(20 + 5 * sin(minute / 7) + if(metric = 'power_kw', 30, 0), 3) AS value
FROM (SELECT arrayJoin(['m-001', 'm-002']) AS machine_id) AS machines
CROSS JOIN (SELECT arrayJoin(['temperature_c', 'power_kw']) AS metric) AS metrics
CROSS JOIN (SELECT number AS minute FROM numbers(60)) AS minutes
WHERE (SELECT count() FROM dev_seed.telemetry) = 0
