-- Conversation threads (Laysi/trustiot-v2#644, split from Laysi/trustiot-v2#146; decisions 29
-- and 31): titles and archiving, soft deletion with a 30-day restore window and the purge after
-- it, discarding the last round of a history, and the record of every question asked
-- (services/assistant/README.md, 「對話的儲存（Postgres）」). Run by `assistant_owner` like
-- 0001_assistant.sql.
--
-- The history stays append-only for the service role: it still has no UPDATE, DELETE or
-- TRUNCATE on `assistant.messages`. A conversation leaves the database only as a whole, when the
-- purge deletes its row 30 days after the owner deleted it; its messages, discarded rounds and
-- runs go with it through `ON DELETE CASCADE`. Referential actions run as the referencing
-- table's owner (`assistant_owner`), which row-level security exempts, so the cascade removes
-- every row of the conversation whatever role deletes it; nothing grants the deleting role
-- anything on the child tables.
--
-- The purge runs as a new platform role, `assistant_platform` (tools/migrate creates it,
-- `PlatformDb` in crates/service-kit opens its transactions, each logged as `platform database
-- access`). Chosen over deleting in a transaction of each tenant as `assistant_app`: that role
-- cannot see which tenants have expired conversations, so the job would need a tenant list from
-- elsewhere, and the service role would hold DELETE on conversations. The platform role sees and
-- deletes only conversations deleted more than 30 days ago (its two policies below), reads only
-- their keys and deletion time, and has nothing else: no INSERT or UPDATE, no other table, no
-- TRUNCATE, no SECURITY DEFINER function. A bug or a compromised service can therefore never
-- remove a conversation that is live or still restorable, and the service role cannot remove
-- anything at all.
--
-- Times are the database's: `created_at`, `started_at` and the like take their defaults (the
-- service role has no INSERT on them), and the triggers below refuse an `archived_at`,
-- `deleted_at`, `last_activity_at`, `ended_at` or `cancel_requested_at` that is not the
-- transaction's now(), and a `deleted_at` that moves while the conversation stays deleted.
-- Otherwise the service role could, for example, back-date `deleted_at` and make a conversation
-- purgeable at once, or keep moving it forward so that it is never purged.
--
-- 30 days is a fixed product rule (decided 2026-10-24, Laysi/trustiot-v2#146), written here as
-- `interval '30 days'` and in services/assistant/src/store.rs as `DELETED_RETENTION_DAYS`; the
-- store's unit test compares the two.

-- Fail fast, not queue the service's reads behind an ALTER's lock; the Job fails and retries.
SET LOCAL lock_timeout = '5s';

-- Titles, archiving, soft deletion and the activity the list sorts by.
ALTER TABLE assistant.conversations
    -- What the list shows. NULL until the chat API or the user names the conversation.
    ADD COLUMN title text CHECK (char_length(title) BETWEEN 1 AND 200),
    -- Who named it: the first question (`provisional`), the user, or the model. A user's title
    -- is never replaced by a generated one (services/assistant/src/store.rs, `set_title`).
    ADD COLUMN title_source text CHECK (title_source IN ('provisional', 'user', 'model')),
    -- Archived conversations leave the default list but stay usable.
    ADD COLUMN archived_at timestamptz,
    -- Soft deletion: set, the conversation counts as gone everywhere except its owner's list of
    -- deleted conversations, from which the owner can restore it within 30 days.
    ADD COLUMN deleted_at timestamptz,
    -- The last time messages were added; the list's order.
    ADD COLUMN last_activity_at timestamptz,
    ADD CONSTRAINT conversations_title_has_source CHECK ((title IS NULL) = (title_source IS NULL));

-- Existing conversations were last active when their last message was stored.
UPDATE assistant.conversations c
SET last_activity_at = greatest(
    c.created_at,
    (SELECT max(m.created_at) FROM assistant.messages m
     WHERE m.tenant_id = c.tenant_id AND m.conversation_id = c.conversation_id)
);

ALTER TABLE assistant.conversations
    ALTER COLUMN last_activity_at SET DEFAULT now(),
    ALTER COLUMN last_activity_at SET NOT NULL;

-- A user's conversations, most recently active first (the list), and their deleted ones, most
-- recently deleted first (the deleted list; the purge also finds expired rows through it, a skip
-- scan over the leading columns). `tenant_id` leads both, like every key here.
CREATE INDEX conversations_by_activity ON assistant.conversations
    (tenant_id, user_id, last_activity_at DESC, conversation_id DESC)
    WHERE deleted_at IS NULL;
CREATE INDEX conversations_deleted ON assistant.conversations
    (tenant_id, user_id, deleted_at DESC, conversation_id DESC)
    WHERE deleted_at IS NOT NULL;

-- Messages leave with their conversation, and only then.
ALTER TABLE assistant.messages
    DROP CONSTRAINT messages_tenant_id_conversation_id_fkey,
    ADD CONSTRAINT messages_conversation_fkey FOREIGN KEY (tenant_id, conversation_id)
        REFERENCES assistant.conversations (tenant_id, conversation_id) ON DELETE CASCADE;

-- At most 32 MiB per message. A message is part of every later request of its conversation, and
-- the AI gateway refuses a request over 32 MiB (`max_request_bytes`, Anthropic Messages' own
-- limit), so a larger message could never be sent again. The largest message the service writes
-- is a step's tool results: one tool result is at most `tools.max_text_bytes` of text plus
-- `tools.max_images` images of `tools.max_image_bytes` (services/assistant/src/config.rs), with
-- the defaults 64 KiB + 4 x 5 MiB = 20.0625 MiB; the remaining 11.9 MiB cover the JSON around it
-- and the other results of the step. A step whose results do not fit fails at the gateway, and
-- a failed round is not stored, so the limit never refuses a round that could be continued.
-- services/assistant/src/store.rs checks the same limit (`MAX_MESSAGE_BYTES`) before writing.
ALTER TABLE assistant.messages
    ADD CONSTRAINT messages_body_size CHECK (octet_length(body) <= 33554432);

-- Rounds taken back out of the history: regenerating the last answer discards the last round,
-- from its question to the end of the history, and the next round is written after it. The
-- messages stay (the history is append-only) and the store leaves them out when it loads the
-- conversation, so the model gets the history as it was before the discarded round, byte for
-- byte. They leave the database with the conversation. Only the last round of the history can be
-- discarded; the store checks that under the conversation's row lock.
CREATE TABLE assistant.discarded_rounds (
    tenant_id uuid NOT NULL,
    conversation_id uuid NOT NULL,
    -- The round's first message (its question) and its last, inclusive.
    from_seq integer NOT NULL CHECK (from_seq >= 0),
    to_seq integer NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, conversation_id, from_seq),
    CHECK (to_seq >= from_seq),
    FOREIGN KEY (tenant_id, conversation_id)
        REFERENCES assistant.conversations (tenant_id, conversation_id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, conversation_id, from_seq)
        REFERENCES assistant.messages (tenant_id, conversation_id, seq) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, conversation_id, to_seq)
        REFERENCES assistant.messages (tenant_id, conversation_id, seq) ON DELETE CASCADE
);

-- One row per question asked: the run (its ID is the run delegation's, Laysi/trustiot-v2#551),
-- how it ended and when. A conversation has at most one run that is `running`: the unique index
-- below, on top of the store's row lock. A run whose deadline passed while it was still
-- `running` (the service restarted mid-question) becomes `interrupted` when the next question of
-- the conversation starts, so it never blocks the conversation.
CREATE TABLE assistant.runs (
    tenant_id uuid NOT NULL,
    run_id uuid NOT NULL,
    conversation_id uuid NOT NULL,
    -- Who asked: the conversation's owner.
    user_id uuid NOT NULL,
    status text NOT NULL DEFAULT 'running'
        CHECK (status IN ('running', 'answered', 'failed', 'cancelled', 'interrupted')),
    -- Why a failed run failed: the agent's error kind, such as `run_expired`.
    result_code text CHECK (result_code ~ '^[a-z][a-z0-9_]{0,63}$'),
    started_at timestamptz NOT NULL DEFAULT now(),
    -- The run delegation's deadline; a run is never longer than an hour (the delegation's
    -- lifetime is at most 15 minutes).
    deadline_at timestamptz NOT NULL,
    ended_at timestamptz,
    -- When the user asked to stop it; the run ends as `cancelled` when it stops.
    cancel_requested_at timestamptz,
    PRIMARY KEY (tenant_id, run_id),
    FOREIGN KEY (tenant_id, conversation_id)
        REFERENCES assistant.conversations (tenant_id, conversation_id) ON DELETE CASCADE,
    CHECK (deadline_at > started_at AND deadline_at <= started_at + interval '1 hour'),
    CHECK ((status = 'running') = (ended_at IS NULL)),
    CHECK (result_code IS NULL OR status = 'failed')
);

CREATE UNIQUE INDEX runs_one_running_per_conversation ON assistant.runs
    (tenant_id, conversation_id) WHERE status = 'running';

ALTER TABLE assistant.discarded_rounds ENABLE ROW LEVEL SECURITY;
ALTER TABLE assistant.runs ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON assistant.discarded_rounds
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

CREATE POLICY tenant_isolation ON assistant.runs
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- The service role may not touch a conversation whose 30 days are over: it cannot restore it,
-- or change anything else of it, while it waits for the purge. Restrictive, so it only narrows
-- the tenant policy (service-kit's session check accepts restrictive policies).
CREATE POLICY expired_stay_deleted ON assistant.conversations
    AS RESTRICTIVE
    FOR UPDATE
    TO assistant_app
    USING (deleted_at IS NULL OR deleted_at > now() - interval '30 days');

-- The purge (crates/service-kit/sql/tenant-table.sql, steps 4 and 5): the platform role sees,
-- across tenants, only conversations deleted more than 30 days ago, and deletes only those.
-- Built-in functions and the table's own columns only, as service-kit's session check requires.
CREATE POLICY platform_access ON assistant.conversations
    FOR SELECT
    TO assistant_platform
    USING (deleted_at <= now() - interval '30 days');

CREATE POLICY platform_purge ON assistant.conversations
    FOR DELETE
    TO assistant_platform
    USING (deleted_at <= now() - interval '30 days');

-- Times only from the database's clock. Invoker's trigger functions: they read only the row
-- being written and run with the rights of whoever writes it. ERRCODE 42501
-- (insufficient_privilege), like a missing grant.
CREATE FUNCTION assistant.conversations_clock() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.archived_at IS DISTINCT FROM OLD.archived_at AND NEW.archived_at <> now() THEN
        RAISE EXCEPTION 'archived_at is set by the database clock'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    IF NEW.deleted_at IS DISTINCT FROM OLD.deleted_at AND NEW.deleted_at <> now() THEN
        RAISE EXCEPTION 'deleted_at is set by the database clock'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    -- A deletion keeps its time until it is restored: moving it to a later now() would keep a
    -- deleted conversation from ever being purged.
    IF OLD.deleted_at IS NOT NULL AND NEW.deleted_at IS NOT NULL
        AND NEW.deleted_at IS DISTINCT FROM OLD.deleted_at THEN
        RAISE EXCEPTION 'deleted_at is set once per deletion'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    IF NEW.last_activity_at IS DISTINCT FROM OLD.last_activity_at
        AND (NEW.last_activity_at <> now() OR NEW.last_activity_at < OLD.last_activity_at) THEN
        RAISE EXCEPTION 'last_activity_at moves forward to the database clock only'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    RETURN NEW;
END;
$$;

REVOKE ALL ON FUNCTION assistant.conversations_clock() FROM PUBLIC;

CREATE TRIGGER conversations_clock
    BEFORE UPDATE OF archived_at, deleted_at, last_activity_at ON assistant.conversations
    FOR EACH ROW EXECUTE FUNCTION assistant.conversations_clock();

-- A run ends once, at the database's now(), and its stop request is set once, likewise.
CREATE FUNCTION assistant.runs_clock() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF OLD.status <> 'running' THEN
        RAISE EXCEPTION 'a run that ended stays as it ended'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    IF NEW.ended_at IS DISTINCT FROM OLD.ended_at AND NEW.ended_at <> now() THEN
        RAISE EXCEPTION 'ended_at is set by the database clock'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    IF NEW.cancel_requested_at IS DISTINCT FROM OLD.cancel_requested_at
        AND (OLD.cancel_requested_at IS NOT NULL OR NEW.cancel_requested_at <> now()) THEN
        RAISE EXCEPTION 'cancel_requested_at is set once, by the database clock'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    RETURN NEW;
END;
$$;

REVOKE ALL ON FUNCTION assistant.runs_clock() FROM PUBLIC;

CREATE TRIGGER runs_clock
    BEFORE UPDATE ON assistant.runs
    FOR EACH ROW EXECUTE FUNCTION assistant.runs_clock();

-- The service role: no table-wide INSERT any more, only the columns it may choose, so the
-- database fills `created_at` and the other times; UPDATE only of the thread's columns and a
-- run's outcome. Still no UPDATE or DELETE on messages and no DELETE anywhere.
REVOKE INSERT ON assistant.conversations, assistant.messages FROM assistant_app;
GRANT INSERT (tenant_id, conversation_id, user_id, prompt_version, system_prompt, tenant_context,
              tools, effort, cache_ttl)
    ON assistant.conversations TO assistant_app;
GRANT UPDATE (title, title_source, archived_at, deleted_at, last_activity_at)
    ON assistant.conversations TO assistant_app;
GRANT INSERT (tenant_id, conversation_id, seq, kind, format, body)
    ON assistant.messages TO assistant_app;
GRANT SELECT ON assistant.discarded_rounds TO assistant_app;
GRANT INSERT (tenant_id, conversation_id, from_seq, to_seq)
    ON assistant.discarded_rounds TO assistant_app;
GRANT SELECT ON assistant.runs TO assistant_app;
GRANT INSERT (tenant_id, run_id, conversation_id, user_id, deadline_at)
    ON assistant.runs TO assistant_app;
GRANT UPDATE (status, result_code, ended_at, cancel_requested_at)
    ON assistant.runs TO assistant_app;

-- The platform role: the schema, the keys and deletion time of conversations (what the purge's
-- statement reads), and DELETE. Nothing on the other tables.
GRANT USAGE ON SCHEMA assistant TO assistant_platform;
GRANT SELECT (tenant_id, conversation_id, deleted_at) ON assistant.conversations
    TO assistant_platform;
GRANT DELETE ON assistant.conversations TO assistant_platform;
