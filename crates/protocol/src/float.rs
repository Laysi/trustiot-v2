//! Lenient decoding of float fields.
//!
//! This crate encodes every float field as a CBOR double. A decoder for a fixed float field
//! (such as a data point's `scale`) also accepts a CBOR half or single and an integer of up to
//! 32 bits, which other CBOR encoders use for the same number (the shortest exact float, an
//! integer for whole numbers), so such fields never break interoperability or a later change of
//! width.
//! Finiteness and ranges are checked by the message's validation, which reports them precisely.

use std::fmt;

use serde::Deserializer;
use serde::de::{self, Visitor};

struct FloatVisitor;

impl Visitor<'_> for FloatVisitor {
    type Value = f64;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a float or a 32-bit integer")
    }

    fn visit_f32<E: de::Error>(self, value: f32) -> Result<f64, E> {
        Ok(f64::from(value))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<f64, E> {
        Ok(value)
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<f64, E> {
        i32::try_from(value)
            .map(f64::from)
            .map_err(|_| E::custom(format!("integer {value} is too large for a float field")))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<f64, E> {
        u32::try_from(value)
            .map(f64::from)
            .map_err(|_| E::custom(format!("integer {value} is too large for a float field")))
    }
}

struct OptionalFloatVisitor;

impl<'de> Visitor<'de> for OptionalFloatVisitor {
    type Value = Option<f64>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a float, a 32-bit integer or null")
    }

    fn visit_none<E: de::Error>(self) -> Result<Option<f64>, E> {
        Ok(None)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Option<f64>, E> {
        Ok(None)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Option<f64>, D::Error> {
        deserializer.deserialize_any(FloatVisitor).map(Some)
    }

    fn visit_f32<E: de::Error>(self, value: f32) -> Result<Option<f64>, E> {
        FloatVisitor.visit_f32(value).map(Some)
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Option<f64>, E> {
        FloatVisitor.visit_f64(value).map(Some)
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Option<f64>, E> {
        FloatVisitor.visit_i64(value).map(Some)
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Option<f64>, E> {
        FloatVisitor.visit_u64(value).map(Some)
    }
}

/// `#[serde(deserialize_with = "crate::float::any")]` for an `f64` field.
pub(crate) fn any<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    deserializer.deserialize_any(FloatVisitor)
}

/// `#[serde(default, deserialize_with = "crate::float::optional")]` for an `Option<f64>` field;
/// `null` is `None` like an absent field.
pub(crate) fn optional<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<f64>, D::Error> {
    deserializer.deserialize_any(OptionalFloatVisitor)
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Fields {
        #[serde(deserialize_with = "super::any")]
        required: f64,
        #[serde(default, deserialize_with = "super::optional")]
        optional: Option<f64>,
    }

    fn decode(bytes: &[u8]) -> Result<Fields, crate::DecodeError> {
        crate::codec::from_slice(bytes)
    }

    #[test]
    fn every_float_width_and_small_integers_are_accepted() {
        // {"required": <x>} with x as double 1.5, single 1.5, integer 2 and integer -3.
        let head = [0xa1, 0x68, b'r', b'e', b'q', b'u', b'i', b'r', b'e', b'd'];
        for (item, expected) in [
            (&[0xfb, 0x3f, 0xf8, 0, 0, 0, 0, 0, 0][..], 1.5),
            (&[0xfa, 0x3f, 0xc0, 0, 0][..], 1.5),
            (&[0xf9, 0x3e, 0x00][..], 1.5),
            (&[0x02][..], 2.0),
            (&[0x22][..], -3.0),
        ] {
            let bytes = [&head[..], item].concat();
            let fields = decode(&bytes).unwrap();
            assert_eq!(fields.required.to_bits(), f64::to_bits(expected));
            assert_eq!(fields.optional, None);
        }
        // An integer beyond 32 bits is refused rather than rounded.
        let bytes = [&head[..], &[0x1b, 0, 0x20, 0, 0, 0, 0, 0, 1][..]].concat();
        assert!(decode(&bytes).is_err());
        // Text is not a number.
        let bytes = [&head[..], &[0x61, b'1'][..]].concat();
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn optional_floats_accept_null_and_values() {
        let encoded = crate::codec::to_vec(&Fields {
            required: 0.25,
            optional: Some(-40.0),
        })
        .unwrap();
        assert_eq!(
            decode(&encoded).unwrap(),
            Fields {
                required: 0.25,
                optional: Some(-40.0)
            }
        );
        // {"required": 0, "optional": null}
        let mut bytes = vec![0xa2, 0x68];
        bytes.extend_from_slice(b"required");
        bytes.push(0x00);
        bytes.push(0x68);
        bytes.extend_from_slice(b"optional");
        bytes.push(0xf6);
        assert_eq!(
            decode(&bytes).unwrap(),
            Fields {
                required: 0.0,
                optional: None
            }
        );
    }
}
