-- Which answer a question's run left (Laysi/trustiot-v2#1043; docs/architecture/ai-assistant.md,
-- 「儀表板」, 「回答帶出的元件」): an answered run records the position of its answer's message,
-- so reading a conversation can list, under each answer, the draft versions the main agent wrote
-- in that turn. Run by the migration role `assistant_owner`, which owns everything here, like
-- 0001_assistant.sql.
--
-- A draft version written by the `dashboard` tool already names the question whose tool call
-- wrote it (`assistant.dashboard_versions.run_id`, 0003); what was missing is the link from a run
-- to the message of its answer. `answer_seq` is that link: set by the service in the same UPDATE
-- that ends the run as `answered` (`store::finish_answered`), in the same transaction that stores
-- the round, and NULL on every other run. A run that failed, was cancelled or was interrupted
-- stored no round and has none, so no answer lists the versions it wrote; a round discarded later
-- (regenerating the last answer) keeps its run's `answer_seq`, but the conversation's reader does
-- not show that answer, so nothing lists them either.
--
-- Chosen over a `run_id` column on `assistant.messages`: the messages table holds every tenant's
-- whole history and every read of a conversation reads it, and the store writes its rows through
-- one generic append; the run is the record of a question and already ends in one UPDATE, under
-- the trigger `runs_clock` (0002) that keeps an ended run unchanged, so `answer_seq` is written
-- once and never moves.
--
-- Rows the previous release stored stay NULL: their answers list no draft versions (an empty
-- list), as the issue asks. A previous release's pod in a rolling update or after a helm rollback
-- (docs/architecture/deployment.md, 「相容規則」) ends runs without naming the column, which
-- leaves it NULL as well.
--
-- The CHECK keeps the column to answered runs, for every role. The foreign key makes it name a
-- stored message of the run's own conversation (MATCH SIMPLE: NULL is not checked). It cascades:
-- a message leaves the database only with its conversation (0001, 0002), whose purge removes the
-- runs too, and a NO ACTION check could run before that purge's cascade reached the runs, so a
-- purge could fail; SET NULL would be an UPDATE of an ended run, which `runs_clock` refuses.
--
-- Both constraints are added `NOT VALID`, like 0006's CHECK: the column is new and has no
-- default, so every row there is holds NULL, which both accept, and nothing needs to be read to
-- know it. Validating would scan assistant.runs under the ALTER's ACCESS EXCLUSIVE lock (every
-- read of a conversation reads its latest run and waits behind it). Every row inserted or updated
-- from now on is checked as with a validated constraint, and the foreign key's cascade works the
-- same.
--
-- Locks: the ALTER holds ACCESS EXCLUSIVE on assistant.runs and SHARE ROW EXCLUSIVE on
-- assistant.messages (for the foreign key: writes of messages wait, reads do not) until the
-- commit; the file changes only the catalog, so that is short. A transaction of the service that
-- holds one of the two tables and waits for the other can meet the Job in a deadlock; Postgres
-- ends one of the two: the Job retries, or the service's transaction fails as on any database
-- error (a request answers 503, a question's round is not stored and its run fails).
--
-- The service role gets column-level UPDATE of `answer_seq` and nothing else new.

-- Fail fast rather than queue the service's reads and writes behind the locks; the Job fails and
-- retries.
SET LOCAL lock_timeout = '5s';

-- Adding a column without a default changes only the catalog.
ALTER TABLE assistant.runs
    ADD COLUMN answer_seq integer,
    ADD CONSTRAINT runs_answer_answered CHECK (answer_seq IS NULL OR status = 'answered')
        NOT VALID,
    ADD CONSTRAINT runs_answer_message FOREIGN KEY (tenant_id, conversation_id, answer_seq)
        REFERENCES assistant.messages (tenant_id, conversation_id, seq) ON DELETE CASCADE
        NOT VALID;

GRANT UPDATE (answer_seq) ON assistant.runs TO assistant_app;
