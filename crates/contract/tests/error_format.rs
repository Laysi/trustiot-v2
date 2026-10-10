//! The M1 error body: JSON round trips, the exact wire format, and the OpenAPI schema.

mod common;

use contract::{ErrorBody, ErrorCode, FieldError, MessageKey, message_key};
use serde_json::{Value, json};
use utoipa::OpenApi;

fn validation_error() -> ErrorBody {
    ErrorBody::new(ErrorCode::ValidationFailed)
        .with_detail("model", "ms300")
        .with_field_error(
            FieldError::new("/name", message_key!("device_model.name_invalid"))
                .with_detail("max", 40),
        )
        .with_field_error(FieldError::new(
            "/points/3/address",
            message_key!("device_model.address_overlap"),
        ))
}

#[test]
fn every_code_round_trips_with_its_default_key() {
    for code in ErrorCode::ALL {
        let body = ErrorBody::new(code);
        let text = serde_json::to_string(&body).expect("serialize");
        assert_eq!(
            text,
            format!(
                r#"{{"code":"{0}","message_key":"error.{0}"}}"#,
                code.as_str()
            )
        );
        let parsed: ErrorBody = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(parsed, body);
        assert_eq!(parsed.http_status(), code.http_status());
    }
}

#[test]
fn full_body_round_trips_in_the_documented_shape() {
    let body = validation_error();
    let value = serde_json::to_value(&body).expect("serialize");
    assert_eq!(
        value,
        json!({
            "code": "validation_failed",
            "message_key": "error.validation_failed",
            "details": {"model": "ms300"},
            "field_errors": [
                {"field": "/name", "message_key": "device_model.name_invalid", "details": {"max": 40}},
                {"field": "/points/3/address", "message_key": "device_model.address_overlap"}
            ]
        })
    );
    let parsed: ErrorBody = serde_json::from_value(value).expect("deserialize");
    assert_eq!(parsed, body);
    assert_eq!(parsed.field_errors().len(), 2);
    assert_eq!(parsed.details()["model"], json!("ms300"));
}

#[test]
fn specific_message_keys_round_trip() {
    let body = ErrorBody::new(ErrorCode::Conflict)
        .with_message_key(MessageKey::new("gateway.already_bound"))
        .with_detail("hardware_id", "gw-0042");
    let text = serde_json::to_string(&body).expect("serialize");
    assert_eq!(
        text,
        r#"{"code":"conflict","message_key":"gateway.already_bound","details":{"hardware_id":"gw-0042"}}"#
    );
    assert_eq!(
        serde_json::from_str::<ErrorBody>(&text).expect("deserialize"),
        body
    );
}

#[test]
fn unknown_fields_from_newer_peers_are_ignored() {
    let parsed: ErrorBody = serde_json::from_value(json!({
        "code": "not_found",
        "message_key": "error.not_found",
        "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736"
    }))
    .expect("deserialize");
    assert_eq!(parsed, ErrorBody::new(ErrorCode::NotFound));
}

#[test]
fn malformed_bodies_are_rejected() {
    for value in [
        json!({"code": "teapot", "message_key": "error.teapot"}),
        json!({"code": "not_found", "message_key": "Not found"}),
        json!({"code": "not_found"}),
        json!({"message_key": "error.not_found"}),
        json!({"code": "not_found", "message_key": "error.not_found", "field_errors": [{"field": "/x"}]}),
    ] {
        assert!(
            serde_json::from_value::<ErrorBody>(value.clone()).is_err(),
            "{value}"
        );
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(ErrorBody, ErrorCode, FieldError, MessageKey)))]
struct ErrorSchemas;

fn error_schemas() -> Value {
    serde_json::to_value(ErrorSchemas::openapi()).expect("OpenAPI document serializes")["components"]
        ["schemas"]
        .clone()
}

#[test]
fn openapi_schema_describes_the_error_body() {
    let schemas = error_schemas();

    let body = &schemas["ErrorBody"];
    assert_eq!(body["type"], json!("object"));
    assert_eq!(body["required"], json!(["code", "message_key"]));
    assert_eq!(
        body["properties"]["code"]["$ref"],
        json!("#/components/schemas/ErrorCode")
    );
    assert_eq!(
        body["properties"]["message_key"]["$ref"],
        json!("#/components/schemas/MessageKey")
    );
    assert_eq!(body["properties"]["details"]["type"], json!("object"));
    assert_eq!(body["properties"]["field_errors"]["type"], json!("array"));
    assert_eq!(
        body["properties"]["field_errors"]["items"]["$ref"],
        json!("#/components/schemas/FieldError")
    );

    let codes: Vec<&str> = ErrorCode::ALL.iter().map(|code| code.as_str()).collect();
    assert_eq!(schemas["ErrorCode"]["type"], json!("string"));
    assert_eq!(schemas["ErrorCode"]["enum"], json!(codes));

    assert_eq!(schemas["MessageKey"]["type"], json!("string"));
    assert_eq!(
        schemas["MessageKey"]["pattern"],
        json!(r"^[a-z0-9_]+(\.[a-z0-9_]+)+$")
    );
    assert_eq!(
        schemas["FieldError"]["required"],
        json!(["field", "message_key"])
    );
}

/// The generated schema is part of the contract: a change must show up in review. Regenerate
/// the fixture with `CONTRACT_UPDATE_FIXTURES=1 cargo test -p contract --test error_format`.
#[test]
fn openapi_schema_matches_the_fixture() {
    common::assert_json_fixture("openapi/m1-error-schemas.json", &error_schemas());
}
