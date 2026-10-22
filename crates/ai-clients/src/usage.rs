//! Token usage by billing category, the form the usage ledger records (decision 28,
//! Laysi/trustiot-v2#140). Each provider module turns its own usage object into this.

/// Tokens of one model call by the categories the price table uses
/// (docs/architecture/ai-assistant.md, 「AI 閘道」 item 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    /// Input billed at the normal input rate (Anthropic: after the last cache breakpoint;
    /// OpenAI Decisions: every input token, see `decisions::DecisionUsage`).
    pub input: u64,
    /// Input written to the cache with the 5-minute lifetime.
    pub cache_write_5m: u64,
    /// Input written to the cache with the 1-hour lifetime.
    pub cache_write_1h: u64,
    /// Input read from the cache.
    pub cache_read: u64,
    /// Output, thinking included (thinking is billed as output even when it is summarized or
    /// omitted from the response).
    pub output: u64,
    /// The provider reported cache writes without splitting them by lifetime; they are
    /// counted as 5-minute writes (the default lifetime), and the ledger should mark the entry.
    pub cache_write_ttl_assumed: bool,
    /// The provider did not report the input count (`input_tokens`), for example because a
    /// proxy stripped `usage`: `input` is 0 here, and the ledger estimates it and marks the
    /// entry instead of recording 0 (docs/architecture/ai-assistant.md, 「AI 閘道」 item 3).
    pub input_missing: bool,
    /// The same for the output count (`output_tokens`) of a provider that bills output.
    pub output_missing: bool,
}

impl TokenUsage {
    /// Every input token, cached or not.
    pub fn total_input(&self) -> u64 {
        self.input
            .saturating_add(self.cache_write_5m)
            .saturating_add(self.cache_write_1h)
            .saturating_add(self.cache_read)
    }

    /// Whether a count the provider should have reported is missing, so the ledger must
    /// estimate it.
    pub fn is_incomplete(&self) -> bool {
        self.input_missing || self.output_missing
    }
}

/// The usage read from an HTTP 200 response that could not be used otherwise
/// ([`crate::Error::InvalidResponse`]): the provider billed the call, so the ledger records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportedUsage {
    /// What the provider reported, one entry per billed attempt with its model (`None`: the
    /// requested model), as `anthropic::Usage::attempts` gives it. A count left out of the
    /// usage object is flagged in the [`TokenUsage`].
    Attempts(Vec<(Option<String>, TokenUsage)>),
    /// No usage could be read: the body is over the size limit, is not JSON, or has no
    /// readable `usage` object. The ledger estimates the call and marks the entry, never 0.
    Unknown,
}
