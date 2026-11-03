-- Sharing a conversation with its whole tenant (Laysi/trustiot-v2#650, split from
-- Laysi/trustiot-v2#146; the user's decision of 2026-10-24): the owner shares a conversation,
-- and the other members of the tenant can read its questions and answers and copy them into a
-- conversation of their own (services/assistant/README.md, 「分享與複製」). Run by
-- `assistant_owner` like 0001_assistant.sql and 0002_conversation_threads.sql.
--
-- One column, `shared_at`: NULL while the conversation is its owner's alone, otherwise when the
-- owner shared it. Who may read a shared conversation is the store's rule
-- (services/assistant/src/store/sharing.rs: shared, not deleted, and not the caller's own);
-- row-level security stays as it is, `tenant_isolation` of 0001 keeps every role in the
-- transaction's tenant, so a shared conversation never reaches another tenant.
--
-- The service role gets column-level UPDATE of `shared_at` and nothing else new. Its time is the
-- database's, like the other times of 0002: the trigger below refuses any `shared_at` but NULL
-- (unsharing) or the transaction's now() (sharing), and refuses moving the time of a
-- conversation that stays shared, so the service cannot back-date a share or make one look new.
--
-- Deleting a conversation unshares it: the trigger `conversations_unshare_deleted` below sets
-- `shared_at` to NULL in every UPDATE that sets `deleted_at`, and the CHECK holds that a deleted
-- conversation is never shared. The trigger keeps the previous release's delete working
-- (docs/architecture/deployment.md, 「相容規則」: a helm rollback, or old and new pods side by side
-- in a rolling update): that release's `store::delete` sets `deleted_at` alone, which the CHECK
-- would refuse on a shared conversation. This release's `store::delete` clears `shared_at` in
-- the same UPDATE as well. A restored conversation comes back unshared. The purge's platform role reaches only conversations
-- deleted more than 30 days ago (0002), which are therefore never shared; it needs nothing new.
--
-- Copies are ordinary conversations of the member who copied: nothing here links them to their
-- source, so unsharing, deleting or purging the source never touches them.

-- Fail fast, not queue the service's reads behind an ALTER's lock; the Job fails and retries.
SET LOCAL lock_timeout = '5s';

ALTER TABLE assistant.conversations
    -- When the owner shared it with the tenant; NULL while it is not shared.
    ADD COLUMN shared_at timestamptz,
    ADD CONSTRAINT conversations_deleted_not_shared
        CHECK (deleted_at IS NULL OR shared_at IS NULL);

-- The "shared with me" list (`store::list_shared`): a tenant's shared conversations that are not
-- deleted, most recently active first, the caller's own filtered out. The statement's
-- conditions imply the predicate, and its order is the index's, so Postgres reads the tenant's
-- shared conversations in list order and stops after a page. Checked with EXPLAIN on a test
-- database with 20,000 conversations in each of two tenants, 1 in 50 shared: an Index Scan
-- using conversations_shared with `tenant_id` as the index condition, no sort (the plan is in
-- docs/test-reports/2026-11-02-650-conversation-sharing.md).
CREATE INDEX conversations_shared ON assistant.conversations
    (tenant_id, last_activity_at DESC, conversation_id DESC)
    WHERE shared_at IS NOT NULL AND deleted_at IS NULL;

-- `shared_at` only from the database's clock, set once per sharing. An invoker's trigger
-- function with a pinned search_path like 0002's `conversations_clock`; ERRCODE 42501
-- (insufficient_privilege), like a missing grant.
CREATE FUNCTION assistant.conversations_shared_clock() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
    AS $$
BEGIN
    IF NEW.shared_at IS DISTINCT FROM OLD.shared_at
        AND NEW.shared_at IS NOT NULL
        AND (OLD.shared_at IS NOT NULL OR NEW.shared_at <> now()) THEN
        RAISE EXCEPTION 'shared_at is set once per sharing, by the database clock'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    RETURN NEW;
END;
$$;

REVOKE ALL ON FUNCTION assistant.conversations_shared_clock() FROM PUBLIC;

CREATE TRIGGER conversations_shared_clock
    BEFORE UPDATE OF shared_at ON assistant.conversations
    FOR EACH ROW EXECUTE FUNCTION assistant.conversations_shared_clock();

-- A delete unshares, whoever runs it: a BEFORE trigger changes NEW before the constraints are
-- checked and needs no privilege on the column it sets. `UPDATE OF deleted_at` fires on the
-- columns an UPDATE names, so a restore (`deleted_at` to NULL) passes through unchanged. Postgres
-- fires a row's BEFORE triggers in name order: `conversations_clock` (0002) first, then
-- `conversations_shared_clock`, then this one; this one only ever sets NULL, which the clock
-- accepts, so the order changes no outcome.
CREATE FUNCTION assistant.conversations_unshare_deleted() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
    AS $$
BEGIN
    IF NEW.deleted_at IS NOT NULL THEN
        NEW.shared_at := NULL;
    END IF;
    RETURN NEW;
END;
$$;

REVOKE ALL ON FUNCTION assistant.conversations_unshare_deleted() FROM PUBLIC;

CREATE TRIGGER conversations_unshare_deleted
    BEFORE UPDATE OF deleted_at ON assistant.conversations
    FOR EACH ROW EXECUTE FUNCTION assistant.conversations_unshare_deleted();

GRANT UPDATE (shared_at) ON assistant.conversations TO assistant_app;
