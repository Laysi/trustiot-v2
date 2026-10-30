-- The tenant's lifecycle state (Laysi/trustiot-v2#776, checklist B1; services/identity/README.md,
-- "租戶狀態"): a platform administrator suspends a tenant with a reason or resumes it through
-- `PUT /api/identity/v1/platform/tenants/{tenant_id}/state`, which changes these columns in a
-- transaction of that tenant and writes `identity.tenant_state_changed` (the new state, reason
-- and version) to the outbox in the same transaction. Run by `identity_owner` like
-- 0001_identity.sql.
--
-- 1. Every tenant has a state: `active`, `suspended` (always with a reason) or `deleted` (the
--    end of a terminated contract, Laysi/trustiot-v2#109; no API sets it yet). Existing tenants
--    are active, in the state they have been in since they were created.
-- 2. `state_version` starts at 1, which `identity.tenant_created` stands for, and goes up by
--    exactly one with every change of the state or the reason. Consumers keep the event with the
--    larger version, so the version must never go back or repeat: the trigger below holds that
--    for every writer, the service role included.
-- 3. A terminated contract (`suspended` with `contract_terminated`) and `deleted` are final: the
--    service answers 409 to a resumption, and the trigger refuses any change but
--    contract_terminated -> deleted. Only a terminated contract is deleted, so a deleted tenant
--    always keeps the reason `contract_terminated`.
-- 4. `state_changed_at` is the database's: the trigger sets it when the state or the reason
--    changes and keeps it otherwise, so no writer can put another time there.
-- 5. The service role gets UPDATE on the state, the reason and the version only, and its INSERT
--    narrows to the two columns creating a tenant writes, so a new tenant always starts active at
--    version 1.

-- Fail fast, not queue every request behind the table lock; the Job fails and retries.
SET LOCAL lock_timeout = '5s';

ALTER TABLE identity.tenants
    ADD COLUMN state text NOT NULL DEFAULT 'active'
        CHECK (state IN ('active', 'suspended', 'deleted')),
    ADD COLUMN suspension_reason text
        CHECK (suspension_reason IN ('security_or_abuse', 'non_payment', 'contract_terminated')),
    ADD COLUMN state_version bigint NOT NULL DEFAULT 1 CHECK (state_version >= 1),
    ADD COLUMN state_changed_at timestamptz,
    -- An active tenant has no reason; a suspended one always has one; a deleted one is the end of
    -- a terminated contract and keeps that reason. `IS NOT DISTINCT FROM`, not `=`: a CHECK
    -- passes when it is NULL, which `NULL = 'contract_terminated'` is.
    ADD CONSTRAINT tenants_state_reason CHECK (
        (state <> 'active' OR suspension_reason IS NULL)
        AND (state <> 'suspended' OR suspension_reason IS NOT NULL)
        AND (state <> 'deleted' OR suspension_reason IS NOT DISTINCT FROM 'contract_terminated')
    );

-- Existing tenants have been active since they were created. The migration role owns the table,
-- so row-level security does not apply to this statement (the table does not FORCE it).
UPDATE identity.tenants SET state_changed_at = created_at;

ALTER TABLE identity.tenants
    ALTER COLUMN state_changed_at SET DEFAULT now(),
    ALTER COLUMN state_changed_at SET NOT NULL;

-- An invoker's trigger function, like identity.sessions_guard (0004_login.sql): it reads only the
-- row being written and runs with the rights of whoever writes it. ERRCODE 42501
-- (insufficient_privilege), like a missing grant. PL/pgSQL ends an IF condition at the first
-- THEN outside parentheses, so the conditions use no CASE.
CREATE FUNCTION identity.tenants_state_guard() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    changed boolean := (NEW.state, NEW.suspension_reason)
        IS DISTINCT FROM (OLD.state, OLD.suspension_reason);
BEGIN
    IF (changed AND NEW.state_version <> OLD.state_version + 1)
        OR (NOT changed AND NEW.state_version <> OLD.state_version) THEN
        RAISE EXCEPTION 'a tenant''s state version goes up by one with each change of its state'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    IF changed AND OLD.state = 'deleted' THEN
        RAISE EXCEPTION 'a deleted tenant stays deleted'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    IF changed AND OLD.suspension_reason = 'contract_terminated' AND NEW.state <> 'deleted' THEN
        RAISE EXCEPTION 'a tenant whose contract was terminated is never resumed'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    IF changed AND NEW.state = 'deleted'
        AND (OLD.state <> 'suspended'
            OR OLD.suspension_reason IS DISTINCT FROM 'contract_terminated') THEN
        RAISE EXCEPTION 'only a tenant whose contract was terminated is deleted'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    -- The time the tenant got its state, by the database clock at the change (after the row lock
    -- was granted), never a writer's. The version, not the time, orders changes, so nothing
    -- compares two changes' times (two transactions' clocks may disagree).
    IF changed THEN
        NEW.state_changed_at := clock_timestamp();
    ELSE
        NEW.state_changed_at := OLD.state_changed_at;
    END IF;
    RETURN NEW;
END;
$$;

REVOKE ALL ON FUNCTION identity.tenants_state_guard() FROM PUBLIC;

CREATE TRIGGER tenants_state_guard
    BEFORE UPDATE OF state, suspension_reason, state_version, state_changed_at
    ON identity.tenants
    FOR EACH ROW EXECUTE FUNCTION identity.tenants_state_guard();

-- Creating a tenant writes its ID and name; everything else takes its default (0002 granted
-- INSERT on the whole table, which would let the service role create a tenant already suspended
-- or at any version).
REVOKE INSERT ON identity.tenants FROM identity_app;
GRANT INSERT (tenant_id, name) ON identity.tenants TO identity_app;

-- Suspending and resuming. `SELECT ... FOR UPDATE`, which makes concurrent requests for one
-- tenant wait for each other, needs the UPDATE grant on a column, which these are. Row-level
-- security (`tenant_isolation`, 0002) keeps the update to the transaction's tenant. The name,
-- the ID, the creation time and the time of the state stay unchangeable for the service role.
GRANT UPDATE (state, suspension_reason, state_version) ON identity.tenants TO identity_app;
