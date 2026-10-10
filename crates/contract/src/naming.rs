//! The dotted-name grammar shared by message keys and event types.

/// Longest dotted name accepted, in bytes.
pub(crate) const MAX_DOTTED_NAME_LEN: usize = 128;

/// Regular expression equivalent of [`is_dotted_name`], for OpenAPI schemas and documentation.
pub(crate) const DOTTED_NAME_PATTERN: &str = r"^[a-z0-9_]+(\.[a-z0-9_]+)+$";

/// Whether `name` is two or more non-empty segments of `a-z`, `0-9` and `_`, separated by dots,
/// and at most [`MAX_DOTTED_NAME_LEN`] bytes long. A `const fn`, so constants are checked at
/// compile time.
pub(crate) const fn is_dotted_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_DOTTED_NAME_LEN {
        return false;
    }
    let mut segments = 1;
    let mut segment_len = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'.' => {
                if segment_len == 0 {
                    return false;
                }
                segments += 1;
                segment_len = 0;
            }
            b'a'..=b'z' | b'0'..=b'9' | b'_' => segment_len += 1,
            _ => return false,
        }
        index += 1;
    }
    segment_len > 0 && segments >= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_dotted_lowercase_names() {
        for name in [
            "error.not_found",
            "device_model.name_invalid",
            "a.b.c",
            "identity.tenant_created",
            "x1.y2",
        ] {
            assert!(is_dotted_name(name), "{name}");
        }
        let longest = format!("a.{}", "b".repeat(MAX_DOTTED_NAME_LEN - 2));
        assert!(is_dotted_name(&longest));
    }

    #[test]
    fn rejects_malformed_names() {
        let too_long = format!("a.{}", "b".repeat(MAX_DOTTED_NAME_LEN - 1));
        for name in [
            "",
            "error",
            ".error",
            "error.",
            "error..not_found",
            "Error.not_found",
            "error.not-found",
            "error.not found",
            "錯誤.not_found",
            too_long.as_str(),
        ] {
            assert!(!is_dotted_name(name), "{name:?}");
        }
    }
}
