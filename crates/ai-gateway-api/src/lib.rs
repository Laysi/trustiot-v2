//! Wire types of the AI gateway's admin API (Laysi/trustiot-v2#606; services/ai-gateway/README.md,
//! 「管理 API」): what the identity service sends to the gateway's admin port to read and change
//! the model roles, and what it gets back. The gateway (`services/ai-gateway/src/admin.rs`) and
//! its one caller, the identity service (Laysi/trustiot-v2#608), share these definitions, so the
//! two cannot drift apart.
//!
//! Only serde types and constants: no HTTP client or server, and never `crates/contract`
//! (decision 28; `tests/no_contract.rs`). The gateway authenticates nobody: who may reach the
//! admin port is the mesh's decision, and whether a person is a platform administrator is the
//! identity service's, made before it calls (README.md, 「驗證方式」).
//!
//! Responses are read leniently (unknown fields are ignored, an unknown error code reads as
//! [`ErrorCode::Unknown`]), so a newer gateway can add to them; the change request is read
//! strictly by the gateway.

use serde::{Deserialize, Serialize};

/// `GET`: every configured model role ([`Roles`]).
pub const ROLES_PATH: &str = "/internal/admin/v1/roles";

/// `PUT` ([`SetModels`]) and `DELETE` (with [`EXPECTED_VERSION_PARAM`]) of one role, in axum's
/// path syntax; [`role_path`] fills it in.
pub const ROLE_PATH: &str = "/internal/admin/v1/roles/{role}";

/// The query parameter of a `DELETE`: the role's version the caller read, as for a `PUT`.
pub const EXPECTED_VERSION_PARAM: &str = "expected_version";

/// The request header naming who asked for a change, required once on every admin request: an
/// opaque text of 1 to [`MAX_ACTOR_CHARS`] printable ASCII characters ([`is_valid_actor`]), such
/// as the platform administrator's user ID as the identity service knows it. The gateway records
/// it in the change log and its logs and never checks it against anything: it is only as true as
/// the caller that sends it, which the mesh limits to the identity service.
pub const ACTOR_HEADER: &str = "ai-gateway-actor";

/// The longest actor, in characters (the change log's `changed_by` holds up to 128).
pub const MAX_ACTOR_CHARS: usize = 128;

/// The longest role name, in characters.
pub const MAX_ROLE_CHARS: usize = 32;

/// Whether `actor` may be sent in [`ACTOR_HEADER`]: 1 to [`MAX_ACTOR_CHARS`] characters, each
/// printable ASCII (`' '` to `'~'`), not starting or ending with a space (HTTP drops those).
pub fn is_valid_actor(actor: &str) -> bool {
    (1..=MAX_ACTOR_CHARS).contains(&actor.len())
        && actor.bytes().all(|byte| (b' '..=b'~').contains(&byte))
        && !actor.starts_with(' ')
        && !actor.ends_with(' ')
}

/// Whether `role` is a model role name: 1 to [`MAX_ROLE_CHARS`] lower-case letters, digits or
/// `_`, starting with a letter. Such a name needs no escaping in a path.
pub fn is_role_name(role: &str) -> bool {
    role.len() <= MAX_ROLE_CHARS
        && role
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && role
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// [`ROLE_PATH`] for `role`; `None` when `role` is not a role name ([`is_role_name`]), so a
/// caller never builds a path from other text.
pub fn role_path(role: &str) -> Option<String> {
    is_role_name(role).then(|| ROLE_PATH.replace("{role}", role))
}

/// The body of a `PUT`: the role's new models and the version the caller read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetModels {
    /// `<provider>/<model>` in fallback order, first choice first: 1 to 8 distinct models of
    /// configured providers whose protocol is the role's.
    pub models: Vec<String>,
    /// [`Role::version`] as the caller read it; the change is refused with
    /// [`ErrorCode::VersionConflict`] when the role has another version by then.
    pub expected_version: i64,
}

/// Where a role's models in effect come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The gateway's configuration (the chart's `ai.roles`): never changed, reset, or the stored
    /// list no longer fits ([`Role::ignored`]).
    Configuration,
    /// A list a platform administrator stored.
    Database,
}

/// A configured model role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    pub role: String,
    /// The role's protocol, which only the configuration sets: `anthropic-messages` or
    /// `openai-decisions` today.
    pub protocol: String,
    /// The models in effect, in fallback order.
    pub models: Vec<String>,
    /// The models the configuration gives the role, which a reset goes back to.
    pub configured_models: Vec<String>,
    pub source: Source,
    /// The version a change or reset must expect: the settings generation of the role's last
    /// change or reset, 0 when it never had one. It only goes up.
    pub version: i64,
    /// A stored list the gateway does not use because it no longer fits the configuration (a
    /// provider was removed, the role's protocol changed); the role then uses its configured
    /// models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored: Option<Ignored>,
    /// When the role was last changed or reset, RFC 3339 in UTC; `None` when it never was.
    #[serde(default)]
    pub updated_at: Option<String>,
    /// The actor of the last change or reset; `None` when there was none.
    #[serde(default)]
    pub updated_by: Option<String>,
}

/// A stored model list that is not in use, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ignored {
    pub models: Vec<String>,
    pub reason: String,
}

/// A configured provider: the models of a role may name it when its protocol is the role's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provider {
    pub name: String,
    /// `anthropic-messages` or `openai-decisions` today.
    pub protocol: String,
}

/// The answer of `GET` [`ROLES_PATH`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roles {
    /// Every configured role, by name.
    pub roles: Vec<Role>,
    /// Every configured provider, by name.
    pub providers: Vec<Provider>,
}

/// Every error answer: `{"error": {"code": …, "message": …}}`, with `cache-control: no-store`.
/// The message never quotes the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDetail {
    pub code: ErrorCode,
    /// For people, in English; not part of the contract.
    pub message: String,
    /// With [`ErrorCode::VersionConflict`]: the role's version now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_version: Option<i64>,
}

/// What went wrong. [`ErrorCode::status`] is the HTTP status the gateway answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// 400: the body is not a [`SetModels`], the `DELETE` lacks a valid
    /// [`EXPECTED_VERSION_PARAM`], or a version is negative.
    InvalidRequest,
    /// 400: [`ACTOR_HEADER`] is missing, repeated or not [`is_valid_actor`].
    InvalidActor,
    /// 413: the body is larger than the gateway reads.
    RequestTooLarge,
    /// 404: no admin route has this path.
    NotFound,
    /// 404: the configuration has no such model role.
    RoleNotFound,
    /// 409: the role is no longer at the expected version; `current_version` says where it is.
    VersionConflict,
    /// 422: not 1 to 8 distinct, well-formed `<provider>/<model>`.
    InvalidModels,
    /// 422: a model names a provider the configuration does not have.
    UnknownProvider,
    /// 422: a model's provider speaks another protocol than the role.
    ProtocolMismatch,
    /// 503: the database could not be read or written; nothing was changed.
    Unavailable,
    /// A code this version of the crate does not know (from a newer gateway).
    // serde_derive accepts `other` on a unit variant of this externally tagged, unit-only enum:
    // the code is read as a variant identifier, and an unrecognized one selects this variant.
    // The workspace already relies on it (`contract::identity::TenantRole::Unrecognized`,
    // `protocol::live`), and the test `error_codes_are_spelled_as_documented_and_unknown_ones_are_read`
    // below reads "something_new" as `Unknown`.
    #[serde(other)]
    Unknown,
}

impl ErrorCode {
    /// The code as it is written on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::InvalidActor => "invalid_actor",
            Self::RequestTooLarge => "request_too_large",
            Self::NotFound => "not_found",
            Self::RoleNotFound => "role_not_found",
            Self::VersionConflict => "version_conflict",
            Self::InvalidModels => "invalid_models",
            Self::UnknownProvider => "unknown_provider",
            Self::ProtocolMismatch => "protocol_mismatch",
            Self::Unavailable => "unavailable",
            Self::Unknown => "unknown",
        }
    }

    /// The HTTP status of an answer with this code ([`ErrorCode::Unknown`]: 500).
    pub fn status(self) -> u16 {
        match self {
            Self::InvalidRequest | Self::InvalidActor => 400,
            Self::RequestTooLarge => 413,
            Self::NotFound | Self::RoleNotFound => 404,
            Self::VersionConflict => 409,
            Self::InvalidModels | Self::UnknownProvider | Self::ProtocolMismatch => 422,
            Self::Unavailable => 503,
            Self::Unknown => 500,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actors_are_printable_ascii_up_to_128_characters() {
        for valid in [
            "admin",
            "user:0f3a-1",
            "a b",
            "~!",
            &"a".repeat(MAX_ACTOR_CHARS),
        ] {
            assert!(is_valid_actor(valid), "{valid:?}");
        }
        for invalid in [
            String::new(),
            "a".repeat(MAX_ACTOR_CHARS + 1),
            "a\nb".to_owned(),
            "a\tb".to_owned(),
            "é".to_owned(),
            "\u{7f}".to_owned(),
            " a".to_owned(),
            "a ".to_owned(),
        ] {
            assert!(!is_valid_actor(&invalid), "{invalid:?}");
        }
    }

    #[test]
    fn role_paths_are_built_from_role_names_only() {
        assert_eq!(
            role_path("main").as_deref(),
            Some("/internal/admin/v1/roles/main")
        );
        assert_eq!(
            role_path("worker_2").as_deref(),
            Some("/internal/admin/v1/roles/worker_2")
        );
        for invalid in ["", "Main", "2nd", "a/b", "../x", "a%2fb", &"a".repeat(33)] {
            assert_eq!(role_path(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn error_codes_are_spelled_as_documented_and_unknown_ones_are_read() {
        let all = [
            ErrorCode::InvalidRequest,
            ErrorCode::InvalidActor,
            ErrorCode::RequestTooLarge,
            ErrorCode::NotFound,
            ErrorCode::RoleNotFound,
            ErrorCode::VersionConflict,
            ErrorCode::InvalidModels,
            ErrorCode::UnknownProvider,
            ErrorCode::ProtocolMismatch,
            ErrorCode::Unavailable,
        ];
        for code in all {
            let json = serde_json::to_value(code).unwrap();
            assert_eq!(json, code.as_str());
            assert_eq!(serde_json::from_value::<ErrorCode>(json).unwrap(), code);
        }
        let conflict = ErrorBody {
            error: ErrorDetail {
                code: ErrorCode::VersionConflict,
                message: "changed".to_owned(),
                current_version: Some(7),
            },
        };
        assert_eq!(
            serde_json::to_value(&conflict).unwrap(),
            serde_json::json!({"error": {"code": "version_conflict", "message": "changed", "current_version": 7}})
        );
        let newer: ErrorBody = serde_json::from_value(
            serde_json::json!({"error": {"code": "something_new", "message": "m", "extra": 1}}),
        )
        .unwrap();
        assert_eq!(newer.error.code, ErrorCode::Unknown);
        assert_eq!(newer.error.current_version, None);
    }

    #[test]
    fn a_change_request_is_read_strictly() {
        let body: SetModels = serde_json::from_value(
            serde_json::json!({"models": ["anthropic/claude-opus-5-5"], "expected_version": 3}),
        )
        .unwrap();
        assert_eq!(body.expected_version, 3);
        assert!(
            serde_json::from_value::<SetModels>(
                serde_json::json!({"models": [], "expected_version": 0, "role": "main"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<SetModels>(serde_json::json!({"models": ["a/b"]})).is_err()
        );
    }

    #[test]
    fn a_role_is_written_with_its_source_and_read_leniently() {
        let role = Role {
            role: "main".to_owned(),
            protocol: "anthropic-messages".to_owned(),
            models: vec!["anthropic/claude-sonnet-5-5".to_owned()],
            configured_models: vec!["anthropic/claude-opus-5-5".to_owned()],
            source: Source::Database,
            version: 4,
            ignored: None,
            updated_at: Some("2026-10-24T00:00:00.000000Z".to_owned()),
            updated_by: Some("user:1".to_owned()),
        };
        let json = serde_json::to_value(&role).unwrap();
        assert_eq!(json["source"], "database");
        assert!(json.get("ignored").is_none(), "{json}");
        let mut newer = json.clone();
        newer["added_later"] = serde_json::json!(true);
        assert_eq!(serde_json::from_value::<Role>(newer).unwrap(), role);
    }
}
