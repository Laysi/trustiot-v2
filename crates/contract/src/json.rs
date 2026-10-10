//! JSON failures reduced to what is safe to log.
//!
//! serde's messages quote values from the document (for example `invalid type: string "…"`),
//! and logs must not contain personal data (M2 in `docs/requirements/rebuild-checklist.md`,
//! Laysi/trustiot#110). [`JsonError`] keeps the category, the position, and the name of a
//! missing field, which comes from our own types; it drops everything else.

use std::fmt;

use serde_json::error::Category;

/// A JSON serialization or deserialization failure, without any value from the document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonError {
    category: &'static str,
    missing_field: Option<String>,
    line: usize,
    column: usize,
}

impl JsonError {
    /// What kind of failure: `syntax error`, `invalid data`, `unexpected end of input`, or
    /// `I/O error`.
    #[must_use]
    pub fn category(&self) -> &'static str {
        self.category
    }

    /// The required field that was absent, when that was the failure.
    #[must_use]
    pub fn missing_field(&self) -> Option<&str> {
        self.missing_field.as_deref()
    }

    /// 1-based line of the failure in the input text; 0 when the input was not text (for
    /// example `serde_json::from_value`).
    #[must_use]
    pub fn line(&self) -> usize {
        self.line
    }

    /// 1-based column of the failure in the input text; 0 when the input was not text.
    #[must_use]
    pub fn column(&self) -> usize {
        self.column
    }
}

impl From<&serde_json::Error> for JsonError {
    fn from(error: &serde_json::Error) -> Self {
        let category = match error.classify() {
            Category::Io => "I/O error",
            Category::Syntax => "syntax error",
            Category::Data => "invalid data",
            Category::Eof => "unexpected end of input",
        };
        // serde formats this one as "missing field `name`"; the name is a field of our type.
        let message = error.to_string();
        let missing_field = message
            .strip_prefix("missing field `")
            .and_then(|rest| rest.split_once('`'))
            .map(|(field, _)| field.to_owned());
        Self {
            category,
            missing_field,
            line: error.line(),
            column: error.column(),
        }
    }
}

impl From<serde_json::Error> for JsonError {
    fn from(error: serde_json::Error) -> Self {
        Self::from(&error)
    }
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.category)?;
        if let Some(field) = &self.missing_field {
            write!(f, ": missing field `{field}`")?;
        }
        if self.line > 0 {
            write!(f, " at line {} column {}", self.line, self.column)?;
        }
        Ok(())
    }
}

impl std::error::Error for JsonError {}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize)]
    #[expect(dead_code, reason = "only deserialized to provoke errors")]
    struct Member {
        email: String,
        age: u32,
    }

    #[test]
    fn values_from_the_document_are_dropped() {
        let input = r#"{"email": "alice@example.com", "age": "alice@example.com"}"#;
        let error = serde_json::from_str::<Member>(input).expect_err("age is not a number");
        assert!(
            error.to_string().contains("alice@example.com"),
            "serde quotes the value"
        );
        let redacted = JsonError::from(&error);
        assert_eq!(redacted.category(), "invalid data");
        assert_eq!(redacted.missing_field(), None);
        assert_eq!(
            redacted.to_string(),
            format!("invalid data at line 1 column {}", error.column())
        );
    }

    #[test]
    fn missing_field_names_are_kept() {
        let error =
            serde_json::from_value::<Member>(serde_json::json!({"email": "bob@example.com"}))
                .expect_err("age is missing");
        let redacted = JsonError::from(error);
        assert_eq!(redacted.missing_field(), Some("age"));
        assert_eq!(redacted.line(), 0);
        assert_eq!(redacted.to_string(), "invalid data: missing field `age`");
    }

    #[test]
    fn syntax_errors_keep_their_position() {
        let error = serde_json::from_str::<Member>(r#"{"email": nope}"#).expect_err("not JSON");
        let redacted = JsonError::from(&error);
        assert_eq!(redacted.category(), "syntax error");
        assert!(!redacted.to_string().contains("nope"));
        assert_eq!(
            (redacted.line(), redacted.column()),
            (error.line(), error.column())
        );
    }
}
