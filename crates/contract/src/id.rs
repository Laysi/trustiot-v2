//! Typed identifiers.
//!
//! New IDs are UUIDv7: globally unique, so a tenant's data can move between SaaS and on-prem
//! deployments without renumbering (portability rule 3 in `docs/architecture/deployment.md`),
//! and ordered by creation time, so B-tree indexes on them stay compact. Parsing accepts any
//! UUID, because the version only matters when an ID is created.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        ///
        /// Serialized as a lowercase hyphenated UUID string.
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
            ToSchema,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// A new, time-ordered UUIDv7 ID.
            #[must_use]
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wraps an existing UUID, for example one read from the database.
            #[must_use]
            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// The underlying UUID.
            #[must_use]
            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl From<Uuid> for $name {
            fn from(uuid: Uuid) -> Self {
                Self(uuid)
            }
        }

        impl From<$name> for Uuid {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(text).map(Self)
            }
        }
    };
}

uuid_id!(
    /// A tenant: one customer organisation; every tenant-scoped row and event carries it.
    TenantId
);
uuid_id!(
    /// A user account; one user can be a member of several tenants.
    UserId
);
uuid_id!(
    /// An event, unique across services; consumers deduplicate deliveries by it.
    EventId
);

#[cfg(test)]
mod tests {
    use uuid::Version;

    use super::*;

    #[test]
    fn generated_ids_are_time_ordered_uuid_v7() {
        let first = TenantId::generate();
        let second = TenantId::generate();
        assert_eq!(first.as_uuid().get_version(), Some(Version::SortRand));
        assert!(first < second, "UUIDv7 sorts by creation time");
    }

    #[test]
    fn serializes_as_a_plain_uuid_string() {
        let text = "01928f6e-7a3c-7d4e-8f00-0123456789ab";
        let id: UserId = text.parse().expect("parse");
        assert_eq!(id.to_string(), text);
        assert_eq!(
            serde_json::to_value(id).expect("serialize"),
            serde_json::json!(text)
        );
        assert_eq!(
            serde_json::from_value::<UserId>(serde_json::json!(text)).expect("deserialize"),
            id
        );
        assert_eq!(Uuid::from(id), *id.as_uuid());
        assert_eq!(UserId::from(Uuid::from(id)), id);
    }

    #[test]
    fn rejects_text_that_is_not_a_uuid() {
        assert!("tenant-1".parse::<TenantId>().is_err());
        assert!(serde_json::from_value::<EventId>(serde_json::json!(42)).is_err());
    }
}
