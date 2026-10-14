-- Template: the tenant column and row-level security of the outbox crate's `outbox_aggregate`
-- (crates/outbox, decision 29, Laysi/trustiot-v2#413). The producing service's migration runs it
-- once, after `outbox::install_producer` and sql/outbox-event.sql. Replace `example_app` with the
-- producer's service role and `example_platform` with the producer's platform role.
--
-- An aggregate belongs to one tenant, or to the platform (tenant_id NULL): the tenant of its
-- events. The primary key stays (aggregate_type, aggregate_id), so an aggregate keeps one
-- sequence and one row lock, as the ordering of crates/outbox, the UNIQUE (aggregate_type,
-- aggregate_id, aggregate_seq) of outbox_event and the consumers' outbox_version expect. A
-- service or platform role that writes an event for an aggregate of another tenant (or of the
-- platform) is refused with 42501; it never starts a second sequence.
--
-- The column comes in the same transaction as its policies, never before them: service-kit
-- refuses every session of a role that may use a table with a tenant_id column but row-level
-- security off, so a column committed on its own would lock the service out.
--
-- tests/tenant_db.rs runs this file as it is.

-- 1. The tenant column. A database that ran the outbox before this file has aggregates already;
--    each gets the tenant of its last event (the role running this file owns both tables, so
--    row-level security does not hide events from it). `publish` writes an aggregate and its
--    event in one transaction, so every aggregate has a last event; a row without one would
--    stay a platform aggregate.
ALTER TABLE outbox_aggregate ADD COLUMN tenant_id uuid;
UPDATE outbox_aggregate AS a
SET tenant_id = e.tenant_id
FROM outbox_event AS e
WHERE e.aggregate_type = a.aggregate_type
  AND e.aggregate_id = a.aggregate_id
  AND e.aggregate_seq = a.last_seq;

-- 2. A writer that leaves the column out gets the transaction's tenant, the only value policy 3
--    accepts. `publish` names the column; the default keeps a producer that still runs the
--    version before this file writing new aggregates while its pods are replaced. Set after
--    step 1, so no existing row takes it.
ALTER TABLE outbox_aggregate
    ALTER COLUMN tenant_id SET DEFAULT NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid;

ALTER TABLE outbox_aggregate ENABLE ROW LEVEL SECURITY;

-- 3. Every role reads and writes only the aggregates of the transaction's tenant, as with a tenant
--    table (sql/tenant-table.sql). Without a tenant, no row matches. `publish` upserts: when the
--    aggregate already exists in another tenant, Postgres checks this policy's USING against
--    the existing row and raises 42501 instead of updating it.
CREATE POLICY tenant_isolation ON outbox_aggregate
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- 4. Platform aggregates (tenant_id NULL) are those of platform-wide events, which only the
--    producer's platform role writes (sql/outbox-event.sql, policy 2). FOR ALL: the upsert reads
--    its row back (RETURNING), inserts, and updates.
CREATE POLICY platform_aggregates ON outbox_aggregate
    TO example_platform
    USING (tenant_id IS NULL)
    WITH CHECK (tenant_id IS NULL);

-- 5. The platform role reaches no tenant's aggregate, whatever tenant its session holds. Policy 3
--    applies to every role and permissive policies are combined with OR, so without this one a
--    platform session that chose its own tenant in `trustiot.tenant_id` would read and advance
--    that tenant's sequences, as sql/outbox-event.sql explains for its policy 3
--    (Laysi/trustiot-v2#360). Service-kit's connection check counts permissive policies only, so
--    it accepts the platform role's sessions because of policy 4.
CREATE POLICY platform_aggregates_only ON outbox_aggregate
    AS RESTRICTIVE
    TO example_platform
    USING (tenant_id IS NULL)
    WITH CHECK (tenant_id IS NULL);

-- 6. Both producer roles upsert the sequence and read it back. No DELETE (an aggregate's sequence
--    never restarts) and never TRUNCATE (row-level security does not apply to it).
GRANT SELECT, INSERT, UPDATE ON outbox_aggregate TO example_app, example_platform;
