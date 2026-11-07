-- The text of a question while it runs (Laysi/trustiot-v2#1157; services/assistant/README.md,
-- 「背景執行」): a question joins the history only with its answer, when its run ends answered
-- (`store::finish_answered`), so until then its text was in no table, and a conversation page
-- opened again while the question ran (a reload, another tab or device) could follow the run
-- but not show what it answers, nor give the text back to the composer when the run then ended
-- without an answer. `question` holds that text on the run while it is `running`, and the owner's
-- reads of the conversation return it (`latest_run.question`). Run by the migration role
-- `assistant_owner`, which owns everything here, like 0001_assistant.sql.
--
-- Written when the run starts, in the INSERT that records it (`store::start_run`): the question
-- asked, or, regenerating the last answer, the discarded round's question, which is the one asked
-- again. Cleared at every end of the run, in the UPDATE that ends it, whatever role writes it:
-- answered, failed, cancelled, interrupted (at shutdown, or by the conversation's next question
-- when the run was left `running` past its deadline). A run that ends without an answer keeps
-- nothing, as for the partial answer (0009_answer_partials.sql): the text of a failed or stopped
-- question is not kept where the history does not keep it. An answered question's text is in the
-- history, with its answer, as before.
--
-- The exception, as for the partial answer: a run whose replica stopped without recording its end
-- (killed, out of memory, a lost node, or the database unreachable while it wrote the end) stays
-- `running` with its text until the conversation's next question marks it interrupted (which
-- clears the text), or until the purge removes the deleted conversation with its runs. Before its
-- deadline the owner's reads still return the text (the run looks running); past it the run reads
-- as interrupted, without it, so no read returns the text it keeps after that. There is no
-- periodic cleanup, the same rule as 0009's. Clearing removes the text from the live table
-- only: the old row version stays until vacuum, and WAL archives and base backups keep it for
-- their retention, as they do a partial answer.
--
-- The clearing is the trigger `runs_question_cleared` below, not the service's UPDATEs: the
-- previous release (a pod during a rolling update, or after a helm rollback;
-- docs/architecture/deployment.md, 「相容規則」) ends runs without naming the column, for example
-- when its next question marks a run this release left `running` past its deadline as
-- interrupted. With the CHECK alone that UPDATE would be refused, and the conversation could take
-- no question on that release again. The trigger clears the text in every UPDATE that moves the
-- run out of `running`, so the service role needs no UPDATE of the column at all: it may name it
-- only when it inserts a run, and a stored text never changes. Ordered after `runs_clock` (0002,
-- triggers of one event fire by name), which refuses any change to a run that already ended.
--
-- The text is a question as the user asked it, as sensitive as the history's messages: row-level
-- security keeps it in its tenant (0002's policy on assistant.runs), and it goes with its run,
-- which goes with its conversation, so the purge of a deleted conversation removes it. Only the
-- owner's reads return it (`GET /conversations/{id}`, the question API's 202 bodies); the readers
-- of a shared conversation get no run at all. The service never logs it.
--
-- The CHECK keeps the column to running runs and bounds the text at 1 to 100000 characters, the
-- question API's limit (`config::MAX_QUESTION_CHARS`, the most `questions.max_chars` may be);
-- `store::start_run` stores no text outside it. It is added `NOT VALID`, like 0007's: the column
-- is new and has no default, so every row there is holds NULL, which it accepts, and nothing
-- needs to be read to know it. Validating would scan assistant.runs under the ALTER's ACCESS
-- EXCLUSIVE lock (every read of a conversation reads its latest run and waits behind it). Every
-- row inserted or updated from now on is checked as with a validated CHECK.
--
-- Rows the previous release stored stay NULL, and so do the runs it starts: their text is not
-- known while they run, as before this file.
--
-- Locks: the ALTER holds ACCESS EXCLUSIVE on assistant.runs until the commit (every read and
-- write of the runs waits: a conversation's reads, a question's start, end and stop), and the
-- CREATE TRIGGER needs nothing more on the same table. The file changes only the catalog, so that
-- is short. It locks no other table, so it cannot meet a transaction of the service in a
-- deadlock.

-- Fail fast rather than queue the service's reads and writes behind the lock; the Job fails and
-- retries.
SET LOCAL lock_timeout = '5s';

-- Adding a column without a default changes only the catalog.
ALTER TABLE assistant.runs
    ADD COLUMN question text,
    ADD CONSTRAINT runs_question_running
        CHECK (question IS NULL
            OR (status = 'running' AND char_length(question) BETWEEN 1 AND 100000))
        NOT VALID;

-- An invoker's trigger function with a pinned search_path, like `runs_clock`: it reads only the
-- row being written and runs with the rights of whoever writes it.
CREATE FUNCTION assistant.runs_question_cleared() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, pg_temp
    AS $$
BEGIN
    NEW.question := NULL;
    RETURN NEW;
END;
$$;

REVOKE ALL ON FUNCTION assistant.runs_question_cleared() FROM PUBLIC;

CREATE TRIGGER runs_question_cleared
    BEFORE UPDATE ON assistant.runs
    FOR EACH ROW
    WHEN (NEW.status <> 'running')
    EXECUTE FUNCTION assistant.runs_question_cleared();

GRANT INSERT (question) ON assistant.runs TO assistant_app;
