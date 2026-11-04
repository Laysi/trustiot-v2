//! Applies the `repo-check` invariants to this repository.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use repo_check::public_paths::{
    INGRESS_TEMPLATE, OPENAPI, bound_paths, compare_methods, compared_methods, openapi_methods,
    parse_public_path_table, route_methods, runtime_code, string_constant,
};
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

/// A public path whose service registers it with axum's `.route(CONSTANT, ...)`.
struct RoutedPublicPath {
    /// The key under `ingress.publicPaths` and in the chart's plan table.
    key: &'static str,
    /// The backend the table sends it to.
    service: &'static str,
    /// The file that defines the path constant.
    constant_file: &'static str,
    /// The path constant.
    constant: &'static str,
    /// The sources that serve the path: every `.route` of it in them counts.
    route_dirs: &'static [&'static str],
}

const ROUTED_PUBLIC_PATHS: [RoutedPublicPath; 4] = [
    RoutedPublicPath {
        key: "login",
        service: "identity",
        constant_file: "services/identity/src/login.rs",
        constant: "LOGIN_PATH",
        route_dirs: &["services/identity/src"],
    },
    // service-kit serves /healthz on the API port in front of the service's own routes; the
    // service could add a route of its own there too.
    RoutedPublicPath {
        key: "healthCheck",
        service: "identity",
        constant_file: "crates/service-kit/src/health.rs",
        constant: "LIVENESS_PATH",
        route_dirs: &["crates/service-kit/src", "services/identity/src"],
    },
    RoutedPublicPath {
        key: "certRenewal",
        service: "certs",
        constant_file: "crates/protocol/src/renewal.rs",
        constant: "RENEWAL_PATH",
        route_dirs: &["services/certs/src"],
    },
    RoutedPublicPath {
        key: "rootCrl",
        service: "certs",
        constant_file: "crates/protocol/src/root_crl.rs",
        constant: "ROOT_CRL_PATH",
        route_dirs: &["services/certs/src"],
    },
];

/// The mqtt bridge takes every path through its fallback, and its handshake admits the method.
const MQTT_BRIDGE_SRC: &str = "services/mqtt-bridge/src";
const MQTT_HANDSHAKE: &str = "services/mqtt-bridge/src/handshake.rs";

/// The runtime Rust files under `dir`, sorted: `tests.rs` modules and `tests` directories are
/// left out (the readers drop a file's own `#[cfg(test)]` part).
fn runtime_rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries =
            fs::read_dir(&dir).unwrap_or_else(|error| panic!("{}: {error}", dir.display()));
        for entry in entries {
            let path = entry.expect("read a directory entry").path();
            if path.is_dir() {
                if path.file_name() != Some("tests".as_ref()) {
                    pending.push(path);
                }
            } else if path.extension() == Some("rs".as_ref())
                && path.file_name() != Some("tests.rs".as_ref())
            {
                found.push(path);
            }
        }
    }
    found.sort();
    assert!(
        !found.is_empty(),
        "expected Rust sources under {}",
        dir.display()
    );
    found
}

/// Laysi/trustiot-v2#904: the methods the ingress admits on each public path (the chart's plan
/// table, Laysi/trustiot-v2#859) are the methods its service serves there, so a service that
/// adds or drops one fails here instead of being refused, or over-admitted, at the ingress.
/// The login's come from the OpenAPI document too, and each fixed path is the service's
/// constant.
#[test]
fn ingress_public_path_methods_are_the_services_methods() {
    let root = workspace().root;
    let rows = parse_public_path_table(&read(&root.join(INGRESS_TEMPLATE)))
        .unwrap_or_else(|error| panic!("{error}"));
    let keys: Vec<&str> = rows.iter().map(|row| row.key.as_str()).collect();
    assert_eq!(
        keys,
        ["login", "healthCheck", "certRenewal", "rootCrl", "mqtt"],
        "{INGRESS_TEMPLATE}: a new or removed public path needs its service's methods read here"
    );
    let mut failures = Vec::new();
    for row in &rows {
        if row.key == "mqtt" {
            assert_eq!(
                row.service, "mqtt-bridge",
                "{INGRESS_TEMPLATE}: mqtt's backend"
            );
            // The bridge serves every path, so the chart leaves the path to the values.
            assert_eq!(
                row.fixed, None,
                "{INGRESS_TEMPLATE}: mqtt's path is not fixed"
            );
            let mut bound = Vec::new();
            for path in runtime_rust_files(&root.join(MQTT_BRIDGE_SRC)) {
                bound.extend(
                    bound_paths(&read(&path))
                        .unwrap_or_else(|error| panic!("{}: {error}", path.display())),
                );
            }
            assert!(
                bound.is_empty(),
                "{MQTT_BRIDGE_SRC} binds {bound:?}; the check reads the methods of its fallback \
                 only, so read these too"
            );
            assert!(
                read(&root.join(MQTT_BRIDGE_SRC).join("lib.rs")).contains(".fallback(upgrade)"),
                "{MQTT_BRIDGE_SRC}/lib.rs: every path goes to `upgrade`, which runs the handshake"
            );
            // Nothing else in the bridge looks at the method, and it reads it only to hand it to
            // the handshake, so the handshake's comparison is the whole answer.
            for path in runtime_rust_files(&root.join(MQTT_BRIDGE_SRC)) {
                if path.ends_with("handshake.rs") {
                    continue;
                }
                let code = runtime_code(&read(&path));
                let reads = code.matches(".method()").count();
                let handed = code.matches("handshake::accept(request.method()").count();
                assert!(
                    !code.contains("Method::") && reads == handed,
                    "{}: only {MQTT_HANDSHAKE} may decide on the method; extend repo-check to \
                     read this file's",
                    path.display()
                );
            }
            let served = compared_methods(&read(&root.join(MQTT_HANDSHAKE)))
                .unwrap_or_else(|error| panic!("{MQTT_HANDSHAKE}: {error}"));
            failures.extend(compare_methods(row, MQTT_HANDSHAKE, &served).err());
            continue;
        }
        let spec = ROUTED_PUBLIC_PATHS
            .iter()
            .find(|spec| spec.key == row.key)
            .unwrap_or_else(|| panic!("no service source for public path {}", row.key));
        assert_eq!(
            row.service, spec.service,
            "{INGRESS_TEMPLATE}: {}'s backend",
            row.key
        );
        let path = string_constant(&read(&root.join(spec.constant_file)), spec.constant)
            .unwrap_or_else(|error| panic!("{}: {error}", spec.constant_file));
        assert_eq!(
            row.fixed.as_deref(),
            Some(path.as_str()),
            "{INGRESS_TEMPLATE}: {} must be fixed to {path}, the service's {}",
            row.key,
            spec.constant
        );
        let sources: Vec<String> = spec
            .route_dirs
            .iter()
            .flat_map(|dir| runtime_rust_files(&root.join(dir)))
            .map(|file| read(&file))
            .collect();
        let served = route_methods(sources.iter().map(String::as_str), spec.constant, &path)
            .unwrap_or_else(|error| panic!("{}: {error}", spec.route_dirs.join(", ")));
        let routes = format!(
            "`.route({}, ...)` in {}",
            spec.constant,
            spec.route_dirs.join(", ")
        );
        failures.extend(compare_methods(row, &routes, &served).err());
        if row.key == "login" {
            let documented = openapi_methods(&read(&root.join(OPENAPI)), &path)
                .unwrap_or_else(|error| panic!("{error}"));
            let openapi = format!("{OPENAPI} paths.{path}");
            failures.extend(compare_methods(row, &openapi, &documented).err());
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
