-- The certs service's confirmation that it applied a tenant's state (Laysi/trustiot-v2#779,
-- checklist B1; services/identity/README.md, "停權的處理中"): the identity service consumes
-- `certs.tenant_state_applied` and keeps, per tenant, the largest state version certs confirmed.
-- `GET /api/identity/v1/platform/tenants/{tenant_id}/state` shows a suspended or deleted tenant as
-- `processing` while that version is smaller than the tenant's `state_version`. Run by
-- `identity_owner` like 0001_identity.sql.
--
-- 1. `certs_applied_version` starts at 0, "certs confirmed nothing yet", for new and existing
--    tenants alike: a tenant suspended before this file shows `processing` until certs confirms.
-- 2. It only goes up (the trigger below, for every writer, the service role included), so a late
--    or replayed confirmation of an older version never takes back a newer one.
-- 3. It never passes `state_version` (the CHECK): a confirmation of a version the identity
--    service never wrote cannot mark a later suspension applied before certs saw it.
-- 4. The service role gets UPDATE on this one column: the consumer writes it in a transaction of
--    the event's tenant, which row-level security (`tenant_isolation`, 0002) keeps to that row.
--
-- Older replicas of the service name the columns they read and write, so they work unchanged on
-- this schema during a rollout. Reverting means dropping the trigger, its function and the column;
-- no other schema object refers to them. The subscription `identity.tenant-state-applied.certs`
-- keeps its position, so after applying this file again the column starts at 0 and every
-- suspension shows `processing` until certs confirms it anew; to rebuild it from the confirmations
-- already written, replay the subscription from the start with `Replay::Reprocess`
-- (crates/outbox/README.md).

-- Fail fast, not queue every request behind the table lock; the Job fails and retries.
SET LOCAL lock_timeout = '5s';

ALTER TABLE identity.tenants
    ADD COLUMN certs_applied_version bigint NOT NULL DEFAULT 0
        CHECK (certs_applied_version >= 0),
    ADD CONSTRAINT tenants_certs_applied_not_ahead
        CHECK (certs_applied_version <= state_version);

-- An invoker's trigger function, like identity.tenants_state_guard (0008_tenant_state.sql):
-- ERRCODE 42501 (insufficient_privilege), like a missing grant.
CREATE FUNCTION identity.tenants_certs_applied_guard() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.certs_applied_version < OLD.certs_applied_version THEN
        RAISE EXCEPTION 'a tenant''s confirmed state version never goes back'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    RETURN NEW;
END;
$$;

REVOKE ALL ON FUNCTION identity.tenants_certs_applied_guard() FROM PUBLIC;

CREATE TRIGGER tenants_certs_applied_guard
    BEFORE UPDATE OF certs_applied_version
    ON identity.tenants
    FOR EACH ROW EXECUTE FUNCTION identity.tenants_certs_applied_guard();

GRANT UPDATE (certs_applied_version) ON identity.tenants TO identity_app;
