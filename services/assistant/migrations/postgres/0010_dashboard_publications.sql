-- Where each published version came from (Laysi/trustiot-v2#1096; docs/architecture/ai-assistant.md,
-- 「儀表板」): publishing a draft's version adds a copy of it to a saved dashboard (0003), and from
-- this file on it also records here which draft and which of its versions the copy came from, so
-- the conversation page shows a draft version as saved after a reload, not only during the visit
-- that saved it. Run by the migration role `assistant_owner`, which owns everything here, like
-- 0001_assistant.sql.
--
-- One row per publish, written by the service in the publish's transaction
-- (services/assistant/src/dashboards/store.rs, `publish`), after the version it names; never
-- changed. A draft's version is published at most once (the primary key): publishing needs the
-- saved dashboard's latest version to be the base of the version published, and the publish
-- moves the saved dashboard past that base for good. A saved dashboard's version comes from at
-- most one publish (the unique key).
--
-- Chosen over two columns on assistant.dashboard_versions: a draft goes with its conversation
-- when the conversation is purged (0003, ON DELETE CASCADE), but the saved dashboard and its
-- versions stay. Columns on the saved version would keep the purged draft's ID, a key to nothing,
-- and could not be set to NULL then: the versions' trigger refuses every UPDATE. A row here goes
-- with the draft instead (both foreign keys to the draft cascade), and the saved dashboard's rows
-- keep exactly what 0003 wrote.
--
-- Versions published before this file have no row: their source stays unknown, and no backfill
-- guesses it. A publish copies content without naming its source, and the same content can be in
-- several versions of a draft (a restore points at an earlier version's content) or in drafts of
-- other conversations (each user's draft of the same saved dashboard), so a match on content
-- would name the wrong version. Such a version shows as not saved, as before; saving it is
-- refused with `dashboard.saved_changed` as before. A previous release's pod during a rolling
-- update, or after a helm rollback, publishes without writing a row here, which is the same
-- unknown source (docs/architecture/deployment.md, 「相容規則」).
--
-- Who may see a row is the service's decision (store.rs: a draft's version's source is read only
-- with that draft, by whoever can see the draft, and only when they can also open the saved
-- dashboard it names; a saved dashboard's versions never show it). Row-level security below only
-- keeps tenants apart, like every tenant table (crates/service-kit/sql/tenant-table.sql).
--
-- Locks: creating a table locks nothing the service uses but the foreign keys' targets,
-- assistant.dashboards and assistant.dashboard_versions, in SHARE ROW EXCLUSIVE until the commit:
-- writes of dashboards wait, reads do not. The file changes only the catalog, so that is short.
-- A write that inserted a version and then updates a dashboard row (a publish or a restore sets
-- the draft's base after the insert) can form a deadlock with these two table locks; Postgres
-- then cancels one side after deadlock_timeout, normally this file, which started waiting first,
-- and the Job retries.

-- Fail fast rather than queue the service's writes behind the locks; the Job fails and retries.
SET LOCAL lock_timeout = '5s';

CREATE TABLE assistant.dashboard_publications (
    tenant_id uuid NOT NULL,
    -- The draft and its version published.
    draft_id uuid NOT NULL,
    draft_version integer NOT NULL CHECK (draft_version BETWEEN 1 AND 10000),
    -- The saved dashboard and its version the publish wrote.
    saved_dashboard_id uuid NOT NULL,
    saved_version integer NOT NULL CHECK (saved_version BETWEEN 1 AND 10000),
    -- Always 'draft' and 'saved': with the foreign keys below they make `draft_id` name a draft
    -- and `saved_dashboard_id` a saved dashboard, as 0003's `saved_kind` does.
    draft_kind text NOT NULL GENERATED ALWAYS AS ('draft') STORED,
    saved_kind text NOT NULL GENERATED ALWAYS AS ('saved') STORED,
    PRIMARY KEY (tenant_id, draft_id, draft_version),
    UNIQUE (tenant_id, saved_dashboard_id, saved_version),
    -- The draft and its version: the row goes with them when the draft's conversation is purged.
    FOREIGN KEY (tenant_id, draft_id, draft_kind)
        REFERENCES assistant.dashboards (tenant_id, dashboard_id, kind) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, draft_id, draft_version)
        REFERENCES assistant.dashboard_versions (tenant_id, dashboard_id, version)
        ON DELETE CASCADE,
    -- NO ACTION: saved dashboards and their versions are never deleted (MS3, 0003).
    FOREIGN KEY (tenant_id, saved_dashboard_id, saved_kind)
        REFERENCES assistant.dashboards (tenant_id, dashboard_id, kind),
    FOREIGN KEY (tenant_id, saved_dashboard_id, saved_version)
        REFERENCES assistant.dashboard_versions (tenant_id, dashboard_id, version)
);

-- A row is never changed, by any role (the service role has no UPDATE anyway), like a version
-- (0003's dashboard_versions_guard). Removing a draft with its conversation still removes its
-- rows (a DELETE, not an UPDATE).
CREATE FUNCTION assistant.dashboard_publications_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
    AS $$
BEGIN
    RAISE EXCEPTION 'dashboard %: the source of version % is never changed',
        OLD.saved_dashboard_id, OLD.saved_version
        USING ERRCODE = 'insufficient_privilege';
END;
$$;

REVOKE ALL ON FUNCTION assistant.dashboard_publications_guard() FROM PUBLIC;

CREATE TRIGGER dashboard_publications_guard
    BEFORE UPDATE ON assistant.dashboard_publications
    FOR EACH ROW EXECUTE FUNCTION assistant.dashboard_publications_guard();

ALTER TABLE assistant.dashboard_publications ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON assistant.dashboard_publications
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- Read, and insert a publish's row. No UPDATE, DELETE or TRUNCATE.
GRANT SELECT ON assistant.dashboard_publications TO assistant_app;
GRANT INSERT (tenant_id, draft_id, draft_version, saved_dashboard_id, saved_version)
    ON assistant.dashboard_publications TO assistant_app;
