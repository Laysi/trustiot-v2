-- Template: row-level security for the outbox crate's `outbox_event` (crates/outbox, decision
-- 29). The producing service's migration runs it after `outbox::install_producer`. Replace
-- `example_app` with the producer's service role, `example_platform` with the producer's
-- platform role, and `example_reader` with the platform role of each consuming service.
--
-- tests/tenant_db.rs runs this file as it is.

ALTER TABLE outbox_event ENABLE ROW LEVEL SECURITY;

-- 1. The service role writes events of the transaction's tenant only: an event of another
--    tenant, or a platform-wide event (tenant_id NULL), fails the WITH CHECK. It reads no event:
--    `publish` inserts without RETURNING, so it gets no SELECT grant.
CREATE POLICY tenant_isolation ON outbox_event
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- 2. Platform-wide events (tenant_id NULL) are written only by the producer's platform role,
--    through service-kit's PlatformDb, which logs the actor and purpose. It cannot write events
--    of a tenant.
CREATE POLICY platform_events ON outbox_event
    FOR INSERT
    TO example_platform
    WITH CHECK (tenant_id IS NULL);

-- 3. Delivery reads every tenant's events, so consumers read them as their platform role, never
--    as their service role (whose sessions service-kit refuses when a policy opens a table to
--    them across tenants). The consumer's handler then works in a tenant transaction of its
--    service role.
CREATE POLICY consumer_read ON outbox_event
    FOR SELECT
    TO example_reader
    USING (true);

GRANT INSERT ON outbox_event TO example_app, example_platform;
GRANT SELECT ON outbox_event TO example_reader;
