-- Monitoring of the ClickHouse server itself (rebuild checklist I2, Laysi/trustiot-v2#92).
--
-- A full disk stops ClickHouse: inserts fail, and the merges that apply the retention TTL need
-- free space to run. The disk watermark alert (services/ingest/alerts/clickhouse_disk_watermark.sql)
-- reads system.disks, which ClickHouse 26.8 shows only to accounts granted SELECT on it.
--
-- The role trustiot_monitoring holds that grant and nothing else: no TrustIoT database, no
-- other system table. The account that evaluates the alerts (Grafana, Laysi/trustiot-v2#183) gets
-- this role; like every account it is created by the migration Job with its password from a
-- Kubernetes Secret, never by hand.

CREATE ROLE IF NOT EXISTS trustiot_monitoring;

GRANT SELECT ON system.disks TO trustiot_monitoring;
