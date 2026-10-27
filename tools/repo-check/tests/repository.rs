//! Applies the `repo-check` invariants to this repository.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use repo_check::{
    DEV_ONLY_CRATES, PROTOTOOLS, RUNTIME_SOURCE_GLOB, RUST_TOOLCHAIN, TEST_ONLY_FEATURES,
    WORKSPACE_MANIFEST, Workspace, check_member_manifest, check_rust_pins, check_rust_version,
    find_agents_files, find_container_removal_sources, find_container_removals_without_volumes,
    find_runtime_sources, find_schema_changes, non_dev_uses, parse_metadata, prototools_pin,
    test_only_feature_uses,
};

/// `cargo metadata --no-deps --format-version 1` of this workspace.
fn metadata() -> String {
    // The cargo binary that is running these tests, so the pinned toolchain answers.
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata should start");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("cargo metadata prints UTF-8")
}

fn workspace() -> Workspace {
    parse_metadata(&metadata()).expect("cargo metadata output should parse")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

#[test]
fn toolchain_pins_agree() {
    let root = workspace().root;
    let prototools = read(&root.join(PROTOTOOLS));
    let toolchain = check_rust_pins(&prototools, &read(&root.join(RUST_TOOLCHAIN)))
        .unwrap_or_else(|error| panic!("{error}"));
    check_rust_version(&read(&root.join(WORKSPACE_MANIFEST)), toolchain)
        .unwrap_or_else(|error| panic!("{error}"));
    prototools_pin(&prototools, "bun").unwrap_or_else(|error| panic!("{error}"));
}

#[test]
fn members_inherit_workspace_edition_and_lints() {
    let workspace = workspace();
    let own_manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    assert!(
        workspace.member_manifests.contains(&own_manifest),
        "{} is not a workspace member: {:?}",
        own_manifest.display(),
        workspace.member_manifests
    );
    for manifest in &workspace.member_manifests {
        check_member_manifest(manifest, &read(manifest)).unwrap_or_else(|error| panic!("{error}"));
    }
}

/// Test support crates stay out of service binaries: their SQL is checked as test code.
#[test]
fn dev_only_crates_are_only_dev_dependencies() {
    let uses = non_dev_uses(&metadata(), DEV_ONLY_CRATES).unwrap_or_else(|error| panic!("{error}"));
    assert!(
        uses.is_empty(),
        "move these to [dev-dependencies]:\n{}",
        uses.join("\n")
    );
}

/// Features that switch a safety check off for tests never reach a build that ships.
#[test]
fn test_only_features_are_only_enabled_by_dev_dependencies() {
    let uses = test_only_feature_uses(&metadata(), TEST_ONLY_FEATURES)
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        uses.is_empty(),
        "only a [dev-dependencies] entry may enable these features:\n{}",
        uses.join("\n")
    );
}

#[test]
fn repository_has_no_agents_md() {
    let root = workspace().root;
    let found = find_agents_files(&root).expect("walk the repository");
    assert!(
        found.is_empty(),
        "agent instructions belong in CLAUDE.md only; remove {found:?}"
    );
}

/// Proves the search above can fail: it finds planted files and skips ignored directories.
#[test]
fn agents_md_search_finds_planted_files() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("agents-md-search");
    if root.exists() {
        fs::remove_dir_all(&root).expect("clear the previous fixture");
    }
    let planted = [
        "AGENTS.md",
        "services/example/agents.md",
        "crates/deep/nested/Agents.MD",
    ];
    let ignored = [
        "CLAUDE.md",
        "AGENTS.md.bak",
        "target/AGENTS.md",
        "frontend/node_modules/pkg/AGENTS.md",
        ".claude/worktrees/other/AGENTS.md",
    ];
    for relative in planted.iter().chain(&ignored) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture paths have a parent"))
            .expect("create fixture directory");
        fs::write(&path, "fixture\n").expect("write fixture file");
    }
    let mut expected: Vec<PathBuf> = planted.iter().map(|relative| root.join(relative)).collect();

    // The common `ln -s CLAUDE.md AGENTS.md` shortcut is reported too.
    #[cfg(unix)]
    {
        let link = root.join("docs/AGENTS.md");
        fs::create_dir_all(root.join("docs")).expect("create fixture directory");
        std::os::unix::fs::symlink("../CLAUDE.md", &link).expect("create fixture symlink");
        expected.push(link);
    }

    let found = find_agents_files(&root).expect("walk the fixture");
    expected.sort();
    assert_eq!(found, expected);

    fs::remove_dir_all(&root).expect("remove the fixture");
}

#[test]
fn runtime_code_never_changes_the_schema() {
    let root = workspace().root;
    let sources = find_runtime_sources(&root).expect("walk services/*/src");
    assert!(
        !sources.is_empty(),
        "expected Rust sources under {RUNTIME_SOURCE_GLOB}"
    );
    let mut found = Vec::new();
    for path in &sources {
        for change in find_schema_changes(&read(path)) {
            found.push(format!(
                "{}:{}: {}",
                path.strip_prefix(&root).unwrap_or(path).display(),
                change.line,
                change.statement
            ));
        }
    }
    assert!(
        found.is_empty(),
        "{RUNTIME_SOURCE_GLOB} must not change a database schema; move these statements to the \
         service's migrations/ (runtime code deletes rows with DELETE FROM, not ALTER TABLE):\n{}",
        found.join("\n")
    );
}

/// Proves the search above covers every service's src tree and nothing else.
#[test]
fn runtime_sources_are_the_rust_files_under_services_src() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("runtime-sources");
    if root.exists() {
        fs::remove_dir_all(&root).expect("clear the previous fixture");
    }
    for relative in [
        "services/a/src/lib.rs",
        "services/a/src/nested/mod.rs",
        "services/a/src/schema.sql",
        "services/a/tests/clickhouse.rs",
        "services/a/migrations/clickhouse/0001_tables.sql",
        "services/b/src/main.rs",
        "crates/c/src/lib.rs",
    ] {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture paths have a parent"))
            .expect("create fixture directory");
        fs::write(&path, "fixture\n").expect("write fixture file");
    }
    let found = find_runtime_sources(&root).expect("walk the fixture");
    let expected: Vec<PathBuf> = [
        "services/a/src/lib.rs",
        "services/a/src/nested/mod.rs",
        "services/b/src/main.rs",
    ]
    .iter()
    .map(|relative| root.join(relative))
    .collect();
    assert_eq!(found, expected);

    fs::remove_dir_all(&root).expect("remove the fixture");
}

/// Laysi/trustiot-v2#334: a container removed with `rm --force` and no `--volumes` leaves the
/// image's anonymous volumes behind, and enough of them use up podman's locks.
#[test]
fn test_containers_are_removed_together_with_their_volumes() {
    let root = workspace().root;
    let sources = find_container_removal_sources(&root).expect("walk the sources");
    assert!(!sources.is_empty(), "expected Rust sources");
    let mut found = Vec::new();
    for path in &sources {
        for line in find_container_removals_without_volumes(&read(path)) {
            found.push(format!(
                "{}:{line}",
                path.strip_prefix(&root).unwrap_or(path).display()
            ));
        }
    }
    assert!(
        found.is_empty(),
        "add \"--volumes\" to these \"rm\", \"--force\" calls so the container's anonymous volumes \
         go with it:\n{}",
        found.join("\n")
    );
}

/// Proves the search above covers the test code and skips what it should.
#[test]
fn container_removal_sources_are_the_rust_files_in_the_code_trees() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("container-removal-sources");
    if root.exists() {
        fs::remove_dir_all(&root).expect("clear the previous fixture");
    }
    for relative in [
        "crates/a/tests/support/mod.rs",
        "services/b/tests/common/mod.rs",
        "edge/c/tests/broker.rs",
        "crates/a/README.md",
        "crates/a/target/debug/build.rs",
        "crates/a/.hidden/x.rs",
        "tools/repo-check/src/lib.rs",
        "tools/sql-check/src/main.rs",
        "crates/a/src/index.ts",
    ] {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture paths have a parent"))
            .expect("create fixture directory");
        fs::write(&path, "fixture\n").expect("write fixture file");
    }
    let found = find_container_removal_sources(&root).expect("walk the fixture");
    let expected: Vec<PathBuf> = [
        "crates/a/tests/support/mod.rs",
        "edge/c/tests/broker.rs",
        "services/b/tests/common/mod.rs",
        "tools/sql-check/src/main.rs",
    ]
    .iter()
    .map(|relative| root.join(relative))
    .collect();
    assert_eq!(found, expected);

    fs::remove_dir_all(&root).expect("remove the fixture");
}
