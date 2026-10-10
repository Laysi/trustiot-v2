//! The aggregate an event belongs to: the unit within which events are ordered.

use std::fmt;

/// The longest aggregate kind, in bytes.
pub const MAX_KIND_LEN: usize = 64;
/// The longest aggregate ID, in bytes.
pub const MAX_ID_LEN: usize = 256;

/// The thing an event is about, such as one tenant or one device. Events of the same aggregate
/// are delivered in the order they were written; events of different aggregates have no
/// guaranteed relative order.
///
/// `kind` is a lower snake case name fixed in code (`tenant`, `device_model`); `id` is the
/// aggregate's own ID as text, usually a UUID.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Aggregate {
    kind: String,
    id: String,
}

impl Aggregate {
    /// An aggregate reference.
    ///
    /// # Errors
    ///
    /// When `kind` is not 1–64 bytes of lower snake case starting with a letter, or `id` is
    /// empty, longer than 256 bytes, or contains control characters.
    pub fn new(kind: impl Into<String>, id: impl Into<String>) -> Result<Self, InvalidAggregate> {
        let kind = kind.into();
        let id = id.into();
        if !is_kind(&kind) {
            return Err(InvalidAggregate::Kind);
        }
        if id.is_empty() || id.len() > MAX_ID_LEN || id.chars().any(char::is_control) {
            return Err(InvalidAggregate::Id);
        }
        Ok(Self { kind, id })
    }

    /// The aggregate kind, for example `tenant`.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The aggregate's ID.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Rebuilds a reference read back from an outbox table, which only holds valid ones.
    pub(crate) fn from_stored(kind: String, id: String) -> Self {
        Self { kind, id }
    }
}

impl fmt::Display for Aggregate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.kind, self.id)
    }
}

fn is_kind(kind: &str) -> bool {
    let bytes = kind.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_KIND_LEN
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
}

/// An [`Aggregate`] could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidAggregate {
    /// The kind is not 1–64 bytes of lower snake case starting with a letter.
    #[error("aggregate kind must be 1-64 bytes of lower snake case starting with a letter")]
    Kind,
    /// The ID is empty, too long, or contains control characters.
    #[error("aggregate id must be 1-256 bytes without control characters")]
    Id,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_lower_snake_case() {
        for kind in ["tenant", "device_model", "a1"] {
            assert!(Aggregate::new(kind, "x").is_ok(), "{kind}");
        }
        for kind in [
            "",
            "Tenant",
            "1tenant",
            "device-model",
            "_x",
            "a".repeat(65).as_str(),
        ] {
            assert_eq!(
                Aggregate::new(kind, "x"),
                Err(InvalidAggregate::Kind),
                "{kind}"
            );
        }
    }

    #[test]
    fn ids_are_bounded_and_printable() {
        assert!(Aggregate::new("tenant", "0199d1a2-7c1e-7000-8000-000000000000").is_ok());
        assert!(Aggregate::new("tenant", "機台 A").is_ok());
        for id in ["", "a\nb", "x".repeat(257).as_str()] {
            assert_eq!(
                Aggregate::new("tenant", id),
                Err(InvalidAggregate::Id),
                "{id:?}"
            );
        }
        assert_eq!(
            Aggregate::new("tenant", "t-1").expect("valid").to_string(),
            "tenant/t-1"
        );
    }
}
