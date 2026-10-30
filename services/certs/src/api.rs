//! What the client API's handlers share ([`crate::workstations`], [`crate::registrations`]):
//! the platform administrator check, the JSON body limits, field errors in M1's format, and
//! responses marked `cache-control: no-store`. Each API passes its own message keys.

use axum::body::{Body, Bytes};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use contract::{ErrorBody, ErrorCode, FieldError, MessageKey};
use serde::Serialize;
use serde_json::{Map, Value};
use service_kit::identity::{Requirement, UserIdentity};

/// Most field errors one response lists; `details.omitted_field_errors` counts the rest.
pub(crate) const MAX_FIELD_ERRORS: usize = 10;

/// Longest unknown field name a field error repeats; longer or odd names point at the body.
const MAX_ECHOED_FIELD_LEN: usize = 64;

/// Marks every response of a router as not to be stored, including the 401 that the
/// [`UserIdentity`] extractor answers.
pub(crate) async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The 403 to answer unless the user is a platform administrator whose token the ingress's
/// cookie check issued: an exchanged token (`act`) means another service calls on the
/// administrator's behalf, and issuing, revoking or registering credentials is the
/// administrator's own act. service-kit's shared check logs the refusal at INFO.
pub(crate) fn refuse(user: &UserIdentity, action: &'static str) -> Option<Response> {
    user.require(Requirement::OwnPlatformAdmin, action)
        .err()
        .map(IntoResponse::into_response)
}

/// Whether the request has exactly one `Content-Type`, `application/json` with or without
/// parameters, compared without regard to ASCII case. A page of another origin cannot send that
/// type with the administrator's cookie without a CORS preflight, which nothing answers.
pub(crate) fn is_json(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(CONTENT_TYPE).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    value.to_str().is_ok_and(|value| {
        let essence = value.split(';').next().unwrap_or_default();
        essence.trim().eq_ignore_ascii_case("application/json")
    })
}

/// The body, at most `max_bytes`, after the content type check; `too_large` with
/// `details.max_bytes` when it is larger.
pub(crate) async fn read_json(
    headers: &HeaderMap,
    body: Body,
    max_bytes: usize,
    too_large: MessageKey,
) -> Result<Bytes, ErrorBody> {
    if !is_json(headers) {
        return Err(ErrorBody::new(ErrorCode::InvalidRequest));
    }
    axum::body::to_bytes(body, max_bytes).await.map_err(|_| {
        ErrorBody::new(ErrorCode::InvalidRequest)
            .with_message_key(too_large)
            .with_detail("max_bytes", max_bytes)
    })
}

/// The field errors of one request, at most [`MAX_FIELD_ERRORS`] of them listed.
#[derive(Default)]
pub(crate) struct Problems {
    listed: Vec<FieldError>,
    omitted: usize,
}

impl Problems {
    pub(crate) fn add(&mut self, error: FieldError) {
        if self.listed.len() < MAX_FIELD_ERRORS {
            self.listed.push(error);
        } else {
            self.omitted += 1;
        }
    }

    pub(crate) fn into_result(self) -> Result<(), ErrorBody> {
        if self.listed.is_empty() {
            return Ok(());
        }
        let mut body = ErrorBody::new(ErrorCode::ValidationFailed);
        for error in self.listed {
            body = body.with_field_error(error);
        }
        if self.omitted > 0 {
            body = body.with_detail("omitted_field_errors", self.omitted);
        }
        Err(body)
    }
}

/// The message keys of [`object`]'s two problems.
#[derive(Clone)]
pub(crate) struct BodyKeys {
    /// The body is not a JSON object (400).
    pub(crate) invalid: MessageKey,
    /// A field the request does not have (422).
    pub(crate) unknown_field: MessageKey,
}

/// The body as a JSON object, with a field error for each field not in `allowed`.
pub(crate) fn object(
    body: &[u8],
    allowed: &[&str],
    keys: BodyKeys,
    problems: &mut Problems,
) -> Result<Map<String, Value>, ErrorBody> {
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(body) else {
        return Err(
            ErrorBody::new(ErrorCode::InvalidRequest).with_message_key(keys.invalid.clone())
        );
    };
    for field in fields.keys() {
        if !allowed.contains(&field.as_str()) {
            let echo = field.len() <= MAX_ECHOED_FIELD_LEN
                && field.bytes().all(|byte| byte.is_ascii_graphic());
            let pointer = if echo {
                format!("/{}", field.replace('~', "~0").replace('/', "~1"))
            } else {
                String::new()
            };
            problems.add(FieldError::new(pointer, keys.unknown_field.clone()));
        }
    }
    Ok(fields)
}

pub(crate) fn ok<T: Serialize>(status: StatusCode, body: &T) -> Response {
    (status, [(CACHE_CONTROL, "no-store")], axum::Json(body)).into_response()
}

pub(crate) fn error(body: ErrorBody) -> Response {
    (
        body.http_status(),
        [(CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}
