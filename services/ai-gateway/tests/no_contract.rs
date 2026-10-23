//! The gateway stays free of the product contracts so it can be split out (decision 28):
//! `cargo tree -p ai-gateway` must not list `contract`, through any kind of dependency. Like
//! crates/ai-clients/tests/no_contract.rs; service-kit's `contract` feature is the usual way in.

use std::path::Path;
use std::process::Command;

/// The package names in `cargo tree -p <package>` (normal, build and dev dependencies).
fn tree(package: &str) -> Vec<String> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(env!("CARGO"))
        .args([
            "tree",
            "--locked",
            "-p",
            package,
            "-e",
            "normal,build,dev",
            "--prefix",
            "none",
            "--format",
            "{p}",
        ])
        .current_dir(&workspace)
        .output()
        .expect("run cargo tree");
    assert!(
        output.status.success(),
        "cargo tree -p {package} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("cargo tree prints UTF-8")
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

#[test]
fn ai_gateway_does_not_depend_on_contract() {
    let names = tree("ai-gateway");
    for expected in ["ai-clients", "service-kit"] {
        assert!(
            names.iter().any(|name| name == expected),
            "the tree looks wrong, {expected} is missing: {names:?}"
        );
    }
    assert!(
        !names.iter().any(|name| name == "contract"),
        "ai-gateway pulls in crates/contract (decision 28 forbids it); service-kit must stay \
         without its default `contract` feature"
    );
}

/// The check would see `contract` if it were there: service-kit's default features pull it in.
#[test]
fn the_check_finds_contract_where_it_is() {
    assert!(tree("service-kit").iter().any(|name| name == "contract"));
}
