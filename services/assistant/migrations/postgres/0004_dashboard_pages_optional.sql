-- A dashboard version's custom page becomes optional (Laysi/trustiot-v2#934; decision 33 as
-- revised on 2026-11-01, docs/architecture/ai-assistant.md, 「儀表板」): a version of components
-- only has its spec and no page, and since only a page is checked, no runtime either. Run by
-- `assistant_owner`, like 0003_dashboards.sql.
--
-- Two of 0003's checks on assistant.dashboard_versions change, nothing else:
-- - a row holding content had all of runtime, title, spec and page; now it has its title and
--   spec, and its runtime and page together or neither (a restore's row still holds none of it,
--   0003's other check, unchanged);
-- - the spec and the page together are at most 56 KiB; a missing page counts as 0 bytes. 0003
--   added the two lengths, which is NULL without a page, and a CHECK that is NULL passes: kept
--   as it was, a version without a page would have no size limit at all.
--
-- Every row already stored has its runtime and its page (0003 required both), so it passes the
-- new checks and reads back the same: no row is changed or rewritten. Adding a CHECK scans the
-- table once to validate the rows under the ALTER's ACCESS EXCLUSIVE lock; the Job runs this file
-- in one transaction, so `NOT VALID` and a later `VALIDATE` would hold the same lock until the
-- commit anyway, and the table holds at most a tenant's few thousand versions.
--
-- Not reversible by a later file without removing the versions that have no page, which an
-- older service cannot read: do not roll the assistant back past Laysi/trustiot-v2#934 once it
-- has written one (services/assistant/README.md, 「儲存」).

-- Fail fast, not queue the service's reads behind the ALTER's lock; the Job fails and retries.
SET LOCAL lock_timeout = '5s';

-- 0003 left both checks unnamed, so Postgres named them after the table in the order they
-- appear (`dashboard_versions_check` for the first table check that reads more than one column,
-- then `_check1`, ...): the content check is the third, the size check the eighth. Stop before
-- changing anything when either name holds something else, rather than drop the wrong check.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT FROM pg_catalog.pg_constraint
        WHERE conrelid = 'assistant.dashboard_versions'::regclass
            AND conname = 'dashboard_versions_check2'
            AND contype = 'c'
            AND pg_catalog.pg_get_constraintdef(oid)
                LIKE '%num_nulls(runtime, title, spec, page) = 0%'
    ) OR NOT EXISTS (
        SELECT FROM pg_catalog.pg_constraint
        WHERE conrelid = 'assistant.dashboard_versions'::regclass
            AND conname = 'dashboard_versions_check7'
            AND contype = 'c'
            AND pg_catalog.pg_get_constraintdef(oid)
                LIKE '%octet_length(spec) + octet_length(page)) <= 57344%'
    ) THEN
        RAISE EXCEPTION 'assistant.dashboard_versions does not have the checks 0003_dashboards.sql made';
    END IF;
END;
$$;

ALTER TABLE assistant.dashboard_versions
    DROP CONSTRAINT dashboard_versions_check2,
    DROP CONSTRAINT dashboard_versions_check7,
    -- A row holds its title and spec, and its runtime and page together or neither (a version of
    -- components only), or it is a restore's row, which points at the row holding the content and
    -- holds none of it (0003's check, unchanged).
    ADD CONSTRAINT dashboard_versions_content CHECK (
        content_version IS NOT NULL
        OR (num_nulls(title, spec) = 0 AND (runtime IS NULL) = (page IS NULL))
    ),
    -- The spec and the page together: at most 56 KiB, a missing page counted as 0 bytes, so a
    -- whole version fits one tool result. A restore's row has no spec and passes.
    ADD CONSTRAINT dashboard_versions_content_size CHECK (
        octet_length(spec) + coalesce(octet_length(page), 0) <= 57344
    );
