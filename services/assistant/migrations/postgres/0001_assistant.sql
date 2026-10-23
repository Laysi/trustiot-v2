-- Assistant service schema (Laysi/trustiot-v2#553; decisions 29 and 31): the stored conversation
-- history of the main agent (docs/architecture/ai-assistant.md, 「對話歷史只往後加」). Run by the
-- migration role `assistant_owner`, which owns everything here; the service role
-- `assistant_app` owns nothing (crates/service-kit/README.md, "角色與 policy 範本"). The
-- migration Job (tools/migrate) creates both roles and the schema `assistant` owned by
-- `assistant_owner` and applies these files in order as `assistant_owner`, each in one
-- transaction. No platform role: nothing reads conversations across tenants.
--
-- Append-only: a conversation's preamble and every message of its history are written once and
-- never changed, because Claude's thinking blocks are bound to the exact conversation that
-- produced them and an edited history makes the next request fail. The service role therefore
-- gets SELECT and INSERT only, no UPDATE, DELETE or TRUNCATE, so a bug or a compromised service
-- cannot rewrite a history. How a user deletes a conversation is Laysi/trustiot-v2#146's to
-- decide; until then the foreign key below is NO ACTION, so no cascade can remove messages
-- behind the grants either.
--
-- Messages are kept as the bytes of the JSON text received from the model or first written by
-- the service, in bytea: not jsonb, which reorders keys and rewrites numbers, and not text, whose
-- bytes depend on the database and client encodings. The service sends them back to the model
-- exactly as stored (services/assistant/src/store.rs reads them back and checks it).
--
-- Both tables hold tenant data and follow crates/service-kit/sql/tenant-table.sql: `tenant_id`
-- leads every key, so a message can only belong to a conversation of its own tenant, and
-- `tenant_isolation` limits every role to the transaction's tenant.

GRANT USAGE ON SCHEMA assistant TO assistant_app;

-- One row per conversation: who it belongs to and the preamble it fixed when it started
-- (services/assistant/src/history.rs, `Preamble`): the system prompt and its version, the
-- tenant context, the tool set, the effort and the cache lifetime. The system prompt is stored
-- whole, not only its version, so a conversation resumes with the text it started with after a
-- release changed the prompt. Laysi/trustiot-v2#146 adds what a conversation thread needs
-- (title, archiving) in its own file.
CREATE TABLE assistant.conversations (
    tenant_id uuid NOT NULL,
    conversation_id uuid NOT NULL,
    -- The user who asked; every tool call of the conversation runs as this user.
    user_id uuid NOT NULL,
    prompt_version text NOT NULL CHECK (char_length(prompt_version) BETWEEN 1 AND 64),
    system_prompt text NOT NULL,
    -- The tenant context block as it was sent (already escaped and tagged); empty when none.
    tenant_context text NOT NULL,
    -- The tool definitions as a JSON array of {name, description, input_schema}, in name order.
    tools text NOT NULL,
    effort text NOT NULL CHECK (effort IN ('low', 'medium', 'high', 'xhigh', 'max')),
    cache_ttl text NOT NULL CHECK (cache_ttl IN ('5m', '1h')),
    -- When it was stored, by the database clock.
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, conversation_id)
);

-- One row per message of a conversation's history, numbered from 0 in the order they are sent
-- to the model. The primary key makes appending safe without locks: two writers that both
-- append at the same position cannot both succeed, so a history never forks.
CREATE TABLE assistant.messages (
    tenant_id uuid NOT NULL,
    conversation_id uuid NOT NULL,
    seq integer NOT NULL CHECK (seq >= 0),
    -- What the message is (history::EntryKind); only questions and answers are shown in the
    -- chat, the rest stays in the history, hidden.
    kind text NOT NULL CHECK (kind IN ('question', 'answer', 'step', 'tool_results', 'note')),
    -- The model format the message is written in (model::Format).
    format text NOT NULL CHECK (format IN ('anthropic_messages')),
    -- The message: the bytes of one JSON object, exactly as sent to the model. The checks are
    -- cheap guards against a write that is not a message at all; the service checks the JSON
    -- itself when it reads the message back.
    body bytea NOT NULL CHECK (
        octet_length(body) >= 2
        AND substring(body FROM 1 FOR 1) = '\x7b'::bytea
        AND substring(body FROM octet_length(body) FOR 1) = '\x7d'::bytea
    ),
    -- When it was stored, by the database clock.
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, conversation_id, seq),
    FOREIGN KEY (tenant_id, conversation_id)
        REFERENCES assistant.conversations (tenant_id, conversation_id)
);

ALTER TABLE assistant.conversations ENABLE ROW LEVEL SECURITY;
ALTER TABLE assistant.messages ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON assistant.conversations
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

CREATE POLICY tenant_isolation ON assistant.messages
    USING (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid);

-- Read and append only. No UPDATE, DELETE or TRUNCATE for any role but the owner.
GRANT SELECT, INSERT ON assistant.conversations TO assistant_app;
GRANT SELECT, INSERT ON assistant.messages TO assistant_app;
