-- Disk watermark of the ClickHouse server (rebuild checklist I2, Laysi/trustiot-v2#92).
--
-- Metric: used_ratio, the share of each local ClickHouse disk in use (1 - free / total, from
-- system.disks, which ClickHouse reads from the file system at query time).
--
-- Alert rule: severity is warning from 0.80 and critical from 0.90; ok below. The alert fires
-- for a disk while its severity is not ok, and an evaluator should require it for 5 minutes
-- before notifying, so that a merge's temporary parts do not page anyone. Why these thresholds and
-- what to do when it fires: docs/architecture/data-retention.md, section 「磁碟水位告警」.
--
-- Run it as an account with the role trustiot_monitoring (0007_monitoring.sql). Grafana evaluates
-- it once it is deployed (Laysi/trustiot-v2#183), as a ClickHouse data source query with used_ratio as
-- the value and disk as the label; services/ingest/tests/disk_watermark.rs fills a real
-- ClickHouse disk and checks each severity.
--
-- One statement, no trailing semicolon, so the file can be sent as it is.

SELECT
    name AS disk,
    path,
    total_space AS total_bytes,
    free_space AS free_bytes,
    round(1 - free_space / total_space, 4) AS used_ratio,
    multiIf(used_ratio >= 0.90, 'critical', used_ratio >= 0.80, 'warning', 'ok') AS severity
FROM system.disks
WHERE NOT is_remote AND total_space > 0
ORDER BY disk
