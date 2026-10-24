-- Gateway and stream IDs as the MQTT protocol defines them, and a completion table keyed by
-- gateway plus stream (Laysi/trustiot-v2#639, split from Laysi/trustiot-v2#91).
--
-- 0002_telemetry.sql and 0003_batch_commits.sql typed gateway_id and stream_id as UUID. The
-- protocol's GatewayId and StreamId are text (crates/protocol/src/ids.rs): a gateway ID is the
-- hardware ID, the common name of the gateway's certificate and the gateway segment of its
-- topics, and the gateway chooses its own stream IDs, for example stream-1. Both are 1 to 64
-- ASCII letters, digits, '.', '_' or '-', starting with a letter or digit. device_id stays UUID:
-- the assets service creates devices with UUIDs, and the query side parses them as UUIDs.
--
-- Telemetry rule 3 of crates/protocol/README.md keys a stream's high-water mark by the gateway of
-- the topic plus the stream ID: two gateways may use the same stream ID. batch_commits is ordered
-- by (stream_id, seq) only, so ReplacingMergeTree would merge two gateways' records for the same
-- stream ID and seq, and one gateway's high-water mark would hide the other's batches as
-- duplicates. A sorting key cannot change its columns' types, and migrations never drop a table,
-- so the completion records move to a new table, trustiot.stream_commits.
--
-- Existing data. Until this file no service wrote either table (the ingest service arrives with
-- Laysi/trustiot-v2#641). Should telemetry hold rows anyway, MODIFY COLUMN converts each UUID to
-- its text form, which is a valid protocol ID, so nothing is lost. trustiot.batch_commits stays as
-- it is, retired: the ingest role loses every privilege on it, nothing reads it as a high-water
-- mark, and the migration Job no longer manages its TTL. Its row policies stay, so it remains
-- deny-by-default for any account.

-- Telemetry: text IDs. Neither column is in the sorting key, so the type can change in place.
-- On rows already written this is a mutation that rewrites the two columns, which ClickHouse
-- runs in the background while reads convert the old parts. Repeating the statement on a String
-- column changes nothing.
ALTER TABLE trustiot.telemetry
MODIFY COLUMN gateway_id String COMMENT 'Gateway that sent the batch: protocol GatewayId, the gateway segment of the MQTT topic, never taken from the payload';

ALTER TABLE trustiot.telemetry
MODIFY COLUMN stream_id String COMMENT 'Gateway batch stream: protocol StreamId, chosen by the gateway, unique only within its gateway, so a batch is (gateway_id, stream_id, batch_seq)';

-- The protocol's identifier rule, so a row whose IDs could not have come from a valid topic or
-- batch is refused instead of written where the batch delete would never find it. Checked on
-- insert only, not against rows already written.
ALTER TABLE trustiot.telemetry
ADD CONSTRAINT IF NOT EXISTS gateway_id_valid CHECK match(gateway_id, '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$');

ALTER TABLE trustiot.telemetry
ADD CONSTRAINT IF NOT EXISTS stream_id_valid CHECK match(stream_id, '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$');

-- Batch completion table (decision 8, rebuild checklist I1).
--
-- The ingest service writes one record after every row of a batch is in trustiot.telemetry and
-- on disk, and acknowledges the batch only after this record is on disk too. A stream's committed
-- high-water mark is max(seq) over the records of one (gateway_id, stream_id), never anything
-- derived from telemetry: an INSERT that spans partitions is not atomic, so telemetry can hold
-- part of a batch that never completed. The answer to a duplicate batch reports row_count of the
-- first commit.
--
-- tenant_id and binding_id record whose gateway binding the batch was written under, for tracing
-- (decided on 2026-10-24, rules in Laysi/trustiot-v2#638). They are not part of the key: the
-- high-water mark belongs to the gateway and its stream, whoever the gateway is bound to.
--
-- ReplacingMergeTree collapses a record written twice for the same (gateway_id, stream_id, seq),
-- but only within one partition and only when a merge runs. Readers therefore never count
-- records as batches: max(seq) and the row_count of any one record are the same either way.
--
-- Retention: the migration Job sets this TTL to the raw data retention on every run, the same
-- number of days as telemetry's received_at TTL (tools/migrate, Laysi/trustiot-v2#92).
-- committed_at is never earlier than the received_at of the batch's rows, so a completion record
-- outlives the rows it describes. The table is created with 3650 days, the longest retention the
-- Job accepts, so it is never shorter than telemetry's TTL, also before the Job aligns it.

CREATE TABLE IF NOT EXISTS trustiot.stream_commits
(
    tenant_id UUID COMMENT 'Tenant the gateway was bound to when the batch was written, from the gateway connection certificate',
    binding_id UUID COMMENT 'Gateway binding in force when the batch was written, for tracing',
    gateway_id String COMMENT 'Gateway that sent the batch: protocol GatewayId, the gateway segment of the MQTT topic',
    stream_id String COMMENT 'Gateway batch stream: protocol StreamId, unique only within its gateway',
    seq UInt64 COMMENT 'Batch sequence number within the stream, from 1',
    row_count UInt64 COMMENT 'Number of telemetry rows written for the batch',
    committed_at DateTime64(3, 'UTC') CODEC(DoubleDelta, ZSTD(1)) COMMENT 'When every row of the batch was on disk, UTC, milliseconds',
    CONSTRAINT tenant_id_set CHECK tenant_id != toUUID('00000000-0000-0000-0000-000000000000'),
    CONSTRAINT binding_id_set CHECK binding_id != toUUID('00000000-0000-0000-0000-000000000000'),
    CONSTRAINT gateway_id_valid CHECK match(gateway_id, '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'),
    CONSTRAINT stream_id_valid CHECK match(stream_id, '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'),
    CONSTRAINT seq_positive CHECK seq >= 1
)
ENGINE = ReplacingMergeTree
PARTITION BY toYYYYMM(committed_at)
ORDER BY (gateway_id, stream_id, seq)
TTL toDateTime(committed_at) + INTERVAL 3650 DAY
SETTINGS
    fsync_after_insert = 1,
    fsync_part_directory = 1
COMMENT 'One record per fully written gateway batch, keyed by gateway and stream, source of each stream''s high-water mark';

-- Access, as 0004_access.sql gave the old table: the ingest service reads and writes completion
-- records but never deletes or changes them, deny_by_default hides every row from any account no
-- other policy names, and the tenant role gets nothing.
GRANT SELECT, INSERT ON trustiot.stream_commits TO trustiot_ingest;

CREATE ROW POLICY IF NOT EXISTS deny_by_default ON trustiot.stream_commits
AS PERMISSIVE FOR SELECT USING 0 TO ALL;

CREATE ROW POLICY IF NOT EXISTS service_all_rows ON trustiot.stream_commits
AS PERMISSIVE FOR SELECT USING 1 TO trustiot_ingest;

-- The retired table: no privilege for the ingest role, so no code can take a high-water mark
-- from it by mistake.
REVOKE ALL ON trustiot.batch_commits FROM trustiot_ingest;

-- The tenant view's columns follow the table's. A view keeps the column list it was created
-- with, types included, so without this statement the view would still declare gateway_id UUID.
-- CREATE VIEW IF NOT EXISTS in 0005_tenant_view.sql never changes an existing view, and
-- ClickHouse 26.8 refuses ALTER TABLE ... MODIFY QUERY on a plain view ("Alter of type
-- 'MODIFY_QUERY' is not supported by storage View", NOT_IMPLEMENTED), although the comment of
-- 0005_tenant_view.sql names it. So the view is replaced: the only CREATE OR REPLACE the
-- migration files allow, for plain views only (tests/migrations.rs). A plain view holds no data,
-- and ClickHouse keeps grants and row policies by the relation's name, so the tenant role's
-- SELECT and the view's deny_by_default and tenant_isolation policies apply to the new view as
-- they did to the old one. In an Atomic database the replacement is an exchange: a query sees
-- the old view or the new one, never none. The statement names the same DEFINER, SQL SECURITY,
-- columns and comment as 0005_tenant_view.sql, still without stream_id or batch_seq, so repeating
-- it changes nothing (tests/clickhouse.rs and tests/migrations.rs read all of it back).
CREATE OR REPLACE VIEW trustiot.tenant_telemetry
DEFINER = trustiot_tenant_view_definer SQL SECURITY DEFINER
AS SELECT
    tenant_id,
    gateway_id,
    device_id,
    point,
    ts,
    received_at,
    value_type,
    value_float,
    value_int,
    value_bool,
    value_string
FROM trustiot.telemetry
COMMENT 'Telemetry readings of the calling tenant account, the only relation tenant accounts may read (0005_tenant_view.sql)';
