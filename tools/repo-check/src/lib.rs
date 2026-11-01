//! Repository invariants for the TrustIoT v2 workspace.
//!
//! Each check is a plain function over file contents, so its failure modes are unit-tested in
//! this file; `tests/repository.rs` applies the checks to the repository itself. Every error
//! names the file and key to fix.

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// proto's tool pins.
pub const PROTOTOOLS: &str = ".prototools";
/// rustup's toolchain file; rustup reads it because proto does not shim cargo.
pub const RUST_TOOLCHAIN: &str = "rust-toolchain.toml";
/// The workspace root manifest.
pub const WORKSPACE_MANIFEST: &str = "Cargo.toml";

/// A release version: `1.99.0`, or `1.99` (patch 0) for a Cargo `rust-version`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    /// Major version.
    pub major: u64,
    /// Minor version.
    pub minor: u64,
    /// Patch version.
    pub patch: u64,
}

impl Version {
    /// Parses an exact `MAJOR.MINOR.PATCH` version. Ranges, channels such as `stable`, and
    /// pre-release suffixes are rejected because they do not pin a single release.
    pub fn parse_exact(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let version = Self {
            major: parse_number(parts.next()?)?,
            minor: parse_number(parts.next()?)?,
            patch: parse_number(parts.next()?)?,
        };
        parts.next().is_none().then_some(version)
    }

    /// Parses a Cargo `rust-version`: `MAJOR.MINOR` or `MAJOR.MINOR.PATCH`.
    pub fn parse_rust_version(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let major = parse_number(parts.next()?)?;
        let minor = parse_number(parts.next()?)?;
        let patch = match parts.next() {
            Some(part) => parse_number(part)?,
            None => 0,
        };
        parts.next().is_none().then_some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Accepts ASCII digits only; `u64::from_str` alone would also accept a leading `+`.
fn parse_number(part: &str) -> Option<u64> {
    if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

/// Why a repository invariant does not hold.
#[derive(Debug, PartialEq, Eq)]
pub enum CheckError {
    /// A file is not valid TOML or JSON.
    Parse {
        /// The file that failed to parse.
        file: String,
        /// The parser's message.
        message: String,
    },
    /// A required key is absent or has the wrong type.
    MissingKey {
        /// The file that lacks the key.
        file: String,
        /// The dotted key path.
        key: &'static str,
    },
    /// A version value has the wrong form.
    InvalidVersion {
        /// The file that holds the value.
        file: String,
        /// The dotted key path.
        key: &'static str,
        /// The value found.
        value: String,
        /// The form the value must take.
        expected: &'static str,
    },
    /// `.prototools` and `rust-toolchain.toml` pin different Rust releases.
    RustPinMismatch {
        /// The `rust` pin in `.prototools`.
        prototools: Version,
        /// The `toolchain.channel` in `rust-toolchain.toml`.
        rust_toolchain: Version,
    },
    /// The workspace `rust-version` is newer than the pinned toolchain, so the pinned toolchain
    /// would refuse to build the workspace.
    RustVersionTooNew {
        /// `workspace.package.rust-version` in `Cargo.toml`.
        rust_version: Version,
        /// The pinned toolchain.
        toolchain: Version,
    },
    /// A workspace member does not inherit a workspace setting.
    NotInherited {
        /// The member's manifest.
        file: String,
        /// The dotted key path that must be `true`.
        key: &'static str,
    },
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse { file, message } => write!(f, "{file}: cannot parse: {message}"),
            Self::MissingKey { file, key } => write!(f, "{file}: `{key}` is missing"),
            Self::InvalidVersion {
                file,
                key,
                value,
                expected,
            } => write!(f, "{file}: `{key}` is `{value}`; expected {expected}"),
            Self::RustPinMismatch {
                prototools,
                rust_toolchain,
            } => write!(
                f,
                "{PROTOTOOLS} pins rust {prototools} but {RUST_TOOLCHAIN} pins {rust_toolchain}; \
                 change both together"
            ),
            Self::RustVersionTooNew {
                rust_version,
                toolchain,
            } => write!(
                f,
                "{WORKSPACE_MANIFEST}: `workspace.package.rust-version` {rust_version} is newer \
                 than the pinned toolchain {toolchain}"
            ),
            Self::NotInherited { file, key } => write!(
                f,
                "{file}: `{key}` must be `true` so the member inherits the workspace setting"
            ),
        }
    }
}

impl std::error::Error for CheckError {}

const EXACT: &str = "an exact MAJOR.MINOR.PATCH version";

fn parse_toml(file: &str, contents: &str) -> Result<toml::Table, CheckError> {
    toml::from_str(contents).map_err(|error| CheckError::Parse {
        file: file.to_owned(),
        message: error.to_string(),
    })
}

/// Follows a key path through nested tables.
fn value_at<'a>(table: &'a toml::Table, path: &[&str]) -> Option<&'a toml::Value> {
    let (last, parents) = path.split_last()?;
    let mut current = table;
    for key in parents {
        current = current.get(*key)?.as_table()?;
    }
    current.get(*last)
}

fn string_at<'a>(
    file: &str,
    table: &'a toml::Table,
    key: &'static str,
) -> Result<&'a str, CheckError> {
    let path: Vec<&str> = key.split('.').collect();
    value_at(table, &path)
        .and_then(toml::Value::as_str)
        .ok_or_else(|| CheckError::MissingKey {
            file: file.to_owned(),
            key,
        })
}

fn exact_version_at(
    file: &str,
    table: &toml::Table,
    key: &'static str,
) -> Result<Version, CheckError> {
    let value = string_at(file, table, key)?;
    Version::parse_exact(value).ok_or_else(|| CheckError::InvalidVersion {
        file: file.to_owned(),
        key,
        value: value.to_owned(),
        expected: EXACT,
    })
}

/// Returns the exact version that `.prototools` pins for `tool`.
pub fn prototools_pin(prototools: &str, tool: &'static str) -> Result<Version, CheckError> {
    let table = parse_toml(PROTOTOOLS, prototools)?;
    exact_version_at(PROTOTOOLS, &table, tool)
}

/// Checks that `.prototools` and `rust-toolchain.toml` pin the same exact Rust release, and
/// returns it.
pub fn check_rust_pins(prototools: &str, rust_toolchain: &str) -> Result<Version, CheckError> {
    let pinned = prototools_pin(prototools, "rust")?;
    let table = parse_toml(RUST_TOOLCHAIN, rust_toolchain)?;
    let channel = exact_version_at(RUST_TOOLCHAIN, &table, "toolchain.channel")?;
    if pinned == channel {
        Ok(pinned)
    } else {
        Err(CheckError::RustPinMismatch {
            prototools: pinned,
            rust_toolchain: channel,
        })
    }
}

/// Checks that the workspace sets `rust-version` and that it is not newer than `toolchain`.
pub fn check_rust_version(workspace_manifest: &str, toolchain: Version) -> Result<(), CheckError> {
    const KEY: &str = "workspace.package.rust-version";
    let table = parse_toml(WORKSPACE_MANIFEST, workspace_manifest)?;
    let value = string_at(WORKSPACE_MANIFEST, &table, KEY)?;
    let rust_version =
        Version::parse_rust_version(value).ok_or_else(|| CheckError::InvalidVersion {
            file: WORKSPACE_MANIFEST.to_owned(),
            key: KEY,
            value: value.to_owned(),
            expected: "MAJOR.MINOR or MAJOR.MINOR.PATCH",
        })?;
    if rust_version <= toolchain {
        Ok(())
    } else {
        Err(CheckError::RustVersionTooNew {
            rust_version,
            toolchain,
        })
    }
}

/// Checks that a workspace member inherits the workspace edition and lints. A member that
/// forgets `[lints] workspace = true` silently builds without the shared lints.
pub fn check_member_manifest(manifest_path: &Path, manifest: &str) -> Result<(), CheckError> {
    let file = manifest_path.display().to_string();
    let table = parse_toml(&file, manifest)?;
    for key in ["package.edition.workspace", "lints.workspace"] {
        let path: Vec<&str> = key.split('.').collect();
        if value_at(&table, &path).and_then(toml::Value::as_bool) != Some(true) {
            return Err(CheckError::NotInherited { file, key });
        }
    }
    Ok(())
}

/// The workspace as reported by `cargo metadata --no-deps --format-version 1`.
#[derive(Debug, PartialEq, Eq)]
pub struct Workspace {
    /// The directory that holds the workspace `Cargo.toml`.
    pub root: PathBuf,
    /// The manifest of every workspace member, sorted.
    pub member_manifests: Vec<PathBuf>,
}

/// Parses `cargo metadata --no-deps --format-version 1` output.
pub fn parse_metadata(metadata_json: &str) -> Result<Workspace, CheckError> {
    const FILE: &str = "cargo metadata output";
    let metadata: serde_json::Value =
        serde_json::from_str(metadata_json).map_err(|error| CheckError::Parse {
            file: FILE.to_owned(),
            message: error.to_string(),
        })?;
    let missing = |key| CheckError::MissingKey {
        file: FILE.to_owned(),
        key,
    };
    let root = metadata
        .get("workspace_root")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| missing("workspace_root"))?;
    let members: Vec<&str> = metadata
        .get("workspace_members")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| missing("workspace_members"))?
        .iter()
        .map(|id| id.as_str().ok_or_else(|| missing("workspace_members")))
        .collect::<Result<_, _>>()?;
    let packages = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| missing("packages"))?;

    let mut member_manifests = Vec::with_capacity(members.len());
    for package in packages {
        let id = package
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| missing("packages.id"))?;
        if members.contains(&id) {
            let manifest = package
                .get("manifest_path")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| missing("packages.manifest_path"))?;
            member_manifests.push(PathBuf::from(manifest));
        }
    }
    if member_manifests.len() != members.len() {
        return Err(missing("packages"));
    }
    member_manifests.sort();
    Ok(Workspace {
        root: PathBuf::from(root),
        member_manifests,
    })
}

/// Workspace crates that other members may use only as `[dev-dependencies]`: test support that
/// must never ship in a service binary. `tools/sql-check` treats their sources as test code
/// (its `TEST_SUPPORT` list), which is safe only while this holds.
pub const DEV_ONLY_CRATES: &[&str] = &["testkit"];

/// Every dependency on a crate in `dev_only` that is not a dev-dependency, as
/// `<package> depends on <crate> as a <normal|build> dependency`, from
/// `cargo metadata --no-deps --format-version 1` output.
pub fn non_dev_uses(metadata_json: &str, dev_only: &[&str]) -> Result<Vec<String>, CheckError> {
    const FILE: &str = "cargo metadata output";
    let metadata: serde_json::Value =
        serde_json::from_str(metadata_json).map_err(|error| CheckError::Parse {
            file: FILE.to_owned(),
            message: error.to_string(),
        })?;
    let missing = |key| CheckError::MissingKey {
        file: FILE.to_owned(),
        key,
    };
    let packages = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| missing("packages"))?;
    let mut uses = Vec::new();
    for package in packages {
        let package_name = package
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| missing("packages.name"))?;
        let dependencies = package
            .get("dependencies")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| missing("packages.dependencies"))?;
        for dependency in dependencies {
            let name = dependency
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| missing("packages.dependencies.name"))?;
            if !dev_only.contains(&name) {
                continue;
            }
            // `kind` is null for a normal dependency, "dev" or "build" otherwise.
            match dependency.get("kind").and_then(serde_json::Value::as_str) {
                Some("dev") => {}
                kind => uses.push(format!(
                    "{package_name} depends on {name} as a {} dependency",
                    kind.unwrap_or("normal")
                )),
            }
        }
    }
    uses.sort();
    Ok(uses)
}

/// Features that only tests may enable, as `(crate, feature)`. Each one switches a safety check
/// off for test code, such as the agent's address guard, which refuses loopback where the fake
/// Modbus endpoint listens (Laysi/trustiot-v2#653), or injects a fault, such as the panic in the
/// certs service's CA reload that a lab run's test image triggers (Laysi/trustiot-v2#851), so a
/// build that ships must never have one.
pub const TEST_ONLY_FEATURES: &[(&str, &str)] = &[
    ("agent", "test-exempt-addresses"),
    ("certs", "e2e-reload-panic"),
];

/// Every way a build could enable a feature in `test_only` outside tests, as one line each, from
/// `cargo metadata --no-deps --format-version 1` output: a normal or build dependency that asks
/// for it, and a feature of any package that turns it on (`<dependency>/<feature>` or
/// `<dependency>?/<feature>`, or another feature of the crate itself, `default` included).
/// A dev-dependency that asks for it is fine: only tests build with dev-dependencies.
pub fn test_only_feature_uses(
    metadata_json: &str,
    test_only: &[(&str, &str)],
) -> Result<Vec<String>, CheckError> {
    const FILE: &str = "cargo metadata output";
    let metadata: serde_json::Value =
        serde_json::from_str(metadata_json).map_err(|error| CheckError::Parse {
            file: FILE.to_owned(),
            message: error.to_string(),
        })?;
    let missing = |key| CheckError::MissingKey {
        file: FILE.to_owned(),
        key,
    };
    let packages = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| missing("packages"))?;
    let mut uses = Vec::new();
    for package in packages {
        let package_name = package
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| missing("packages.name"))?;
        let dependencies = package
            .get("dependencies")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| missing("packages.dependencies"))?;
        // The name a feature string uses for each dependency (its rename, if any), and the crate.
        let mut dependency_keys = Vec::new();
        for dependency in dependencies {
            let name = dependency
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| missing("packages.dependencies.name"))?;
            let key = dependency
                .get("rename")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(name);
            dependency_keys.push((key, name));
            let kind = dependency.get("kind").and_then(serde_json::Value::as_str);
            if kind == Some("dev") {
                continue;
            }
            let features = dependency
                .get("features")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| missing("packages.dependencies.features"))?;
            for (krate, feature) in test_only {
                if name == *krate
                    && features
                        .iter()
                        .any(|enabled| enabled.as_str() == Some(*feature))
                {
                    uses.push(format!(
                        "{package_name} enables {krate}/{feature} through a {} dependency",
                        kind.unwrap_or("normal")
                    ));
                }
            }
        }
        let features = package
            .get("features")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| missing("packages.features"))?;
        for (feature_name, enables) in features {
            let enables = enables
                .as_array()
                .ok_or_else(|| missing("packages.features"))?;
            for enabled in enables.iter().filter_map(serde_json::Value::as_str) {
                for (krate, feature) in test_only {
                    let own = package_name == *krate && enabled == *feature;
                    let through_dependency =
                        enabled
                            .split_once('/')
                            .is_some_and(|(dependency, enabled_feature)| {
                                let key = dependency.strip_suffix('?').unwrap_or(dependency);
                                enabled_feature == *feature
                                    && dependency_keys
                                        .iter()
                                        .any(|(name, target)| *name == key && target == krate)
                            });
                    if own || through_dependency {
                        uses.push(format!(
                            "{package_name} feature {feature_name} enables {krate}/{feature}"
                        ));
                    }
                }
            }
        }
    }
    uses.sort();
    Ok(uses)
}

/// Directories the `AGENTS.md` search skips: build output, installed dependencies, and hidden
/// directories such as `.git` and the agent worktrees under `.claude`.
fn is_skipped_dir(name: &OsStr) -> bool {
    name == "target" || name == "node_modules" || name.as_encoded_bytes().starts_with(b".")
}

/// Lists every file or symlink named `AGENTS.md` (any letter case) under `root`, sorted.
/// Agent instructions live only in `CLAUDE.md` (docs/architecture/repository-layout.md).
/// Symlinks are reported, never followed.
pub fn find_agents_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            // `DirEntry::file_type` does not follow symlinks.
            if entry.file_type()?.is_dir() {
                if !is_skipped_dir(&name) {
                    pending.push(entry.path());
                }
            } else if name
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case("AGENTS.md"))
            {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Directories of runtime code that must never change a database schema: the `src` directory
/// of every cloud service (decision 14; Laysi/trustiot#103). Schema changes live in each
/// service's `migrations/` and are applied by the migration Job.
pub const RUNTIME_SOURCE_GLOB: &str = "services/*/src";

/// Lists the Rust files under `services/*/src`, sorted.
pub fn find_runtime_sources(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let services = root.join("services");
    if !services.is_dir() {
        return Ok(found);
    }
    let mut pending = Vec::new();
    for entry in fs::read_dir(&services)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            pending.push(entry.path().join("src"));
        }
    }
    while let Some(dir) = pending.pop() {
        if !dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                pending.push(path);
            } else if path.extension() == Some(OsStr::new("rs")) {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// A schema-changing SQL statement in source code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaChange {
    /// 1-based line of the statement's first word.
    pub line: usize,
    /// The statement's leading words, upper-cased, for example `ALTER TABLE`.
    pub statement: String,
}

const SCHEMA_VERBS: [&str; 9] = [
    "CREATE", "ALTER", "DROP", "RENAME", "TRUNCATE", "ATTACH", "DETACH", "EXCHANGE", "UNDROP",
];
/// Words that may sit between the verb and the object, as in `CREATE OR REPLACE TEMPORARY TABLE`.
const SCHEMA_MODIFIERS: [&str; 11] = [
    "OR",
    "REPLACE",
    "TEMPORARY",
    "TEMP",
    "IF",
    "NOT",
    "EXISTS",
    "MATERIALIZED",
    "LIVE",
    "WINDOW",
    "UNLOGGED",
];
const SCHEMA_OBJECTS: [&str; 5] = ["TABLE", "TABLES", "DATABASE", "VIEW", "DICTIONARY"];

/// Finds SQL statements that create, alter or drop tables, views, dictionaries or databases in
/// Rust source. Words are compared case-insensitively and may span lines; identifiers such as
/// `create_table` are single words and do not match. Lines that start with `//` (comments and
/// doc comments) are skipped. `ALTER TABLE ... DELETE` matches too: runtime code deletes rows
/// with `DELETE FROM`, which needs no schema privilege.
pub fn find_schema_changes(source: &str) -> Vec<SchemaChange> {
    let mut words: Vec<(usize, String)> = Vec::new();
    for (index, line) in source.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        words.extend(
            line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .filter(|word| !word.is_empty())
                .map(|word| (index + 1, word.to_ascii_uppercase())),
        );
    }
    let mut found = Vec::new();
    for (start, (line, verb)) in words.iter().enumerate() {
        if !SCHEMA_VERBS.contains(&verb.as_str()) {
            continue;
        }
        let object = words
            .iter()
            .skip(start + 1)
            .find(|(_, word)| !SCHEMA_MODIFIERS.contains(&word.as_str()));
        if let Some((_, object)) = object
            && SCHEMA_OBJECTS.contains(&object.as_str())
        {
            found.push(SchemaChange {
                line: *line,
                statement: format!("{verb} {object}"),
            });
        }
    }
    found
}

/// Directories searched for Rust code that removes containers, test support included (the
/// dev environment's TypeScript is checked by its own end-to-end test). `tools/repo-check` is
/// left out because its own tests hold the examples.
pub const CONTAINER_REMOVAL_ROOTS: [&str; 5] = ["crates", "services", "edge", "factory", "tools"];

/// Lists the Rust files under [`CONTAINER_REMOVAL_ROOTS`], sorted. Build output, installed
/// dependencies, hidden directories and `tools/repo-check` are skipped.
pub fn find_container_removal_sources(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending: Vec<PathBuf> = CONTAINER_REMOVAL_ROOTS
        .iter()
        .map(|name| root.join(name))
        .filter(|path| path.is_dir())
        .collect();
    let own = root.join("tools").join("repo-check");
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                if !is_skipped_dir(&entry.file_name()) && path != own {
                    pending.push(path);
                }
            } else if path.extension() == Some(OsStr::new("rs")) {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Finds container removals that keep the container's anonymous volumes: an argument list that
/// starts with `"rm"` and holds `"--force"` (or `"-f"`) but neither `"--volumes"` nor `"-v"`,
/// as in `.args(["rm", "--force", &name])`. Returns the 1-based line of each `"rm"`.
///
/// The Postgres, ClickHouse and Mosquitto images declare `VOLUME`s, so every container the tests
/// start gets anonymous volumes, and `rm --force` alone leaves them behind: a workstation ran out
/// of podman locks with 2,011 of them (Laysi/trustiot-v2#334). Lists such as
/// `["volume", "rm", ...]` or `["image", "rm", ...]` do not start with `"rm"` and are not matched.
pub fn find_container_removals_without_volumes(source: &str) -> Vec<usize> {
    let mut found = Vec::new();
    for (start, _) in source.match_indices("\"rm\"") {
        // `start` is the byte offset of an ASCII match, so it is a character boundary.
        let (before, rest) = source.split_at(start);
        if !before.trim_end().ends_with('[') {
            continue;
        }
        let list = rest.split(']').next().unwrap_or(rest);
        let has = |argument: &str| list.contains(&format!("\"{argument}\""));
        if (has("--force") || has("-f")) && !has("--volumes") && !has("-v") {
            found.push(before.matches('\n').count() + 1);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1_99_0: Version = Version {
        major: 1,
        minor: 99,
        patch: 0,
    };

    const TOOLCHAIN: &str = "[toolchain]\nchannel = \"1.99.0\"\ncomponents = [\"clippy\"]\n";

    #[test]
    fn exact_version_accepts_only_major_minor_patch() {
        assert_eq!(Version::parse_exact("1.99.0"), Some(V1_99_0));
        for text in [
            "",
            "1.99",
            "1.99.0.1",
            "stable",
            "1.99.0-beta.1",
            "+1.99.0",
            "1..0",
            "1.99.x",
            " 1.99.0",
        ] {
            assert_eq!(Version::parse_exact(text), None, "{text:?}");
        }
    }

    #[test]
    fn rust_version_accepts_two_or_three_parts() {
        assert_eq!(Version::parse_rust_version("1.99"), Some(V1_99_0));
        assert_eq!(
            Version::parse_rust_version("1.99.1"),
            Some(Version {
                patch: 1,
                ..V1_99_0
            })
        );
        for text in ["", "1", "1.99.0.1", "1.99-nightly", "stable"] {
            assert_eq!(Version::parse_rust_version(text), None, "{text:?}");
        }
    }

    #[test]
    fn versions_order_numerically() {
        let v1_100_0 = Version {
            minor: 100,
            ..V1_99_0
        };
        assert!(V1_99_0 < v1_100_0);
        assert_eq!(v1_100_0.to_string(), "1.100.0");
    }

    #[test]
    fn matching_rust_pins_pass() {
        let prototools = "# comment\nbun = \"1.4.2\"\nrust = \"1.99.0\"\n";
        assert_eq!(check_rust_pins(prototools, TOOLCHAIN), Ok(V1_99_0));
        assert_eq!(
            prototools_pin(prototools, "bun"),
            Ok(Version {
                major: 1,
                minor: 4,
                patch: 2
            })
        );
    }

    #[test]
    fn different_rust_pins_fail() {
        assert_eq!(
            check_rust_pins("rust = \"1.98.1\"\n", TOOLCHAIN),
            Err(CheckError::RustPinMismatch {
                prototools: Version {
                    major: 1,
                    minor: 98,
                    patch: 1
                },
                rust_toolchain: V1_99_0,
            })
        );
    }

    #[test]
    fn inexact_rust_pins_fail() {
        assert!(matches!(
            check_rust_pins("rust = \"1.99\"\n", TOOLCHAIN),
            Err(CheckError::InvalidVersion { key: "rust", .. })
        ));
        assert!(matches!(
            check_rust_pins("rust = \"1.99.0\"\n", "[toolchain]\nchannel = \"stable\"\n"),
            Err(CheckError::InvalidVersion {
                key: "toolchain.channel",
                ..
            })
        ));
    }

    #[test]
    fn missing_or_malformed_pins_fail() {
        assert!(matches!(
            check_rust_pins("bun = \"1.4.2\"\n", TOOLCHAIN),
            Err(CheckError::MissingKey { key: "rust", .. })
        ));
        assert!(matches!(
            check_rust_pins("rust = 1.99\n", TOOLCHAIN),
            Err(CheckError::MissingKey { key: "rust", .. })
        ));
        assert!(matches!(
            check_rust_pins("rust = \"1.99.0\"\n", "channel = \"1.99.0\"\n"),
            Err(CheckError::MissingKey {
                key: "toolchain.channel",
                ..
            })
        ));
        assert!(matches!(
            check_rust_pins("rust = \n", TOOLCHAIN),
            Err(CheckError::Parse { .. })
        ));
    }

    #[test]
    fn rust_version_must_not_exceed_toolchain() {
        let manifest = |rust_version: &str| {
            format!("[workspace]\n[workspace.package]\nrust-version = \"{rust_version}\"\n")
        };
        assert_eq!(check_rust_version(&manifest("1.99"), V1_99_0), Ok(()));
        assert_eq!(check_rust_version(&manifest("1.85.1"), V1_99_0), Ok(()));
        assert_eq!(
            check_rust_version(&manifest("1.99.1"), V1_99_0),
            Err(CheckError::RustVersionTooNew {
                rust_version: Version {
                    patch: 1,
                    ..V1_99_0
                },
                toolchain: V1_99_0,
            })
        );
        assert!(matches!(
            check_rust_version(&manifest("latest"), V1_99_0),
            Err(CheckError::InvalidVersion { .. })
        ));
        assert!(matches!(
            check_rust_version("[workspace]\n", V1_99_0),
            Err(CheckError::MissingKey { .. })
        ));
    }

    #[test]
    fn member_must_inherit_edition_and_lints() {
        let path = Path::new("crates/example/Cargo.toml");
        let inherits = "[package]\nname = \"example\"\nedition.workspace = true\n\n[lints]\nworkspace = true\n";
        assert_eq!(check_member_manifest(path, inherits), Ok(()));

        let cases = [
            (
                "[package]\nname = \"example\"\nedition.workspace = true\n",
                "lints.workspace",
            ),
            (
                "[package]\nname = \"example\"\nedition.workspace = true\n[lints]\nworkspace = false\n",
                "lints.workspace",
            ),
            (
                "[package]\nname = \"example\"\nedition = \"2024\"\n[lints]\nworkspace = true\n",
                "package.edition.workspace",
            ),
        ];
        for (manifest, key) in cases {
            assert_eq!(
                check_member_manifest(path, manifest),
                Err(CheckError::NotInherited {
                    file: path.display().to_string(),
                    key,
                }),
                "{manifest}"
            );
        }
    }

    #[test]
    fn metadata_lists_member_manifests() {
        let metadata = r#"{
            "workspace_root": "/repo",
            "workspace_members": ["path+file:///repo/tools/b#0.1.0", "path+file:///repo/crates/a#0.1.0"],
            "packages": [
                {"id": "path+file:///repo/tools/b#0.1.0", "manifest_path": "/repo/tools/b/Cargo.toml"},
                {"id": "path+file:///repo/crates/a#0.1.0", "manifest_path": "/repo/crates/a/Cargo.toml"},
                {"id": "registry+https://example#c@1.0.0", "manifest_path": "/registry/c/Cargo.toml"}
            ]
        }"#;
        assert_eq!(
            parse_metadata(metadata),
            Ok(Workspace {
                root: PathBuf::from("/repo"),
                member_manifests: vec![
                    PathBuf::from("/repo/crates/a/Cargo.toml"),
                    PathBuf::from("/repo/tools/b/Cargo.toml"),
                ],
            })
        );
    }

    #[test]
    fn metadata_errors_are_reported() {
        assert!(matches!(
            parse_metadata("not json"),
            Err(CheckError::Parse { .. })
        ));
        assert!(matches!(
            parse_metadata(r#"{"workspace_members": [], "packages": []}"#),
            Err(CheckError::MissingKey {
                key: "workspace_root",
                ..
            })
        ));
        // A member without a package entry means the output is malformed; refuse to check a
        // partial member list.
        assert!(matches!(
            parse_metadata(
                r#"{"workspace_root": "/repo", "workspace_members": ["a"], "packages": []}"#
            ),
            Err(CheckError::MissingKey {
                key: "packages",
                ..
            })
        ));
    }

    #[test]
    fn test_only_features_are_enabled_only_by_dev_dependencies() {
        let metadata = r#"{"packages": [
            {"name": "agent", "features": {"test-exempt-addresses": [], "fast": []},
             "dependencies": [
                {"name": "agent", "kind": "dev", "features": ["test-exempt-addresses"]},
                {"name": "libc", "kind": null, "features": []}
            ]},
            {"name": "gateway", "features": {}, "dependencies": [
                {"name": "agent", "kind": null, "features": ["fast"]}
            ]},
            {"name": "probe", "features": {}, "dependencies": [
                {"name": "agent", "kind": "dev", "features": ["test-exempt-addresses"]}
            ]}
        ]}"#;
        assert_eq!(
            test_only_feature_uses(metadata, TEST_ONLY_FEATURES),
            Ok(vec![])
        );

        let metadata = r#"{"packages": [
            {"name": "agent", "features": {
                "test-exempt-addresses": [],
                "default": ["test-exempt-addresses"],
                "lab": ["fast", "test-exempt-addresses"]
             }, "dependencies": []},
            {"name": "gateway", "features": {}, "dependencies": [
                {"name": "agent", "kind": null, "features": ["test-exempt-addresses"]}
            ]},
            {"name": "build-tool", "features": {}, "dependencies": [
                {"name": "agent", "kind": "build", "features": ["test-exempt-addresses"]}
            ]},
            {"name": "renamed", "features": {
                "on": ["edge-agent/test-exempt-addresses"],
                "maybe": ["edge-agent?/test-exempt-addresses"],
                "unrelated": ["other/test-exempt-addresses"]
             }, "dependencies": [
                {"name": "agent", "rename": "edge-agent", "kind": null, "optional": true,
                 "features": []},
                {"name": "other", "kind": null, "features": []}
            ]}
        ]}"#;
        assert_eq!(
            test_only_feature_uses(metadata, TEST_ONLY_FEATURES),
            Ok(vec![
                "agent feature default enables agent/test-exempt-addresses".to_owned(),
                "agent feature lab enables agent/test-exempt-addresses".to_owned(),
                "build-tool enables agent/test-exempt-addresses through a build dependency"
                    .to_owned(),
                "gateway enables agent/test-exempt-addresses through a normal dependency"
                    .to_owned(),
                "renamed feature maybe enables agent/test-exempt-addresses".to_owned(),
                "renamed feature on enables agent/test-exempt-addresses".to_owned(),
            ])
        );
        // The certs test images' reload panic (Laysi/trustiot-v2#851) is checked the same way.
        let metadata = r#"{"packages": [
            {"name": "certs", "features": {
                "e2e-reload-panic": [],
                "default": ["e2e-reload-panic"]
             }, "dependencies": [
                {"name": "certs", "kind": "dev", "features": ["e2e-reload-panic"]}
            ]},
            {"name": "bundle", "features": {}, "dependencies": [
                {"name": "certs", "kind": null, "features": ["e2e-reload-panic"]}
            ]}
        ]}"#;
        assert_eq!(
            test_only_feature_uses(metadata, TEST_ONLY_FEATURES),
            Ok(vec![
                "bundle enables certs/e2e-reload-panic through a normal dependency".to_owned(),
                "certs feature default enables certs/e2e-reload-panic".to_owned(),
            ])
        );
        assert!(matches!(
            test_only_feature_uses(
                r#"{"packages": [{"name": "a", "dependencies": []}]}"#,
                TEST_ONLY_FEATURES
            ),
            Err(CheckError::MissingKey {
                key: "packages.features",
                ..
            })
        ));
    }

    #[test]
    fn dev_only_crates_must_be_dev_dependencies() {
        let metadata = r#"{"packages": [
            {"name": "assets", "dependencies": [
                {"name": "testkit", "kind": "dev"},
                {"name": "sqlx", "kind": null}
            ]},
            {"name": "alerts", "dependencies": [{"name": "testkit", "kind": null}]},
            {"name": "ingest", "dependencies": [{"name": "testkit", "kind": "build"}]},
            {"name": "testkit", "dependencies": []}
        ]}"#;
        assert_eq!(
            non_dev_uses(metadata, DEV_ONLY_CRATES),
            Ok(vec![
                "alerts depends on testkit as a normal dependency".to_owned(),
                "ingest depends on testkit as a build dependency".to_owned(),
            ])
        );
        assert!(matches!(
            non_dev_uses(r#"{"packages": [{"name": "a"}]}"#, DEV_ONLY_CRATES),
            Err(CheckError::MissingKey {
                key: "packages.dependencies",
                ..
            })
        ));
    }

    #[test]
    fn schema_changes_are_found() {
        let source = r#"
            const A: &str = "CREATE TABLE IF NOT EXISTS t (a UInt8) ENGINE = MergeTree";
            const B: &str = "create or replace temporary table t AS SELECT 1";
            let c = "ALTER
                TABLE trustiot.telemetry ADD COLUMN x UInt8";
            let d = format!("DROP DATABASE {name}");
            let e = "ALTER TABLE t DELETE WHERE 1";
            let f = "CREATE MATERIALIZED VIEW IF NOT EXISTS v TO t AS SELECT 1";
            let g = "TRUNCATE TABLE t; RENAME TABLE a TO b; EXCHANGE TABLES a AND b";
        "#;
        let found: Vec<(usize, String)> = find_schema_changes(source)
            .into_iter()
            .map(|change| (change.line, change.statement))
            .collect();
        let expected: Vec<(usize, String)> = [
            (2, "CREATE TABLE"),
            (3, "CREATE TABLE"),
            (4, "ALTER TABLE"),
            (6, "DROP DATABASE"),
            (7, "ALTER TABLE"),
            (8, "CREATE VIEW"),
            (9, "TRUNCATE TABLE"),
            (9, "RENAME TABLE"),
            (9, "EXCHANGE TABLES"),
        ]
        .into_iter()
        .map(|(line, statement)| (line, statement.to_owned()))
        .collect();
        assert_eq!(found, expected);
    }

    #[test]
    fn data_statements_comments_and_identifiers_are_not_schema_changes() {
        let source = r#"
            // Runtime code never runs CREATE TABLE; the migration Job does.
            /// ALTER TABLE belongs in migrations/.
            const INSERT: &str = "INSERT INTO trustiot.telemetry (a) VALUES (1)";
            const DELETE: &str = "DELETE FROM trustiot.telemetry WHERE stream_id = {s:String}";
            const USER: &str = "CREATE USER tenant_x IDENTIFIED WITH sha256_password BY 'x'";
            const GRANT: &str = "GRANT trustiot_tenant_reader TO tenant_x";
            fn create_table() {}
            let drop_table_count = 0;
            let created = "the table was created";
        "#;
        assert_eq!(find_schema_changes(source), Vec::new());
    }

    #[test]
    fn container_removals_that_keep_volumes_are_found() {
        let source = r#"
            Command::new(runtime).args(["rm", "--force", &self.name]);
            podmanOk(["rm", "-f", name]);
            runtime_output(runtime, &[
                "rm",
                "--force",
                &name,
            ]);
        "#;
        assert_eq!(
            find_container_removals_without_volumes(source),
            vec![2, 3, 5]
        );
    }

    #[test]
    fn container_removals_with_volumes_and_other_removals_are_not_found() {
        let source = r#"
            Command::new(runtime).args(["rm", "--force", "--volumes", &self.name]);
            podmanOk(["rm", "--force", "--volumes", "--time", "10", ...names]);
            podmanOk(["rm", "-f", "-v", name]);
            podmanOk(["volume", "rm", "--force", volume]);
            podmanOk(["network", "rm", "--force", network]);
            podmanOk(["image", "rm", "--force", image]);
            podmanOk(["rmi", "--force", image]);
            podmanOk(["rm", name]);
            let unterminated = "rm";
        "#;
        assert_eq!(
            find_container_removals_without_volumes(source),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn errors_name_the_file_and_key() {
        let error = CheckError::InvalidVersion {
            file: PROTOTOOLS.to_owned(),
            key: "rust",
            value: "1.99".to_owned(),
            expected: EXACT,
        };
        assert_eq!(
            error.to_string(),
            ".prototools: `rust` is `1.99`; expected an exact MAJOR.MINOR.PATCH version"
        );
    }
}
