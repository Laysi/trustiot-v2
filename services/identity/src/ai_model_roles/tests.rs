use axum::body::Bytes;
use axum::http::HeaderValue;
use contract::identity::Actor as TokenActor;
use contract::{InternalClaims, ServiceName, TenantGrant, TenantId, TenantRole};
use serde_json::{Value, json};
use testkit::identity::TestIssuer;

use super::*;

fn answered(status: u16, body: &Value) -> Exchange {
    Exchange::Answered {
        status: StatusCode::from_u16(status).expect("a status"),
        body: Bytes::from(body.to_string()),
    }
}

fn raw(status: u16, body: &str) -> Exchange {
    Exchange::Answered {
        status: StatusCode::from_u16(status).expect("a status"),
        body: Bytes::from(body.to_owned()),
    }
}

fn role_json() -> Value {
    json!({
        "role": "main",
        "protocol": "anthropic-messages",
        "models": ["anthropic/claude-sonnet-5-5"],
        "configured_models": ["anthropic/claude-opus-5-5"],
        "source": "database",
        "version": 4,
        "updated_at": "2026-10-24T00:00:00.000000Z",
        "updated_by": "user:1",
        "added_later": true
    })
}

fn gateway_error(code: &str, current_version: Option<i64>) -> Value {
    let mut error = json!({"code": code, "message": "m"});
    if let Some(version) = current_version {
        error["current_version"] = json!(version);
    }
    json!({ "error": error })
}

#[test]
fn only_the_status_decides_success() {
    let role: api::Role = read_answer(answered(200, &role_json())).expect("a role");
    assert_eq!(role.version, 4);

    // A 2xx that is not the expected type.
    assert_eq!(
        read_answer::<api::Role>(raw(200, "not json")),
        Err(Failure::UnreadableSuccess { status: 200 })
    );
    assert_eq!(
        read_answer::<api::Role>(answered(201, &json!({"roles": []}))),
        Err(Failure::UnreadableSuccess { status: 201 })
    );

    // A known code with its own status.
    assert_eq!(
        read_answer::<api::Role>(answered(409, &gateway_error("version_conflict", Some(7)))),
        Err(Failure::Refused {
            status: 409,
            code: api::ErrorCode::VersionConflict,
            current_version: Some(7),
        })
    );
    for (status, code, known) in [
        (404, "role_not_found", api::ErrorCode::RoleNotFound),
        (422, "invalid_models", api::ErrorCode::InvalidModels),
        (422, "unknown_provider", api::ErrorCode::UnknownProvider),
        (422, "protocol_mismatch", api::ErrorCode::ProtocolMismatch),
        (503, "unavailable", api::ErrorCode::Unavailable),
        (400, "invalid_actor", api::ErrorCode::InvalidActor),
    ] {
        assert_eq!(
            read_answer::<api::Role>(answered(status, &gateway_error(code, None))),
            Err(Failure::Refused {
                status,
                code: known,
                current_version: None,
            }),
            "{code}"
        );
    }

    // Without an error body this build reads (axum's 405, hyper's 400, the gateway's 500 and
    // 504), with an unknown code, with a code of another status, and a success body under an
    // error status: never a success.
    for (status, body) in [
        (405, String::new()),
        (400, "Bad Request".to_owned()),
        (500, String::new()),
        (
            504,
            json!({"error": {"code": "timeout", "message": "m"}}).to_string(),
        ),
        (400, gateway_error("something_new", None).to_string()),
        (422, gateway_error("role_not_found", None).to_string()),
        (500, role_json().to_string()),
        (302, String::new()),
    ] {
        assert_eq!(
            read_answer::<api::Role>(raw(status, &body)),
            Err(Failure::Unrecognized { status }),
            "{status} {body}"
        );
    }

    assert_eq!(
        read_answer::<api::Role>(Exchange::Lost("the call timed out")),
        Err(Failure::Lost("the call timed out"))
    );
    assert_eq!(
        read_answer::<api::Role>(Exchange::NotSent("no connection")),
        Err(Failure::NotSent("no connection"))
    );
}

/// `(status, code, message key, field errors)` of the M1 answer.
fn answer(failure: &Failure, operation: Operation) -> (u16, String, String, Value, Value) {
    let body = failure.body(operation);
    let status = body.http_status().as_u16();
    let value = serde_json::to_value(body).expect("serialize");
    (
        status,
        value["code"].as_str().expect("code").to_owned(),
        value["message_key"].as_str().expect("key").to_owned(),
        value.get("details").cloned().unwrap_or(Value::Null),
        value.get("field_errors").cloned().unwrap_or(Value::Null),
    )
}

fn refused(status: u16, code: api::ErrorCode, current_version: Option<i64>) -> Failure {
    Failure::Refused {
        status,
        code,
        current_version,
    }
}

#[test]
fn the_gateways_refusals_map_to_m1() {
    for operation in [Operation::Set, Operation::Reset, Operation::List] {
        assert_eq!(
            answer(&refused(404, api::ErrorCode::RoleNotFound, None), operation),
            (
                404,
                "not_found".to_owned(),
                "ai_model_role.not_found".to_owned(),
                Value::Null,
                Value::Null
            )
        );
        assert_eq!(
            answer(
                &refused(409, api::ErrorCode::VersionConflict, Some(9)),
                operation
            ),
            (
                409,
                "conflict".to_owned(),
                "ai_model_role.version_conflict".to_owned(),
                json!({"current_version": 9}),
                Value::Null
            )
        );
        for (code, key) in [
            (
                api::ErrorCode::InvalidModels,
                "ai_model_role.invalid_models",
            ),
            (
                api::ErrorCode::UnknownProvider,
                "ai_model_role.unknown_provider",
            ),
            (
                api::ErrorCode::ProtocolMismatch,
                "ai_model_role.protocol_mismatch",
            ),
        ] {
            assert_eq!(
                answer(&refused(422, code, None), operation),
                (
                    422,
                    "validation_failed".to_owned(),
                    "error.validation_failed".to_owned(),
                    Value::Null,
                    json!([{"field": "/models", "message_key": key}])
                )
            );
        }
        // A refusal of what the service built itself, or nothing sent: nothing changed.
        for failure in [
            refused(400, api::ErrorCode::InvalidRequest, None),
            refused(400, api::ErrorCode::InvalidActor, None),
            refused(413, api::ErrorCode::RequestTooLarge, None),
            refused(404, api::ErrorCode::NotFound, None),
            Failure::NotSent("no connection"),
            Failure::NotSent("too many calls in flight"),
        ] {
            let (status, code, key, ..) = answer(&failure, operation);
            assert_eq!(
                (status, code.as_str(), key.as_str()),
                (503, "unavailable", "error.unavailable"),
                "{failure:?}"
            );
        }
    }
    // An unclear outcome: a change may have taken effect, so the page reads the roles again. A
    // change's answer is 504, never the 503 the ingress retries; a read's is 503.
    for failure in [
        refused(503, api::ErrorCode::Unavailable, None),
        Failure::Lost("the call timed out"),
        Failure::Unrecognized { status: 504 },
        Failure::Unrecognized { status: 500 },
        Failure::UnreadableSuccess { status: 200 },
    ] {
        for (operation, expected) in [
            (
                Operation::Set,
                (504, "timeout", "ai_model_role.outcome_unknown"),
            ),
            (
                Operation::Reset,
                (504, "timeout", "ai_model_role.outcome_unknown"),
            ),
            (Operation::List, (503, "unavailable", "error.unavailable")),
        ] {
            let (status, code, key, details, fields) = answer(&failure, operation);
            assert_eq!(
                (status, code.as_str(), key.as_str()),
                expected,
                "{failure:?} {operation:?}"
            );
            assert_eq!((details, fields), (Value::Null, Value::Null));
        }
    }
}

#[test]
fn the_failures_are_logged_by_severity_and_kind_only() {
    assert_eq!(
        refused(409, api::ErrorCode::VersionConflict, Some(1)).severity(),
        Severity::Expected
    );
    assert_eq!(
        refused(503, api::ErrorCode::Unavailable, None).severity(),
        Severity::Unavailable
    );
    assert_eq!(
        Failure::NotSent("too many calls in flight").severity(),
        Severity::Unavailable
    );
    for failure in [
        Failure::NotSent("no connection"),
        Failure::Lost("the call timed out"),
        Failure::Unrecognized { status: 500 },
        Failure::UnreadableSuccess { status: 200 },
        refused(400, api::ErrorCode::InvalidActor, None),
    ] {
        assert_eq!(failure.severity(), Severity::Fault, "{failure:?}");
    }
    assert_eq!(
        refused(422, api::ErrorCode::ProtocolMismatch, None).kind(),
        "protocol_mismatch"
    );
    assert_eq!(Failure::Unrecognized { status: 500 }.status(), Some(500));
}

#[test]
fn the_gateways_roles_become_the_api_types() {
    let role: api::Role = serde_json::from_value(role_json()).expect("a role");
    let role = ModelRole::from(role);
    assert_eq!(
        serde_json::to_value(&role).expect("serialize"),
        json!({
            "role": "main",
            "protocol": "anthropic-messages",
            "models": ["anthropic/claude-sonnet-5-5"],
            "configured_models": ["anthropic/claude-opus-5-5"],
            "source": "database",
            "version": 4,
            "ignored": null,
            "updated_at": "2026-10-24T00:00:00Z",
            "updated_by": "user:1"
        })
    );

    let roles: api::Roles = serde_json::from_value(json!({
        "roles": [{
            "role": "router",
            "protocol": "openai-decisions",
            "models": ["openai/gpt-6-luna"],
            "configured_models": ["openai/gpt-6-luna"],
            "source": "configuration",
            "version": 3,
            "ignored": {"models": ["gone/model"], "reason": "provider gone is not configured"},
            "updated_at": "not a time",
            "updated_by": null
        }],
        "providers": [{"name": "openai", "protocol": "openai-decisions"}]
    }))
    .expect("roles");
    assert_eq!(
        serde_json::to_value(ModelRoles::from(roles)).expect("serialize"),
        json!({
            "roles": [{
                "role": "router",
                "protocol": "openai-decisions",
                "models": ["openai/gpt-6-luna"],
                "configured_models": ["openai/gpt-6-luna"],
                "source": "configuration",
                "version": 3,
                "ignored": {"models": ["gone/model"], "reason": "provider gone is not configured"},
                "updated_at": null,
                "updated_by": null
            }],
            "providers": [{"name": "openai", "protocol": "openai-decisions"}]
        })
    );
}

#[test]
fn the_body_is_one_json_object_of_two_fields() {
    let parsed: SetRoleModels =
        json_object(br#"{"models": ["a/b"], "expected_version": 2, "extra": 1}"#)
            .expect("unknown fields are ignored");
    assert_eq!(
        parsed,
        SetRoleModels {
            models: vec!["a/b".to_owned()],
            expected_version: 2,
        }
    );
    for refused in [
        r#"[["a/b"], 2]"#,
        r#"{"models": "a/b", "expected_version": 2}"#,
        r#"{"models": [1], "expected_version": 2}"#,
        r#"{"models": ["a/b"], "expected_version": 2.5}"#,
        r#"{"models": ["a/b"], "expected_version": "2"}"#,
        r#"{"models": ["a/b"]}"#,
        r#"{"expected_version": 2}"#,
        r#"{"models": ["a/b"], "expected_version": 2, "expected_version": 3}"#,
        "",
    ] {
        assert!(
            json_object::<SetRoleModels>(refused.as_bytes()).is_none(),
            "{refused}"
        );
    }
}

fn claims(platform_admin: bool, act: Option<ServiceName>) -> InternalClaims {
    let mut claims = TestIssuer::claims(ServiceName::Identity, None);
    claims.platform_admin = platform_admin;
    claims.act = act.map(|sub| TokenActor { sub });
    claims
}

#[test]
fn only_a_platform_administrators_own_request_passes() {
    let issuer = TestIssuer::new();
    let user = |claims: &InternalClaims| issuer.user(ServiceName::Identity, claims);
    let admin = user(&claims(true, None));
    assert!(refuse_caller(&admin, Operation::Set).is_none());
    assert_eq!(actor(&admin), format!("user:{}", admin.user_id()));
    assert!(api::is_valid_actor(&actor(&admin)));

    let mut tenant_admin = claims(false, None);
    tenant_admin.tenant = Some(TenantGrant {
        id: TenantId::generate(),
        role: TenantRole::Admin,
    });
    let mut exchanged = claims(true, Some(ServiceName::Assistant));
    exchanged.sid = None;
    for (what, claims) in [
        ("a tenant administrator", tenant_admin),
        ("a user", claims(false, None)),
        ("an exchanged token", exchanged),
    ] {
        for operation in [Operation::List, Operation::Set, Operation::Reset] {
            let refusal = refuse_caller(&user(&claims), operation).expect(what);
            assert_eq!(refusal.status(), StatusCode::FORBIDDEN, "{what}");
        }
    }
}

#[test]
fn a_change_must_be_a_same_site_json_request() {
    let issuer = TestIssuer::new();
    let admin = issuer.user(ServiceName::Identity, &claims(true, None));
    let headers = |pairs: &[(&'static str, &'static str)]| {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, HeaderValue::from_static(value));
        }
        headers
    };
    let json = headers(&[
        ("content-type", "application/json"),
        ("sec-fetch-site", "same-origin"),
    ]);
    for method in [Method::PUT, Method::DELETE] {
        assert!(refuse_write(&method, &json, &admin, Operation::Set).is_none());
        for (what, refused, status) in [
            (
                "cross-site",
                headers(&[
                    ("content-type", "application/json"),
                    ("sec-fetch-site", "cross-site"),
                ]),
                StatusCode::FORBIDDEN,
            ),
            (
                "another origin",
                headers(&[
                    ("content-type", "application/json"),
                    ("host", "app.example.test"),
                    ("origin", "https://attacker.example.test"),
                ]),
                StatusCode::FORBIDDEN,
            ),
            (
                "a form",
                headers(&[("content-type", "application/x-www-form-urlencoded")]),
                StatusCode::BAD_REQUEST,
            ),
            ("no content type", headers(&[]), StatusCode::BAD_REQUEST),
        ] {
            let refusal = refuse_write(&method, &refused, &admin, Operation::Set).expect(what);
            assert_eq!(refusal.status(), status, "{method} {what}");
        }
    }
}

#[test]
fn role_names_are_checked_before_anything_is_sent() {
    assert_eq!(
        role_name(Ok(Path("main".to_owned()))).expect("a role name"),
        "main"
    );
    for name in ["Main", "../roles", "a/b", "", "2nd"] {
        let refusal = role_name(Ok(Path(name.to_owned()))).expect_err(name);
        assert_eq!(refusal.status(), StatusCode::NOT_FOUND, "{name}");
    }
}

#[test]
fn the_spec_lists_the_routes_and_their_answers() {
    let spec = serde_json::to_value(crate::openapi()).expect("the spec");
    for (method, operation_id, statuses) in [
        (
            "get",
            "listPlatformAiModelRoles",
            &["200", "401", "403", "503"][..],
        ),
        (
            "put",
            "setPlatformAiModelRole",
            &[
                "200", "400", "401", "403", "404", "409", "422", "503", "504",
            ][..],
        ),
        (
            "delete",
            "resetPlatformAiModelRole",
            &["200", "400", "401", "403", "404", "409", "503", "504"][..],
        ),
    ] {
        let path = if method == "get" {
            MODEL_ROLES_PATH
        } else {
            MODEL_ROLE_PATH
        };
        let operation = &spec["paths"][path][method];
        assert_eq!(operation["operationId"], operation_id);
        for status in statuses {
            assert!(
                operation["responses"][status].is_object(),
                "{method} {status} is documented"
            );
        }
    }
    // Every field of a role is in every answer, `null` or not.
    for name in ["ModelRole", "ModelRoles", "ModelProvider", "IgnoredModels"] {
        let schema = &spec["components"]["schemas"][name];
        let mut properties: Vec<&String> = schema["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .collect();
        let mut required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required")
            .iter()
            .map(|field| field.as_str().expect("a field name"))
            .collect();
        properties.sort();
        required.sort_unstable();
        assert_eq!(properties, required, "{name}");
    }
}
