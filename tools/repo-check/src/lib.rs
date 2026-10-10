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
