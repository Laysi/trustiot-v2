//! The bodies of the three write entries (create, update, import) and their rules (rebuild
//! checklist C1 and F5). Every entry runs the same checks, in two steps:
//!
//! 1. Each field on its own, read by hand from the JSON so every problem becomes a field error
//!    of the M1 format with a JSON Pointer and a message key: unknown fields, the model name,
//!    each data point's key, function code, address, type, byte order, length, scale, offset
//!    and description.
//! 2. When every field passed, the rules across fields and points, which are crates/protocol's
//!    (`validate_model_points`, the same function the gateway's configuration runs): unique
//!    keys, a type that fits the function code, a length for strings only, addresses within
//!    65535 that do not overlap, finite non-zero scaling of numeric points only.
//!
//! A body that is not a JSON object is a 400 `invalid_request`; every rule broken is a 422
//! `validation_failed` listing up to [`MAX_FIELD_ERRORS`] field errors. Nothing is written
//! unless both steps pass.

use contract::{ErrorBody, ErrorCode, FieldError, MessageKey};
use protocol::config::{
    ByteOrder, DataPoint, DataType, FunctionCode, MAX_STRING_REGISTERS, validate_model_points,
};
use protocol::{Issue, IssueCode, MAX_ID_LEN, MAX_MODEL_NAME_LEN, ModelName, PointKey};
use serde::Serialize;
use serde_json::{Map, Value};
use unicode_properties::GeneralCategory;
use unicode_properties::general_category::UnicodeGeneralCategory;

/// Most data points one model holds.
pub const MAX_POINTS: usize = 1000;

/// Longest data point description, in characters (Unicode scalar values, as Postgres'
/// `char_length` counts them).
pub const MAX_DESCRIPTION_CHARS: usize = 512;

/// Most field errors one response lists; `details.omitted_field_errors` counts the rest.
pub const MAX_FIELD_ERRORS: usize = 100;

/// The `format` of an exported model.
pub const DOCUMENT_FORMAT: &str = "trustiot.device_model";

/// The `format_version` of an exported model, the only one an import accepts.
pub const DOCUMENT_FORMAT_VERSION: u64 = 1;

/// The body is not a JSON object (400).
pub const BODY_INVALID: MessageKey = MessageKey::new("device_model.body_invalid");
/// A field the entry does not know.
pub const UNKNOWN_FIELD: MessageKey = MessageKey::new("device_model.unknown_field");
/// A required field is missing or `null`.
pub const FIELD_REQUIRED: MessageKey = MessageKey::new("device_model.field_required");
/// The name breaks the naming rule; `details.max` is 40.
pub const NAME_INVALID: MessageKey = MessageKey::new("device_model.name_invalid");
/// An update names the model differently: the name never changes (409 is for conflicts with
/// other writers; this is the request's own mistake, so 422).
pub const NAME_IMMUTABLE: MessageKey = MessageKey::new("device_model.name_immutable");
/// An update's `revision` is not a positive integer.
pub const REVISION_INVALID: MessageKey = MessageKey::new("device_model.revision_invalid");
/// `points` is not an array.
pub const POINTS_INVALID: MessageKey = MessageKey::new("device_model.points_invalid");
/// More than [`MAX_POINTS`] points; `details.max`.
pub const TOO_MANY_POINTS: MessageKey = MessageKey::new("device_model.too_many_points");
/// A point is not an object, or breaks a rule without a key of its own.
pub const POINT_INVALID: MessageKey = MessageKey::new("device_model.point_invalid");
/// A point key breaks the identifier rule; `details.max` is 64.
pub const POINT_KEY_INVALID: MessageKey = MessageKey::new("device_model.point_key_invalid");
/// Two points of the model have the same key (at the second one).
pub const POINT_KEY_DUPLICATE: MessageKey = MessageKey::new("device_model.point_key_duplicate");
/// The function code is not 1–4.
pub const FUNCTION_CODE_INVALID: MessageKey = MessageKey::new("device_model.function_code_invalid");
/// The address is not an integer 0–65535.
pub const ADDRESS_INVALID: MessageKey = MessageKey::new("device_model.address_invalid");
/// The point's registers run past address 65535.
pub const ADDRESS_OUT_OF_RANGE: MessageKey = MessageKey::new("device_model.address_out_of_range");
/// The point reads addresses an earlier point of the same function code reads.
pub const POINTS_OVERLAP: MessageKey = MessageKey::new("device_model.points_overlap");
/// The data type is not one of `details.allowed`.
pub const DATA_TYPE_INVALID: MessageKey = MessageKey::new("device_model.data_type_invalid");
/// A coil or discrete input (function code 1 or 2) with a type other than `bool`.
pub const FUNCTION_TYPE_MISMATCH: MessageKey =
    MessageKey::new("device_model.function_type_mismatch");
/// The byte order is not one of `details.allowed`.
pub const BYTE_ORDER_INVALID: MessageKey = MessageKey::new("device_model.byte_order_invalid");
/// A byte order the point cannot have: none for single bits, only `abcd` or `badc` for strings.
pub const BYTE_ORDER_UNEXPECTED: MessageKey = MessageKey::new("device_model.byte_order_unexpected");
/// The length is not an integer `details.min`–`details.max`.
pub const LENGTH_INVALID: MessageKey = MessageKey::new("device_model.length_invalid");
/// A `string` point without a length.
pub const LENGTH_REQUIRED: MessageKey = MessageKey::new("device_model.length_required");
/// A length on a point that is not a `string`.
pub const LENGTH_UNEXPECTED: MessageKey = MessageKey::new("device_model.length_unexpected");
/// Scale or offset is not a finite number, the scale is 0, or the point is `bool` or `string`.
pub const SCALING_INVALID: MessageKey = MessageKey::new("device_model.scaling_invalid");
/// The description is not text of 1 to `details.max` characters without control characters
/// (other than line feed and tab), format characters, or line and paragraph separators.
pub const DESCRIPTION_INVALID: MessageKey = MessageKey::new("device_model.description_invalid");
/// An import's `format` is not [`DOCUMENT_FORMAT`].
pub const FORMAT_UNSUPPORTED: MessageKey = MessageKey::new("device_model.format_unsupported");
/// An import's `format_version` is not one this release reads.
pub const FORMAT_VERSION_UNSUPPORTED: MessageKey =
    MessageKey::new("device_model.format_version_unsupported");

/// The fields of a create body.
const CREATE_FIELDS: &[&str] = &["name", "points"];
/// The fields of an update body.
const UPDATE_FIELDS: &[&str] = &["name", "revision", "points"];
/// The fields of an import body: an exported model.
const IMPORT_FIELDS: &[&str] = &["format", "format_version", "name", "points"];
/// The fields of a data point.
const POINT_FIELDS: &[&str] = &[
    "key",
    "function_code",
    "address",
    "data_type",
    "byte_order",
    "length",
    "scale",
    "offset",
    "description",
];

/// An unknown field's name is shown in its pointer only when it is this short and printable
/// ASCII; otherwise the error points at the object that holds it, so a response never echoes
/// an unbounded or invisible string.
const MAX_ECHOED_FIELD_LEN: usize = 64;

/// Every data type, in the order the API documents them. [`all_data_types_are_listed`] fails
/// when crates/protocol gains one that is missing here.
pub const DATA_TYPES: [DataType; 10] = [
    DataType::Bool,
    DataType::U16,
    DataType::I16,
    DataType::U32,
    DataType::I32,
    DataType::U64,
    DataType::I64,
    DataType::F32,
    DataType::F64,
    DataType::String,
];

/// Every byte order, `abcd` (the default) first.
pub const BYTE_ORDERS: [ByteOrder; 4] = [
    ByteOrder::Abcd,
    ByteOrder::Badc,
    ByteOrder::Cdab,
    ByteOrder::Dcba,
];

/// The JSON string crates/protocol encodes `value` as, for example `u16` or `cdab`: the names
/// are defined once, by protocol's serde attributes.
#[must_use]
pub fn wire_name<T: Serialize>(value: T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(name)) => name,
        // The unit variants of DataType and ByteOrder always serialize as strings.
        _ => String::new(),
    }
}

/// The value crates/protocol decodes from the JSON string `name`, if any.
#[must_use]
pub fn from_wire_name<T: serde::de::DeserializeOwned>(name: &str) -> Option<T> {
    serde_json::from_value(Value::String(name.to_owned())).ok()
}

/// One data point that passed every rule, with its description.
#[derive(Clone, Debug, PartialEq)]
pub struct Point {
    /// The point as the gateway reads it.
    pub point: DataPoint,
    /// What the value means, trimmed; never empty.
    pub description: Option<String>,
}

/// A create or import body that passed every rule.
#[derive(Clone, Debug, PartialEq)]
pub struct NewModel {
    /// The model's name.
    pub name: ModelName,
    /// Its data points, in order.
    pub points: Vec<Point>,
}

/// An update body that passed every rule.
#[derive(Clone, Debug, PartialEq)]
pub struct Replacement {
    /// The name the client believes the model has, if it sent one; it must be the stored name.
    pub name: Option<String>,
    /// The revision the client last read, if it sent one; the update is refused when the model
    /// changed since.
    pub revision: Option<u64>,
    /// The new data points, which replace all the old ones.
    pub points: Vec<Point>,
}

/// `POST /api/assets/v1/device-models`: `{"name", "points"}`.
///
/// # Errors
///
/// The response body: 400 when the body is not a JSON object, 422 with the field errors.
pub fn parse_create(body: &[u8]) -> Result<NewModel, ErrorBody> {
    let map = object(body)?;
    let mut errors = Errors::default();
    reject_unknown(&map, CREATE_FIELDS, "", &mut errors);
    let name = name(&map, &mut errors);
    let points = points(&map, &mut errors);
    errors.finish()?;
    match (name, points) {
        (Some(name), Some(points)) => Ok(NewModel { name, points }),
        _ => Err(Errors::default().into_body()),
    }
}

/// `POST /api/assets/v1/device-models/import`: an exported model, `{"format",
/// "format_version", "name", "points"}`, under the same rules as a create.
///
/// # Errors
///
/// As [`parse_create`]; also when the format or its version is not this release's.
pub fn parse_import(body: &[u8]) -> Result<NewModel, ErrorBody> {
    let map = object(body)?;
    let mut errors = Errors::default();
    reject_unknown(&map, IMPORT_FIELDS, "", &mut errors);
    if let Some(format) = required(&map, "format", "", &mut errors)
        && format.as_str() != Some(DOCUMENT_FORMAT)
    {
        errors.add(
            FieldError::new("/format", FORMAT_UNSUPPORTED)
                .with_detail("supported", DOCUMENT_FORMAT),
        );
    }
    if let Some(version) = required(&map, "format_version", "", &mut errors)
        && version.as_u64() != Some(DOCUMENT_FORMAT_VERSION)
    {
        errors.add(
            FieldError::new("/format_version", FORMAT_VERSION_UNSUPPORTED)
                .with_detail("supported", vec![DOCUMENT_FORMAT_VERSION]),
        );
    }
    let name = name(&map, &mut errors);
    let points = points(&map, &mut errors);
    errors.finish()?;
    match (name, points) {
        (Some(name), Some(points)) => Ok(NewModel { name, points }),
        _ => Err(Errors::default().into_body()),
    }
}

/// `PUT /api/assets/v1/device-models/{model_id}`: `{"points", "name"?, "revision"?}`.
///
/// # Errors
///
/// As [`parse_create`]. Whether `name` is the stored name and `revision` the current one is
/// checked against the database afterwards.
pub fn parse_update(body: &[u8]) -> Result<Replacement, ErrorBody> {
    let map = object(body)?;
    let mut errors = Errors::default();
    reject_unknown(&map, UPDATE_FIELDS, "", &mut errors);
    let name = match optional(&map, "name") {
        None => None,
        Some(Value::String(name)) => Some(name.clone()),
        Some(_) => {
            errors.add(name_invalid());
            None
        }
    };
    let revision = match optional(&map, "revision") {
        None => None,
        Some(value) => {
            let revision = value.as_u64().filter(|revision| *revision >= 1);
            if revision.is_none() {
                errors.add(FieldError::new("/revision", REVISION_INVALID));
            }
            revision
        }
    };
    let points = points(&map, &mut errors);
    errors.finish()?;
    match points {
        Some(points) => Ok(Replacement {
            name,
            revision,
            points,
        }),
        None => Err(Errors::default().into_body()),
    }
}

/// The body as a JSON object.
fn object(body: &[u8]) -> Result<Map<String, Value>, ErrorBody> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(ErrorBody::new(ErrorCode::InvalidRequest).with_message_key(BODY_INVALID)),
    }
}

/// The field errors found so far.
#[derive(Default)]
struct Errors {
    listed: Vec<FieldError>,
    omitted: usize,
}

impl Errors {
    fn add(&mut self, error: FieldError) {
        if self.listed.len() < MAX_FIELD_ERRORS {
            self.listed.push(error);
        } else {
            self.omitted += 1;
        }
    }

    /// How many errors were found, listed or not.
    fn count(&self) -> usize {
        self.listed.len() + self.omitted
    }

    fn finish(self) -> Result<(), ErrorBody> {
        if self.count() == 0 {
            Ok(())
        } else {
            Err(self.into_body())
        }
    }

    fn into_body(self) -> ErrorBody {
        let mut body = ErrorBody::new(ErrorCode::ValidationFailed);
        for error in self.listed {
            body = body.with_field_error(error);
        }
        if self.omitted > 0 {
            body = body.with_detail("omitted_field_errors", self.omitted);
        }
        body
    }
}

/// A field error for every field of `map` not in `allowed`; `at` is the pointer to `map`.
fn reject_unknown(map: &Map<String, Value>, allowed: &[&str], at: &str, errors: &mut Errors) {
    for field in map.keys() {
        if !allowed.contains(&field.as_str()) {
            let echo = field.len() <= MAX_ECHOED_FIELD_LEN
                && field.bytes().all(|byte| byte.is_ascii_graphic());
            let pointer = if echo {
                format!("{at}/{}", escape_pointer(field))
            } else {
                at.to_owned()
            };
            errors.add(FieldError::new(pointer, UNKNOWN_FIELD));
        }
    }
}

/// RFC 6901: `~` is written `~0` and `/` is written `~1` inside a pointer's segment.
fn escape_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

/// The field `name` of `map`, `None` when it is missing or `null`.
fn optional<'a>(map: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    map.get(name).filter(|value| !value.is_null())
}

/// The field `name` of `map`; a missing or `null` one is a field error under `at`.
fn required<'a>(
    map: &'a Map<String, Value>,
    name: &str,
    at: &str,
    errors: &mut Errors,
) -> Option<&'a Value> {
    let value = optional(map, name);
    if value.is_none() {
        errors.add(FieldError::new(format!("{at}/{name}"), FIELD_REQUIRED));
    }
    value
}

fn name_invalid() -> FieldError {
    FieldError::new("/name", NAME_INVALID).with_detail("max", MAX_MODEL_NAME_LEN)
}

fn name(map: &Map<String, Value>, errors: &mut Errors) -> Option<ModelName> {
    let value = required(map, "name", "", errors)?;
    let name = value.as_str().and_then(|text| ModelName::new(text).ok());
    if name.is_none() {
        errors.add(name_invalid());
    }
    name
}

/// The data points, when every one passed every rule.
fn points(map: &Map<String, Value>, errors: &mut Errors) -> Option<Vec<Point>> {
    let Value::Array(items) = required(map, "points", "", errors)? else {
        errors.add(FieldError::new("/points", POINTS_INVALID));
        return None;
    };
    if items.len() > MAX_POINTS {
        errors.add(FieldError::new("/points", TOO_MANY_POINTS).with_detail("max", MAX_POINTS));
        return None;
    }
    let mut points = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        if let Some(parsed) = point(index, item, errors) {
            points.push(parsed);
        }
    }
    if points.len() != items.len() {
        return None;
    }
    // Step 2: the rules across fields and points, crates/protocol's.
    let data_points: Vec<DataPoint> = points.iter().map(|point| point.point.clone()).collect();
    if let Err(invalid) = validate_model_points(&data_points) {
        for issue in invalid.issues() {
            errors.add(issue_error(issue));
        }
        return None;
    }
    Some(points)
}

/// One data point, when every field passed its rule.
fn point(index: usize, item: &Value, errors: &mut Errors) -> Option<Point> {
    let at = format!("/points/{index}");
    let Value::Object(map) = item else {
        errors.add(FieldError::new(at, POINT_INVALID));
        return None;
    };
    let before = errors.count();
    reject_unknown(map, POINT_FIELDS, &at, errors);
    let field = |name: &str| format!("{at}/{name}");

    let key = required(map, "key", &at, errors).and_then(|value| {
        let key = value.as_str().and_then(|text| PointKey::new(text).ok());
        if key.is_none() {
            errors.add(
                FieldError::new(field("key"), POINT_KEY_INVALID).with_detail("max", MAX_ID_LEN),
            );
        }
        key
    });
    let function_code = required(map, "function_code", &at, errors).and_then(|value| {
        let code = value
            .as_u64()
            .and_then(|number| u8::try_from(number).ok())
            .and_then(|number| FunctionCode::try_from(number).ok());
        if code.is_none() {
            errors.add(
                FieldError::new(field("function_code"), FUNCTION_CODE_INVALID)
                    .with_detail("min", 1)
                    .with_detail("max", 4),
            );
        }
        code
    });
    let address = required(map, "address", &at, errors).and_then(|value| {
        let address = value.as_u64().and_then(|number| u16::try_from(number).ok());
        if address.is_none() {
            errors.add(
                FieldError::new(field("address"), ADDRESS_INVALID)
                    .with_detail("min", 0)
                    .with_detail("max", u16::MAX),
            );
        }
        address
    });
    let data_type = required(map, "data_type", &at, errors).and_then(|value| {
        let data_type = value.as_str().and_then(from_wire_name::<DataType>);
        if data_type.is_none() {
            errors.add(
                FieldError::new(field("data_type"), DATA_TYPE_INVALID)
                    .with_detail("allowed", DATA_TYPES.map(wire_name).to_vec()),
            );
        }
        data_type
    });
    let byte_order = match optional(map, "byte_order") {
        None => Some(ByteOrder::default()),
        Some(value) => {
            let order = value.as_str().and_then(from_wire_name::<ByteOrder>);
            if order.is_none() {
                errors.add(
                    FieldError::new(field("byte_order"), BYTE_ORDER_INVALID)
                        .with_detail("allowed", BYTE_ORDERS.map(wire_name).to_vec()),
                );
            }
            order
        }
    };
    let length = match optional(map, "length") {
        None => Some(None),
        Some(value) => {
            let length = value
                .as_u64()
                .and_then(|number| u16::try_from(number).ok())
                .filter(|length| (1..=MAX_STRING_REGISTERS).contains(length));
            if length.is_none() {
                errors.add(
                    FieldError::new(field("length"), LENGTH_INVALID)
                        .with_detail("min", 1)
                        .with_detail("max", MAX_STRING_REGISTERS),
                );
            }
            length.map(Some)
        }
    };
    let mut number = |name: &str| match optional(map, name) {
        None => Some(None),
        Some(value) => {
            let number = value.as_f64().filter(|number| number.is_finite());
            if number.is_none() {
                errors.add(FieldError::new(field(name), SCALING_INVALID));
            }
            number.map(Some)
        }
    };
    let scale = number("scale");
    let offset = number("offset");
    let description = match optional(map, "description") {
        None => Some(None),
        Some(value) => {
            let description = value.as_str().and_then(parse_description);
            if description.is_none() {
                errors.add(
                    FieldError::new(field("description"), DESCRIPTION_INVALID)
                        .with_detail("max", MAX_DESCRIPTION_CHARS),
                );
            }
            description
        }
    };

    if errors.count() != before {
        return None;
    }
    Some(Point {
        point: DataPoint {
            key: key?,
            function_code: function_code?,
            address: address?,
            data_type: data_type?,
            byte_order: byte_order?,
            length: length?,
            scale: scale?,
            offset: offset?,
        },
        description: description?,
    })
}

/// A description: `Some(None)` when it is empty after trimming (no description), `None` when
/// it breaks the rule.
fn parse_description(text: &str) -> Option<Option<String>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Some(None);
    }
    let valid = trimmed.chars().count() <= MAX_DESCRIPTION_CHARS && !trimmed.chars().any(refused);
    valid.then(|| Some(trimmed.to_owned()))
}

/// Whether a description may not hold `c`: control characters other than line feed and tab,
/// format characters (zero-width characters, bidirectional overrides and isolates, the byte
/// order mark), and the line and paragraph separators. They make a description read
/// differently from what it holds, for a person and for the AI that reads it.
fn refused(c: char) -> bool {
    if c == '\n' || c == '\t' {
        return false;
    }
    matches!(
        c.general_category(),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
    )
}

/// The field error for one of crates/protocol's issues: its path, for example
/// `points[3].address`, as a JSON Pointer, `/points/3/address`.
fn issue_error(issue: &Issue) -> FieldError {
    let key = match issue.code {
        IssueCode::Duplicate => POINT_KEY_DUPLICATE,
        IssueCode::AddressOutOfRange => ADDRESS_OUT_OF_RANGE,
        IssueCode::OverlappingPoints => POINTS_OVERLAP,
        IssueCode::InvalidScaling => SCALING_INVALID,
        IssueCode::FunctionTypeMismatch => FUNCTION_TYPE_MISMATCH,
        IssueCode::MissingLength => LENGTH_REQUIRED,
        IssueCode::UnexpectedLength => LENGTH_UNEXPECTED,
        IssueCode::UnexpectedByteOrder => BYTE_ORDER_UNEXPECTED,
        IssueCode::OutOfRange => LENGTH_INVALID,
        _ => POINT_INVALID,
    };
    FieldError::new(pointer(&issue.path), key)
}

/// `points[3].address` as `/points/3/address`; the empty path as the empty pointer.
fn pointer(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let mut pointer = String::with_capacity(path.len() + 1);
    pointer.push('/');
    for c in path.chars() {
        match c {
            '[' | '.' => pointer.push('/'),
            ']' => {}
            other => pointer.push(other),
        }
    }
    pointer
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Fails to compile when crates/protocol gains a data type or byte order, until the lists
    /// above have it.
    #[test]
    fn all_data_types_are_listed() {
        for data_type in DATA_TYPES {
            match data_type {
                DataType::Bool
                | DataType::U16
                | DataType::I16
                | DataType::U32
                | DataType::I32
                | DataType::U64
                | DataType::I64
                | DataType::F32
                | DataType::F64
                | DataType::String => {}
            }
        }
        for order in BYTE_ORDERS {
            match order {
                ByteOrder::Abcd | ByteOrder::Badc | ByteOrder::Cdab | ByteOrder::Dcba => {}
            }
        }
        let names: Vec<String> = DATA_TYPES.map(wire_name).to_vec();
        assert_eq!(
            names,
            [
                "bool", "u16", "i16", "u32", "i32", "u64", "i64", "f32", "f64", "string"
            ]
        );
        assert_eq!(
            BYTE_ORDERS.map(wire_name).to_vec(),
            ["abcd", "badc", "cdab", "dcba"]
        );
        for data_type in DATA_TYPES {
            assert_eq!(
                from_wire_name::<DataType>(&wire_name(data_type)),
                Some(data_type)
            );
        }
        assert_eq!(from_wire_name::<DataType>("U16"), None);
        assert_eq!(from_wire_name::<ByteOrder>("ABCD"), None);
    }

    fn valid_points() -> Value {
        json!([
            {"key": "voltage", "function_code": 3, "address": 0, "data_type": "u16",
             "scale": 0.1, "description": "Phase A voltage, V"},
            {"key": "energy", "function_code": 4, "address": 0, "data_type": "u32",
             "byte_order": "cdab", "scale": 0.01, "offset": -40},
            {"key": "serial", "function_code": 3, "address": 10, "data_type": "string",
             "length": 8, "byte_order": "badc"},
            {"key": "running", "function_code": 1, "address": 0, "data_type": "bool"},
        ])
    }

    fn create(points: Value) -> Value {
        json!({"name": "power-meter", "points": points})
    }

    fn import(points: Value) -> Value {
        json!({"format": DOCUMENT_FORMAT, "format_version": 1, "name": "power-meter",
               "points": points})
    }

    fn update(points: Value) -> Value {
        json!({"points": points})
    }

    fn bytes(value: &Value) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }

    /// The (field, message key) pairs of a 422.
    fn field_errors(result: Result<impl std::fmt::Debug, ErrorBody>) -> Vec<(String, String)> {
        let body = result.expect_err("the body is refused");
        assert_eq!(body.code(), ErrorCode::ValidationFailed, "{body:?}");
        body.field_errors()
            .iter()
            .map(|error| (error.field.clone(), error.message_key.as_str().to_owned()))
            .collect()
    }

    /// Runs `check` on the field errors of the same points through all three entries.
    fn every_entry(points: &Value, check: impl Fn(&str, Vec<(String, String)>)) {
        check(
            "create",
            field_errors(parse_create(&bytes(&create(points.clone())))),
        );
        check(
            "import",
            field_errors(parse_import(&bytes(&import(points.clone())))),
        );
        check(
            "update",
            field_errors(parse_update(&bytes(&update(points.clone())))),
        );
    }

    #[test]
    fn a_valid_model_passes_every_entry() {
        let created = parse_create(&bytes(&create(valid_points()))).unwrap();
        assert_eq!(created.name.as_str(), "power-meter");
        assert_eq!(created.points.len(), 4);
        let voltage = &created.points[0];
        assert_eq!(voltage.point.scale, Some(0.1));
        assert_eq!(voltage.description.as_deref(), Some("Phase A voltage, V"));
        let energy = &created.points[1];
        assert_eq!(energy.point.byte_order, ByteOrder::Cdab);
        assert_eq!(energy.point.offset, Some(-40.0));
        assert_eq!(created.points[2].point.length, Some(8));
        assert_eq!(created.points[3].point.byte_order, ByteOrder::Abcd);

        assert_eq!(
            parse_import(&bytes(&import(valid_points()))).unwrap(),
            created
        );
        let replacement = parse_update(&bytes(&json!({
            "name": "power-meter", "revision": 3, "points": valid_points()
        })))
        .unwrap();
        assert_eq!(replacement.points, created.points);
        assert_eq!(replacement.name.as_deref(), Some("power-meter"));
        assert_eq!(replacement.revision, Some(3));

        // No points is a valid model; null optional fields are absent ones.
        assert!(
            parse_create(&bytes(&create(json!([]))))
                .unwrap()
                .points
                .is_empty()
        );
        let nulls = json!([{"key": "a", "function_code": 3, "address": 0, "data_type": "u16",
            "byte_order": null, "length": null, "scale": null, "offset": null,
            "description": null}]);
        assert_eq!(
            parse_create(&bytes(&create(nulls))).unwrap().points.len(),
            1
        );
        // A blank description is no description.
        let blank = json!([{"key": "a", "function_code": 3, "address": 0, "data_type": "u16",
            "description": "  \n "}]);
        assert_eq!(
            parse_create(&bytes(&create(blank))).unwrap().points[0].description,
            None
        );
    }

    #[test]
    fn a_body_that_is_not_an_object_is_a_bad_request() {
        for body in [&b"not json"[..], b"[]", b"\"x\"", b"null", b"{\"name\": "] {
            for result in [
                parse_create(body).map(|_| ()),
                parse_import(body).map(|_| ()),
                parse_update(body).map(|_| ()),
            ] {
                let error = result.expect_err("refused");
                assert_eq!(error.code(), ErrorCode::InvalidRequest);
                assert_eq!(error.message_key(), &BODY_INVALID);
                assert!(error.field_errors().is_empty());
            }
        }
    }

    #[test]
    fn names_follow_the_naming_rule() {
        for name in ["a", "power-meter", "pm3000", "a-1-b2", &"a".repeat(40)] {
            let body = json!({"name": name, "points": []});
            assert!(parse_create(&bytes(&body)).is_ok(), "{name}");
            let mut imported = import(json!([]));
            imported["name"] = json!(name);
            assert!(parse_import(&bytes(&imported)).is_ok(), "{name}");
        }
        for name in [
            json!(""),
            json!("Power-meter"),
            json!("1meter"),
            json!("-meter"),
            json!("meter-"),
            json!("power--meter"),
            json!("power_meter"),
            json!("power meter"),
            json!("電表"),
            json!("a".repeat(41)),
            json!(42),
            json!(["power-meter"]),
        ] {
            let body = json!({"name": name, "points": []});
            assert_eq!(
                field_errors(parse_create(&bytes(&body))),
                [("/name".to_owned(), NAME_INVALID.as_str().to_owned())],
                "{name}"
            );
            let mut imported = import(json!([]));
            imported["name"] = name.clone();
            assert_eq!(
                field_errors(parse_import(&bytes(&imported))),
                [("/name".to_owned(), NAME_INVALID.as_str().to_owned())],
                "{name}"
            );
        }
        let error = parse_create(&bytes(&json!({"name": "A", "points": []}))).unwrap_err();
        assert_eq!(error.field_errors()[0].details["max"], json!(40));
        assert_eq!(
            field_errors(parse_create(&bytes(&json!({"points": []})))),
            [("/name".to_owned(), FIELD_REQUIRED.as_str().to_owned())]
        );
        assert_eq!(
            field_errors(parse_update(&bytes(&json!({"name": 1, "points": []})))),
            [("/name".to_owned(), NAME_INVALID.as_str().to_owned())]
        );
    }

    /// Every per-field and cross-point rule, through every write entry: the same points give the
    /// same field errors whether they are created, imported or replaced.
    #[test]
    fn every_point_rule_is_enforced_on_every_entry() {
        let base = json!({"key": "p", "function_code": 3, "address": 0, "data_type": "u16"});
        let with = |changes: Value| {
            let mut point = base.clone();
            for (field, value) in changes.as_object().unwrap() {
                if value.is_null() {
                    point.as_object_mut().unwrap().remove(field);
                } else {
                    point[field] = value.clone();
                }
            }
            point
        };
        let cases: Vec<(Value, &str, MessageKey)> = vec![
            // Field rules.
            (
                json!([with(json!({"key": null}))]),
                "/points/0/key",
                FIELD_REQUIRED,
            ),
            (
                json!([with(json!({"key": ""}))]),
                "/points/0/key",
                POINT_KEY_INVALID,
            ),
            (
                json!([with(json!({"key": "-p"}))]),
                "/points/0/key",
                POINT_KEY_INVALID,
            ),
            (
                json!([with(json!({"key": "a/b"}))]),
                "/points/0/key",
                POINT_KEY_INVALID,
            ),
            (
                json!([with(json!({"key": "溫度"}))]),
                "/points/0/key",
                POINT_KEY_INVALID,
            ),
            (
                json!([with(json!({"key": "k".repeat(65)}))]),
                "/points/0/key",
                POINT_KEY_INVALID,
            ),
            (
                json!([with(json!({"key": 7}))]),
                "/points/0/key",
                POINT_KEY_INVALID,
            ),
            (
                json!([with(json!({"function_code": 0}))]),
                "/points/0/function_code",
                FUNCTION_CODE_INVALID,
            ),
            (
                json!([with(json!({"function_code": 5}))]),
                "/points/0/function_code",
                FUNCTION_CODE_INVALID,
            ),
            (
                json!([with(json!({"function_code": 6}))]),
                "/points/0/function_code",
                FUNCTION_CODE_INVALID,
            ),
            (
                json!([with(json!({"function_code": 16}))]),
                "/points/0/function_code",
                FUNCTION_CODE_INVALID,
            ),
            (
                json!([with(json!({"function_code": 3.5}))]),
                "/points/0/function_code",
                FUNCTION_CODE_INVALID,
            ),
            (
                json!([with(json!({"function_code": "3"}))]),
                "/points/0/function_code",
                FUNCTION_CODE_INVALID,
            ),
            (
                json!([with(json!({"function_code": null}))]),
                "/points/0/function_code",
                FIELD_REQUIRED,
            ),
            (
                json!([with(json!({"address": -1}))]),
                "/points/0/address",
                ADDRESS_INVALID,
            ),
            (
                json!([with(json!({"address": 65_536}))]),
                "/points/0/address",
                ADDRESS_INVALID,
            ),
            (
                json!([with(json!({"address": "0"}))]),
                "/points/0/address",
                ADDRESS_INVALID,
            ),
            (
                json!([with(json!({"address": null}))]),
                "/points/0/address",
                FIELD_REQUIRED,
            ),
            (
                json!([with(json!({"data_type": "float"}))]),
                "/points/0/data_type",
                DATA_TYPE_INVALID,
            ),
            (
                json!([with(json!({"data_type": "U16"}))]),
                "/points/0/data_type",
                DATA_TYPE_INVALID,
            ),
            (
                json!([with(json!({"data_type": null}))]),
                "/points/0/data_type",
                FIELD_REQUIRED,
            ),
            (
                json!([with(json!({"byte_order": "big"}))]),
                "/points/0/byte_order",
                BYTE_ORDER_INVALID,
            ),
            (
                json!([with(json!({"data_type": "string", "length": 0}))]),
                "/points/0/length",
                LENGTH_INVALID,
            ),
            (
                json!([with(json!({"data_type": "string", "length": 126}))]),
                "/points/0/length",
                LENGTH_INVALID,
            ),
            (
                json!([with(json!({"data_type": "string", "length": "8"}))]),
                "/points/0/length",
                LENGTH_INVALID,
            ),
            (
                json!([with(json!({"scale": "0.1"}))]),
                "/points/0/scale",
                SCALING_INVALID,
            ),
            (
                json!([with(json!({"offset": true}))]),
                "/points/0/offset",
                SCALING_INVALID,
            ),
            (
                json!([with(json!({"description": 5}))]),
                "/points/0/description",
                DESCRIPTION_INVALID,
            ),
            (
                json!([with(
                    json!({"description": "x".repeat(MAX_DESCRIPTION_CHARS + 1)})
                )]),
                "/points/0/description",
                DESCRIPTION_INVALID,
            ),
            (
                json!([with(json!({"description": "a\u{202E}b"}))]),
                "/points/0/description",
                DESCRIPTION_INVALID,
            ),
            (
                json!([with(json!({"description": "a\u{0}b"}))]),
                "/points/0/description",
                DESCRIPTION_INVALID,
            ),
            (
                json!([with(json!({"description": "a\u{2028}b"}))]),
                "/points/0/description",
                DESCRIPTION_INVALID,
            ),
            (
                json!([with(json!({"unit": "V"}))]),
                "/points/0/unit",
                UNKNOWN_FIELD,
            ),
            (json!(["p"]), "/points/0", POINT_INVALID),
            // Rules across fields and points (crates/protocol).
            (
                json!([base.clone(), with(json!({"address": 5}))]),
                "/points/1/key",
                POINT_KEY_DUPLICATE,
            ),
            (
                json!([
                    with(json!({"data_type": "u32"})),
                    with(json!({"key": "q", "address": 1}))
                ]),
                "/points/1/address",
                POINTS_OVERLAP,
            ),
            (
                json!([with(json!({"address": 65_535, "data_type": "u32"}))]),
                "/points/0/address",
                ADDRESS_OUT_OF_RANGE,
            ),
            (
                json!([with(json!({"function_code": 1}))]),
                "/points/0/data_type",
                FUNCTION_TYPE_MISMATCH,
            ),
            (
                json!([with(
                    json!({"function_code": 2, "data_type": "bool", "byte_order": "badc"})
                )]),
                "/points/0/byte_order",
                BYTE_ORDER_UNEXPECTED,
            ),
            (
                json!([with(
                    json!({"data_type": "string", "byte_order": "cdab", "length": 2})
                )]),
                "/points/0/byte_order",
                BYTE_ORDER_UNEXPECTED,
            ),
            (
                json!([with(json!({"data_type": "string"}))]),
                "/points/0/length",
                LENGTH_REQUIRED,
            ),
            (
                json!([with(json!({"length": 2}))]),
                "/points/0/length",
                LENGTH_UNEXPECTED,
            ),
            (
                json!([with(json!({"scale": 0}))]),
                "/points/0/scale",
                SCALING_INVALID,
            ),
            (
                json!([with(json!({"data_type": "bool", "scale": 2}))]),
                "/points/0/scale",
                SCALING_INVALID,
            ),
            (
                json!([with(
                    json!({"data_type": "string", "length": 2, "offset": 1})
                )]),
                "/points/0/scale",
                SCALING_INVALID,
            ),
        ];
        for (points, field, key) in cases {
            every_entry(&points, |entry, errors| {
                assert_eq!(
                    errors,
                    [(field.to_owned(), key.as_str().to_owned())],
                    "{entry}: {points}"
                );
            });
        }
    }

    #[test]
    fn overlaps_are_per_function_code() {
        // Holding register 0 and input register 0 are different addresses; coil 0 too.
        let points = json!([
            {"key": "a", "function_code": 3, "address": 0, "data_type": "u32"},
            {"key": "b", "function_code": 4, "address": 0, "data_type": "u32"},
            {"key": "c", "function_code": 1, "address": 0, "data_type": "bool"},
            {"key": "d", "function_code": 2, "address": 0, "data_type": "bool"},
            {"key": "e", "function_code": 3, "address": 2, "data_type": "f64"},
        ]);
        assert!(parse_create(&bytes(&create(points))).is_ok());
    }

    #[test]
    fn the_points_must_be_a_bounded_list() {
        every_entry(&json!({"key": "p"}), |entry, errors| {
            assert_eq!(
                errors,
                [("/points".to_owned(), POINTS_INVALID.as_str().to_owned())],
                "{entry}"
            );
        });
        let too_many: Vec<Value> = (0..=MAX_POINTS)
            .map(|index| {
                json!({"key": format!("p{index}"), "function_code": 3,
                                "address": index, "data_type": "u16"})
            })
            .collect();
        every_entry(&Value::Array(too_many), |entry, errors| {
            assert_eq!(
                errors,
                [("/points".to_owned(), TOO_MANY_POINTS.as_str().to_owned())],
                "{entry}"
            );
        });
        let most: Vec<Value> = (0..MAX_POINTS)
            .map(|index| {
                json!({"key": format!("p{index}"), "function_code": 3,
                                "address": index, "data_type": "u16"})
            })
            .collect();
        assert!(parse_create(&bytes(&create(Value::Array(most)))).is_ok());
        assert_eq!(
            field_errors(parse_create(&bytes(&json!({"name": "m"})))),
            [("/points".to_owned(), FIELD_REQUIRED.as_str().to_owned())]
        );
    }

    #[test]
    fn unknown_fields_are_refused_without_echoing_odd_names() {
        let mut body = create(json!([]));
        body["tenant_id"] = json!("01928f6e-7a3c-7d4e-8f00-00000000000b");
        body["a/b~c"] = json!(1);
        body["x".repeat(65)] = json!(1);
        body["zero\u{200B}width"] = json!(1);
        let mut errors = field_errors(parse_create(&bytes(&body)));
        errors.sort();
        let unknown = UNKNOWN_FIELD.as_str().to_owned();
        assert_eq!(
            errors,
            [
                (String::new(), unknown.clone()),
                (String::new(), unknown.clone()),
                ("/a~1b~0c".to_owned(), unknown.clone()),
                ("/tenant_id".to_owned(), unknown.clone()),
            ]
        );
        // Each entry has its own fields: `revision` only on update, `format` only on import.
        let mut body = create(json!([]));
        body["revision"] = json!(1);
        assert_eq!(
            field_errors(parse_create(&bytes(&body))),
            [("/revision".to_owned(), unknown.clone())]
        );
        let mut body = update(json!([]));
        body["format"] = json!(DOCUMENT_FORMAT);
        assert_eq!(
            field_errors(parse_update(&bytes(&body))),
            [("/format".to_owned(), unknown.clone())]
        );
    }

    #[test]
    fn an_import_must_be_this_format() {
        let mut body = import(valid_points());
        body["format"] = json!("other.format");
        body["format_version"] = json!(2);
        assert_eq!(
            field_errors(parse_import(&bytes(&body))),
            [
                ("/format".to_owned(), FORMAT_UNSUPPORTED.as_str().to_owned()),
                (
                    "/format_version".to_owned(),
                    FORMAT_VERSION_UNSUPPORTED.as_str().to_owned()
                ),
            ]
        );
        let body = json!({"name": "power-meter", "points": []});
        assert_eq!(
            field_errors(parse_import(&bytes(&body))),
            [
                ("/format".to_owned(), FIELD_REQUIRED.as_str().to_owned()),
                (
                    "/format_version".to_owned(),
                    FIELD_REQUIRED.as_str().to_owned()
                ),
            ]
        );
    }

    #[test]
    fn an_update_revision_is_a_positive_integer() {
        for revision in [json!(0), json!(-1), json!(1.5), json!("1")] {
            let body = json!({"revision": revision, "points": []});
            assert_eq!(
                field_errors(parse_update(&bytes(&body))),
                [("/revision".to_owned(), REVISION_INVALID.as_str().to_owned())],
                "{revision}"
            );
        }
        // The name of an update is only compared with the stored one, so any string passes here.
        let body = json!({"name": "Not A Valid Name", "points": []});
        assert_eq!(
            parse_update(&bytes(&body)).unwrap().name.as_deref(),
            Some("Not A Valid Name")
        );
    }

    #[test]
    fn many_errors_are_listed_up_to_the_limit() {
        let points: Vec<Value> = (0..150).map(|_| json!({"key": ""})).collect();
        let error = parse_create(&bytes(&create(Value::Array(points)))).unwrap_err();
        assert_eq!(error.field_errors().len(), MAX_FIELD_ERRORS);
        // 150 points with 4 errors each (key invalid, 3 required fields missing).
        assert_eq!(
            error.details()["omitted_field_errors"],
            json!(150 * 4 - MAX_FIELD_ERRORS)
        );
    }

    #[test]
    fn protocol_paths_become_json_pointers() {
        assert_eq!(pointer("points[3].address"), "/points/3/address");
        assert_eq!(pointer("points[12]"), "/points/12");
        assert_eq!(pointer(""), "");
    }
}
