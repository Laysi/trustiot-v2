-- Dashboards of the assistant (Laysi/trustiot-v2#150; decision 33; docs/architecture/ai-assistant.md,
-- 「儀表板」): a dashboard is a page and the datasets it shows, kept as versions that are only
-- ever added. Run by the migration role `assistant_owner`, which owns everything here, like
-- 0001_assistant.sql; the service role `assistant_app` owns nothing.
--
-- Two kinds share one table:
-- - a draft belongs to one conversation: the `dashboard` tool writes it, its conversation's owner
--   sees it, and it goes when the conversation is purged (ON DELETE CASCADE below);
-- - a saved dashboard belongs to the tenant: a user publishes a draft into it, every member of
--   the tenant sees it, and it has no conversation, so purging one never touches it. It is only
--   archived and unarchived, never deleted (MS3).
--
-- A draft remembers the saved dashboard it was taken from or published to, and that dashboard's
-- version it is based on: publishing is refused when the saved dashboard has a newer version
-- (services/assistant/src/dashboards/store.rs).
--
-- Who may see a row is the service's decision (store.rs, `conversation_access`): row-level
-- security below only keeps tenants apart, like every tenant table
-- (crates/service-kit/sql/tenant-table.sql).
--
-- The service role reads both tables, inserts the columns a write names, and updates only a
-- draft's saved dashboard and base version and a saved dashboard's archive time; it has no
-- DELETE. The triggers hold every role, the owner included, to what those updates may do, and
-- keep versions unchanged once written.

-- One row per dashboard.
CREATE TABLE assistant.dashboards (
    tenant_id uuid NOT NULL,
    -- UUIDv7, made by the service.
    dashboard_id uuid NOT NULL,
    kind text NOT NULL CHECK (kind IN ('draft', 'saved')),
    -- A draft's conversation; NULL for a saved dashboard.
    conversation_id uuid,
    -- A draft's saved dashboard (the one it was taken from or published to) and the version of
    -- it the draft is based on; NULL until it has one, and always NULL for a saved dashboard.
    saved_dashboard_id uuid,
    base_saved_version integer CHECK (base_saved_version BETWEEN 1 AND 10000),
    -- Always 'saved': with the foreign key below it makes `saved_dashboard_id` name a saved
    -- dashboard, never a draft.
    saved_kind text NOT NULL GENERATED ALWAYS AS ('saved') STORED,
    -- A draft's conversation owner, or the user who first published a saved dashboard.
    created_by uuid NOT NULL,
    -- By the database clock.
    created_at timestamptz NOT NULL DEFAULT now(),
    -- When a saved dashboard was archived, by the database clock (the trigger sets it); NULL
    -- while it is not.
    archived_at timestamptz,
    PRIMARY KEY (tenant_id, dashboard_id),
    UNIQUE (tenant_id, dashboard_id, kind),
    CHECK ((kind = 'draft') = (conversation_id IS NOT NULL)),
    CHECK (kind = 'draft' OR (saved_dashboard_id IS NULL AND base_saved_version IS NULL)),
    CHECK (kind = 'saved' OR archived_at IS NULL),
    CHECK ((saved_dashboard_id IS NULL) = (base_saved_version IS NULL)),
    FOREIGN KEY (tenant_id, conversation_id)
        REFERENCES assistant.conversations (tenant_id, conversation_id) ON DELETE CASCADE,
    -- NO ACTION: saved dashboards are never deleted (MS3), and deleting a draft never touches
    -- the saved dashboard it points at.
    FOREIGN KEY (tenant_id, saved_dashboard_id, saved_kind)
        REFERENCES assistant.dashboards (tenant_id, dashboard_id, kind)
);

-- A conversation's drafts (its list, and the cascade when it is purged).
CREATE INDEX dashboards_conversation
    ON assistant.dashboards (tenant_id, conversation_id)
    WHERE conversation_id IS NOT NULL;

-- A conversation has at most one draft of a saved dashboard: the tool writes into that one
-- instead of taking another.
CREATE UNIQUE INDEX dashboards_one_draft_per_saved
    ON assistant.dashboards (tenant_id, conversation_id, saved_dashboard_id)
    WHERE kind = 'draft' AND saved_dashboard_id IS NOT NULL;

-- The tenant's saved dashboards.
CREATE INDEX dashboards_saved
    ON assistant.dashboards (tenant_id)
    WHERE kind = 'saved';

-- One row per version of a dashboard, numbered from 1 without gaps. A version is never changed:
-- restoring an older one adds a copy that names its source, and publishing adds a copy of the
-- draft's version to the saved dashboard.
CREATE TABLE assistant.dashboard_versions (
    tenant_id uuid NOT NULL,
    dashboard_id uuid NOT NULL,
    version integer NOT NULL CHECK (version BETWEEN 1 AND 10000),
    -- The version before this one; with the foreign key below, versions have no gaps.
    previous_version integer GENERATED ALWAYS AS (NULLIF(version - 1, 0)) STORED,
    -- How it came to be (contract::api::dashboard::public::VersionOrigin).
    origin text NOT NULL CHECK (origin IN ('agent', 'publish', 'restore')),
    -- For a restore, the version restored.
    restored_from integer,
    -- The sandbox runtime the page was checked with and runs with
    -- (contract::api::dashboard::RuntimeVersion).
    runtime smallint NOT NULL CHECK (runtime >= 1),
    title text NOT NULL CHECK (char_length(title) BETWEEN 1 AND 120),
    description text CHECK (char_length(description) BETWEEN 1 AND 500),
    -- The datasets, computations and constants (contract::api::dashboard::spec::Spec) as the
    -- compact JSON text the service wrote: text rather than jsonb, so the size below counts the
    -- same bytes as the service's limit and the spec reads back exactly as written.
    spec text NOT NULL CHECK (jsonb_typeof(spec::jsonb) = 'object'),
    -- The page: an HTML fragment with its scripts, at most 40 KiB.
    page text NOT NULL CHECK (octet_length(page) BETWEEN 1 AND 40960),
    -- Who asked for it: the conversation's owner for the tool's versions, the user who
    -- published or restored otherwise.
    created_by uuid NOT NULL,
    -- The question whose tool call wrote it, for the tool's versions only.
    run_id uuid,
    -- By the database clock.
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, dashboard_id, version),
    CHECK ((origin = 'restore') = (restored_from IS NOT NULL)),
    CHECK (restored_from IS NULL OR restored_from BETWEEN 1 AND version - 1),
    CHECK ((origin = 'agent') = (run_id IS NOT NULL)),
    -- The spec and the page together: at most 56 KiB, so a whole version fits one tool result.
    CHECK (octet_length(spec) + octet_length(page) <= 57344),
    FOREIGN KEY (tenant_id, dashboard_id)
        REFERENCES assistant.dashboards (tenant_id, dashboard_id) ON DELETE CASCADE,
    -- NO ACTION, checked at the end of the statement: the cascade that removes a draft's
    -- versions removes all of them in one statement and passes.
    FOREIGN KEY (tenant_id, dashboard_id, previous_version)
        REFERENCES assistant.dashboard_versions (tenant_id, dashboard_id, version),
    FOREIGN KEY (tenant_id, dashboard_id, restored_from)
        REFERENCES assistant.dashboard_versions (tenant_id, dashboard_id, version)
);

-- What an update of a dashboard may change, for every role: a draft's saved dashboard is set
-- once, its base version only rises, and a saved dashboard's archive time is the database's
-- clock when it was archived (archiving it again keeps that time) or NULL. Nothing else changes.
-- An invoker's function with a pinned search_path, like certs.gateway_registrations_guard;
-- ERRCODE 42501 (insufficient_privilege). The generated column is left out of the comparison:
-- a BEFORE trigger sees it before it is computed.
CREATE FUNCTION assistant.dashboards_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
    AS $$
BEGIN
    IF to_jsonb(NEW) - ARRAY['saved_dashboard_id', 'base_saved_version', 'archived_at', 'saved_kind']
        IS DISTINCT FROM
        to_jsonb(OLD) - ARRAY['saved_dashboard_id', 'base_saved_version', 'archived_at', 'saved_kind'] THEN
        RAISE EXCEPTION 'dashboard %: only the saved dashboard, the base version and the archive time change',
            OLD.dashboard_id
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    IF OLD.saved_dashboard_id IS NOT NULL
        AND NEW.saved_dashboard_id IS DISTINCT FROM OLD.saved_dashboard_id THEN
        RAISE EXCEPTION 'dashboard %: a draft''s saved dashboard is set once', OLD.dashboard_id
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    IF OLD.base_saved_version IS NOT NULL
        AND (NEW.base_saved_version IS NULL OR NEW.base_saved_version < OLD.base_saved_version) THEN
        RAISE EXCEPTION 'dashboard %: a draft''s base version only rises', OLD.dashboard_id
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    IF NEW.archived_at IS NOT NULL THEN
        NEW.archived_at := coalesce(OLD.archived_at, now());
    END IF;
    RETURN NEW;
END;
$$;

REVOKE ALL ON FUNCTION assistant.dashboards_guard() FROM PUBLIC;

CREATE TRIGGER dashboards_guard
    BEFORE UPDATE ON assistant.dashboards
    FOR EACH ROW EXECUTE FUNCTION assistant.dashboards_guard();

-- A version is never changed, by any role. Removing a draft with its conversation still
-- removes its versions (a DELETE, not an UPDATE).
CREATE FUNCTION assistant.dashboard_versions_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
    AS $$
BEGIN
    RAISE EXCEPTION 'dashboard %: version % is never changed', OLD.dashboard_id, OLD.version
        USING ERRCODE = 'insufficient_privilege';
END;
$$;

REVOKE ALL ON FUNCTION assistant.dashboard_versions_guard() FROM PUBLIC;

CREATE TRIGGER dashboard_versions_guard
    BEFORE UPDATE ON assistant.dashboard_versions
    FOR EACH ROW EXECUTE FUNCTION assistant.dashboard_versions_guard();

ALTER TABLE assistant.dashboards ENABLE ROW LEVEL SECURITY;
ALTER TABLE assistant.dashboard_versions ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON assistant.dashboards
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

CREATE POLICY tenant_isolation ON assistant.dashboard_versions
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- Read; insert the columns a new row names (the clocks and the archive time are the database's);
-- update a draft's link to its saved dashboard and a saved dashboard's archive time. Locking a
-- dashboard's row (FOR UPDATE) for a write needs that UPDATE grant. No DELETE or TRUNCATE.
GRANT SELECT ON assistant.dashboards TO assistant_app;
GRANT INSERT (tenant_id, dashboard_id, kind, conversation_id, saved_dashboard_id,
    base_saved_version, created_by) ON assistant.dashboards TO assistant_app;
GRANT UPDATE (saved_dashboard_id, base_saved_version, archived_at)
    ON assistant.dashboards TO assistant_app;
GRANT SELECT ON assistant.dashboard_versions TO assistant_app;
GRANT INSERT (tenant_id, dashboard_id, version, origin, restored_from, runtime, title,
    description, spec, page, created_by, run_id) ON assistant.dashboard_versions TO assistant_app;
