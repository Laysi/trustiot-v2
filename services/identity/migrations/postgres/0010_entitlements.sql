-- A tenant's plan entitlements (Laysi/trustiot-v2#111, checklist B3; services/identity/README.md,
-- "方案權益"; docs/product/plans.md): every tenant has a plan, the overrides of its contract
-- (Enterprise plans only) and the resulting entitlements, a snapshot the service works out from
-- the plan's values and the overrides when it writes them. Each new snapshot and the
-- `identity.tenant_entitlements_changed` event, which carries it with its version, are written in
-- one transaction of that tenant. Run by `identity_owner` like 0001_identity.sql.
--
-- 1. `plan` is one of the four plans; every tenant, existing ones included, starts on `starter`.
-- 2. `entitlement_overrides` is a JSON object of the entitlements a contract sets apart from the
--    plan's values (the service checks names and values); only an Enterprise plan has any.
-- 3. `entitlements` is the snapshot the last event carried. Tenants created before this file have
--    none yet (NULL, version 0) until the service gives them their first snapshot and event
--    (`entitlements::backfill`); a tenant created from now on gets it in the transaction that
--    creates it. NULL exactly at version 0, and never back to NULL (the version never goes back).
-- 4. `entitlements_version` goes up by exactly one with every change of the plan, the overrides
--    or the snapshot, and stays otherwise. Consumers keep the event with the larger version, so it
--    must never go back or repeat: the trigger below holds that for every writer, the service role
--    included.
-- 5. `entitlements_changed_at` is the database's: the trigger sets it when a new version is
--    written and keeps it otherwise, so no writer can put another time there.
-- 6. The service role gets UPDATE on the four columns it writes; its INSERT stays the two columns
--    creating a tenant writes (0008), so a new tenant always starts on `starter` at version 0, which
--    the creating transaction turns into version 1 with its event.
--
-- Adding columns with constant defaults rewrites no rows. Older replicas name the columns they
-- read and write, so they work unchanged on this schema during a rollout; a tenant one of them
-- creates stays at version 0 until the service's periodic backfill gives it its first snapshot.
-- Reverting means dropping the trigger, its function, the two constraints and the five columns; no
-- other schema object refers to them. The events already written stay in the outbox and the event
-- log.

-- Fail fast, not queue every request behind the table lock; the Job fails and retries.
SET LOCAL lock_timeout = '5s';

ALTER TABLE identity.tenants
    ADD COLUMN plan text NOT NULL DEFAULT 'starter'
        CHECK (plan IN ('starter', 'business', 'enterprise', 'enterprise_on_prem')),
    ADD COLUMN entitlement_overrides jsonb NOT NULL DEFAULT '{}'::jsonb
        CHECK (jsonb_typeof(entitlement_overrides) = 'object'),
    ADD COLUMN entitlements jsonb CHECK (jsonb_typeof(entitlements) = 'object'),
    ADD COLUMN entitlements_version bigint NOT NULL DEFAULT 0 CHECK (entitlements_version >= 0),
    ADD COLUMN entitlements_changed_at timestamptz,
    -- A snapshot and its time exist exactly from version 1 on.
    ADD CONSTRAINT tenants_entitlements_versioned CHECK (
        (entitlements IS NULL) = (entitlements_version = 0)
        AND (entitlements_changed_at IS NULL) = (entitlements_version = 0)
    ),
    -- Overrides belong to an Enterprise contract (docs/product/plans.md).
    ADD CONSTRAINT tenants_overrides_enterprise CHECK (
        plan IN ('enterprise', 'enterprise_on_prem') OR entitlement_overrides = '{}'::jsonb
    );

-- An invoker's trigger function, like identity.tenants_state_guard (0008_tenant_state.sql):
-- ERRCODE 42501 (insufficient_privilege), like a missing grant.
CREATE FUNCTION identity.tenants_entitlements_guard() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    changed boolean := (NEW.plan, NEW.entitlement_overrides, NEW.entitlements)
        IS DISTINCT FROM (OLD.plan, OLD.entitlement_overrides, OLD.entitlements);
BEGIN
    IF (changed AND NEW.entitlements_version <> OLD.entitlements_version + 1)
        OR (NOT changed AND NEW.entitlements_version <> OLD.entitlements_version) THEN
        RAISE EXCEPTION 'a tenant''s entitlements version goes up by one with each change'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    -- The time of the snapshot, by the database clock at the change (after the row lock was
    -- granted), never a writer's. The version, not the time, orders changes.
    IF changed THEN
        NEW.entitlements_changed_at := clock_timestamp();
    ELSE
        NEW.entitlements_changed_at := OLD.entitlements_changed_at;
    END IF;
    RETURN NEW;
END;
$$;

REVOKE ALL ON FUNCTION identity.tenants_entitlements_guard() FROM PUBLIC;

CREATE TRIGGER tenants_entitlements_guard
    BEFORE UPDATE OF plan, entitlement_overrides, entitlements, entitlements_version,
        entitlements_changed_at
    ON identity.tenants
    FOR EACH ROW EXECUTE FUNCTION identity.tenants_entitlements_guard();

-- Changing a tenant's plan, overrides and snapshot. Row-level security (`tenant_isolation`, 0002)
-- keeps the update to the transaction's tenant; `entitlements_changed_at` stays the trigger's.
GRANT UPDATE (plan, entitlement_overrides, entitlements, entitlements_version)
    ON identity.tenants TO identity_app;
