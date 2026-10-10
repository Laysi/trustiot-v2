//! String-coded enums that stay decodable when a newer peer adds a code.

/// Longest code an [`open_enum!`] accepts for a value it does not know.
pub(crate) const MAX_CODE_LEN: usize = 64;

/// Whether `code` is a snake_case code: 1–64 bytes of `a-z`, `0-9` and `_`, starting with a
/// letter. Unknown codes must have this shape too, so a code is always safe to store and log.
pub(crate) fn is_valid_code(code: &str) -> bool {
    let bytes = code.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes.len() <= MAX_CODE_LEN
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
}

/// Defines an enum encoded as a snake_case string, with an `Other(String)` variant that keeps
/// codes added by a newer peer instead of failing the whole message. Use it for values whose
/// set grows over time (outcomes, error kinds); keep closed enums for values a receiver must
/// understand to act correctly.
macro_rules! open_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $(
                $(#[$variant_meta:meta])*
                $variant:ident => $wire:literal,
            )+
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $(
                $(#[$variant_meta])*
                $variant,
            )+
            /// A code this build does not know, kept as received. Encoding it is allowed only
            /// when it is a valid snake_case code.
            Other(String),
        }

        impl $name {
            /// Every code this build knows.
            pub const KNOWN: &'static [&'static str] = &[$($wire),+];

            /// The wire code.
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $wire,)+
                    Self::Other(code) => code,
                }
            }

            /// Maps a wire code, keeping unknown ones as [`Self::Other`].
            pub fn from_code(code: &str) -> Self {
                match code {
                    $($wire => Self::$variant,)+
                    other => Self::Other(other.to_owned()),
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let code = self.as_str();
                if !$crate::open_enum::is_valid_code(code) {
                    return Err(serde::ser::Error::custom(format!(
                        "invalid {} code {code:?}",
                        stringify!($name)
                    )));
                }
                serializer.serialize_str(code)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let code = <String as serde::Deserialize>::deserialize(deserializer)?;
                if !$crate::open_enum::is_valid_code(&code) {
                    let excerpt: String = code.chars().take($crate::open_enum::MAX_CODE_LEN).collect();
                    return Err(serde::de::Error::custom(format!(
                        "invalid {} code {excerpt:?}: codes are 1-64 lowercase letters, digits or '_', starting with a letter",
                        stringify!($name)
                    )));
                }
                Ok(Self::from_code(&code))
            }
        }
    };
}

pub(crate) use open_enum;

#[cfg(test)]
mod tests {
    use super::*;

    open_enum! {
        /// Test enum.
        pub enum Fruit {
            /// Apple.
            Apple => "apple",
            /// Blood orange.
            BloodOrange => "blood_orange",
        }
    }

    #[test]
    fn codes_have_one_shape() {
        for code in ["a", "blood_orange", "x1", &"a".repeat(MAX_CODE_LEN)] {
            assert!(is_valid_code(code), "{code:?}");
        }
        for code in [
            "",
            "Apple",
            "1a",
            "_a",
            "a-b",
            "a b",
            &"a".repeat(MAX_CODE_LEN + 1),
        ] {
            assert!(!is_valid_code(code), "{code:?}");
        }
    }

    #[test]
    fn unknown_codes_are_kept() {
        assert_eq!(Fruit::from_code("apple"), Fruit::Apple);
        assert_eq!(Fruit::from_code("kiwi"), Fruit::Other("kiwi".to_owned()));
        assert_eq!(Fruit::Other("kiwi".to_owned()).as_str(), "kiwi");
        assert_eq!(Fruit::KNOWN, &["apple", "blood_orange"]);
        assert_eq!(Fruit::BloodOrange.to_string(), "blood_orange");
    }
}
