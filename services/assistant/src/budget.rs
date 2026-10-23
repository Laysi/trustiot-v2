//! The token budget of one question (docs/architecture/ai-assistant.md, 「Effort 與預算」).
//!
//! There is no step limit: a long analysis takes as many model calls as it needs. What bounds
//! it is the tokens its calls process, counted from each response's usage (every attempt the
//! gateway made, cache reads included, since they still cost and fill the context). At the
//! wind-down share the main agent is told to stop calling tools and answer; at the full budget
//! it is not allowed further tool calls. Before each call the agent checks that the call fits
//! what is left (crate::agent): the request's estimated input plus its output limit, which is
//! lowered to the room left, so no call can take the question far past its budget.

use crate::model::TokenCount;

#[derive(Clone, Debug)]
pub struct Budget {
    limit: u64,
    wind_down_at: u64,
    spent: u64,
}

impl Budget {
    /// `wind_down_percent` is between 1 and 99 (checked by the configuration).
    pub fn new(limit: u64, wind_down_percent: u8) -> Self {
        Self {
            limit,
            wind_down_at: limit.saturating_mul(u64::from(wind_down_percent)) / 100,
            spent: 0,
        }
    }

    pub fn record(&mut self, usage: &TokenCount) {
        self.spent = self.spent.saturating_add(usage.processed());
    }

    pub fn spent(&self) -> u64 {
        self.spent
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// What the question may still process.
    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.spent)
    }

    /// Time to tell the model to answer with what it has.
    pub fn winding_down(&self) -> bool {
        self.spent >= self.wind_down_at
    }

    /// No more tool calls.
    pub fn exhausted(&self) -> bool {
        self.spent >= self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, cache_read: u64, output: u64) -> TokenCount {
        TokenCount {
            input,
            cache_read,
            output,
            ..TokenCount::default()
        }
    }

    #[test]
    fn every_processed_token_counts_and_the_thresholds_are_inclusive() {
        let mut budget = Budget::new(1_000, 80);
        budget.record(&usage(100, 600, 99));
        assert_eq!(budget.spent(), 799);
        assert_eq!(budget.remaining(), 201);
        assert!(!budget.winding_down());
        budget.record(&usage(1, 0, 0));
        assert!(budget.winding_down());
        assert!(!budget.exhausted());
        budget.record(&TokenCount {
            cache_write: 200,
            ..TokenCount::default()
        });
        assert!(budget.exhausted());
    }

    #[test]
    fn large_budgets_do_not_overflow() {
        let mut budget = Budget::new(u64::MAX, 99);
        budget.record(&usage(u64::MAX, u64::MAX, 1));
        assert!(budget.exhausted());
        assert_eq!(budget.remaining(), 0);
    }
}
