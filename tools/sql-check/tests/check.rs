//! Applies the SQL check to its fixtures and to the whole workspace.

use std::fs;
use std::path::{Path, PathBuf};

use sql_check::{Finding, Rule, check_source, check_tree};

const RULES: [Rule; 7] = [
    Rule::DynamicSql,
    Rule::AssertSqlSafe,
    Rule::DynamicQueryBuilder,
    Rule::LeakedString,
    Rule::SqlBuiltFromStrings,
    Rule::SessionSetting,
    Rule::UncheckedInclude,
];

fn fixture(name: &str) -> (PathBuf, String) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let source = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    (path, source)
}

/// Checks a fixture as if it were a service's production source (not under `tests/`, where SQL
/// may also come from variables).
fn check_fixture(name: &str) -> Vec<Finding> {
    let (file, source) = fixture(name);
    let path = Path::new("services/example/src").join(name);
    check_source(&path, file.parent(), &source, false).unwrap_or_else(|error| panic!("{error}"))
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

/// The rules a fixture line's trailing comment names, such as `// DynamicSql AssertSqlSafe`.
fn marked_rules(line: &str) -> Vec<String> {
    let Some((_, comment)) = line.rsplit_once("// ") else {
        return Vec::new();
    };
    let names: Vec<String> = comment.split_whitespace().map(str::to_owned).collect();
    let all_rules = names
        .iter()
        .all(|name| RULES.iter().any(|rule| format!("{rule:?}") == *name));
    if all_rules { names } else { Vec::new() }
}

/// The concatenated fixture marks each line the check must report with a trailing comment
/// naming the rules; the check reports exactly those lines and rules.
#[test]
fn concatenated_sql_is_reported_on_every_marked_line() {
    let (_, source) = fixture("concatenated.rs");
    let mut expected: Vec<(usize, String)> = source
        .lines()
        .enumerate()
        .flat_map(|(index, line)| {
            marked_rules(line)
                .into_iter()
                .map(move |rule| (index + 1, rule))
        })
        .collect();
    expected.sort();
    let findings = check_fixture("concatenated.rs");
    let mut found: Vec<(usize, String)> = findings
        .iter()
        .map(|finding| (finding.line, format!("{:?}", finding.rule)))
        .collect();
    found.sort();
    found.dedup();
    assert_eq!(found, expected);
    for finding in findings {
        println!("{finding}");
    }
}

#[test]
fn every_rule_is_exercised_by_the_fixture() {
    let found: Vec<Rule> = check_fixture("concatenated.rs")
        .iter()
        .map(|finding| finding.rule)
        .collect();
    for rule in RULES {
        assert!(found.contains(&rule), "{rule:?} missing from {found:?}");
    }
}

#[test]
fn parameterized_sql_is_not_reported() {
    let findings = check_fixture("parameterized.rs");
    assert!(findings.is_empty(), "{findings:#?}");
}

/// The CI gate: no Rust file in the workspace builds SQL from strings or sets the tenant
/// outside service-kit's tenant module.
#[test]
fn the_workspace_has_no_string_built_sql() {
    let findings = check_tree(&workspace_root()).unwrap_or_else(|error| panic!("{error}"));
    let report: Vec<String> = findings.iter().map(ToString::to_string).collect();
    assert!(
        findings.is_empty(),
        "SQL built from strings (decision 29):\n{}",
        report.join("\n")
    );
}

/// The allow-list and the skip list name paths that exist, so a rename cannot silently drop or
/// widen an exception.
#[test]
fn listed_paths_exist() {
    let root = workspace_root();
    for allowed in sql_check::SESSION_SETTING_ALLOWED {
        assert!(root.join(allowed).is_file(), "{allowed} does not exist");
    }
    for skipped in sql_check::SKIPPED {
        assert!(root.join(skipped).is_dir(), "{skipped} does not exist");
    }
    for crate_dir in sql_check::TEST_SUPPORT {
        assert!(
            root.join(crate_dir).join("Cargo.toml").is_file(),
            "{crate_dir} is not a crate"
        );
    }
    for exception in sql_check::SET_CONFIG_ALLOWED {
        assert!(
            root.join(exception.path).is_file(),
            "{} does not exist",
            exception.path
        );
        let setting = exception.setting.to_ascii_lowercase();
        assert!(
            !setting.starts_with("trustiot.")
                && setting != "role"
                && setting != "session_authorization",
            "{}: {} cannot be allowed ({})",
            exception.path,
            exception.setting,
            exception.reason
        );
    }
    for exception in sql_check::EXCEPTIONS {
        let path = Path::new(exception.path);
        assert!(
            root.join(path).is_file(),
            "{} does not exist",
            exception.path
        );
        assert!(
            path.components()
                .any(|component| component.as_os_str() == "tests")
                && !path
                    .components()
                    .any(|component| component.as_os_str() == "src"),
            "{} is not a test file ({})",
            exception.path,
            exception.reason
        );
        assert!(
            !exception.rules.contains(&Rule::UncheckedInclude),
            "{}: included files are always checked",
            exception.path
        );
    }
}

/// The workspace check sees each allowed file, and finds its session SQL only because it is
/// allowed: the same source elsewhere is a finding. So every entry is still needed.
#[test]
fn the_allowed_files_are_the_only_exceptions() {
    let root = workspace_root();
    for allowed in sql_check::SESSION_SETTING_ALLOWED {
        let source = fs::read_to_string(root.join(allowed)).expect("read the allowed file");
        let dir = root.join(allowed);
        let elsewhere = check_source(
            Path::new("crates/other/src/lib.rs"),
            dir.parent(),
            &source,
            false,
        )
        .unwrap_or_else(|error| panic!("{error}"));
        assert!(
            elsewhere
                .iter()
                .any(|finding| finding.rule == Rule::SessionSetting),
            "{allowed}: {elsewhere:?}"
        );
    }
}
