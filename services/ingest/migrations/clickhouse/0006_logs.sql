-- Service logs and traces written by the OpenTelemetry collector (decision 22,
-- Laysi/trustiot-v2#73).
--
-- The trustiot_logs database comes from 0001_databases.sql: the migration Job and the tests apply
-- the files in file name order, so it exists before these statements run.
--
-- The collector's ClickHouse exporter runs with create_schema off, so it never creates or
-- alters tables: these statements own the schema, and the exporter only INSERTs. Column names
-- and types follow the exporter's DDL (exporter/clickhouseexporter/internal/sqltemplates of
-- opentelemetry-collector-contrib v0.161.0, the version deploy/chart pins); the exporter reads
-- the columns with DESC TABLE at start. A collector upgrade that adds columns needs a new
-- migration (ALTER TABLE ... ADD COLUMN IF NOT EXISTS), see the exporter's upgrade notes.
--
-- Differences from the exporter's default DDL:
-- - ReceivedAt is the server time of the insert (a column default, the exporter does not
--   send it). Partitions and the retention TTL use it, not the emitter's Timestamp, so a wrong
--   clock on the sending side cannot make rows expire early or never (Laysi/trustiot#180).
-- - No materialized columns for Kubernetes resource attributes, and no trace id lookup table
--   or materialized view: only MergeTree tables in this database.
--
-- Retention: 30 days for logs and traces. Change it with a new migration
-- (ALTER TABLE ... MODIFY TTL). The collector redacts credentials, personal data, request
-- bodies and chat content before it writes here (deploy/chart/README.md).

CREATE TABLE IF NOT EXISTS trustiot_logs.otel_logs
(
    Timestamp DateTime64(9) CODEC(Delta(8), ZSTD(1)) COMMENT 'Event time from the emitter, nanoseconds',
    TraceId String CODEC(ZSTD(1)) COMMENT 'W3C trace id, empty outside a trace',
    SpanId String CODEC(ZSTD(1)) COMMENT 'W3C span id',
    TraceFlags UInt8 COMMENT 'W3C trace flags',
    SeverityText LowCardinality(String) CODEC(ZSTD(1)) COMMENT 'Level as text, for example INFO',
    SeverityNumber UInt8 COMMENT 'OpenTelemetry severity number, 1 to 24',
    ServiceName LowCardinality(String) CODEC(ZSTD(1)) COMMENT 'service.name resource attribute',
    Body String CODEC(ZSTD(1)) COMMENT 'Log message, redacted by the collector',
    ResourceSchemaUrl LowCardinality(String) CODEC(ZSTD(1)),
    ResourceAttributes Map(LowCardinality(String), String) CODEC(ZSTD(1)),
    ScopeSchemaUrl LowCardinality(String) CODEC(ZSTD(1)),
    ScopeName String CODEC(ZSTD(1)) COMMENT 'Instrumentation scope, for the services the tracing target',
    ScopeVersion LowCardinality(String) CODEC(ZSTD(1)),
    ScopeAttributes Map(LowCardinality(String), String) CODEC(ZSTD(1)),
    LogAttributes Map(LowCardinality(String), String) CODEC(ZSTD(1)) COMMENT 'Structured fields of the event, redacted by the collector',
    EventName String CODEC(ZSTD(1)),
    ReceivedAt DateTime DEFAULT now() CODEC(Delta(4), ZSTD(1)) COMMENT 'Server time of the insert, drives partitioning and retention',
    INDEX idx_trace_id TraceId TYPE bloom_filter(0.001) GRANULARITY 1,
    INDEX idx_res_attr_key mapKeys(ResourceAttributes) TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_res_attr_value mapValues(ResourceAttributes) TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_scope_attr_key mapKeys(ScopeAttributes) TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_scope_attr_value mapValues(ScopeAttributes) TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_log_attr_key mapKeys(LogAttributes) TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_log_attr_value mapValues(LogAttributes) TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_lower_body lower(Body) TYPE tokenbf_v1(32768, 3, 0) GRANULARITY 8
)
ENGINE = MergeTree
PARTITION BY toDate(ReceivedAt)
ORDER BY (toStartOfFiveMinutes(Timestamp), ServiceName, Timestamp)
TTL ReceivedAt + INTERVAL 30 DAY
SETTINGS index_granularity = 8192, ttl_only_drop_parts = 1
COMMENT 'Service logs from the OpenTelemetry collector, 30 day retention';

CREATE TABLE IF NOT EXISTS trustiot_logs.otel_traces
(
    Timestamp DateTime64(9) CODEC(Delta(8), ZSTD(1)) COMMENT 'Span start time from the emitter, nanoseconds',
    TraceId String CODEC(ZSTD(1)),
    SpanId String CODEC(ZSTD(1)),
    ParentSpanId String CODEC(ZSTD(1)),
    TraceState String CODEC(ZSTD(1)),
    SpanName LowCardinality(String) CODEC(ZSTD(1)),
    SpanKind LowCardinality(String) CODEC(ZSTD(1)),
    ServiceName LowCardinality(String) CODEC(ZSTD(1)),
    ResourceAttributes Map(LowCardinality(String), String) CODEC(ZSTD(1)),
    ScopeName String CODEC(ZSTD(1)),
    ScopeVersion String CODEC(ZSTD(1)),
    SpanAttributes Map(LowCardinality(String), String) CODEC(ZSTD(1)) COMMENT 'Redacted by the collector',
    Duration UInt64 CODEC(ZSTD(1)) COMMENT 'Nanoseconds',
    StatusCode LowCardinality(String) CODEC(ZSTD(1)),
    StatusMessage String CODEC(ZSTD(1)),
    Events Nested
    (
        Timestamp DateTime64(9),
        Name LowCardinality(String),
        Attributes Map(LowCardinality(String), String)
    ) CODEC(ZSTD(1)),
    Links Nested
    (
        TraceId String,
        SpanId String,
        TraceState String,
        Attributes Map(LowCardinality(String), String)
    ) CODEC(ZSTD(1)),
    ReceivedAt DateTime DEFAULT now() CODEC(Delta(4), ZSTD(1)) COMMENT 'Server time of the insert, drives partitioning and retention',
    INDEX idx_trace_id TraceId TYPE bloom_filter(0.001) GRANULARITY 1,
    INDEX idx_res_attr_key mapKeys(ResourceAttributes) TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_res_attr_value mapValues(ResourceAttributes) TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_span_attr_key mapKeys(SpanAttributes) TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_span_attr_value mapValues(SpanAttributes) TYPE bloom_filter(0.01) GRANULARITY 1,
    INDEX idx_duration Duration TYPE minmax GRANULARITY 1
)
ENGINE = MergeTree
PARTITION BY toDate(ReceivedAt)
ORDER BY (ServiceName, SpanName, toDateTime(Timestamp))
TTL ReceivedAt + INTERVAL 30 DAY
SETTINGS index_granularity = 8192, ttl_only_drop_parts = 1
COMMENT 'Service traces from the OpenTelemetry collector, 30 day retention';

-- The collector's account gets this role. Like every account it is created by the migration Job
-- with its password from a Kubernetes Secret (deploy/chart: observability.collector.clickhouse):
--
--   CREATE USER <name> IDENTIFIED WITH sha256_password BY '<password>';
--   GRANT trustiot_otel_collector TO <name>;
--
-- INSERT writes rows; SHOW COLUMNS lets the exporter read the columns (DESC TABLE) at start.
-- No SELECT: the collector never reads log data back, and no other database is reachable.
CREATE ROLE IF NOT EXISTS trustiot_otel_collector;

GRANT INSERT, SHOW COLUMNS ON trustiot_logs.otel_logs TO trustiot_otel_collector;

GRANT INSERT, SHOW COLUMNS ON trustiot_logs.otel_traces TO trustiot_otel_collector;
