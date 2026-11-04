//! How many questions this replica runs at once, and the memory they hold
//! (services/assistant/README.md, 「記憶體」).
//!
//! A running question holds its conversation in memory for as long as it runs: the history it
//! was loaded with, then the round it adds. Every model call also serializes the whole of it into
//! the request's body, a second copy, and reads the model's response. A history may be close to
//! [`crate::store::MAX_HISTORY_BYTES`] (16 MiB), so the user's own running limit alone does not
//! bound what a few users' questions hold together. Each question therefore takes a [`Seat`]
//! before anything of it is stored or sent:
//!
//! - a seat counts against `questions.max_running_per_replica`, and reserves [`ROUND_BYTES`] of
//!   `questions.memory` for the round itself;
//! - once its conversation's size is known (measured in the database before it is loaded,
//!   [`crate::store::admit_question`]), the seat reserves the history and the preamble twice: the
//!   conversation and a request's copy ([`question_bytes`]).
//!
//! A question that does not get its seat or its memory is refused at once, not queued: a
//! question runs for minutes, so a wait short enough for a request would rarely end in a seat.
//! One whose size alone exceeds the budget takes the whole budget: it runs when nothing else
//! does, rather than never.
//!
//! The sizes are estimates from the stored bytes, not measured allocations; [`ROUND_BYTES`] is an
//! estimate of an ordinary round, not a bound (Laysi/trustiot-v2#694 measures the service's
//! memory and tunes the budgets).

use std::sync::{Arc, Mutex, PoisonError};

/// What a running question is reserved for its own round, besides the history it was loaded
/// with: the model's responses and the tool results it adds (in the conversation and in each
/// request's copy) and the response being read. 4 MiB covers a round of about 1.5 MiB, twice,
/// and a response of 1 MiB: a long analysis of tool results of a few KiB each and responses of a
/// few thousand tokens. An estimate (see the module documentation).
pub const ROUND_BYTES: usize = 4 << 20;

/// What a question with `stored` bytes of history and preamble reserves, before the budget caps
/// it: the round, and the history twice.
#[must_use]
pub fn question_bytes(stored: u64) -> usize {
    let stored = usize::try_from(stored).unwrap_or(usize::MAX);
    ROUND_BYTES.saturating_add(stored.saturating_mul(2))
}

/// The replica's seats and memory for questions. Cheap to clone; the clones share them.
#[derive(Clone, Debug)]
pub struct Capacity {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    max_running: usize,
    budget: usize,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    running: usize,
    reserved: usize,
}

/// What is full when a question is refused, for its log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Full {
    /// `running` (every seat is taken) or `memory` (the budget has no room for it).
    pub reason: &'static str,
    /// Questions running in the replica, this one not counted.
    pub running: usize,
    /// Bytes reserved by them.
    pub reserved: usize,
    /// What this question asked for.
    pub wanted: usize,
}

/// One question's seat and the memory it reserved; both are given back when this is dropped.
#[derive(Debug)]
pub struct Seat {
    shared: Arc<Shared>,
    bytes: usize,
}

impl Capacity {
    /// `max_running` seats sharing `budget` bytes.
    #[must_use]
    pub fn new(max_running: usize, budget: usize) -> Self {
        Self {
            shared: Arc::new(Shared {
                max_running,
                budget,
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// A seat with [`ROUND_BYTES`] (at most the whole budget) reserved.
    ///
    /// # Errors
    ///
    /// [`Full`] when every seat is taken or the budget has no room for the round.
    pub fn seat(&self) -> Result<Seat, Full> {
        let shared = &self.shared;
        let bytes = ROUND_BYTES.min(shared.budget);
        let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
        let full = |reason| Full {
            reason,
            running: state.running,
            reserved: state.reserved,
            wanted: bytes,
        };
        if state.running >= shared.max_running {
            return Err(full("running"));
        }
        if state.reserved.saturating_add(bytes) > shared.budget {
            return Err(full("memory"));
        }
        state.running += 1;
        state.reserved += bytes;
        drop(state);
        Ok(Seat {
            shared: Arc::clone(shared),
            bytes,
        })
    }

    /// Questions running and bytes reserved now.
    #[must_use]
    pub fn in_use(&self) -> (usize, usize) {
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        (state.running, state.reserved)
    }
}

impl Seat {
    /// Grows the reservation to [`question_bytes`] of `stored` (at most the whole budget).
    ///
    /// # Errors
    ///
    /// [`Full`] when the budget has no room for it; the seat keeps what it had.
    pub fn reserve(&mut self, stored: u64) -> Result<(), Full> {
        let shared = &self.shared;
        let target = question_bytes(stored).min(shared.budget);
        let extra = target.saturating_sub(self.bytes);
        let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.reserved.saturating_add(extra) > shared.budget {
            return Err(Full {
                reason: "memory",
                running: state.running - 1,
                reserved: state.reserved - self.bytes,
                wanted: target,
            });
        }
        state.reserved += extra;
        drop(state);
        self.bytes += extra;
        Ok(())
    }

    /// The bytes this seat holds.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.running -= 1;
        state.reserved -= self.bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1 << 20;

    #[test]
    fn a_question_reserves_its_round_and_its_history_twice() {
        assert_eq!(question_bytes(0), ROUND_BYTES);
        assert_eq!(question_bytes(1 << 20), ROUND_BYTES + 2 * MIB);
        assert_eq!(question_bytes(u64::MAX), usize::MAX);
    }

    #[test]
    fn seats_are_counted_and_given_back() {
        let capacity = Capacity::new(2, 100 * MIB);
        let first = capacity.seat().expect("a first seat");
        let second = capacity.seat().expect("a second seat");
        let full = capacity.seat().expect_err("no third seat");
        assert_eq!(full.reason, "running");
        assert_eq!((full.running, full.reserved), (2, 2 * ROUND_BYTES));
        drop(first);
        let third = capacity.seat().expect("a seat again once one ended");
        drop((second, third));
        assert_eq!(capacity.in_use(), (0, 0));
    }

    #[test]
    fn the_history_fits_the_budget_or_the_question_is_refused() {
        // Room for two rounds and a history of 1 MiB beside one of them.
        let capacity = Capacity::new(8, 2 * ROUND_BYTES + 2 * MIB);
        let mut first = capacity.seat().expect("a seat");
        first.reserve(0).expect("an empty history");
        assert_eq!(first.bytes(), ROUND_BYTES);
        let mut second = capacity.seat().expect("a seat");
        let full = second
            .reserve(2 << 20)
            .expect_err("4 MiB more do not fit beside the first");
        assert_eq!(full.reason, "memory");
        assert_eq!(
            (full.running, full.reserved, full.wanted),
            (1, ROUND_BYTES, ROUND_BYTES + 4 * MIB)
        );
        assert_eq!(second.bytes(), ROUND_BYTES, "it keeps what it had");
        second.reserve(1 << 20).expect("2 MiB more fit");
        assert_eq!(capacity.in_use(), (2, 2 * ROUND_BYTES + 2 * MIB));
        drop(first);
        drop(second);
        assert_eq!(capacity.in_use(), (0, 0));
    }

    #[test]
    fn a_question_larger_than_the_budget_runs_alone() {
        let capacity = Capacity::new(8, 10 * MIB);
        let mut large = capacity.seat().expect("a seat");
        large.reserve(64 << 20).expect("capped at the budget");
        assert_eq!(large.bytes(), 10 * MIB);
        let full = capacity.seat().expect_err("nothing beside it");
        assert_eq!(full.reason, "memory");
        drop(large);
        let mut small = capacity.seat().expect("a seat");
        small.reserve(1 << 20).expect("a small history");
        let mut large = capacity.seat().expect("a seat for the round");
        large
            .reserve(64 << 20)
            .expect_err("the large one waits for the small one");
    }

    /// The README's numbers (「記憶體」): with the default settings and a preamble of 50 KiB, how
    /// many questions of a history of each size run at once.
    #[test]
    fn the_default_budget_seats_the_documented_questions() {
        let defaults = crate::config::Questions::default();
        let preamble = 50 << 10;
        for (history, seated) in [
            (400 << 10, 8),
            (1 << 20, 6),
            (2 << 20, 4),
            ((16 << 20) - 1, 1),
        ] {
            let capacity = Capacity::new(defaults.max_running_per_replica, defaults.memory);
            let mut seats = Vec::new();
            while let Ok(mut seat) = capacity.seat() {
                if seat.reserve(history + preamble).is_err() {
                    break;
                }
                seats.push(seat);
            }
            assert_eq!(seats.len(), seated, "a history of {history} bytes");
        }
    }

    #[test]
    fn a_budget_smaller_than_a_round_still_seats_one_question() {
        let capacity = Capacity::new(8, MIB);
        let seat = capacity.seat().expect("one seat");
        assert_eq!(seat.bytes(), MIB);
        assert_eq!(capacity.seat().expect_err("no second").reason, "memory");
    }
}
