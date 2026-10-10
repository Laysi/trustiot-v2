-- Telemetry narrow table (decision 14; rebuild checklist I2).
--
-- One row per reading: fixed columns, never altered at runtime (Laysi/trustiot#103). Each
-- reading's value sits in the column for its type; the other value columns keep their
-- defaults, which the value_matches_type constraint enforces.
--
-- Times are UTC with millisecond precision. ts is the gateway's reading time; received_at is
-- the server's receive time, stamped by the ingest service. Partitions and the retention TTL
-- use received_at, so an unsynchronised gateway clock that sends old timestamps cannot make
-- rows expire early (Laysi/trustiot#180).
--
-- Retention: 400 days is a placeholder until Laysi/trustiot-v2#92 sets the platform value; it
-- changes through a new migration (ALTER TABLE ... MODIFY TTL), and batch_commits must keep
-- its records at least as long (0003_batch_commits.sql).
--
-- Durability (decision 8): fsync_after_insert and fsync_part_directory make a successful
-- INSERT mean the part is on disk, not only in the page cache. The non-replicated
-- deduplication window drops a retried block that carries the same insert_deduplication_token;
-- it only counts recent blocks, so long-offline resends are deduplicated through
-- batch_commits instead (rebuild checklist I1).
--
-- stream_id and batch_seq identify the gateway batch a row came from, so the ingest service can
-- delete the rows of a batch that has no completion record before writing it again (I1).

CREATE TABLE IF NOT EXISTS trustiot.telemetry
(
    tenant_id UUID COMMENT 'Tenant that owns the gateway, taken from the gateway connection certificate, never from the payload',
    gateway_id UUID COMMENT 'Gateway that sent the batch',
    stream_id UUID COMMENT 'Gateway batch stream, batch sequence numbers are unique within a stream',
    batch_seq UInt64 COMMENT 'Sequence number of the batch within its stream',
    device_id UUID COMMENT 'Device the reading belongs to',
    point LowCardinality(String) COMMENT 'Data point key within the device model (C1)',
    ts DateTime64(3, 'UTC') CODEC(DoubleDelta, ZSTD(1)) COMMENT 'Reading time from the gateway, UTC, milliseconds',
    received_at DateTime64(3, 'UTC') CODEC(DoubleDelta, ZSTD(1)) COMMENT 'Server receive time, UTC, milliseconds, drives partitioning and retention',
    value_type Enum8('float' = 1, 'int' = 2, 'bool' = 3, 'string' = 4) COMMENT 'Which value column holds the reading',
    value_float Float64 DEFAULT 0 CODEC(Gorilla, ZSTD(1)),
    value_int Int64 DEFAULT 0 CODEC(T64, ZSTD(1)),
    value_bool Bool DEFAULT false,
    value_string String DEFAULT '' CODEC(ZSTD(1)),
    CONSTRAINT tenant_id_set CHECK tenant_id != toUUID('00000000-0000-0000-0000-000000000000'),
    CONSTRAINT point_length CHECK length(point) BETWEEN 1 AND 128,
    CONSTRAINT value_matches_type CHECK
        (value_type = 'float' OR value_float = 0)
        AND (value_type = 'int' OR value_int = 0)
        AND (value_type = 'bool' OR value_bool = false)
        AND (value_type = 'string' OR value_string = '')
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(received_at)
ORDER BY (tenant_id, device_id, point, ts)
TTL toDateTime(received_at) + INTERVAL 400 DAY
SETTINGS
    fsync_after_insert = 1,
    fsync_part_directory = 1,
    non_replicated_deduplication_window = 1000
COMMENT 'Tenant telemetry readings, tenant isolation by row policy (0004_access.sql)';
