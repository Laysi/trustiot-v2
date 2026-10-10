//! A serde deserializer over merged TOML values that accepts environment-variable strings for
//! numbers, booleans, and lists, tracks the dotted key path of every value, and never puts a
//! configuration value into an error message (a misplaced secret must not reach the logs).

use std::fmt::{self, Display};

use serde::de::{
    self, DeserializeSeed, EnumAccess, Expected, IntoDeserializer, MapAccess, SeqAccess,
    Unexpected, VariantAccess, Visitor,
};
use toml::Value;

use super::PLATFORM_SECTION;

/// A deserialization failure, located by dotted key path once it leaves the value it came from.
#[derive(Debug)]
pub(crate) struct DeError {
    pub(crate) path: Option<String>,
    pub(crate) kind: DeErrorKind,
}

#[derive(Debug)]
pub(crate) enum DeErrorKind {
    /// A required field is absent; the path names the field itself.
    Missing,
    /// Any other failure, described without the offending value.
    Message(String),
}

impl DeError {
    fn message(message: impl Into<String>) -> Self {
        Self {
            path: None,
            kind: DeErrorKind::Message(message.into()),
        }
    }

    /// Keeps the innermost location: the first value that sees an unlocated error locates it.
    fn at(mut self, path: &str) -> Self {
        if self.path.is_none() {
            self.path = Some(path.to_owned());
        }
        self
    }
}

impl Display for DeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            DeErrorKind::Missing => f.write_str("is required"),
            DeErrorKind::Message(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for DeError {}

/// Describes what kind of value was found, without the value.
fn kind_of(unexpected: &Unexpected<'_>) -> &'static str {
    match unexpected {
        Unexpected::Bool(_) => "a boolean",
        Unexpected::Unsigned(_) | Unexpected::Signed(_) => "an integer",
        Unexpected::Float(_) => "a floating-point number",
        Unexpected::Char(_) => "a character",
        Unexpected::Str(_) => "a string",
        Unexpected::Bytes(_) => "bytes",
        Unexpected::Unit => "an empty value",
        Unexpected::Option => "an optional value",
        Unexpected::NewtypeStruct => "a newtype struct",
        Unexpected::Seq => "a list",
        Unexpected::Map => "a table",
        Unexpected::Enum
        | Unexpected::UnitVariant
        | Unexpected::NewtypeVariant
        | Unexpected::TupleVariant
        | Unexpected::StructVariant => "an enum variant",
        Unexpected::Other(_) => "an unsupported value",
    }
}

fn one_of(names: &[&str]) -> String {
    if names.is_empty() {
        return "nothing (this table takes no keys)".to_owned();
    }
    names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl de::Error for DeError {
    fn custom<T: Display>(message: T) -> Self {
        Self::message(message.to_string())
    }

    fn invalid_type(unexpected: Unexpected<'_>, expected: &dyn Expected) -> Self {
        Self::message(format!(
            "expected {expected}, found {}",
            kind_of(&unexpected)
        ))
    }

    fn invalid_value(unexpected: Unexpected<'_>, expected: &dyn Expected) -> Self {
        Self::message(format!(
            "found {} that is not allowed here, expected {expected}",
            kind_of(&unexpected)
        ))
    }

    fn unknown_variant(_variant: &str, expected: &'static [&'static str]) -> Self {
        Self::message(format!(
            "unknown value, expected one of {}",
            one_of(expected)
        ))
    }

    fn unknown_field(_field: &str, expected: &'static [&'static str]) -> Self {
        Self::message(format!("unknown key, expected one of {}", one_of(expected)))
    }

    fn missing_field(field: &'static str) -> Self {
        Self {
            // Relative to the table that lacks it; `locate` prefixes the table's path.
            path: Some(field.to_owned()),
            kind: DeErrorKind::Missing,
        }
    }
}

fn child(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

/// Adds the location of the value being deserialized to an error raised inside it. A missing
/// field is reported relative to the table that lacks it, so it becomes `path.field` once.
fn locate(error: DeError, path: &str) -> DeError {
    match error {
        DeError {
            kind: DeErrorKind::Missing,
            path: Some(field),
        } if !path.is_empty() && !field.contains('.') => DeError {
            path: Some(child(path, &field)),
            kind: DeErrorKind::Missing,
        },
        other => other.at(path),
    }
}

/// Deserializes one value; `path` is its dotted key path (empty for the root table).
pub(crate) struct ValueDe {
    value: Value,
    path: String,
}

impl ValueDe {
    pub(crate) fn new(value: Value, path: String) -> Self {
        Self { value, path }
    }
}

macro_rules! parse_from_string {
    ($($method:ident => $visit:ident as $ty:ty, $what:literal;)*) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
                match self.value {
                    Value::String(text) => {
                        let path = self.path;
                        match text.trim().parse::<$ty>() {
                            Ok(parsed) => visitor.$visit(parsed).map_err(|e| locate(e, &path)),
                            Err(_) => Err(DeError::message(concat!(
                                "expected ", $what, ", found a string that is not one"
                            ))
                            .at(&path)),
                        }
                    }
                    _ => self.deserialize_any(visitor),
                }
            }
        )*
    };
}

impl<'de> de::Deserializer<'de> for ValueDe {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        let path = self.path;
        let result = match self.value {
            Value::String(text) => visitor.visit_string(text),
            Value::Integer(number) => visitor.visit_i64(number),
            Value::Float(number) => visitor.visit_f64(number),
            Value::Boolean(flag) => visitor.visit_bool(flag),
            Value::Datetime(datetime) => visitor.visit_string(datetime.to_string()),
            Value::Array(items) => visitor.visit_seq(SeqDe::new(items, path.clone())),
            Value::Table(table) => visitor.visit_map(MapDe::new(table, path.clone())),
        };
        result.map_err(|error| locate(error, &path))
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.value {
            Value::String(text) => {
                let path = self.path;
                let flag = match text.trim().to_ascii_lowercase().as_str() {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(DeError::message(
                            "expected `true` or `false`, found another string",
                        )
                        .at(&path));
                    }
                };
                visitor.visit_bool(flag).map_err(|e| locate(e, &path))
            }
            _ => self.deserialize_any(visitor),
        }
    }

    parse_from_string! {
        deserialize_i8 => visit_i64 as i64, "an integer";
        deserialize_i16 => visit_i64 as i64, "an integer";
        deserialize_i32 => visit_i64 as i64, "an integer";
        deserialize_i64 => visit_i64 as i64, "an integer";
        deserialize_i128 => visit_i128 as i128, "an integer";
        deserialize_u8 => visit_u64 as u64, "a non-negative integer";
        deserialize_u16 => visit_u64 as u64, "a non-negative integer";
        deserialize_u32 => visit_u64 as u64, "a non-negative integer";
        deserialize_u64 => visit_u64 as u64, "a non-negative integer";
        deserialize_u128 => visit_u128 as u128, "a non-negative integer";
        deserialize_f32 => visit_f64 as f64, "a number";
        deserialize_f64 => visit_f64 as f64, "a number";
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        // An absent key never reaches here (serde treats it as `None`); a present one is `Some`.
        let path = self.path.clone();
        visitor.visit_some(self).map_err(|e| locate(e, &path))
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.value {
            // Environment variables carry lists as comma-separated strings.
            Value::String(text) => {
                let items = text
                    .split(',')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(|item| Value::String(item.to_owned()))
                    .collect();
                let path = self.path;
                visitor
                    .visit_seq(SeqDe::new(items, path.clone()))
                    .map_err(|e| locate(e, &path))
            }
            _ => self.deserialize_any(visitor),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match &self.value {
            Value::Table(table) if table.is_empty() => visitor.visit_unit(),
            _ => self.deserialize_any(visitor),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        let path = self.path.clone();
        visitor
            .visit_newtype_struct(self)
            .map_err(|e| locate(e, &path))
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        let path = self.path;
        let result = match self.value {
            Value::String(text) => visitor.visit_enum(text.into_deserializer()),
            Value::Table(table) if table.len() == 1 => match table.into_iter().next() {
                Some((variant, value)) => {
                    let value_path = child(&path, &variant);
                    visitor.visit_enum(TableEnum {
                        variant,
                        value: ValueDe::new(value, value_path),
                    })
                }
                None => Err(DeError::message("expected a table with a single key")),
            },
            _ => Err(DeError::message(
                "expected a string or a table with a single key",
            )),
        };
        result.map_err(|e| locate(e, &path))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        mut self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        // Every service gets every platform setting; a configuration type without a `platform`
        // field never sees them, so it can still reject unknown keys.
        if self.path.is_empty()
            && !fields.contains(&PLATFORM_SECTION)
            && let Value::Table(table) = &mut self.value
        {
            table.remove(PLATFORM_SECTION);
        }
        self.deserialize_any(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_unit()
    }

    serde::forward_to_deserialize_any! {
        char str string bytes byte_buf unit_struct tuple tuple_struct map identifier
    }
}

struct SeqDe {
    items: std::iter::Enumerate<std::vec::IntoIter<Value>>,
    path: String,
}

impl SeqDe {
    fn new(items: Vec<Value>, path: String) -> Self {
        Self {
            items: items.into_iter().enumerate(),
            path,
        }
    }
}

impl<'de> SeqAccess<'de> for SeqDe {
    type Error = DeError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, DeError> {
        match self.items.next() {
            Some((index, value)) => {
                let path = format!("{}[{index}]", self.path);
                seed.deserialize(ValueDe::new(value, path)).map(Some)
            }
            None => Ok(None),
        }
    }
}

struct MapDe {
    entries: toml::map::IntoIter<String, Value>,
    pending: Option<(Value, String)>,
    path: String,
}

impl MapDe {
    fn new(table: toml::Table, path: String) -> Self {
        Self {
            entries: table.into_iter(),
            pending: None,
            path,
        }
    }
}

impl<'de> MapAccess<'de> for MapDe {
    type Error = DeError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, DeError> {
        let Some((key, value)) = self.entries.next() else {
            return Ok(None);
        };
        let path = child(&self.path, &key);
        let deserializer: de::value::StringDeserializer<DeError> = key.into_deserializer();
        let parsed = seed.deserialize(deserializer).map_err(|e| e.at(&path))?;
        self.pending = Some((value, path));
        Ok(Some(parsed))
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, DeError> {
        let Some((value, path)) = self.pending.take() else {
            return Err(DeError::message("a value was requested before its key"));
        };
        seed.deserialize(ValueDe::new(value, path))
    }
}

struct TableEnum {
    variant: String,
    value: ValueDe,
}

impl<'de> EnumAccess<'de> for TableEnum {
    type Error = DeError;
    type Variant = ValueDe;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, ValueDe), DeError> {
        let deserializer: de::value::StringDeserializer<DeError> = self.variant.into_deserializer();
        let variant = seed.deserialize(deserializer)?;
        Ok((variant, self.value))
    }
}

impl<'de> VariantAccess<'de> for ValueDe {
    type Error = DeError;

    fn unit_variant(self) -> Result<(), DeError> {
        de::Deserialize::deserialize(self)
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, DeError> {
        seed.deserialize(self)
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, DeError> {
        de::Deserializer::deserialize_seq(self, visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        de::Deserializer::deserialize_map(self, visitor)
    }
}
