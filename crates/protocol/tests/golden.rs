//! Golden-file tests: the exact bytes of every message type are pinned, so any change to the
//! wire format fails here and shows up in review.
//!
//! - `tests/golden/current/` holds what this build encodes for each sample in `common`. After an
//!   intended format change, regenerate it with
//!   `UPDATE_GOLDEN=1 cargo test -p protocol --test golden` and review the diff.
//! - `tests/golden/v1-frozen/` holds payloads as the first v1 release encoded them. It is never
//!   regenerated: every later build must still decode these payloads to the same content
//!   (backward compatibility). Add files when a revision adds a message; never edit or delete
//!   them.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use common::{Golden, cases, golden_dir, read_hex, to_hex};
use protocol::topic::Channel;
use serde_json::Value as Json;

fn updating() -> bool {
    std::env::var_os("UPDATE_GOLDEN").is_some()
}

fn pretty(json: &Json) -> String {
    let mut text = serde_json::to_string_pretty(json).unwrap();
    text.push('\n');
    text
}

#[test]
fn encodings_match_the_current_golden_files() {
    let dir = golden_dir("current");
    if updating() {
        fs::create_dir_all(&dir).unwrap();
    }
    let mut failures = Vec::new();
    for case in cases() {
        let encoded = case.encode();
        let json = pretty(&case.json());
        let hex_path = dir.join(format!("{}.cbor.hex", case.name()));
        let json_path = dir.join(format!("{}.json", case.name()));
        if updating() {
            fs::write(&hex_path, to_hex(&encoded)).unwrap();
            fs::write(&json_path, &json).unwrap();
            continue;
        }
        let golden = read_hex(&hex_path);
        if golden != encoded {
            failures.push(format!(
                "{}: encoding changed\n--- golden\n{}+++ encoded\n{}",
                case.name(),
                to_hex(&golden),
                to_hex(&encoded)
            ));
        }
        if fs::read_to_string(&json_path).unwrap() != json {
            failures.push(format!("{}: JSON rendering changed\n{json}", case.name()));
        }
        if let Err(error) = case.decodes_to_sample(&golden) {
            failures.push(format!(
                "{}: golden does not decode to the sample: {error}",
                case.name()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{}\n\nIf the change is intended, regenerate with UPDATE_GOLDEN=1 and review the diff.",
        failures.join("\n\n")
    );
}

#[test]
fn every_case_round_trips() {
    // Independent of the files: what this build encodes, it decodes to the same message.
    for case in cases() {
        if let Err(error) = case.decodes_to_sample(&case.encode()) {
            panic!("{} does not round-trip: {error}", case.name());
        }
    }
}

#[test]
fn every_channel_has_a_golden_case() {
    let covered: Vec<Channel> = cases().iter().map(|case| case.channel()).collect();
    for channel in Channel::ALL {
        assert!(covered.contains(&channel), "{channel:?} has no golden case");
    }
}

/// The base names of the `*.cbor.hex` files in `dir`; also checks that each has its `.json`
/// and that nothing else is there.
fn golden_names(dir: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut json = BTreeSet::new();
    for entry in fs::read_dir(dir).unwrap() {
        let file = entry.unwrap().file_name().into_string().unwrap();
        if let Some(name) = file.strip_suffix(".cbor.hex") {
            names.insert(name.to_owned());
        } else if let Some(name) = file.strip_suffix(".json") {
            json.insert(name.to_owned());
        } else {
            panic!("unexpected file {file} in {}", dir.display());
        }
    }
    assert_eq!(
        names,
        json,
        "every payload in {} needs its JSON",
        dir.display()
    );
    names
}

#[test]
fn the_current_golden_files_are_exactly_the_cases() {
    if updating() {
        return;
    }
    let expected: BTreeSet<String> = cases().iter().map(|case| case.name().to_owned()).collect();
    assert_eq!(golden_names(&golden_dir("current")), expected);
}

/// Checks that `decoded` holds everything in `frozen`: equal scalars, objects with at least
/// the frozen keys, and arrays that start with the frozen elements. A newer build may add
/// fields when it renders an old payload, but must not lose or change any.
fn contains(frozen: &Json, decoded: &Json, path: &str) -> Result<(), String> {
    match (frozen, decoded) {
        (Json::Object(frozen), Json::Object(decoded)) => {
            for (key, value) in frozen {
                let inner = decoded
                    .get(key)
                    .ok_or_else(|| format!("{path}.{key} is missing"))?;
                contains(value, inner, &format!("{path}.{key}"))?;
            }
            Ok(())
        }
        (Json::Array(frozen), Json::Array(decoded)) if decoded.len() >= frozen.len() => frozen
            .iter()
            .zip(decoded)
            .enumerate()
            .try_for_each(|(index, (frozen, decoded))| {
                contains(frozen, decoded, &format!("{path}[{index}]"))
            }),
        _ if frozen == decoded => Ok(()),
        _ => Err(format!("{path}: was {frozen}, now {decoded}")),
    }
}

#[test]
fn frozen_v1_payloads_still_decode() {
    let dir = golden_dir("v1-frozen");
    let cases = cases();
    let names = golden_names(&dir);
    // Every message type that existed when v1 was frozen.
    assert_eq!(
        names.len(),
        18,
        "v1-frozen files are never added to or removed by accident"
    );
    for name in names {
        let case: &dyn Golden = cases
            .iter()
            .find(|case| case.name() == name)
            .map(AsRef::as_ref)
            .unwrap_or_else(|| panic!("frozen payload {name} has no case to decode it"));
        let payload = read_hex(&dir.join(format!("{name}.cbor.hex")));
        let frozen: Json =
            serde_json::from_str(&fs::read_to_string(dir.join(format!("{name}.json"))).unwrap())
                .unwrap();
        let decoded = case
            .decode(&payload)
            .unwrap_or_else(|error| panic!("frozen payload {name} no longer decodes: {error}"));
        if let Err(difference) = contains(&frozen, &decoded, "") {
            panic!("frozen payload {name} decodes differently: {difference}");
        }
    }
}

#[test]
fn the_subset_check_catches_lost_and_changed_fields() {
    let frozen = serde_json::json!({"a": 1, "b": [1, {"c": true}]});
    assert!(
        contains(
            &frozen,
            &serde_json::json!({"a": 1, "b": [1, {"c": true}], "new": 2}),
            ""
        )
        .is_ok()
    );
    assert!(contains(&frozen, &serde_json::json!({"b": [1, {"c": true}]}), "").is_err());
    assert!(
        contains(
            &frozen,
            &serde_json::json!({"a": 2, "b": [1, {"c": true}]}),
            ""
        )
        .is_err()
    );
    assert!(contains(&frozen, &serde_json::json!({"a": 1, "b": [1]}), "").is_err());
    assert!(
        contains(
            &frozen,
            &serde_json::json!({"a": 1, "b": [1, {"c": false}]}),
            ""
        )
        .is_err()
    );
}
