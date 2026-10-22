-- The event layer and operation log (Laysi/trustiot-v2#130; services/identity/README.md,
-- "事件層與操作紀錄"; docs/architecture/decisions.md, decision 34). Every service writes its
-- events to its own outbox in the transaction of the change (crates/outbox); the identity
-- service consumes the outbox of every producer and keeps each event here, once. The rows are
-- the platform's operation log (checklist K2): platform administrators read them per tenant in
-- the back office, and a tenant's administrators read their own tenant's.
--
-- Append-only: no role but the table's owner (`identity_owner`, the migration Job) may change
-- or remove a row. The service role inserts (the consumer) and reads under row-level security
-- (a tenant administrator's query); the platform role only reads (the back office). Neither
-- gets UPDATE, DELETE or TRUNCATE, so a bug or a compromised service cannot rewrite history.
--
-- Run by `identity_owner` like 0001_identity.sql. The outbox consumer tables
-- (`outbox_subscription` and the others) are created by the migration Job itself, because the
-- registry lists identity as a consumer of the producers in `src/schema.rs` (`CONSUMES`).

-- One row per tenant event: tenant data, so it follows crates/service-kit/sql/tenant-table.sql.
-- The columns are the envelope's (crates/contract, "事件") and the outbox's aggregate; the
-- payload is kept as the producer wrote it, whatever its version.
CREATE TABLE identity.event_log (
    tenant_id uuid NOT NULL,
    event_id uuid NOT NULL,
    -- When the change happened, by the producer's clock (the envelope's `occurred_at`).
    occurred_at timestamptz NOT NULL,
    event_type text NOT NULL,
    version integer NOT NULL CHECK (version >= 1),
    producer text NOT NULL,
    aggregate_type text NOT NULL,
    aggregate_id text NOT NULL,
    aggregate_seq bigint NOT NULL CHECK (aggregate_seq >= 1),
    -- Who made the change (the envelope's `actor`): a user through the API, or a service's own
    -- work; both NULL when the producer did not say.
    actor_user_id uuid,
    actor_service text,
    payload jsonb NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
    -- When the event log stored it, by the database clock.
    recorded_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, event_id),
    CHECK (actor_user_id IS NULL OR actor_service IS NULL)
);

-- The queries: one tenant's events in a time range, newest first, paged by (time, event ID).
CREATE INDEX event_log_by_time ON identity.event_log (tenant_id, occurred_at, event_id);

ALTER TABLE identity.event_log ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON identity.event_log
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- The back office reads every tenant's events (a platform administrator, through PlatformDb,
-- which logs the actor and purpose). Read only: the platform role writes nothing here.
CREATE POLICY platform_access ON identity.event_log
    FOR SELECT
    TO identity_platform
    USING (true);

-- Platform-wide events (the envelope's `tenant_id` is null): no tenant data, so no tenant
-- column and no row-level security. The consumer writes them in a transaction of the service
-- role without a tenant, which cannot write a row of the table above.
CREATE TABLE identity.platform_event_log (
    event_id uuid PRIMARY KEY,
    occurred_at timestamptz NOT NULL,
    event_type text NOT NULL,
    version integer NOT NULL CHECK (version >= 1),
    producer text NOT NULL,
    aggregate_type text NOT NULL,
    aggregate_id text NOT NULL,
    aggregate_seq bigint NOT NULL CHECK (aggregate_seq >= 1),
    actor_user_id uuid,
    actor_service text,
    payload jsonb NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
    recorded_at timestamptz NOT NULL DEFAULT now(),
    CHECK (actor_user_id IS NULL OR actor_service IS NULL)
);

CREATE INDEX platform_event_log_by_time ON identity.platform_event_log (occurred_at, event_id);

-- INSERT for the consumer, SELECT for a tenant administrator's query (row-level security keeps
-- it to the transaction's tenant). The platform-wide log is written by the consumer only; the
-- service role does not read it. No UPDATE, DELETE or TRUNCATE for anyone: append-only.
GRANT SELECT, INSERT ON identity.event_log TO identity_app;
GRANT INSERT ON identity.platform_event_log TO identity_app;
GRANT SELECT ON identity.event_log, identity.platform_event_log TO identity_platform;
