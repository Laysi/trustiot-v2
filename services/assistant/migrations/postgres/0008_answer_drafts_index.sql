-- The index of an answered run's answer position (Laysi/trustiot-v2#1043; 0007_answer_drafts.sql
-- added the column). Run by the migration role `assistant_owner`, which owns everything here,
-- like 0001_assistant.sql.
--
-- Why the index. 0007's foreign key `runs_answer_message` cascades from a message to the run
-- whose `(tenant_id, conversation_id, answer_seq)` names it. Purging a conversation deletes its
-- messages, and for every deleted message the cascade deletes from assistant.runs by those three
-- columns. Without an index that leads with them, each of those deletes reads the conversation's
-- runs through the prefix of `runs_by_conversation` (0006) and filters them, so a purge costs the
-- conversation's messages times its runs, in one transaction for a whole batch of conversations.
-- Partial (`answer_seq IS NOT NULL`): the cascade looks up a message's position, never NULL, and
-- only answered runs from this release on have one, so the runs that the previous release
-- stored and the runs that did not end answered stay out of the index.
--
-- Why a file of its own rather than a statement in 0007. The Job runs a file in one transaction
-- and holds every lock until the commit. 0007's ALTER holds ACCESS EXCLUSIVE on assistant.runs,
-- so building the index there would read the whole table (one row per question asked) under that
-- lock, and every read of a conversation, which reads its latest run, would wait for the scan.
-- Here the file is its own transaction, after 0007 committed: the index needs 0007's column, so
-- it cannot come first.
--
-- Locks: `CREATE INDEX` holds SHARE on assistant.runs from the start of its scan to the commit.
-- Reads of the runs go on (reading a conversation, its latest run, the running count); writes of
-- the runs wait for the scan: a question's start and end (the end stores the answer's round in
-- the same transaction), a stop, deleting a conversation that has a running question, and the
-- purge. The file locks no other table of the service, so unlike 0007 it cannot hold one table
-- while it waits for another; a service transaction that writes runs and holds a lock on the
-- messages only waits for the commit.
--
-- `lock_timeout`: the SHARE lock waits for every open transaction that writes runs, and writes
-- that come later queue behind that wait (reads do not, as SHARE does not conflict with them).
-- 5 seconds bounds the wait, as in 0006 and 0007: the Job fails and retries rather than queue the
-- service's writes. It bounds waiting for the lock only, not the scan once the lock is held.
--
-- Why not `CREATE INDEX CONCURRENTLY`, which would let the writes go on: it cannot run inside a
-- transaction block, and the Job runs every file in one transaction, so the migration rules
-- forbid it (tools/migrate/tests/registry.rs, `FORBIDDEN`). The cost accepted is the wait of the
-- writes above for one scan of assistant.runs, the same as 0006's `runs_by_conversation`.
--
-- No grant: the index is the table owner's, and the cascade runs as the table's owner.

-- Fail fast rather than queue the service's writes behind the lock; the Job fails and retries.
SET LOCAL lock_timeout = '5s';

-- The foreign key's cascade from a message to its run (0007's `runs_answer_message`).
CREATE INDEX runs_by_answer ON assistant.runs (tenant_id, conversation_id, answer_seq)
    WHERE answer_seq IS NOT NULL;
