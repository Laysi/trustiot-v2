//! The caller's request body: checked at the top level and sent on byte for byte, except the
//! value of `model`, which the gateway sets to the model it calls (decision 15: native protocol,
//! no re-encoding). Thinking blocks and their signatures, cache breakpoints, system messages,
//! key order, spacing and number formats all reach the provider as the caller wrote them.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt;

use bytes::Bytes;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;

/// Why a body cannot be forwarded. Messages never quote the body.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BodyError {
    #[error("the request body is not UTF-8")]
    NotUtf8,
    #[error("the request body is not valid JSON ({0})")]
    NotJson(String),
    #[error("the request body must be a JSON object")]
    NotObject,
    #[error("the request body has a top-level field more than once")]
    DuplicateField,
    #[error("`stream` must be true or false")]
    StreamNotBoolean,
}

/// A request body split around its `model` value.
#[derive(Debug)]
pub struct ModelSlot<'a> {
    /// Everything before the `model` value (with the opening `{` when the field is added).
    before: &'a str,
    /// Everything after it.
    after: &'a str,
    /// `None` when the body has a `model` field whose value is replaced; otherwise the field is
    /// added after the opening `{`, with a comma when other fields follow.
    insert: Option<bool>,
    /// The body's `stream` field (`false` when absent).
    pub streaming: bool,
}

impl<'a> ModelSlot<'a> {
    /// Checks `body`: UTF-8 JSON, an object without repeated top-level fields, and (when
    /// `read_stream`) a boolean `stream` field if there is one.
    pub fn parse(body: &'a [u8], read_stream: bool) -> Result<Self, BodyError> {
        let text = std::str::from_utf8(body).map_err(|_| BodyError::NotUtf8)?;
        let Fields(fields) =
            serde_json::from_str::<Fields<'a>>(text).map_err(|error| json_error(&error))?;
        let mut names = BTreeSet::<&str>::new();
        if !fields.iter().all(|(name, _)| names.insert(name.as_ref())) {
            return Err(BodyError::DuplicateField);
        }
        let streaming = match fields.iter().find(|(name, _)| name == "stream") {
            Some((_, value)) if read_stream => serde_json::from_str::<bool>(value.get())
                .map_err(|_| BodyError::StreamNotBoolean)?,
            _ => false,
        };
        let internal = || BodyError::NotJson("the field positions could not be read".into());
        if let Some((_, value)) = fields.iter().find(|(name, _)| name == "model") {
            let (start, end) = span(text, value.get()).ok_or_else(internal)?;
            return Ok(Self {
                before: text.get(..start).ok_or_else(internal)?,
                after: text.get(end..).ok_or_else(internal)?,
                insert: None,
                streaming,
            });
        }
        // Only whitespace precedes the opening brace of an object.
        let open = text.find('{').ok_or(BodyError::NotObject)? + 1;
        Ok(Self {
            before: text.get(..open).ok_or_else(internal)?,
            after: text.get(open..).ok_or_else(internal)?,
            insert: Some(!fields.is_empty()),
            streaming,
        })
    }

    /// The body with `model` set to `model`.
    pub fn with_model(&self, model: &str) -> Bytes {
        let value = serde_json::Value::String(model.to_owned()).to_string();
        let mut out = Vec::with_capacity(self.before.len() + value.len() + self.after.len() + 10);
        out.extend_from_slice(self.before.as_bytes());
        match self.insert {
            None => out.extend_from_slice(value.as_bytes()),
            Some(comma) => {
                out.extend_from_slice(b"\"model\":");
                out.extend_from_slice(value.as_bytes());
                if comma {
                    out.push(b',');
                }
            }
        }
        out.extend_from_slice(self.after.as_bytes());
        Bytes::from(out)
    }
}

/// Where `part`, a slice of `text`, starts and ends in it.
fn span(text: &str, part: &str) -> Option<(usize, usize)> {
    let start = part.as_ptr().addr().checked_sub(text.as_ptr().addr())?;
    let end = start.checked_add(part.len())?;
    let same = text
        .get(start..end)
        .is_some_and(|slice| std::ptr::eq(slice.as_ptr(), part.as_ptr()));
    same.then_some((start, end))
}

/// A serde_json error without its text, which can quote the body: the category and position.
fn json_error(error: &serde_json::Error) -> BodyError {
    match error.classify() {
        serde_json::error::Category::Data => BodyError::NotObject,
        serde_json::error::Category::Syntax
        | serde_json::error::Category::Eof
        | serde_json::error::Category::Io => {
            BodyError::NotJson(format!("line {} column {}", error.line(), error.column()))
        }
    }
}

/// The top-level fields of an object in order, each value as the raw text it was written as.
struct Fields<'a>(Vec<(Cow<'a, str>, &'a RawValue)>);

impl<'de> serde::Deserialize<'de> for Fields<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldsVisitor;

        impl<'de> Visitor<'de> for FieldsVisitor {
            type Value = Fields<'de>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut fields = Vec::new();
                while let Some(Name(name)) = map.next_key()? {
                    let value: &'de RawValue = map.next_value()?;
                    fields.push((name, value));
                }
                Ok(Fields(fields))
            }
        }

        deserializer.deserialize_map(FieldsVisitor)
    }
}

/// A field name, borrowed from the body unless it has escapes.
struct Name<'a>(Cow<'a, str>);

impl<'de> serde::Deserialize<'de> for Name<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NameVisitor;

        impl<'de> Visitor<'de> for NameVisitor {
            type Value = Name<'de>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a field name")
            }

            fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
                Ok(Name(Cow::Borrowed(value)))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(Name(Cow::Owned(value.to_owned())))
            }

            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(Name(Cow::Owned(value)))
            }
        }

        deserializer.deserialize_str(NameVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(body: &str, model: &str) -> String {
        let slot = ModelSlot::parse(body.as_bytes(), true).unwrap();
        String::from_utf8(slot.with_model(model).to_vec()).unwrap()
    }

    #[test]
    fn only_the_model_value_changes() {
        let body = "\n { \"max_tokens\" : 1.50e3 ,\"model\":\t\"caller\" , \"x\":{\"model\":\"inner\"},\"t\":\"\\u00e9\\/\"}\n";
        assert_eq!(
            set(body, "claude-opus-5-5"),
            body.replacen("\"caller\"", "\"claude-opus-5-5\"", 1)
        );
        // A model value of another type is replaced too.
        assert_eq!(
            set(r#"{"model":{"a":[1,2]},"b":2}"#, "m"),
            r#"{"model":"m","b":2}"#
        );
        // An escaped field name is still `model`; the name stays as written.
        assert_eq!(set(r#"{"model":"x"}"#, "m"), r#"{"model":"m"}"#);
        // Model IDs are encoded as JSON strings.
        assert_eq!(set(r#"{"model":"x"}"#, "a\"b"), r#"{"model":"a\"b"}"#);
    }

    #[test]
    fn a_missing_model_is_added_first() {
        assert_eq!(
            set(" {\"max_tokens\":1, \"messages\":[]}", "m"),
            " {\"model\":\"m\",\"max_tokens\":1, \"messages\":[]}"
        );
        assert_eq!(set("{ }", "m"), "{\"model\":\"m\" }");
    }

    #[test]
    fn stream_is_read_only_when_asked() {
        assert!(
            ModelSlot::parse(br#"{"stream":true}"#, true)
                .unwrap()
                .streaming
        );
        assert!(
            !ModelSlot::parse(br#"{"stream":false}"#, true)
                .unwrap()
                .streaming
        );
        assert!(!ModelSlot::parse(br#"{}"#, true).unwrap().streaming);
        assert_eq!(
            ModelSlot::parse(br#"{"stream":"yes"}"#, true).unwrap_err(),
            BodyError::StreamNotBoolean
        );
        assert!(
            !ModelSlot::parse(br#"{"stream":"yes"}"#, false)
                .unwrap()
                .streaming
        );
    }

    #[test]
    fn bodies_that_cannot_be_forwarded_are_refused_without_quoting_them() {
        let refused: [(&[u8], BodyError); 5] = [
            (b"\xff{}", BodyError::NotUtf8),
            (b"[1,2]", BodyError::NotObject),
            (b"\"secret text\"", BodyError::NotObject),
            (br#"{"model":"a","model":"b"}"#, BodyError::DuplicateField),
            (br#"{"model":"a","model":"b"}"#, BodyError::DuplicateField),
        ];
        for (body, expected) in refused {
            assert_eq!(ModelSlot::parse(body, true).unwrap_err(), expected);
        }
        let broken: [&[u8]; 3] = [b"{\"secret text\"", b"{} trailing", b""];
        for body in broken {
            let error = ModelSlot::parse(body, true).unwrap_err();
            assert!(matches!(error, BodyError::NotJson(_)), "{error:?}");
            assert!(!error.to_string().contains("secret"), "{error}");
        }
    }
}
