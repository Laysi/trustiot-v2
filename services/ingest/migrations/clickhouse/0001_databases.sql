-- Databases owned by the ingest service (decision 14).
--
-- trustiot holds tenant telemetry and the batch completion table. trustiot_logs holds service
-- and gateway logs written by the OpenTelemetry collector (decision 22,
-- Laysi/trustiot-v2#73); it is a separate database so that it can be moved, retained or
-- dropped independently of tenant data.

CREATE DATABASE IF NOT EXISTS trustiot
COMMENT 'TrustIoT tenant telemetry, schema in services/ingest/migrations/clickhouse';

CREATE DATABASE IF NOT EXISTS trustiot_logs
COMMENT 'TrustIoT service and gateway logs (OpenTelemetry collector)';
