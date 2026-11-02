-- What the answer guard removed from an answer (Laysi/trustiot-v2#646; services/assistant/README.md,
-- 「回答的過濾」). The guard (src/answer_guard.rs, Laysi/trustiot-v2#150) takes code, HTML and SQL
-- out of an answer before the answer joins the history, so the stored message holds only the
-- cleaned text and cannot tell that anything was taken out. A question now runs in the
-- background and its answer reaches the chat only by reading the conversation, which shows a
-- notice under an answer the guard changed (`assistant.answer_content_removed`): the kinds it
-- removed are stored with the answer's message, written by the same INSERT and read back with
-- it. Run by the migration role `assistant_owner`, which owns everything here, like
-- 0001_assistant.sql.
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
-- The service role may name the column when it inserts a message; it has no UPDATE or DELETE on
-- messages (0001, 0002), so what an answer's row says was removed never changes, like the
-- message itself. Nothing else is granted.

-- Fail fast rather than queue the service's reads and writes behind the ALTER's lock; the Job
-- fails and retries. Adding a column without a default changes only the catalog; the CHECK reads
-- every row once (all NULL in the new column) under the same lock.
SET LOCAL lock_timeout = '5s';

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
    );

GRANT INSERT (removed) ON assistant.messages TO assistant_app;
