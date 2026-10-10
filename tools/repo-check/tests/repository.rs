//! Applies the `repo-check` invariants to this repository.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use repo_check::{
    PROTOTOOLS, RUST_TOOLCHAIN, WORKSPACE_MANIFEST, Workspace, check_member_manifest,
    check_rust_pins, check_rust_version, find_agents_files, parse_metadata, prototools_pin,
};

fn workspace() -> Workspace {
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
    let stdout = String::from_utf8(output.stdout).expect("cargo metadata prints UTF-8");
    parse_metadata(&stdout).expect("cargo metadata output should parse")
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
