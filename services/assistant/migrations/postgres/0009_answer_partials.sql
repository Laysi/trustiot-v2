-- The start of an answer being written (Laysi/trustiot-v2#147; docs/architecture/ai-assistant.md,
-- point 7 of 「主 agent」): while a question runs, the replica that runs it keeps here the text of
-- the answer that may already go out (the paragraphs the model finished in the answer tool, after
-- the answer guard), so an answer stream on any replica sends it before the question ends and a
-- reconnect goes on from where it stopped. Run by the migration role `assistant_owner`, which owns
-- everything here, like 0001_assistant.sql.
--
-- One row per running question at most, and only while it runs: the service writes it at most
-- once a second, only while the run is `running` (the write locks the run's row FOR SHARE first,
-- so it cannot land after the run's end), and every end of the run deletes it in the same
-- transaction: the end the replica running the question records (answered, failed, cancelled,
-- or interrupted at shutdown), and the conversation's next question, which marks a run left
-- `running` past its deadline (a replica that stopped mid-question) interrupted. Until then such a
-- row stays, but no stream reads it (past its deadline the run reads as interrupted), and the
-- purge of its conversation removes it too. The stored answer supersedes the row: an answered
-- run's answer starts with every text written here. A run that ends without an answer keeps
-- nothing.
--
-- The text is the start of the answer the history stores, so it is as sensitive as an answer:
-- row-level security keeps it in its tenant, and it goes with its run (ON DELETE CASCADE), which
-- goes with its conversation, so the purge of a deleted conversation removes it. The service
-- role may DELETE here, unlike the other tables: a row is scratch space that lives only while a
-- question runs, and keeping the text of a failed or stopped question would keep what the history
-- does not.
--
-- The CHECK bounds the text at 512 KiB, twice the answer tool's limit (`answer::MAX_ANSWER_BYTES`,
-- 256 KiB), which the service stops writing at.
--
-- Locks: creating a table locks nothing the service uses but the foreign key's target,
-- assistant.runs, in SHARE ROW EXCLUSIVE until the commit: writes of the runs wait (a question's
-- start, end and stop), reads do not. The file changes only the catalog, so that is short.

-- Fail fast rather than queue the service's writes behind the lock; the Job fails and retries.
SET LOCAL lock_timeout = '5s';

CREATE TABLE assistant.answer_partials (
    tenant_id uuid NOT NULL,
    run_id uuid NOT NULL,
    -- What of the answer may go out so far: whole paragraphs, after the answer guard.
    text text NOT NULL CHECK (octet_length(text) <= 524288),
    PRIMARY KEY (tenant_id, run_id),
    FOREIGN KEY (tenant_id, run_id)
        REFERENCES assistant.runs (tenant_id, run_id) ON DELETE CASCADE
);

ALTER TABLE assistant.answer_partials ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON assistant.answer_partials
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

GRANT SELECT, DELETE ON assistant.answer_partials TO assistant_app;
GRANT INSERT (tenant_id, run_id, text) ON assistant.answer_partials TO assistant_app;
GRANT UPDATE (text) ON assistant.answer_partials TO assistant_app;
