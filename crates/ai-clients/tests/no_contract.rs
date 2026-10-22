//! The AI gateway does not depend on the product contracts (decision 28), so neither does this
//! crate: `cargo tree -p ai-clients` must not list `contract`, through any kind of dependency.

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
fn ai_clients_does_not_depend_on_contract() {
    let names = tree("ai-clients");
    assert!(
        names.iter().any(|name| name == "reqwest"),
        "the tree looks wrong: {names:?}"
    );
    assert!(
        !names.iter().any(|name| name == "contract"),
        "ai-clients pulls in crates/contract (decision 28 forbids it)"
    );
}

/// The check would see `contract` if it were there: `outbox` depends on it.
#[test]
fn the_check_finds_contract_where_it_is() {
    assert!(tree("outbox").iter().any(|name| name == "contract"));
}
