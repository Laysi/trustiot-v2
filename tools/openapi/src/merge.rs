//! Merging several OpenAPI documents into one, deterministically and without silently losing
//! anything: a collision is an error that names both sources.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

/// HTTP method keys of a path item; each is an operation that exactly one source may define.
const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// Top-level keys a source may carry. `info` is dropped (the merged document has its own); a
/// source that sets any other key (`servers`, `security`, `webhooks`, ...) would change what the
/// merged document means for every other service, so it is rejected instead of guessed at.
const ACCEPTED_KEYS: [&str; 5] = ["openapi", "info", "paths", "components", "tags"];

/// Why documents cannot be merged.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum MergeError {
    #[error("{source_name}: the document is not a JSON object")]
    NotAnObject { source_name: String },
    #[error("{source_name} uses OpenAPI {found} but {first} uses {expected}")]
    VersionMismatch {
        source_name: String,
        found: String,
        first: String,
        expected: String,
    },
    #[error(
        "{source_name} sets the top-level key `{key}`, which a merged spec cannot take from one service"
    )]
    UnsupportedKey { source_name: String, key: String },
    #[error("{source_name}: `{place}` is malformed: {reason}")]
    Malformed {
        source_name: String,
        place: String,
        reason: &'static str,
    },
    #[error("{first} and {second} both define `{item}`")]
    Collision {
        first: String,
        second: String,
        item: String,
    },
    #[error(
        "operationId `{id}` is used by {first} and {second}; ids must be unique for client generation"
    )]
    DuplicateOperationId {
        id: String,
        first: String,
        second: String,
    },
    #[error("`{reference}` is referenced but not defined in any source")]
    DanglingRef { reference: String },
}

/// One input of the merge: a name for error messages and the document as JSON.
pub struct Document {
    pub name: String,
    pub value: Value,
}

/// A path item under construction, and which source supplied each of its keys.
#[derive(Default)]
struct PathEntry {
    item: Map<String, Value>,
    owners: BTreeMap<String, String>,
}

/// Merges `documents` (in the given order, which only matters for error messages) into one
/// document with the given `title` and `version`.
///
/// - `paths`: a path may be shared between sources only for different methods; the same method
///   on the same path is a [`MergeError::Collision`].
/// - `components`: a name defined by several sources must have an identical definition.
/// - `tags`: merged by name; the same name with a different definition is a collision.
/// - Every `operationId` must be unique, and every `#/components/...` reference must resolve.
///
/// The result has all object keys in sorted order and every list of tags sorted by name, so the
/// same input always produces the same text.
pub fn merge(documents: &[Document], title: &str, version: &str) -> Result<Value, MergeError> {
    let openapi_version = openapi_version(documents)?;
    let mut paths: BTreeMap<String, PathEntry> = BTreeMap::new();
    let mut components: BTreeMap<String, BTreeMap<String, (String, Value)>> = BTreeMap::new();
    let mut tags: BTreeMap<String, (String, Value)> = BTreeMap::new();
    let mut operation_ids: BTreeMap<String, String> = BTreeMap::new();

    for document in documents {
        let name = document.name.as_str();
        let object = document
            .value
            .as_object()
            .ok_or_else(|| MergeError::NotAnObject {
                source_name: name.to_owned(),
            })?;
        for (key, value) in object {
            if !ACCEPTED_KEYS.contains(&key.as_str()) && !is_empty(value) {
                return Err(MergeError::UnsupportedKey {
                    source_name: name.to_owned(),
                    key: key.clone(),
                });
            }
        }
        for (path, item) in entries(object, "paths", name)? {
            let item = item.as_object().ok_or_else(|| MergeError::Malformed {
                source_name: name.to_owned(),
                place: format!("paths.{path}"),
                reason: "a path item must be an object",
            })?;
            let entry = paths.entry(path.clone()).or_default();
            for (key, value) in item {
                if let Some(existing) = entry.item.get(key) {
                    // A non-operation key (summary, parameters, ...) shared by identical
                    // definitions is harmless; anything else is a routing collision.
                    if !METHODS.contains(&key.as_str()) && existing == value {
                        continue;
                    }
                    return Err(MergeError::Collision {
                        first: entry.owners.get(key).cloned().unwrap_or_default(),
                        second: name.to_owned(),
                        item: item_label(path, key),
                    });
                }
                if METHODS.contains(&key.as_str())
                    && let Some(id) = value.get("operationId").and_then(Value::as_str)
                    && let Some(first) = operation_ids.insert(id.to_owned(), name.to_owned())
                {
                    return Err(MergeError::DuplicateOperationId {
                        id: id.to_owned(),
                        first,
                        second: name.to_owned(),
                    });
                }
                entry.owners.insert(key.clone(), name.to_owned());
                entry.item.insert(key.clone(), value.clone());
            }
        }
        if let Some(sections) = object.get("components").filter(|value| !is_empty(value)) {
            let sections = sections.as_object().ok_or_else(|| MergeError::Malformed {
                source_name: name.to_owned(),
                place: "components".to_owned(),
                reason: "must be an object",
            })?;
            for (section, definitions) in sections {
                let definitions = definitions
                    .as_object()
                    .ok_or_else(|| MergeError::Malformed {
                        source_name: name.to_owned(),
                        place: format!("components.{section}"),
                        reason: "must be an object",
                    })?;
                let merged = components.entry(section.clone()).or_default();
                for (definition, value) in definitions {
                    match merged.get(definition) {
                        Some((_, existing)) if existing == value => {}
                        Some((first, _)) => {
                            return Err(MergeError::Collision {
                                first: first.clone(),
                                second: name.to_owned(),
                                item: format!("components.{section}.{definition} (differently)"),
                            });
                        }
                        None => {
                            merged.insert(definition.clone(), (name.to_owned(), value.clone()));
                        }
                    }
                }
            }
        }
        if let Some(list) = object.get("tags").filter(|value| !is_empty(value)) {
            let list = list.as_array().ok_or_else(|| MergeError::Malformed {
                source_name: name.to_owned(),
                place: "tags".to_owned(),
                reason: "must be an array",
            })?;
            for tag in list {
                let tag_name = tag.get("name").and_then(Value::as_str).ok_or_else(|| {
                    MergeError::Malformed {
                        source_name: name.to_owned(),
                        place: "tags".to_owned(),
                        reason: "every tag needs a string `name`",
                    }
                })?;
                match tags.get(tag_name) {
                    Some((_, existing)) if existing == tag => {}
                    Some((first, _)) => {
                        return Err(MergeError::Collision {
                            first: first.clone(),
                            second: name.to_owned(),
                            item: format!("tag `{tag_name}` (differently)"),
                        });
                    }
                    None => {
                        tags.insert(tag_name.to_owned(), (name.to_owned(), tag.clone()));
                    }
                }
            }
        }
    }

    let mut merged = json!({
        "openapi": openapi_version,
        "info": {"title": title, "version": version},
        "paths": paths
            .into_iter()
            .map(|(path, entry)| (path, Value::Object(entry.item)))
            .collect::<Map<String, Value>>(),
    });
    if let Some(root) = merged.as_object_mut() {
        if !components.is_empty() {
            let sections: Map<String, Value> = components
                .into_iter()
                .map(|(section, definitions)| {
                    let definitions: Map<String, Value> = definitions
                        .into_iter()
                        .map(|(definition, (_, value))| (definition, value))
                        .collect();
                    (section, Value::Object(definitions))
                })
                .collect();
            root.insert("components".to_owned(), Value::Object(sections));
        }
        if !tags.is_empty() {
            let list = tags.into_values().map(|(_, tag)| tag).collect();
            root.insert("tags".to_owned(), Value::Array(list));
        }
    }
    let merged = canonicalize(merged);
    check_references(&merged)?;
    Ok(merged)
}

fn openapi_version(documents: &[Document]) -> Result<String, MergeError> {
    let mut first: Option<(&str, String)> = None;
    for document in documents {
        let found = document
            .value
            .get("openapi")
            .and_then(Value::as_str)
            .ok_or_else(|| MergeError::Malformed {
                source_name: document.name.clone(),
                place: "openapi".to_owned(),
                reason: "the OpenAPI version string is missing",
            })?;
        match &first {
            None => first = Some((&document.name, found.to_owned())),
            Some((first_name, expected)) if expected != found => {
                return Err(MergeError::VersionMismatch {
                    source_name: document.name.clone(),
                    found: found.to_owned(),
                    first: (*first_name).to_owned(),
                    expected: expected.clone(),
                });
            }
            Some(_) => {}
        }
    }
    first
        .map(|(_, version)| version)
        .ok_or_else(|| MergeError::Malformed {
            source_name: "(none)".to_owned(),
            place: "documents".to_owned(),
            reason: "there is nothing to merge",
        })
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(items) => items.is_empty(),
        Value::Object(fields) => fields.is_empty(),
        _ => false,
    }
}

/// The entries of an optional object member such as `paths`.
fn entries<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    source_name: &str,
) -> Result<Vec<(&'a String, &'a Value)>, MergeError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(fields)) => Ok(fields.iter().collect()),
        Some(_) => Err(MergeError::Malformed {
            source_name: source_name.to_owned(),
            place: key.to_owned(),
            reason: "must be an object",
        }),
    }
}

fn item_label(path: &str, key: &str) -> String {
    if METHODS.contains(&key) {
        format!("{} {path}", key.to_uppercase())
    } else {
        format!("`{key}` of path {path} (differently)")
    }
}

/// Rebuilds every object with its keys in sorted order. Lists keep their order: it is part of
/// the meaning of `required`, `enum` and `oneOf`.
pub fn canonicalize(value: Value) -> Value {
    match value {
        Value::Object(fields) => {
            let sorted: BTreeMap<String, Value> = fields
                .into_iter()
                .map(|(key, value)| (key, canonicalize(value)))
                .collect();
            // Inserting in sorted order is what keeps the order when serde_json's
            // `preserve_order` feature is switched on by some dependency.
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonicalize).collect()),
        other => other,
    }
}

/// Every `$ref` into `#/components/` must point at something the merged document defines.
fn check_references(root: &Value) -> Result<(), MergeError> {
    fn walk(root: &Value, value: &Value) -> Result<(), MergeError> {
        match value {
            Value::Object(fields) => {
                for (key, child) in fields {
                    if key == "$ref"
                        && let Some(reference) = child.as_str()
                        && let Some(pointer) = reference.strip_prefix('#')
                        && pointer.starts_with("/components/")
                        && root.pointer(pointer).is_none()
                    {
                        return Err(MergeError::DanglingRef {
                            reference: reference.to_owned(),
                        });
                    }
                    walk(root, child)?;
                }
                Ok(())
            }
            Value::Array(items) => items.iter().try_for_each(|item| walk(root, item)),
            _ => Ok(()),
        }
    }
    walk(root, root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(name: &str, value: Value) -> Document {
        Document {
            name: name.to_owned(),
            value,
        }
    }

    fn operation(id: &str) -> Value {
        json!({"operationId": id, "responses": {"200": {"description": "ok"}}})
    }

    fn merged(documents: &[Document]) -> Result<Value, MergeError> {
        merge(documents, "API", "1")
    }

    #[test]
    fn merges_paths_components_and_tags_in_sorted_order() {
        let a = document(
            "a",
            json!({
                "openapi": "3.1.0",
                "info": {"title": "a", "version": "9"},
                "paths": {"/v1/b": {"get": operation("b_get")}},
                "components": {"schemas": {"Zed": {"type": "string"}}},
                "tags": [{"name": "zeta"}],
            }),
        );
        let b = document(
            "b",
            json!({
                "openapi": "3.1.0",
                "info": {"title": "b", "version": "9"},
                "paths": {
                    "/v1/a": {"get": operation("a_get")},
                    "/v1/b": {"post": operation("b_post")},
                },
                "components": {"schemas": {"Alpha": {"type": "integer"}}},
                "tags": [{"name": "alpha"}, {"name": "zeta"}],
            }),
        );
        let result = merged(&[a, b]).expect("merge");
        let text = serde_json::to_string(&result).expect("serialize");
        assert_eq!(
            text,
            serde_json::to_string(&json!({
                "components": {"schemas": {"Alpha": {"type": "integer"}, "Zed": {"type": "string"}}},
                "info": {"title": "API", "version": "1"},
                "openapi": "3.1.0",
                "paths": {
                    "/v1/a": {"get": operation("a_get")},
                    "/v1/b": {"get": operation("b_get"), "post": operation("b_post")},
                },
                "tags": [{"name": "alpha"}, {"name": "zeta"}],
            }))
            .expect("serialize")
        );
    }

    #[test]
    fn result_does_not_depend_on_source_order() {
        let a = || {
            document(
                "a",
                json!({"openapi": "3.1.0", "paths": {"/a": {"get": operation("a")}}}),
            )
        };
        let b = || {
            document(
                "b",
                json!({"openapi": "3.1.0", "paths": {"/b": {"get": operation("b")}}}),
            )
        };
        assert_eq!(merged(&[a(), b()]), merged(&[b(), a()]));
    }

    #[test]
    fn the_same_operation_in_two_sources_collides() {
        let make = |name| {
            document(
                name,
                json!({"openapi": "3.1.0", "paths": {"/v1/x": {"get": operation(name)}}}),
            )
        };
        assert_eq!(
            merged(&[make("a"), make("b")]),
            Err(MergeError::Collision {
                first: "a".to_owned(),
                second: "b".to_owned(),
                item: "GET /v1/x".to_owned(),
            })
        );
    }

    #[test]
    fn duplicate_operation_ids_are_rejected() {
        let a = document(
            "a",
            json!({"openapi": "3.1.0", "paths": {"/a": {"get": operation("list")}}}),
        );
        let b = document(
            "b",
            json!({"openapi": "3.1.0", "paths": {"/b": {"get": operation("list")}}}),
        );
        assert_eq!(
            merged(&[a, b]),
            Err(MergeError::DuplicateOperationId {
                id: "list".to_owned(),
                first: "a".to_owned(),
                second: "b".to_owned(),
            })
        );
    }

    #[test]
    fn identical_schemas_merge_and_different_ones_collide() {
        let schema = |kind| json!({"openapi": "3.1.0", "components": {"schemas": {"Shared": {"type": kind}}}});
        let same = merged(&[
            document("a", schema("string")),
            document("b", schema("string")),
        ])
        .expect("identical definitions merge");
        assert_eq!(same["components"]["schemas"]["Shared"]["type"], "string");
        assert_eq!(
            merged(&[
                document("a", schema("string")),
                document("b", schema("integer"))
            ]),
            Err(MergeError::Collision {
                first: "a".to_owned(),
                second: "b".to_owned(),
                item: "components.schemas.Shared (differently)".to_owned(),
            })
        );
    }

    #[test]
    fn dangling_references_are_rejected() {
        let a = document(
            "a",
            json!({
                "openapi": "3.1.0",
                "components": {"schemas": {"A": {"$ref": "#/components/schemas/Missing"}}},
            }),
        );
        assert_eq!(
            merged(&[a]),
            Err(MergeError::DanglingRef {
                reference: "#/components/schemas/Missing".to_owned()
            })
        );
    }

    #[test]
    fn references_may_resolve_to_another_source() {
        let a = document(
            "a",
            json!({
                "openapi": "3.1.0",
                "components": {"schemas": {"A": {"$ref": "#/components/schemas/B"}}},
            }),
        );
        let b = document(
            "b",
            json!({"openapi": "3.1.0", "components": {"schemas": {"B": {"type": "string"}}}}),
        );
        merged(&[a, b]).expect("a reference to another source's schema resolves");
    }

    #[test]
    fn server_wide_keys_and_version_mismatches_are_rejected() {
        let with_servers = document(
            "a",
            json!({"openapi": "3.1.0", "servers": [{"url": "https://example.test"}]}),
        );
        assert_eq!(
            merged(&[with_servers]),
            Err(MergeError::UnsupportedKey {
                source_name: "a".to_owned(),
                key: "servers".to_owned()
            })
        );
        let old = document("a", json!({"openapi": "3.0.3"}));
        let new = document("b", json!({"openapi": "3.1.0"}));
        assert!(matches!(
            merged(&[old, new]),
            Err(MergeError::VersionMismatch { .. })
        ));
    }

    #[test]
    fn malformed_and_empty_input_is_rejected() {
        assert!(matches!(
            merged(&[document("a", json!([]))]),
            Err(MergeError::Malformed { .. } | MergeError::NotAnObject { .. })
        ));
        assert!(matches!(merged(&[]), Err(MergeError::Malformed { .. })));
        let bad_paths = document("a", json!({"openapi": "3.1.0", "paths": {"/a": 1}}));
        assert!(matches!(
            merged(&[bad_paths]),
            Err(MergeError::Malformed { .. })
        ));
    }
}
