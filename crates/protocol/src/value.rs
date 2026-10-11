//! Telemetry values.

use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Longest text value, in bytes. A Modbus read returns at most 125 registers (250 bytes); the
/// margin covers lossy UTF-8 conversion of such a string.
pub const MAX_TEXT_BYTES: usize = 1024;

/// One reading's value after decoding and scaling.
///
/// The CBOR item's own type carries the value type, so a sample costs no extra type tag: a
/// boolean is a CBOR simple value, an integer a CBOR integer, a float a CBOR float, and text a
/// CBOR text string. This crate sends floats as single precision when that is exact and as
/// double otherwise; it also accepts half precision, which other encoders use for short
/// values. Floats must be finite: a device that reports NaN or infinity has not
/// produced a measurement, and the agent counts it as a read failure
/// ([`crate::health::FailureKind::InvalidValue`]) instead of sending it.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// A coil, discrete input, or register flag.
    Bool(bool),
    /// An integer register value without scaling. Values outside `i64` cannot be sent.
    Int(i64),
    /// A float register value, or any value after scale and offset (rebuild checklist F5).
    Float(f64),
    /// A string register value, at most [`MAX_TEXT_BYTES`] bytes and without U+0000.
    Text(String),
}

/// Why a [`Value`] cannot be sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueError {
    /// The float is NaN or infinite.
    NonFiniteFloat,
    /// The text is longer than [`MAX_TEXT_BYTES`].
    TextTooLong {
        /// The text's length in bytes.
        len: usize,
    },
    /// The text contains U+0000. The agent strips the NUL padding of a Modbus string register
    /// before sending it; a NUL that remains is not a measurement, like a NaN float.
    NulInText,
}

impl fmt::Display for ValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFiniteFloat => f.write_str("float value is NaN or infinite"),
            Self::TextTooLong { len } => write!(
                f,
                "text value is {len} bytes; the limit is {MAX_TEXT_BYTES} bytes"
            ),
            Self::NulInText => f.write_str("text value contains U+0000"),
        }
    }
}

impl std::error::Error for ValueError {}

impl Value {
    /// Checks the invariants the variants cannot express: finite floats, and bounded text
    /// without U+0000.
    pub fn validate(&self) -> Result<(), ValueError> {
        match self {
            Self::Bool(_) | Self::Int(_) => Ok(()),
            Self::Float(value) if value.is_finite() => Ok(()),
            Self::Float(_) => Err(ValueError::NonFiniteFloat),
            Self::Text(text) if text.len() > MAX_TEXT_BYTES => {
                Err(ValueError::TextTooLong { len: text.len() })
            }
            Self::Text(text) if crate::codec::has_nul(text) => Err(ValueError::NulInText),
            Self::Text(_) => Ok(()),
        }
    }
}

/// `value` as `f32` when that loses nothing, so floats such as Modbus `f32` registers take 5
/// bytes instead of 9.
fn exact_f32(value: f64) -> Option<f32> {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the result is used only when converting back reproduces the same bits"
    )]
    let narrow = value as f32;
    (f64::from(narrow).to_bits() == value.to_bits()).then_some(narrow)
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        match self {
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Int(value) => serializer.serialize_i64(*value),
            Self::Float(value) => match exact_f32(*value) {
                Some(narrow) => serializer.serialize_f32(narrow),
                None => serializer.serialize_f64(*value),
            },
            Self::Text(text) => serializer.serialize_str(text),
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ValueVisitor)
    }
}

struct ValueVisitor;

impl ValueVisitor {
    fn checked<E: de::Error>(value: Value) -> Result<Value, E> {
        value.validate().map_err(E::custom)?;
        Ok(value)
    }
}

impl Visitor<'_> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a boolean, a 64-bit signed integer, a finite float, or text")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Int(value))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        i64::try_from(value).map(Value::Int).map_err(|_| {
            E::custom(format!(
                "integer {value} does not fit a 64-bit signed value"
            ))
        })
    }

    fn visit_f32<E: de::Error>(self, value: f32) -> Result<Value, E> {
        Self::checked(Value::Float(f64::from(value)))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Self::checked(Value::Float(value))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Self::checked(Value::Text(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        Self::checked(Value::Text(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_narrow_only_when_exact() {
        assert_eq!(exact_f32(230.5), Some(230.5));
        assert_eq!(exact_f32(-0.0).map(f32::to_bits), Some((-0.0f32).to_bits()));
        assert_eq!(exact_f32(230.47), None);
        assert_eq!(exact_f32(1e300), None);
        assert_eq!(exact_f32(f64::from(f32::MAX)), Some(f32::MAX));
    }

    #[test]
    fn invalid_values_are_reported() {
        assert_eq!(
            Value::Float(f64::NAN).validate(),
            Err(ValueError::NonFiniteFloat)
        );
        assert_eq!(
            Value::Float(f64::NEG_INFINITY).validate(),
            Err(ValueError::NonFiniteFloat)
        );
        assert_eq!(
            Value::Text("x".repeat(MAX_TEXT_BYTES + 1)).validate(),
            Err(ValueError::TextTooLong {
                len: MAX_TEXT_BYTES + 1
            })
        );
        assert_eq!(Value::Text("x".repeat(MAX_TEXT_BYTES)).validate(), Ok(()));
        assert_eq!(
            Value::Text("AB\0".to_owned()).validate(),
            Err(ValueError::NulInText)
        );
        // Other control characters are left to whoever renders the text.
        assert_eq!(Value::Text("a\tb\nc\u{1b}".to_owned()).validate(), Ok(()));
        assert_eq!(Value::Int(i64::MIN).validate(), Ok(()));
    }
}
