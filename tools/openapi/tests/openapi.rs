//! Applies the generator to this repository: the committed `api/openapi.json` must be what the
//! code generates now. This is the check that fails when an API type changes without
//! regenerating the spec (CI also runs the generator and diffs the file).

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn committed() -> String {
    let path = repo_root().join("api/openapi.json");
    fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "read {}: {error}; create it with `cargo run -p openapi`",
            path.display()
        )
    })
}

#[test]
fn committed_spec_is_what_the_code_generates() {
    let generated = openapi::generate().expect("generate");
    if let Err(message) = openapi::check(&committed(), &generated) {
        panic!("{message}");
    }
}

/// Proves the check above can fail: a spec that differs from the code, as after changing an API
/// type without regenerating, is rejected.
#[test]
fn a_stale_spec_is_rejected() {
    let generated = openapi::generate().expect("generate");
    let mut document: Value = serde_json::from_str(&generated).expect("generated JSON");
    document["components"]["schemas"]["ErrorBody"]["properties"]["trace_id"] =
        json!({"type": "string"});
    let stale = openapi::render(&document);
    let error = openapi::check(&stale, &generated).expect_err("a stale spec must fail");
    assert!(error.contains("cargo run -p openapi"), "{error}");
}

#[test]
fn spec_contains_the_m1_error_schemas() {
    let document: Value = serde_json::from_str(&committed()).expect("committed spec is JSON");
    let schemas = &document["components"]["schemas"];
    for name in ["ErrorBody", "ErrorCode", "FieldError", "MessageKey"] {
        assert!(schemas.get(name).is_some(), "schema {name} is missing");
    }
    let codes: Vec<&str> = contract::ErrorCode::ALL
        .iter()
        .map(|code| code.as_str())
        .collect();
    assert_eq!(schemas["ErrorCode"]["enum"], json!(codes));
    assert_eq!(
        schemas["ErrorBody"]["required"],
        json!(["code", "message_key"])
    );
}

#[test]
fn every_service_directory_is_registered() {
    let services = repo_root().join("services");
    let dirs = service_dirs(&services);
    let registered: Vec<&str> = openapi::SERVICES.iter().map(|source| source.name).collect();
    if let Err(problems) =
        openapi::check_service_dirs(&dirs, &registered, openapi::SERVICES_WITHOUT_API)
    {
        panic!("{}", problems.join("\n"));
    }
}

/// The directories of `services/` that are Rust packages (README.md and the like are skipped).
fn service_dirs(services: &Path) -> Vec<String> {
    let mut dirs: Vec<String> = fs::read_dir(services)
        .expect("read services/")
        .map(|entry| entry.expect("read an entry of services/").path())
        .filter(|path| path.join("Cargo.toml").is_file())
        .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
        .collect();
    dirs.sort();
    dirs
}

/// A stand-in for a service: a handler documented with utoipa whose error response refers to the
/// shared M1 schema, which the service does not declare itself.
mod fake_service {
    use contract::ErrorBody;
    use utoipa::{OpenApi, ToSchema};

    #[derive(ToSchema)]
    #[allow(dead_code, reason = "only the generated schema is inspected")]
    pub struct Widget {
        pub name: String,
    }

    #[utoipa::path(
        get,
        path = "/v1/widgets",
        operation_id = "widgets_list",
        responses(
            (status = 200, body = Vec<Widget>),
            (status = 403, body = ErrorBody),
        )
    )]
    #[allow(dead_code, reason = "only the generated path item is inspected")]
    fn list() {}

    #[derive(OpenApi)]
    #[openapi(paths(list), components(schemas(Widget)))]
    struct Api;

    pub fn openapi() -> utoipa::openapi::OpenApi {
        Api::openapi()
    }
}

#[test]
fn a_service_spec_merges_with_the_shared_error_schema() {
    let service = openapi::Source {
        name: "widgets",
        openapi: fake_service::openapi,
    };
    let sources: Vec<&openapi::Source> = openapi::SHARED.iter().chain([&service]).collect();
    let text = openapi::generate_from(&sources).expect("generate");
    let document: Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(
        document["paths"]["/v1/widgets"]["get"]["operationId"],
        json!("widgets_list")
    );
    assert!(document["components"]["schemas"].get("Widget").is_some());
    // The service's reference resolves to the shared schema, which the service did not declare.
    assert_eq!(
        document["paths"]["/v1/widgets"]["get"]["responses"]["403"]["content"]["application/json"]
            ["schema"]["$ref"],
        json!("#/components/schemas/ErrorBody")
    );
    assert!(document["components"]["schemas"].get("ErrorBody").is_some());
    // Adding a service changes the generated spec, so the committed one is stale until
    // regenerated.
    assert!(openapi::check(&committed(), &text).is_err());
}
