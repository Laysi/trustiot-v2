//! A conversation: its fixed preamble and its append-only history
//! (docs/architecture/ai-assistant.md, 「對話歷史只往後加」).
//!
//! The history lives in memory here; [`crate::store`] keeps it in Postgres
//! (Laysi/trustiot-v2#553) with the same append-only rule ([`Conversation::append`] is the only
//! way a message joins), and loads a stored conversation back with the preamble it started with.

use std::fmt;
use std::sync::Arc;

use contract::{TenantId, UserId};
use uuid::Uuid;

use crate::model::{CacheTtl, Effort, NativeMessage};
use crate::tools::ToolSpec;

/// Who a conversation belongs to: the tenant and the user who asked. Every tool call runs with
/// this principal (decision 31).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Principal {
    pub tenant_id: TenantId,
    pub user_id: UserId,
}

/// The versioned system prompt shared by every tenant (the first cached block).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemPrompt {
    /// Stored with each conversation, so a conversation keeps the version it started with and
    /// an answer can be traced to the prompt that produced it.
    pub version: String,
    pub text: String,
}

/// What a conversation fixes when it starts and never changes afterwards: the system prompt
/// and its version, the tenant context, the tool set and the model settings that are part of
/// the cache key. Changing any of these mid-conversation would invalidate the cache and the
/// model's preserved thinking; a new tool set or prompt applies to new conversations only.
#[derive(Debug, PartialEq, Eq)]
pub struct Preamble {
    system_prompt: SystemPrompt,
    tenant_context: String,
    tools: Vec<ToolSpec>,
    effort: Effort,
    cache_ttl: CacheTtl,
}

impl Preamble {
    /// `tools` are put in name order, so the same tool set always gives the same bytes.
    pub fn new(
        system_prompt: SystemPrompt,
        tenant_context: String,
        mut tools: Vec<ToolSpec>,
        effort: Effort,
        cache_ttl: CacheTtl,
    ) -> Self {
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            system_prompt,
            tenant_context,
            tools,
            effort,
            cache_ttl,
        }
    }

    pub fn system_prompt(&self) -> &SystemPrompt {
        &self.system_prompt
    }

    /// The tenant's own context (its device types, its terms), the second cached block, already
    /// escaped and marked as data (`prompt::tenant_context_block`). Empty until the tenant
    /// context work lands (Laysi/trustiot-v2#148).
    pub fn tenant_context(&self) -> &str {
        &self.tenant_context
    }

    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    pub fn effort(&self) -> Effort {
        self.effort
    }

    pub fn cache_ttl(&self) -> CacheTtl {
        self.cache_ttl
    }
}

/// What a history message is, which decides whether the chat shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// The user's question.
    Question,
    /// The model's final reply to a question.
    Answer,
    /// A model turn that called tools (its thinking and tool calls).
    Step,
    /// The results of a step's tool calls.
    ToolResults,
    /// A service-generated instruction (a mid-conversation system message), such as the
    /// wind-down near the token budget.
    Note,
}

impl EntryKind {
    /// Whether the chat shows the entry. The rest stays in the history, hidden: the model needs
    /// it, and the data behind an answer can be traced (decision 31).
    pub fn shown(self) -> bool {
        matches!(self, Self::Question | Self::Answer)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Question => "question",
            Self::Answer => "answer",
            Self::Step => "step",
            Self::ToolResults => "tool_results",
            Self::Note => "note",
        }
    }

    /// The kind called `name` (see [`EntryKind::as_str`]).
    pub fn from_name(name: &str) -> Option<Self> {
        [
            Self::Question,
            Self::Answer,
            Self::Step,
            Self::ToolResults,
            Self::Note,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == name)
    }
}

/// A conversation. Messages are only appended; nothing in the history is changed or removed,
/// so every request starts with the bytes of the previous one (cache hits, and thinking blocks
/// that stay valid).
pub struct Conversation {
    id: Uuid,
    principal: Principal,
    preamble: Arc<Preamble>,
    messages: Vec<NativeMessage>,
    kinds: Vec<EntryKind>,
    measured: Option<Measured>,
}

/// What the model last counted of a conversation: the preamble and the first `messages` of the
/// history weigh `tokens`. Since the history only grows, the count stays true for that prefix;
/// the budget check estimates only what came after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Measured {
    pub messages: usize,
    pub tokens: u64,
}

impl Conversation {
    pub fn new(principal: Principal, preamble: Arc<Preamble>) -> Self {
        Self {
            id: Uuid::now_v7(),
            principal,
            preamble,
            messages: Vec::new(),
            kinds: Vec::new(),
            measured: None,
        }
    }

    /// A stored conversation as it was saved ([`crate::store::load`]). The model's last count
    /// of the history is not stored, so the first call after loading estimates it again.
    pub(crate) fn restore(
        id: Uuid,
        principal: Principal,
        preamble: Arc<Preamble>,
        entries: Vec<(EntryKind, NativeMessage)>,
    ) -> Self {
        let (kinds, messages) = entries.into_iter().unzip();
        Self {
            id,
            principal,
            preamble,
            messages,
            kinds,
            measured: None,
        }
    }

    /// The latest count of the history by the model, if any call reported one.
    pub fn measured(&self) -> Option<Measured> {
        self.measured
    }

    /// Records that the whole history, as it is now, weighs `tokens`.
    pub(crate) fn record_measured(&mut self, tokens: u64) {
        self.measured = Some(Measured {
            messages: self.messages.len(),
            tokens,
        });
    }

    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn principal(&self) -> Principal {
        self.principal
    }

    pub fn preamble(&self) -> &Arc<Preamble> {
        &self.preamble
    }

    /// The history in order, as it is sent to the model.
    pub fn messages(&self) -> &[NativeMessage] {
        &self.messages
    }

    /// The history with what each message is.
    pub fn entries(&self) -> impl Iterator<Item = (EntryKind, &NativeMessage)> {
        self.kinds.iter().copied().zip(self.messages.iter())
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Adds a message at the end.
    pub fn append(&mut self, kind: EntryKind, message: NativeMessage) {
        self.messages.push(message);
        self.kinds.push(kind);
    }
}

impl fmt::Debug for Conversation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conversation")
            .field("id", &self.id)
            .field("principal", &self.principal)
            .field("prompt_version", &self.preamble.system_prompt.version)
            .field("messages", &self.messages.len())
            .finish()
    }
}
