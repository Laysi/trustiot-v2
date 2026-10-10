//! The field-name safety net: values of fields whose names look like credentials, personal data,
//! request bodies, or chat content are replaced before they leave the process, in the JSON log
//! lines and in the OTLP log and span attributes alike.
//!
//! This is a backstop, not the rule. The rule (docs/architecture/observability.md) is that code
//! never records such values at all; a value interpolated into a log message or recorded under
//! an innocuous name is not caught here.

/// The replacement for a redacted value.
pub const REDACTED: &str = "[REDACTED]";

/// Sensitive wherever they occur in the name, separators removed (`db_password`, `apiKey`).
const SUBSTRINGS: &[&str] = &[
    "accesskey",
    "accesstoken",
    "apikey",
    "authorization",
    "authtoken",
    "bearer",
    "connectionstring",
    "cookie",
    "credential",
    "databaseurl",
    "idtoken",
    "passphrase",
    "passwd",
    "password",
    "privatekey",
    "refreshtoken",
    "secret",
    "sessionid",
    "signingkey",
];

/// Sensitive when one segment of the name is exactly this word (`user.email`, but not
/// `tokenizer`).
const SEGMENTS: &[&str] = &[
    "email", "emails", "jwt", "otp", "phone", "prompt", "prompts", "token", "tokens",
];

/// Payload segments: sensitive unless the name ends in a measurement or descriptor
/// (`http.request.body.size`, `content_type`).
const PAYLOAD: &[&str] = &["body", "completion", "content", "contents"];

/// Last segments that describe a payload rather than contain it.
const DESCRIPTORS: &[&str] = &[
    "bytes", "count", "encoding", "len", "length", "size", "type",
];

/// Splits a field name into lower-case words at separators and camelCase boundaries.
fn segments(name: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() {
            if !current.is_empty() {
                segments.push(std::mem::take(&mut current));
            }
            previous_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && previous_lower && !current.is_empty() {
            segments.push(std::mem::take(&mut current));
        }
        previous_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        current.push(c.to_ascii_lowercase());
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// Whether a field with this name must not be logged or exported.
pub fn is_sensitive(name: &str) -> bool {
    let segments = segments(name);
    let joined = segments.concat();
    if SUBSTRINGS.iter().any(|word| joined.contains(word)) {
        return true;
    }
    if segments
        .iter()
        .any(|segment| SEGMENTS.contains(&segment.as_str()))
    {
        return true;
    }
    let describes = segments
        .last()
        .is_some_and(|last| DESCRIPTORS.contains(&last.as_str()));
    !describes
        && segments
            .iter()
            .any(|segment| PAYLOAD.contains(&segment.as_str()))
}

#[cfg(test)]
mod tests {
    use super::{is_sensitive, segments};

    #[test]
    fn names_split_at_separators_and_camel_case() {
        assert_eq!(segments("refreshToken"), ["refresh", "token"]);
        assert_eq!(segments("x-api-key"), ["x", "api", "key"]);
        assert_eq!(
            segments("http.request.body.size"),
            ["http", "request", "body", "size"]
        );
        assert_eq!(segments("HTTPHeader"), ["httpheader"]);
        assert_eq!(segments("v2Token"), ["v2", "token"]);
    }

    #[test]
    fn credentials_personal_data_and_payloads_are_sensitive() {
        for name in [
            "password",
            "db_password",
            "dbpassword",
            "Authorization",
            "http.request.header.authorization",
            "cookie",
            "set-cookie",
            "token",
            "access_token",
            "accesstoken",
            "refreshToken",
            "client_secret",
            "api_key",
            "apiKey",
            "x-api-key",
            "private_key",
            "session_id",
            "sessionId",
            "database_url",
            "connection_string",
            "email",
            "user.email",
            "emailAddress",
            "phone",
            "body",
            "request_body",
            "http.request.body",
            "body_text",
            "content",
            "message.content",
            "chat_contents",
            "prompt",
            "completion",
            "jwt",
            "otp",
        ] {
            assert!(is_sensitive(name), "{name} should be redacted");
        }
    }

    #[test]
    fn operational_fields_are_kept() {
        for name in [
            "message",
            "http.request.method",
            "http.route",
            "http.response.status_code",
            "http.request.body.size",
            "content_type",
            "content_length",
            "duration_ms",
            "error",
            "address",
            "tenant_id",
            "user_id",
            "job",
            "service.name",
            "trace_id",
            "signal",
            "keyboard",
            "tokenizer_model",
        ] {
            assert!(!is_sensitive(name), "{name} should be kept");
        }
    }
}
