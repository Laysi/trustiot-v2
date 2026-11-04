-- The question API's storage (Laysi/trustiot-v2#646; services/assistant/README.md, 「儲存」):
-- what the answer guard removed from an answer, and an index for a conversation's latest
-- question. Run by the migration role `assistant_owner`, which owns everything here, like
-- 0001_assistant.sql.
--
-- What the answer guard removed. The guard (src/answer_guard.rs, Laysi/trustiot-v2#150) takes
-- code, HTML and SQL out of an answer before the answer joins the history, so the stored message
-- holds only the cleaned text and cannot tell that anything was taken out. A question now runs
-- in the background and its answer reaches the chat only by reading the conversation, which
-- shows a notice under an answer the guard changed (`assistant.answer_content_removed`): the
-- kinds it removed are stored with the answer's message, written by the same INSERT and read
-- back with it.
--
-- A `text[]` of the kinds' names rather than a `smallint` bitmask: a row says `{html,sql}` to
-- whoever reads it (an operator, a later migration, a test), where a bitmask means something
-- only next to the service's code (`Removed::bit`), and the size makes no difference on the few
-- answers the guard changes. The CHECK enumerates every value the column may hold: the seven
-- non-empty sets of the three known kinds, each kind once and in the order the service writes
-- them (`Removed::ALL`: code, html, sql). So an unknown kind, a kind twice, an empty array or
-- another order is refused (SQLSTATE 23514) for every role, and so is a value on any message
-- but an answer. NULL is "nothing removed", on every kind of message: one way to say it. A new
-- kind needs a migration that widens the CHECK, as it would with a bitmask.
--
-- The CHECK is added `NOT VALID`, so adding it does not scan assistant.messages, which holds
-- every tenant's whole history, under the ALTER's ACCESS EXCLUSIVE lock (every read and write of
-- the service waits behind it). Nothing would be found: the column is new and has no default,
-- so it is NULL on every row there is, which the CHECK accepts. Every row inserted or updated
-- from now on is checked as with a validated CHECK. The Job runs a file in one transaction and
-- the ALTER's lock is held until the commit, so a `VALIDATE CONSTRAINT` here would scan under
-- that same lock and gain nothing; a later file may validate it (SHARE UPDATE EXCLUSIVE, which
-- blocks no read or write) should a tool need `convalidated`.
--
-- The service role may name the column when it inserts a message; it has no UPDATE or DELETE on
-- messages (0001, 0002), so what an answer's row says was removed never changes, like the
-- message itself. Nothing else is granted.
--
-- The latest question of a conversation (`store::latest_run`): every read of a conversation
-- runs it, polling clients every few seconds. Without this index the newest run of a
-- conversation was found by reading the tenant's runs backwards along the primary key
-- `(tenant_id, run_id)` until one of the conversation's, which for a conversation without runs
-- (or an unknown ID) is every run of the tenant. Ordered by `started_at`, the database's clock
-- when the run was recorded, with the run ID as the tie-break: run IDs are UUIDv7 from the
-- replicas' clocks, which may be skewed. The purge's cascade from a conversation to its runs uses
-- the index too. Building it holds a SHARE lock on assistant.runs (question starts and ends wait)
-- for as long as reading the runs takes; the table has one row per question asked.

-- Fail fast rather than queue the service's reads and writes behind the locks; the Job fails and
-- retries.
SET LOCAL lock_timeout = '5s';

-- Adding a column without a default changes only the catalog.
ALTER TABLE assistant.messages
    ADD COLUMN removed text[],
    ADD CONSTRAINT messages_removed_kinds CHECK (
        removed IS NULL
        OR (
            kind = 'answer'
            AND removed IN (
                ARRAY['code'], ARRAY['html'], ARRAY['sql'],
                ARRAY['code', 'html'], ARRAY['code', 'sql'], ARRAY['html', 'sql'],
                ARRAY['code', 'html', 'sql']
            )
        )
    ) NOT VALID;

GRANT INSERT (removed) ON assistant.messages TO assistant_app;

CREATE INDEX runs_by_conversation ON assistant.runs
    (tenant_id, conversation_id, started_at DESC, run_id DESC);
