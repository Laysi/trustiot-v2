//! Helpers shared by the integration tests; each test binary uses some of them.
#![allow(
    dead_code,
    reason = "each test binary includes this module but uses only some helpers"
)]

pub mod peer;

use std::fs;
use std::path::PathBuf;

use serde_json::Value;

/// Set to `1` to rewrite JSON fixtures from the current code instead of comparing with them.
const UPDATE_ENV: &str = "CONTRACT_UPDATE_FIXTURES";

/// The absolute path of a file under `tests/fixtures/`.
pub fn fixture_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(relative)
}

/// Compares `actual` with the JSON fixture at `relative`, or rewrites the fixture when
/// `CONTRACT_UPDATE_FIXTURES=1`.
pub fn assert_json_fixture(relative: &str, actual: &Value) {
    let path = fixture_path(relative);
    let pretty = format!(
        "{}\n",
        serde_json::to_string_pretty(actual).expect("serialize")
    );
    if std::env::var(UPDATE_ENV).as_deref() == Ok("1") {
        fs::create_dir_all(path.parent().expect("fixture has a parent")).expect("create dir");
        fs::write(&path, pretty).expect("write fixture");
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "read {}: {error}; create it with {UPDATE_ENV}=1",
            path.display()
        )
    });
    let expected_value: Value = serde_json::from_str(&expected).expect("fixture is JSON");
    assert!(
        expected_value == *actual,
        "{} differs from the code; if the change is intended, rerun with {UPDATE_ENV}=1 and \
         review the diff.\n--- fixture\n{expected}--- code\n{pretty}",
        path.display()
    );
}
