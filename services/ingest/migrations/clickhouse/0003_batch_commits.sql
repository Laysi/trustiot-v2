-- Batch completion table (decision 8; rebuild checklist I1).
--
-- The ingest service writes one record after every row of a batch is in trustiot.telemetry and
-- on disk, and acknowledges the batch only after this record is on disk too. A stream's
-- committed high-water mark is max(seq) here, never anything derived from telemetry: an INSERT
-- that spans partitions is not atomic, so telemetry can hold part of a batch that never
-- completed.
--
-- Retention: records expire 400 days after committed_at, the same interval as telemetry's
-- received_at TTL. committed_at is never earlier than the received_at of the batch's rows, so
-- a completion record outlives the rows it describes. Change both TTLs together
-- (Laysi/trustiot-v2#92); the schema test fails when this interval is shorter.
--
-- ReplacingMergeTree collapses a record written twice for the same (stream_id, seq).

CREATE TABLE IF NOT EXISTS trustiot.batch_commits
(
    tenant_id UUID COMMENT 'Tenant that owns the stream',
    gateway_id UUID COMMENT 'Gateway that sent the batch',
    stream_id UUID COMMENT 'Gateway batch stream',
    seq UInt64 COMMENT 'Batch sequence number within the stream',
    row_count UInt64 COMMENT 'Number of telemetry rows written for the batch',
    committed_at DateTime64(3, 'UTC') CODEC(DoubleDelta, ZSTD(1)) COMMENT 'When every row of the batch was on disk, UTC, milliseconds',
    CONSTRAINT tenant_id_set CHECK tenant_id != toUUID('00000000-0000-0000-0000-000000000000')
)
ENGINE = ReplacingMergeTree
PARTITION BY toYYYYMM(committed_at)
ORDER BY (stream_id, seq)
TTL toDateTime(committed_at) + INTERVAL 400 DAY
SETTINGS
    fsync_after_insert = 1,
    fsync_part_directory = 1
COMMENT 'One record per fully written gateway batch, source of each stream''s high-water mark';
