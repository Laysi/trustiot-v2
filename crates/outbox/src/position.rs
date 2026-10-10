//! Positions in a producer's event log.

use std::fmt;
use std::str::FromStr;

/// Where an event sits in its producer's log, and where a subscription has read up to.
///
/// Events are ordered by `(tx, id)`: `tx` is the writing transaction's ID, raised when needed
/// to stay after the previous event of the same aggregate, and `id` the row's insertion number.
/// A consumer only reads events whose `tx` is below the oldest transaction still running, so an
/// event that commits later always sorts after everything already read: no event is skipped,
/// and the order never changes once read. See the crate README for the argument.
///
/// Written as `<tx>:<id>`, for example `751:12`; [`Position::START`] is `0:0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Position {
    tx: i64,
    id: i64,
}

impl Position {
    /// Before every event.
    pub const START: Self = Self { tx: 0, id: 0 };

    /// A position from its parts, as stored in the outbox tables.
    #[must_use]
    pub const fn new(tx: i64, id: i64) -> Self {
        Self { tx, id }
    }

    /// The ordering transaction ID.
    #[must_use]
    pub const fn tx(self) -> i64 {
        self.tx
    }

    /// The row's insertion number.
    #[must_use]
    pub const fn id(self) -> i64 {
        self.id
    }

    /// The position right before this event's: replaying from it delivers this event again,
    /// then everything after it.
    #[must_use]
    pub const fn just_before(self) -> Self {
        Self {
            tx: self.tx,
            id: self.id.saturating_sub(1),
        }
    }
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.tx, self.id)
    }
}

impl FromStr for Position {
    type Err = InvalidPosition;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (tx, id) = text.split_once(':').ok_or(InvalidPosition)?;
        let part = |part: &str| {
            part.parse::<i64>()
                .ok()
                .filter(|value| *value >= 0)
                .ok_or(InvalidPosition)
        };
        Ok(Self::new(part(tx)?, part(id)?))
    }
}

/// A position was not `<tx>:<id>` with two non-negative integers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a position is written `<tx>:<id>` with two non-negative integers")]
pub struct InvalidPosition;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_order_by_transaction_then_row() {
        assert!(Position::new(5, 100) < Position::new(6, 1));
        assert!(Position::new(6, 1) < Position::new(6, 2));
        assert!(Position::START < Position::new(0, 1));
    }

    #[test]
    fn positions_round_trip_as_text() {
        let position = Position::new(751, 12);
        assert_eq!(position.to_string(), "751:12");
        assert_eq!("751:12".parse::<Position>(), Ok(position));
        assert_eq!("0:0".parse::<Position>(), Ok(Position::START));
        for text in ["", "1", "1:", ":1", "-1:2", "1:x", "1:2:3"] {
            assert_eq!(text.parse::<Position>(), Err(InvalidPosition), "{text}");
        }
    }
}
