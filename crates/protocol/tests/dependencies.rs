//! The gateway (`aarch64-unknown-linux-gnu`) builds this crate, so everything
//! it pulls in must be pure Rust: no native library, no C toolchain, no cloud SDK. This test
//! pins the crates compiled for the gateway target; adding one fails it until someone checks
//! the crate and extends the list. `cargo build -p protocol --target aarch64-unknown-linux-gnu`
//! proves that the build itself works.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::process::Command;

use serde_json::Value as Json;

const GATEWAY_TARGET: &str = "aarch64-unknown-linux-gnu";

/// The crates `protocol` may depend on, directly or not, for the gateway target. Everything
/// here is pure Rust. `half` (with `cfg-if` and `zerocopy`) decodes CBOR half-precision floats;
/// the derive macros and their parser crates only run on the build machine.
const ALLOWED: &[&str] = &[
    "cbor4ii",
    "half",
    "cfg-if",
    "zerocopy",
    "zerocopy-derive",
    "serde",
    "serde_core",
    "serde_derive",
    "proc-macro2",
    "quote",
    "syn",
    "unicode-ident",
];

#[test]
fn the_gateway_build_pulls_only_pure_rust_crates() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--filter-platform",
            GATEWAY_TARGET,
        ])
        .current_dir(&workspace)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Json = serde_json::from_slice(&output.stdout).unwrap();

    let packages: BTreeMap<&str, &Json> = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|package| (package["id"].as_str().unwrap(), package))
        .collect();
    let nodes: BTreeMap<&str, &Json> = metadata["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| (node["id"].as_str().unwrap(), node))
        .collect();
    let root = metadata["workspace_members"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Json::as_str)
        .find(|id| packages[id]["name"] == "protocol")
        .unwrap();

    // Follow normal and build dependencies (not dev-dependencies) from `protocol`.
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([root]);
    while let Some(id) = queue.pop_front() {
        for dep in nodes[id]["deps"].as_array().unwrap() {
            let compiled = dep["dep_kinds"]
                .as_array()
                .unwrap()
                .iter()
                .any(|kind| kind["kind"].is_null() || kind["kind"] == "build");
            let dep_id = dep["pkg"].as_str().unwrap();
            if compiled && seen.insert(dep_id) {
                queue.push_back(dep_id);
            }
        }
    }

    let names: BTreeSet<&str> = seen
        .iter()
        .map(|id| packages[id]["name"].as_str().unwrap())
        .collect();
    let allowed: BTreeSet<&str> = ALLOWED.iter().copied().collect();
    assert_eq!(
        names, allowed,
        "the crates compiled for the gateway changed; check that each new crate is pure Rust \
         (no `links`, no C build, no cloud SDK), builds for {GATEWAY_TARGET}, and update ALLOWED"
    );
    for id in &seen {
        assert!(
            packages[id]["links"].is_null(),
            "{} links a native library",
            packages[id]["name"]
        );
    }
}
