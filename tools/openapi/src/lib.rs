//! Generates `api/openapi.json`: the spec every service exposes from code, merged with the shared
//! M1 error schemas into one deterministic document that is committed to the repository.
//!
//! Run `cargo run -p openapi` to regenerate it and `cargo run -p openapi -- --check` to verify it
//! is current. CI regenerates the file and fails on any difference, and `tests/openapi.rs` does
//! the same comparison under `cargo test --workspace`.
//!
//! # Adding a service
//!
//! A service that serves an HTTP API exposes its spec from its library target:
//! `pub fn openapi() -> utoipa::openapi::OpenApi`, usually `ApiDoc::openapi()` of a
//! `#[derive(OpenApi)]` type that lists its handlers and schemas. Then add the crate to this
//! crate's `Cargo.toml` and one line to [`SERVICES`]. [`check_service_dirs`] fails the tests
//! when a directory under `services/` is in neither [`SERVICES`] nor [`SERVICES_WITHOUT_API`],
//! so a new service cannot be forgotten.

mod merge;

use std::collections::BTreeSet;

use serde_json::Value;
use utoipa::openapi::OpenApi;

pub use merge::{Document, MergeError, canonicalize, merge};

/// The title of the merged document.
pub const API_TITLE: &str = "TrustIoT API";

/// The version of the merged document. It is the version of the API contract, not of the
/// software: raise it deliberately when the contract changes in a way clients must notice, so
/// that a release does not by itself change `api/openapi.json`.
pub const API_VERSION: &str = "0.1.0";

/// One source of OpenAPI: a name for error messages and the function that builds its document.
pub struct Source {
    /// For a service, its directory name under `services/`.
    pub name: &'static str,
    pub openapi: fn() -> OpenApi,
}

/// The sources that are not services and go into every generated document: the M1 error format
/// from `crates/contract`.
pub const SHARED: &[Source] = &[Source {
    name: "contract",
    openapi: shared_schemas,
}];

/// The services that expose a spec, in no particular order (the output is sorted).
pub const SERVICES: &[Source] = &[
    // The tenant API (Laysi/trustiot-v2#65); the ext_authz check under /internal is not part of
    // the client API. Login and logout since Laysi/trustiot-v2#103.
    Source {
        name: "identity",
        openapi: identity::openapi,
    },
    // The MS0 example API (Laysi/trustiot-v2#65).
    Source {
        name: "assets",
        openapi: assets::openapi,
    },
    // The CA (Laysi/trustiot-v2#78): the platform administrators' workstation certificate API
    // (Laysi/trustiot-v2#534). Its CRLs and CA chain under /internal, for the MQTT broker, are not
    // part of the client API.
    Source {
        name: "certs",
        openapi: certs::openapi,
    },
    // The history API of device charts (Laysi/trustiot-v2#119).
    Source {
        name: "telemetry-query",
        openapi: telemetry_query::openapi,
    },
];

/// Directories under `services/` that have no HTTP API of their own to document (workers, for
/// example). Listing one here is a decision to review.
pub const SERVICES_WITHOUT_API: &[&str] = &[
    // Takes gateway telemetry from MQTT and writes it to ClickHouse; no HTTP API besides the
    // health endpoints (docs/architecture/deployment.md). It talks to gateways over MQTT and to
    // the assets service through its events (Laysi/trustiot-v2#640).
    "ingest",
    // Keeps each gateway's desired configuration, generated from the assets service's events
    // (Laysi/trustiot-v2#719); no HTTP API besides the health endpoints. It delivers the
    // configurations to gateways over MQTT (Laysi/trustiot-v2#720).
    "gateway-ops",
    // Serves the SPA's static files (frontend/) on the app host; no API besides the health
    // endpoints (Laysi/trustiot-v2#123, services/web/README.md).
    "web",
    // The AI gateway (Laysi/trustiot-v2#139, decision 28): its internal API under /internal takes
    // the AI providers' own protocols (Anthropic Messages, OpenAI Decisions) and returns their
    // responses unchanged, so the providers' API references describe it; the gateway's part
    // (paths, headers, errors) is in services/ai-gateway/README.md. It does not depend on
    // crates/contract, whose error types every spec here shares, and the client API never
    // reaches it (decision 27).
    "ai-gateway",
    // The AI assistant's agent loop (Laysi/trustiot-v2#145, decision 31). It serves only the
    // health endpoints until the chat API (Laysi/trustiot-v2#146) adds its routes and its
    // `openapi()`; move it to `SERVICES` then.
    "assistant",
    // Relays the gateways' MQTT inside WebSocket to the broker as TCP (Laysi/trustiot-v2#85,
    // decision 6); its only endpoint is the WebSocket upgrade on the public path, described in
    // services/mqtt-bridge/README.md, and no HTTP API besides the health endpoints.
    "mqtt-bridge",
];

fn shared_schemas() -> OpenApi {
    use utoipa::OpenApi as _;
    contract::openapi::SharedSchemas::openapi()
}

/// Why generation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the OpenAPI document of {name} does not serialize: {source}")]
    Serialize {
        name: String,
        source: serde_json::Error,
    },
    #[error(transparent)]
    Merge(#[from] MergeError),
}

/// The merged document of `sources` as the text that is committed: two-space indentation, keys in
/// sorted order, one trailing newline.
pub fn generate_from(sources: &[&Source]) -> Result<String, Error> {
    let mut documents = Vec::with_capacity(sources.len());
    for source in sources {
        let value = serde_json::to_value((source.openapi)()).map_err(|error| Error::Serialize {
            name: source.name.to_owned(),
            source: error,
        })?;
        documents.push(Document {
            name: source.name.to_owned(),
            value,
        });
    }
    let merged = merge(&documents, API_TITLE, API_VERSION)?;
    Ok(render(&merged))
}

/// The text of `api/openapi.json` for the current code.
pub fn generate() -> Result<String, Error> {
    let sources: Vec<&Source> = SHARED.iter().chain(SERVICES).collect();
    generate_from(&sources)
}

/// The committed text of a merged document.
pub fn render(document: &Value) -> String {
    // Serializing a `Value` cannot fail: every key is a string.
    let mut text = serde_json::to_string_pretty(document).unwrap_or_default();
    text.push('\n');
    text
}

/// Compares the committed document with the generated one.
///
/// # Errors
///
/// A message that names the first differing line and how to regenerate.
pub fn check(committed: &str, generated: &str) -> Result<(), String> {
    if committed == generated {
        return Ok(());
    }
    let mut committed_lines = committed.lines();
    let mut generated_lines = generated.lines();
    let mut line = 1_usize;
    let detail = loop {
        match (committed_lines.next(), generated_lines.next()) {
            (Some(old), Some(new)) if old == new => line += 1,
            (old, new) => {
                break format!(
                    "first difference at line {line}:\n  committed: {}\n  generated: {}",
                    old.unwrap_or("(end of file)"),
                    new.unwrap_or("(end of file)")
                );
            }
        }
    };
    Err(format!(
        "api/openapi.json is out of date with the code; run `cargo run -p openapi` and commit \
         the result.\n{detail}"
    ))
}

/// Checks that every service directory is either in `registered` (it exposes a spec) or in
/// `without_api`, and that neither list names a directory that does not exist.
///
/// # Errors
///
/// One message per problem.
pub fn check_service_dirs(
    dirs: &[String],
    registered: &[&str],
    without_api: &[&str],
) -> Result<(), Vec<String>> {
    let existing: BTreeSet<&str> = dirs.iter().map(String::as_str).collect();
    let mut problems = Vec::new();
    for dir in &existing {
        let listed = registered.contains(dir) as u8 + without_api.contains(dir) as u8;
        match listed {
            0 => problems.push(format!(
                "services/{dir} is not in SERVICES (it exposes an OpenAPI spec) or in \
                 SERVICES_WITHOUT_API (it has no HTTP API); see tools/openapi/src/lib.rs"
            )),
            1 => {}
            _ => problems.push(format!(
                "services/{dir} is in both SERVICES and SERVICES_WITHOUT_API"
            )),
        }
    }
    for name in registered.iter().chain(without_api) {
        if !existing.contains(name) {
            problems.push(format!(
                "`{name}` is listed in tools/openapi/src/lib.rs but there is no services/{name}"
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_names_the_first_difference() {
        assert_eq!(check("a\nb\n", "a\nb\n"), Ok(()));
        let error = check("a\nb\n", "a\nc\n").expect_err("differs");
        assert!(error.contains("line 2"), "{error}");
        assert!(error.contains("committed: b"), "{error}");
        assert!(error.contains("generated: c"), "{error}");
        assert!(check("a\n", "a\nb\n").is_err());
        assert!(check("a\nb\n", "a\n").is_err());
    }

    #[test]
    fn service_dirs_must_be_listed_exactly_once() {
        let dirs = vec!["identity".to_owned(), "worker".to_owned()];
        assert_eq!(
            check_service_dirs(&dirs, &["identity"], &["worker"]),
            Ok(())
        );
        let problems = check_service_dirs(&dirs, &["identity"], &[]).expect_err("worker missing");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("services/worker"), "{problems:?}");
        let problems =
            check_service_dirs(&dirs, &["identity", "ghost"], &["worker"]).expect_err("stale");
        assert!(problems[0].contains("no services/ghost"), "{problems:?}");
        let problems = check_service_dirs(&dirs, &["identity", "worker"], &["worker"])
            .expect_err("both lists");
        assert!(problems[0].contains("both"), "{problems:?}");
    }

    #[test]
    fn generation_is_deterministic_and_ends_with_a_newline() {
        let first = generate().expect("generate");
        assert_eq!(first, generate().expect("generate again"));
        assert!(first.ends_with("}\n"));
    }
}
