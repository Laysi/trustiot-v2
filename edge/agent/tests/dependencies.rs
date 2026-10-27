//! What the gateway build of the agent (`aarch64-unknown-linux-gnu`) may pull in
//! (Laysi/trustiot-v2#86, decision 10):
//!
//! - no public root store: the outer TLS trusts only the PEM files in the configuration and the
//!   inner TLS only our CA, so neither `webpki-roots` nor `rustls-native-certs` may be compiled
//!   in (a root store in the binary would be trusted silently by whatever picks it up);
//! - no OpenSSL, no aws-lc and no TPM C library: one crypto implementation (ring), and nothing
//!   that needs a C library on the gateway at run time;
//! - the only crate that builds C code is ring (its assembly and C, with the target's C
//!   compiler; `cargo zigbuild` provides one for cross builds).
//!
//! Normal and build dependencies count; dev-dependencies only run on the build machine.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::process::Command;

use serde_json::Value as Json;

const GATEWAY_TARGET: &str = "aarch64-unknown-linux-gnu";

/// Crates that must never be compiled into the agent.
const DENIED: &[&str] = &[
    "webpki-roots",
    "rustls-native-certs",
    "openssl",
    "openssl-sys",
    "native-tls",
    "aws-lc-rs",
    "aws-lc-sys",
    "aws-lc-fips-sys",
    "tss-esapi",
    "tss-esapi-sys",
];

/// Crates that may declare `links` (build native code).
const MAY_LINK: &[&str] = &["ring"];

#[test]
fn the_gateway_build_has_no_root_store_and_one_crypto_library() {
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
        .find(|id| packages[id]["name"] == "agent")
        .unwrap();

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
    assert!(
        names.contains("ring") && names.contains("rustls"),
        "{names:?}"
    );
    let denied: Vec<&str> = DENIED
        .iter()
        .copied()
        .filter(|name| names.contains(name))
        .collect();
    assert!(denied.is_empty(), "the agent pulls in {denied:?}");
    let linking: BTreeSet<&str> = seen
        .iter()
        .filter(|id| !packages[*id]["links"].is_null())
        .map(|id| packages[id]["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        linking,
        MAY_LINK.iter().copied().collect(),
        "a new crate builds native code; check that it cross-builds with cargo zigbuild and needs \
         no library on the gateway, then extend MAY_LINK"
    );
}
