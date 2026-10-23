//! Conversations in Postgres (Laysi/trustiot-v2#553; docs/architecture/ai-assistant.md,
//! 「對話歷史只往後加」): the preamble a conversation fixed when it started, and its history, one
//! row per message, stored as the bytes sent to the model and read back byte for byte. Schema:
//! `migrations/postgres/0001_assistant.sql`.
//!
//! Append-only at both ends. The database gives the service role SELECT and INSERT only, so no
//! code path of the service can change or remove a stored message; and this module only ever
//! inserts. Every function takes a [`TenantTransaction`], so row-level security limits it to
//! the transaction's tenant (decision 29), and the caller decides when the writes commit.
//!
//! How the chat API (Laysi/trustiot-v2#146) uses it: [`create`] when a conversation starts (or
//! after its first question), [`append`] after each question with the number of messages
//! already stored, in a transaction of the conversation's tenant, and [`load`] to continue a
//! conversation. Appending is idempotent: messages that are already stored with the same bytes
//! are skipped, so retrying after a commit whose outcome was lost is safe; a stored message
//! that differs is a [`StoreError::Conflict`], never overwritten.

use std::sync::Arc;

use contract::{TenantId, UserId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
use service_kit::tenant::TenantTransaction;
use sqlx::Row;
use uuid::Uuid;

use crate::history::{Conversation, EntryKind, Preamble, Principal, SystemPrompt};
use crate::model::{CacheTtl, Effort, Format, NativeMessage};
use crate::tools::ToolSpec;

/// `$1` tenant, `$2` conversation, `$3` user, then the preamble. Returns the conversation when
/// this statement stored it, nothing when it was stored before.
const INSERT_CONVERSATION: &str = "INSERT INTO assistant.conversations
    (tenant_id, conversation_id, user_id, prompt_version, system_prompt, tenant_context, tools,
     effort, cache_ttl)
VALUES ($1::uuid, $2::uuid, $3::uuid, $4, $5, $6, $7, $8, $9)
ON CONFLICT (tenant_id, conversation_id) DO NOTHING
RETURNING conversation_id";

/// `$1` tenant, `$2` conversation.
const SELECT_CONVERSATION: &str = "SELECT user_id::text AS user_id, prompt_version,
    system_prompt, tenant_context, tools, effort, cache_ttl
FROM assistant.conversations
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid";

/// `$1` tenant, `$2` conversation: the position after the last stored message.
const NEXT_SEQ: &str = "SELECT coalesce(max(seq)::bigint + 1, 0) AS next
FROM assistant.messages
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid";

/// `$1` tenant, `$2` conversation, `$3` position, `$4` kind, `$5` format, `$6` the bytes.
/// Returns the position when this statement stored the message, nothing when one was there.
const INSERT_MESSAGE: &str = "INSERT INTO assistant.messages
    (tenant_id, conversation_id, seq, kind, format, body)
VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6)
ON CONFLICT (tenant_id, conversation_id, seq) DO NOTHING
RETURNING seq";

/// `$1` tenant, `$2` conversation, `$3` position.
const SELECT_MESSAGE: &str = "SELECT kind, format, body
FROM assistant.messages
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND seq = $3";

/// `$1` tenant, `$2` conversation: the whole history in order.
const SELECT_MESSAGES: &str = "SELECT seq, kind, format, body
FROM assistant.messages
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid
ORDER BY seq";

/// SQLSTATE `foreign_key_violation`: a message for a conversation that is not stored.
const FOREIGN_KEY_VIOLATION: &str = "23503";

/// Why storing or loading a conversation failed. Carries no message content.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The transaction is for another tenant than the conversation's.
    #[error("the transaction's tenant is not the conversation's")]
    WrongTenant,
    /// [`append`] for a conversation that [`create`] has not stored.
    #[error("the conversation is not stored")]
    NotStored,
    /// [`append`] from a position after the end of the stored history, which would leave a gap.
    #[error("appending from message {from}, but only {stored} messages are stored")]
    Gap { from: usize, stored: usize },
    /// What is stored at a position, or as the conversation's preamble, differs from the
    /// conversation being written: another writer appended first. Nothing is overwritten; the
    /// caller loads the stored conversation again.
    #[error("the stored conversation differs at {0}")]
    Conflict(&'static str),
    /// The conversation cannot be stored so that it reads back exactly as it is.
    #[error("the conversation cannot be stored: {0}")]
    Unstorable(&'static str),
    /// A stored row does not read back as a conversation.
    #[error("a stored conversation is corrupt: {0}")]
    Corrupt(&'static str),
    /// The database failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// A tool definition as stored, in the order of [`ToolSpec`]'s fields.
#[derive(Serialize)]
struct StoredTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoadedTool {
    name: String,
    description: String,
    input_schema: Value,
}

impl From<LoadedTool> for ToolSpec {
    fn from(tool: LoadedTool) -> Self {
        Self {
            name: tool.name,
            description: tool.description,
            input_schema: tool.input_schema,
        }
    }
}

/// The tool definitions as stored: a JSON array. The request sends them from the parsed
/// `serde_json::Value`, so they must parse back to the same value, or a resumed conversation
/// would send different tool definitions than it started with (serde_json reads a few long
/// floating-point literals one bit off unless its `float_roundtrip` feature is on).
fn tools_json(tools: &[ToolSpec]) -> Result<String, StoreError> {
    let json = serde_json::to_string(&stored_tools(tools))
        .map_err(|_| StoreError::Unstorable("the tool definitions are not JSON"))?;
    let again = parse_tools(&json)
        .ok()
        .and_then(|parsed| serde_json::to_string(&stored_tools(&parsed)).ok());
    if again.as_deref() != Some(json.as_str()) {
        return Err(StoreError::Unstorable(
            "the tool definitions do not read back as the same JSON",
        ));
    }
    Ok(json)
}

fn parse_tools(json: &str) -> Result<Vec<ToolSpec>, serde_json::Error> {
    serde_json::from_str::<Vec<LoadedTool>>(json)
        .map(|tools| tools.into_iter().map(ToolSpec::from).collect())
}

fn stored_tools(tools: &[ToolSpec]) -> Vec<StoredTool<'_>> {
    tools
        .iter()
        .map(|tool| StoredTool {
            name: &tool.name,
            description: &tool.description,
            input_schema: &tool.input_schema,
        })
        .collect()
}

/// Postgres `text` cannot hold U+0000; refuse it up front with a clear error rather than a
/// database one.
fn text<'a>(value: &'a str, what: &'static str) -> Result<&'a str, StoreError> {
    if value.contains('\0') {
        return Err(StoreError::Unstorable(what));
    }
    Ok(value)
}

fn check_tenant(tx: &TenantTransaction, tenant: TenantId) -> Result<(), StoreError> {
    if tx.tenant_id() == tenant {
        Ok(())
    } else {
        Err(StoreError::WrongTenant)
    }
}

/// Stores a new conversation, its preamble and every message it has so far, in `tx`, which
/// must be a transaction of the conversation's tenant. Returns the number of messages stored,
/// the `stored` to give [`append`] next.
///
/// Storing a conversation that is already stored with the same owner and preamble is not an
/// error (a retry); with a different one it is a [`StoreError::Conflict`].
///
/// # Errors
///
/// [`StoreError::WrongTenant`], [`StoreError::Unstorable`], [`StoreError::Conflict`],
/// [`StoreError::Corrupt`] or a database failure; the caller rolls `tx` back then.
pub async fn create(
    tx: &mut TenantTransaction,
    conversation: &Conversation,
) -> Result<usize, StoreError> {
    let principal = conversation.principal();
    check_tenant(tx, principal.tenant_id)?;
    let preamble = conversation.preamble();
    let tools = tools_json(preamble.tools())?;
    let inserted = sqlx::query(INSERT_CONVERSATION)
        .bind(principal.tenant_id.to_string())
        .bind(conversation.id().to_string())
        .bind(principal.user_id.to_string())
        .bind(text(
            &preamble.system_prompt().version,
            "the system prompt version contains U+0000",
        )?)
        .bind(text(
            &preamble.system_prompt().text,
            "the system prompt contains U+0000",
        )?)
        .bind(text(
            preamble.tenant_context(),
            "the tenant context contains U+0000",
        )?)
        .bind(&tools)
        .bind(preamble.effort().as_str())
        .bind(preamble.cache_ttl().as_str())
        .fetch_optional(&mut **tx)
        .await?;
    if inserted.is_none() {
        // Stored before: only a retry of the same conversation may pass.
        let stored = select_conversation(tx, principal.tenant_id, conversation.id())
            .await?
            .ok_or(StoreError::Corrupt(
                "a conversation that conflicted on insert is not readable",
            ))?;
        if stored.user_id != principal.user_id || *stored.preamble != **preamble {
            return Err(StoreError::Conflict("the conversation's owner or preamble"));
        }
    }
    append(tx, conversation, 0).await
}

/// Stores the messages of `conversation` from position `stored` on, in `tx`, which must be a
/// transaction of the conversation's tenant. `stored` is what [`create`] or the previous
/// [`append`] returned; returns the new count, the conversation's length.
///
/// A message already stored at its position with the same kind and bytes is skipped, so a
/// retry is harmless. One that differs means another writer appended first: the call fails
/// with [`StoreError::Conflict`] and writes nothing (the caller rolls `tx` back).
///
/// # Errors
///
/// [`StoreError::WrongTenant`], [`StoreError::NotStored`], [`StoreError::Gap`],
/// [`StoreError::Conflict`], [`StoreError::Unstorable`] or a database failure.
pub async fn append(
    tx: &mut TenantTransaction,
    conversation: &Conversation,
    stored: usize,
) -> Result<usize, StoreError> {
    let tenant = conversation.principal().tenant_id;
    check_tenant(tx, tenant)?;
    let tenant_text = tenant.to_string();
    let id_text = conversation.id().to_string();
    if stored > conversation.len() {
        return Err(StoreError::Gap {
            from: stored,
            stored: conversation.len(),
        });
    }
    let next: i64 = sqlx::query(NEXT_SEQ)
        .bind(&tenant_text)
        .bind(&id_text)
        .fetch_one(&mut **tx)
        .await?
        .try_get("next")?;
    let next = usize::try_from(next).map_err(|_| StoreError::Corrupt("a negative position"))?;
    if stored > next {
        return Err(StoreError::Gap {
            from: stored,
            stored: next,
        });
    }
    if next > conversation.len() {
        // Another writer stored more than this conversation has.
        return Err(StoreError::Conflict("the end of the history"));
    }
    for (seq, (kind, message)) in conversation.entries().enumerate().skip(stored) {
        let position = i32::try_from(seq)
            .map_err(|_| StoreError::Unstorable("the history has too many messages"))?;
        let inserted = sqlx::query(INSERT_MESSAGE)
            .bind(&tenant_text)
            .bind(&id_text)
            .bind(position)
            .bind(kind.as_str())
            .bind(message.format().as_str())
            .bind(message.json().as_bytes())
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                let code = error
                    .as_database_error()
                    .and_then(|database| database.code());
                if code.as_deref() == Some(FOREIGN_KEY_VIOLATION) {
                    StoreError::NotStored
                } else {
                    StoreError::Database(error)
                }
            })?;
        if inserted.is_some() {
            continue;
        }
        let row = sqlx::query(SELECT_MESSAGE)
            .bind(&tenant_text)
            .bind(&id_text)
            .bind(position)
            .fetch_one(&mut **tx)
            .await?;
        let same = row.try_get::<String, _>("kind")? == kind.as_str()
            && row.try_get::<String, _>("format")? == message.format().as_str()
            && row.try_get::<Vec<u8>, _>("body")? == message.json().as_bytes();
        if !same {
            return Err(StoreError::Conflict("a message of the history"));
        }
    }
    Ok(conversation.len())
}

/// A stored conversation's owner and preamble.
struct StoredConversation {
    user_id: UserId,
    preamble: Arc<Preamble>,
}

async fn select_conversation(
    tx: &mut TenantTransaction,
    tenant: TenantId,
    id: Uuid,
) -> Result<Option<StoredConversation>, StoreError> {
    let Some(row) = sqlx::query(SELECT_CONVERSATION)
        .bind(tenant.to_string())
        .bind(id.to_string())
        .fetch_optional(&mut **tx)
        .await?
    else {
        return Ok(None);
    };
    let user_id: UserId = row
        .try_get::<String, _>("user_id")?
        .parse()
        .map_err(|_| StoreError::Corrupt("a user ID"))?;
    let tools = parse_tools(&row.try_get::<String, _>("tools")?)
        .map_err(|_| StoreError::Corrupt("the tool definitions"))?;
    let effort = Effort::from_name(&row.try_get::<String, _>("effort")?)
        .ok_or(StoreError::Corrupt("the effort"))?;
    let cache_ttl = CacheTtl::from_name(&row.try_get::<String, _>("cache_ttl")?)
        .ok_or(StoreError::Corrupt("the cache lifetime"))?;
    let preamble = Preamble::new(
        SystemPrompt {
            version: row.try_get("prompt_version")?,
            text: row.try_get("system_prompt")?,
        },
        row.try_get("tenant_context")?,
        tools,
        effort,
        cache_ttl,
    );
    Ok(Some(StoredConversation {
        user_id,
        preamble: Arc::new(preamble),
    }))
}

/// The conversation `id` of `principal`, as it was stored, in `tx`, which must be a
/// transaction of the principal's tenant. `None` when the tenant has no such conversation or it
/// belongs to another user of the tenant: the conversation's tools run as its owner, so only
/// the owner continues it (who else may read it is Laysi/trustiot-v2#146's to decide).
///
/// Every message reads back as the bytes that were stored; a row that does not (an unknown
/// kind or format, bytes that are not one JSON value, a gap in the positions) fails the load
/// instead of sending the model an altered history.
///
/// # Errors
///
/// [`StoreError::WrongTenant`], [`StoreError::Corrupt`] or a database failure.
pub async fn load(
    tx: &mut TenantTransaction,
    principal: Principal,
    id: Uuid,
) -> Result<Option<Conversation>, StoreError> {
    check_tenant(tx, principal.tenant_id)?;
    let Some(stored) = select_conversation(tx, principal.tenant_id, id).await? else {
        return Ok(None);
    };
    if stored.user_id != principal.user_id {
        return Ok(None);
    }
    let rows = sqlx::query(SELECT_MESSAGES)
        .bind(principal.tenant_id.to_string())
        .bind(id.to_string())
        .fetch_all(&mut **tx)
        .await?;
    let mut entries = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let seq: i32 = row.try_get("seq")?;
        if usize::try_from(seq).ok() != Some(index) {
            return Err(StoreError::Corrupt("a gap in the history's positions"));
        }
        let kind = EntryKind::from_name(&row.try_get::<String, _>("kind")?)
            .ok_or(StoreError::Corrupt("a message's kind"))?;
        let format = Format::from_name(&row.try_get::<String, _>("format")?)
            .ok_or(StoreError::Corrupt("a message's format"))?;
        let body: Vec<u8> = row.try_get("body")?;
        entries.push((kind, NativeMessage::new(format, raw_message(body)?)));
    }
    Ok(Some(Conversation::restore(
        id,
        principal,
        stored.preamble,
        entries,
    )))
}

/// The stored bytes as a message, unchanged: they must be UTF-8 and exactly one JSON value.
/// Parsing as a `RawValue` keeps the text as it is and drops only surrounding whitespace, so an
/// equal length proves the message is the stored bytes.
fn raw_message(body: Vec<u8>) -> Result<Box<RawValue>, StoreError> {
    let length = body.len();
    let text = String::from_utf8(body).map_err(|_| StoreError::Corrupt("a message's bytes"))?;
    let raw = RawValue::from_string(text).map_err(|_| StoreError::Corrupt("a message's JSON"))?;
    if raw.get().len() != length {
        return Err(StoreError::Corrupt("a message's JSON"));
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool(schema: Value) -> ToolSpec {
        ToolSpec {
            name: "count_readings".into(),
            description: "Counts readings.".into(),
            input_schema: schema,
        }
    }

    #[test]
    fn tool_definitions_read_back_as_the_same_json() {
        let tools = vec![tool(json!({
            "type": "object",
            "properties": {"hours": {"type": "number", "minimum": 0.1, "maximum": 1e300}},
            "required": ["hours"]
        }))];
        let json = tools_json(&tools).expect("storable");
        let parsed = parse_tools(&json).expect("parses");
        assert_eq!(parsed, tools);
        assert_eq!(
            serde_json::to_string(&stored_tools(&parsed)).expect("JSON"),
            json
        );
    }

    #[test]
    fn a_message_reads_back_only_as_its_exact_bytes() {
        let message = br#"{"role":"assistant","content":[{"type":"thinking","thinking":"x","signature":"c2ln"}]}"#;
        let raw = raw_message(message.to_vec()).expect("a message");
        assert_eq!(raw.get().as_bytes(), message);
        for corrupt in [
            b" {\"role\":\"user\"}".to_vec(),
            b"{\"role\":\"user\"} ".to_vec(),
            b"{\"role\":".to_vec(),
            b"{}{}".to_vec(),
            vec![b'{', 0xff, b'}'],
        ] {
            assert!(
                matches!(raw_message(corrupt), Err(StoreError::Corrupt(_))),
                "refused"
            );
        }
    }

    #[test]
    fn text_with_u0000_is_refused_up_front() {
        assert!(matches!(
            text("a\0b", "x"),
            Err(StoreError::Unstorable("x"))
        ));
        assert_eq!(text("ab", "x").expect("text"), "ab");
    }
}
