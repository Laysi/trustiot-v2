//! The M1 error format (`docs/requirements/rebuild-checklist.md`, row M1).
//!
//! Every error response from every service has the same JSON body, [`ErrorBody`], with a code
//! from one closed set, [`ErrorCode`]. Clients branch on the code, never on text. Bodies carry
//! no user-facing text: the frontend looks up [`MessageKey`] in its translations for the user's
//! language and fills in the `details` values.
//!
//! ```json
//! {
//!   "code": "validation_failed",
//!   "message_key": "error.validation_failed",
//!   "field_errors": [
//!     {"field": "/name", "message_key": "device_model.name_invalid", "details": {"max": 40}}
//!   ]
//! }
//! ```

use std::borrow::Cow;
use std::fmt;

use http::StatusCode;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::{ObjectBuilder, Schema, Type};
use utoipa::{PartialSchema, ToSchema};

use crate::naming::{DOTTED_NAME_PATTERN, MAX_DOTTED_NAME_LEN, is_dotted_name};

/// What went wrong, as one of a closed set of codes (M1). The HTTP status follows from the
/// code; a more specific reason goes in the message key, not in a new code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// 400: the request is malformed: not JSON, wrong types, or missing fields.
    InvalidRequest,
    /// 422: the request is well-formed but breaks a rule; `field_errors` names the fields.
    ValidationFailed,
    /// 401: no valid identity: the session or internal identity token is missing, invalid, or
    /// expired. The frontend sends the user to log in again.
    Unauthenticated,
    /// 403: the identity is valid but lacks the role or permission.
    PermissionDenied,
    /// 403: the tenant is suspended (B1); the frontend shows only the suspension page.
    TenantSuspended,
    /// 409: the request names a user or a tenant the session no longer has (Laysi/trustiot-v2#678):
    /// the page was showing another user or tenant than the session acts as, because another tab
    /// of the browser switched tenants or signed in as someone else. Nothing was done; the
    /// frontend reads the session again and shows what it is now before anything is sent again.
    SessionChanged,
    /// 403: a plan limit or quota is used up (B2, B3); the frontend offers an upgrade. Retrying
    /// does not help.
    QuotaExceeded,
    /// 404: the resource does not exist, or the caller's tenant cannot see it.
    NotFound,
    /// 409: the request conflicts with the current state, for example a duplicate or a
    /// concurrent change.
    Conflict,
    /// 429: too many requests; retry later.
    RateLimited,
    /// 500: an unexpected failure. Never carries details: the cause goes to the logs only.
    Internal,
    /// 503: a dependency is down or overloaded, or the service is shutting down; retry later.
    Unavailable,
    /// 504: a dependency did not answer within its timeout; retry later.
    Timeout,
}

impl ErrorCode {
    /// Every code, in declaration order.
    pub const ALL: [Self; 13] = [
        Self::InvalidRequest,
        Self::ValidationFailed,
        Self::Unauthenticated,
        Self::PermissionDenied,
        Self::TenantSuspended,
        Self::SessionChanged,
        Self::QuotaExceeded,
        Self::NotFound,
        Self::Conflict,
        Self::RateLimited,
        Self::Internal,
        Self::Unavailable,
        Self::Timeout,
    ];

    /// The wire value, for example `not_found`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::ValidationFailed => "validation_failed",
            Self::Unauthenticated => "unauthenticated",
            Self::PermissionDenied => "permission_denied",
            Self::TenantSuspended => "tenant_suspended",
            Self::SessionChanged => "session_changed",
            Self::QuotaExceeded => "quota_exceeded",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::RateLimited => "rate_limited",
            Self::Internal => "internal",
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
        }
    }

    /// The HTTP status a response with this code uses.
    #[must_use]
    pub const fn http_status(self) -> StatusCode {
        match self {
            Self::InvalidRequest => StatusCode::BAD_REQUEST,
            Self::ValidationFailed => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::PermissionDenied | Self::TenantSuspended | Self::QuotaExceeded => {
                StatusCode::FORBIDDEN
            }
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::SessionChanged | Self::Conflict => StatusCode::CONFLICT,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Timeout => StatusCode::GATEWAY_TIMEOUT,
        }
    }

    /// The generic message key for this code, `error.<code>`; the frontend always has a
    /// translation for it.
    #[must_use]
    pub const fn default_message_key(self) -> MessageKey {
        match self {
            Self::InvalidRequest => MessageKey::new("error.invalid_request"),
            Self::ValidationFailed => MessageKey::new("error.validation_failed"),
            Self::Unauthenticated => MessageKey::new("error.unauthenticated"),
            Self::PermissionDenied => MessageKey::new("error.permission_denied"),
            Self::TenantSuspended => MessageKey::new("error.tenant_suspended"),
            Self::SessionChanged => MessageKey::new("error.session_changed"),
            Self::QuotaExceeded => MessageKey::new("error.quota_exceeded"),
            Self::NotFound => MessageKey::new("error.not_found"),
            Self::Conflict => MessageKey::new("error.conflict"),
            Self::RateLimited => MessageKey::new("error.rate_limited"),
            Self::Internal => MessageKey::new("error.internal"),
            Self::Unavailable => MessageKey::new("error.unavailable"),
            Self::Timeout => MessageKey::new("error.timeout"),
        }
    }

    /// Whether the same request may succeed later without any change: `rate_limited`,
    /// `unavailable` and `timeout`.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::RateLimited | Self::Unavailable | Self::Timeout)
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The key of a translated message, for example `error.not_found` or
/// `device_model.name_invalid`: two or more dot-separated segments of `a-z`, `0-9` and `_`, at
/// most 128 bytes. It names a message in the frontend's translations; it is never the message.
///
/// Write keys as constants, `const NAME_INVALID: MessageKey =
/// MessageKey::new("device_model.name_invalid");`, or with [`message_key!`](crate::message_key),
/// so a malformed key fails the build.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct MessageKey(Cow<'static, str>);

impl MessageKey {
    /// Longest key accepted, in bytes.
    pub const MAX_LEN: usize = MAX_DOTTED_NAME_LEN;

    /// A key written in the source code.
    ///
    /// # Panics
    ///
    /// When `key` is malformed. In a `const` item or [`message_key!`](crate::message_key) that is
    /// a compile error, which is how keys should be written.
    #[must_use]
    pub const fn new(key: &'static str) -> Self {
        assert!(is_dotted_name(key), "malformed message key");
        Self(Cow::Borrowed(key))
    }

    /// A key that arrived at run time, for example in a peer service's error body.
    ///
    /// # Errors
    ///
    /// When `key` is malformed.
    pub fn parse(key: impl Into<String>) -> Result<Self, InvalidMessageKey> {
        let key = key.into();
        if is_dotted_name(&key) {
            Ok(Self(Cow::Owned(key)))
        } else {
            Err(InvalidMessageKey)
        }
    }

    /// The key text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MessageKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for MessageKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let key = String::deserialize(deserializer)?;
        Self::parse(key).map_err(serde::de::Error::custom)
    }
}

impl PartialSchema for MessageKey {
    fn schema() -> RefOr<Schema> {
        ObjectBuilder::new()
            .schema_type(Type::String)
            .pattern(Some(DOTTED_NAME_PATTERN))
            .max_length(Some(Self::MAX_LEN))
            .description(Some(
                "Key of the translated message the client shows; never the message itself.",
            ))
            .examples(["error.not_found"])
            .into()
    }
}

impl ToSchema for MessageKey {}

/// The text is not a well-formed [`MessageKey`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "message key must be two or more dot-separated segments of a-z, 0-9 and _, at most 128 bytes"
)]
pub struct InvalidMessageKey;

/// A [`MessageKey`] checked at compile time.
///
/// ```
/// let key = contract::message_key!("device_model.name_invalid");
/// assert_eq!(key.as_str(), "device_model.name_invalid");
/// ```
///
/// A malformed key, such as text meant for users, does not compile:
///
/// ```compile_fail,E0080
/// let key = contract::message_key!("Device name is invalid");
/// ```
#[macro_export]
macro_rules! message_key {
    ($key:literal) => {
        const { $crate::error::MessageKey::new($key) }
    };
}

/// The JSON body of every error response (M1). Clients ignore fields they do not know, so
/// fields are only ever added as optional ones.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ErrorBody {
    /// What went wrong; clients branch on this.
    code: ErrorCode,
    /// The message the client shows, translated by the client.
    message_key: MessageKey,
    /// Values the translated message refers to, for example `{"max": 40}`. Never user-facing
    /// text, internal error text, or personal data. Absent when empty.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    #[schema(value_type = Object)]
    details: Map<String, Value>,
    /// The request fields that were rejected. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    field_errors: Vec<FieldError>,
}

impl ErrorBody {
    /// A body with `code` and its default message key, `error.<code>`. Refine it with the
    /// `with_*` methods; the response status is [`ErrorBody::http_status`].
    #[must_use]
    pub fn new(code: ErrorCode) -> Self {
        Self {
            code,
            message_key: code.default_message_key(),
            details: Map::new(),
            field_errors: Vec::new(),
        }
    }

    /// Replaces the message key with a more specific one, for example
    /// `gateway.already_bound` for a `conflict`.
    #[must_use]
    pub fn with_message_key(mut self, message_key: MessageKey) -> Self {
        self.message_key = message_key;
        self
    }

    /// Adds a value the translated message refers to. Ignored for `internal`, which never
    /// carries details: the cause belongs in the logs.
    #[must_use]
    pub fn with_detail(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        if self.code != ErrorCode::Internal {
            self.details.insert(name.into(), value.into());
        }
        self
    }

    /// Adds a rejected request field. Ignored for `internal`, like [`ErrorBody::with_detail`].
    #[must_use]
    pub fn with_field_error(mut self, field_error: FieldError) -> Self {
        if self.code != ErrorCode::Internal {
            self.field_errors.push(field_error);
        }
        self
    }

    /// What went wrong.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        self.code
    }

    /// The message the client shows.
    #[must_use]
    pub fn message_key(&self) -> &MessageKey {
        &self.message_key
    }

    /// Values the message refers to.
    #[must_use]
    pub fn details(&self) -> &Map<String, Value> {
        &self.details
    }

    /// The rejected request fields.
    #[must_use]
    pub fn field_errors(&self) -> &[FieldError] {
        &self.field_errors
    }

    /// The HTTP status of the response that carries this body.
    #[must_use]
    pub fn http_status(&self) -> StatusCode {
        self.code.http_status()
    }
}

impl From<ErrorCode> for ErrorBody {
    fn from(code: ErrorCode) -> Self {
        Self::new(code)
    }
}

impl fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.code, self.message_key)
    }
}

impl std::error::Error for ErrorBody {}

/// A rejected field of the request body.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FieldError {
    /// JSON Pointer (RFC 6901) to the rejected value in the request body, for example
    /// `/points/3/address`; an empty string means the whole body.
    pub field: String,
    /// The message the client shows next to the field.
    pub message_key: MessageKey,
    /// Values the message refers to. Absent when empty.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    #[schema(value_type = Object)]
    pub details: Map<String, Value>,
}

impl FieldError {
    /// A rejected field without details.
    #[must_use]
    pub fn new(field: impl Into<String>, message_key: MessageKey) -> Self {
        Self {
            field: field.into(),
            message_key,
            details: Map::new(),
        }
    }

    /// Adds a value the field's message refers to.
    #[must_use]
    pub fn with_detail(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.details.insert(name.into(), value.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_values_status_and_default_keys_follow_the_code() {
        for code in ErrorCode::ALL {
            let json = serde_json::to_value(code).expect("serialize");
            assert_eq!(json, Value::from(code.as_str()));
            assert_eq!(
                serde_json::from_value::<ErrorCode>(json).expect("deserialize"),
                code
            );
            assert_eq!(
                code.default_message_key().as_str(),
                format!("error.{}", code.as_str())
            );
            let status = code.http_status();
            assert!(
                status.is_client_error() || status.is_server_error(),
                "{code}: {status}"
            );
        }
    }

    #[test]
    fn only_transient_codes_are_retryable() {
        let retryable: Vec<ErrorCode> = ErrorCode::ALL
            .into_iter()
            .filter(|code| code.is_retryable())
            .collect();
        assert_eq!(
            retryable,
            [
                ErrorCode::RateLimited,
                ErrorCode::Unavailable,
                ErrorCode::Timeout
            ]
        );
    }

    #[test]
    fn message_keys_are_validated() {
        const KEY: MessageKey = MessageKey::new("gateway.already_bound");
        assert_eq!(KEY.as_str(), "gateway.already_bound");
        assert_eq!(
            crate::message_key!("error.not_found").to_string(),
            "error.not_found"
        );
        assert_eq!(
            MessageKey::parse("device_model.name_invalid").map(|key| key.to_string()),
            Ok("device_model.name_invalid".to_owned())
        );
        for key in [
            "",
            "not_found",
            "Error.not_found",
            "裝置.名稱",
            "a..b",
            "a.b c",
        ] {
            assert_eq!(MessageKey::parse(key), Err(InvalidMessageKey), "{key:?}");
        }
    }

    #[test]
    #[should_panic(expected = "malformed message key")]
    fn malformed_key_literal_panics_outside_const() {
        let key = std::hint::black_box("Not a key");
        // Leak to get the `&'static str` that `new` takes, as a literal would provide.
        let _ = MessageKey::new(Box::leak(key.to_owned().into_boxed_str()));
    }

    #[test]
    fn internal_errors_never_carry_details() {
        let body = ErrorBody::new(ErrorCode::Internal)
            .with_detail("query", "SELECT * FROM members")
            .with_field_error(FieldError::new(
                "/name",
                MessageKey::new("member.name_invalid"),
            ));
        assert!(body.details().is_empty());
        assert!(body.field_errors().is_empty());
        assert_eq!(
            serde_json::to_value(&body).expect("serialize"),
            serde_json::json!({"code": "internal", "message_key": "error.internal"})
        );
    }

    #[test]
    fn display_names_code_and_key_only() {
        let body = ErrorBody::new(ErrorCode::Conflict)
            .with_message_key(MessageKey::new("gateway.already_bound"))
            .with_detail("gateway", "gw-1");
        assert_eq!(body.to_string(), "conflict (gateway.already_bound)");
        assert_eq!(body.http_status(), StatusCode::CONFLICT);
        assert_eq!(
            ErrorBody::from(ErrorCode::NotFound),
            ErrorBody::new(ErrorCode::NotFound)
        );
    }
}
