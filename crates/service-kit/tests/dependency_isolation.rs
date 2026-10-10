//! services/ai-gateway must not depend on crates/contract (decision 28) but uses this kit, so
//! everything in the kit that needs the contract crate sits behind the kit's `contract` feature,
//! which is on by default. This check runs `cargo tree` on the kit with default features off and
//! on, as part of `cargo test`, so CI fails as soon as a contract dependency slips outside the
//! feature, or the feature stops pulling the crate in (which would make the first check vacuous).

use std::process::Command;

/// Package names in the kit's normal and build dependency tree.
fn dependency_names(default_features: bool) -> Vec<String> {
    let mut command = Command::new(env!("CARGO"));
    command.args([
        "tree",
        "--locked",
        "--package",
        "service-kit",
        "--edges",
        "normal,build",
        "--prefix",
        "none",
        "--format",
        "{p}",
    ]);
    if !default_features {
        command.arg("--no-default-features");
    }
    let output = command
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo tree");
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let names: Vec<String> = String::from_utf8(output.stdout)
        .expect("cargo tree prints UTF-8")
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect();
    // The tree really lists the kit and its dependencies, so an empty or failed listing cannot
    // pass for a clean one.
    assert!(names.iter().any(|name| name == "service-kit"), "{names:?}");
    assert!(names.iter().any(|name| name == "axum"), "{names:?}");
    names
}

#[test]
fn contract_is_not_a_dependency_without_the_contract_feature() {
    let names = dependency_names(false);
    assert!(
        !names.iter().any(|name| name == "contract"),
        "service-kit depends on contract outside the `contract` feature"
    );
}

#[test]
fn the_default_contract_feature_brings_in_contract() {
    let names = dependency_names(true);
    assert!(names.iter().any(|name| name == "contract"), "{names:?}");
}
