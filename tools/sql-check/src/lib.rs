//! Static check that SQL in the workspace is never built from strings (decision 29).
//!
//! Row-level security does not stop SQL injection: injected SQL runs as the service role and can
//! change the tenant setting. Services therefore only run parameterized SQL: sqlx's compile-time
//! macros, or static SQL with bound parameters (`query("... $1").bind(value)`, `QueryBuilder`'s
//! `push_bind`). sqlx 0.9 already refuses a runtime `String` as SQL; this check closes the ways
//! around that and the ways to switch tenants from SQL:
//!
//! | Rule | Rejects |
//! | --- | --- |
//! | [`Rule::DynamicSql`] | `sqlx::query`, `query_as`, `query_scalar`, `query_with`, `query_as_with`, `query_scalar_with`, and `raw_sql` with SQL that is not a string literal, `include_str!`, `concat!` of literals (nested `concat!` and `stringify!` included), or a constant: a `SCREAMING_CASE` path with a module (`queries::LIST`, `Self::LIST`), or a single `SCREAMING_CASE` name that the file declares as a `const` or `static` or imports with `use`, and never binds as a variable or parameter |
//! | [`Rule::AssertSqlSafe`] | any use of `AssertSqlSafe`, sqlx's opt-out of that check |
//! | [`Rule::DynamicQueryBuilder`] | `QueryBuilder::new`/`with_arguments`, `push`, `push_unseparated`, and `separated` with anything but static SQL; values go through `push_bind` |
//! | [`Rule::LeakedString`] | `.leak()` and `Box::leak` in files that use sqlx: a leaked `String` passes runtime text as `&'static str` |
//! | [`Rule::SqlBuiltFromStrings`] | `format!`, `format_args!`, `write!`, `writeln!`, `+`, `push_str`, and `insert_str` with text that starts like SQL (`SELECT`, `WHERE`, ...) |
//! | [`Rule::SessionSetting`] | SQL that sets or resets `trustiot.*` settings or the role, calls `set_config` (except with the literal name of a setting in [`SET_CONFIG_SETTINGS`], or of the file's entry in [`SET_CONFIG_ALLOWED`]), or names `pg_settings`, outside [`SESSION_SETTING_ALLOWED`]: only service-kit's tenant module sets the tenant. The check reads string literals, the joined text of each `concat!`, the files `include_str!`/`include_bytes!` name, and the static text a file passes to query builders, joined in source order; a query builder fragment that names `trustiot` is reported too |
//! | [`Rule::UncheckedInclude`] | `include_str!`/`include_bytes!` of a file the check cannot read (a path that is not a literal, or a missing file), whose SQL it therefore cannot check |
//!
//! Test code (files under a `tests` directory, `#[cfg(test)]` modules, and the dev-only crates
//! in [`TEST_SUPPORT`]) may also pass SQL through a plain variable to the first and third rules,
//! such as a loop over a table of literals; every other rule applies to it unchanged.
//!
//! The check reads Rust syntax, not types, so it works on names: a `query` function must be
//! called as `sqlx::query` or imported from `sqlx`, and a query builder must be a local variable,
//! parameter, or closure parameter the check saw being made one. Doc comments are not checked.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use proc_macro2::{Literal, Span, TokenStream, TokenTree};
use syn::parse::{ParseStream, Parser};
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{Expr, Lit, LitStr, Macro, Pat, Token, UseTree};

/// Files allowed to contain SQL that sets the tenant: the kit's tenant transactions.
pub const SESSION_SETTING_ALLOWED: &[&str] = &["crates/service-kit/src/tenant.rs"];

/// Crates, relative to the workspace root, whose sources are test code: crates used only as
/// `[dev-dependencies]` (`tools/repo-check` enforces it), so their SQL never runs in a service.
/// `crates/testkit` runs the probes of the cross-tenant suite, whose SQL its macros build at
/// compile time from each declared table name, and passes them on as variables.
pub const TEST_SUPPORT: &[&str] = &["crates/testkit"];

/// Whether `path` (relative to the workspace root) is test code: under a `tests` directory or
/// in a [`TEST_SUPPORT`] crate.
fn is_test_path(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == "tests")
        || TEST_SUPPORT.iter().any(|dir| path.starts_with(dir))
}

/// Directories, relative to the workspace root, that the workspace check skips: this tool,
/// whose source and fixtures name every pattern it rejects. Its fixtures are checked by its own
/// tests instead.
pub const SKIPPED: &[&str] = &["tools/sql-check"];

/// Settings any code may change with `set_config`: they bound time or label the session, and
/// change neither the role nor which rows or objects a query reaches.
pub const SET_CONFIG_SETTINGS: &[&str] = &[
    "application_name",
    "idle_in_transaction_session_timeout",
    "lock_timeout",
    "statement_timeout",
];

/// A file that may call `set_config` for one more setting, and why.
pub struct SettingException {
    /// The file, relative to the workspace root.
    pub path: &'static str,
    /// The setting name, as written in the SQL.
    pub setting: &'static str,
    /// Why, including where the value comes from.
    pub reason: &'static str,
}

/// Files that may call `set_config` for a setting outside [`SET_CONFIG_SETTINGS`].
pub const SET_CONFIG_ALLOWED: &[SettingException] = &[
    SettingException {
        path: "crates/outbox/src/consumer.rs",
        setting: "search_path",
        reason: "the read transaction searches only the producer's schema; the value is the \
                 configured schema name, which ConsumerConfig::new accepts only when \
                 is_schema_name holds (lower-case letters, digits, and _, not pg_*)",
    },
    SettingException {
        path: "crates/outbox/src/consumer.rs",
        setting: "outbox.delivery",
        reason: "a transaction-level marker the consumer sets before calling the handler and \
                 reads after it, to tell whether the handler ended the transaction; the value \
                 is a UUID the consumer generates",
    },
    SettingException {
        path: "crates/outbox/tests/delivery.rs",
        setting: "outbox_test.tenant",
        reason: "a test-only setting the test handler sets and its policies read",
    },
];

/// Rules waived for a test file that builds SQL from strings: every rule but the ones against
/// session settings and unreadable includes.
pub const DYNAMIC_SQL: &[Rule] = &[
    Rule::DynamicSql,
    Rule::AssertSqlSafe,
    Rule::DynamicQueryBuilder,
    Rule::LeakedString,
    Rule::SqlBuiltFromStrings,
];

/// A test file that may break some rules, and why.
pub struct Exception {
    /// The file, relative to the workspace root; always under a `tests` directory.
    pub path: &'static str,
    /// The rules that do not apply to it.
    pub rules: &'static [Rule],
    /// Why.
    pub reason: &'static str,
}

/// Test files that may break some rules. Tests that create per-run roles, schemas, or users
/// need identifiers in DDL, which cannot be bound.
pub const EXCEPTIONS: &[Exception] = &[
    Exception {
        path: "crates/outbox/tests/delivery.rs",
        rules: DYNAMIC_SQL,
        reason: "creates per-run roles and grants on per-run schemas",
    },
    Exception {
        path: "crates/outbox/tests/support/mod.rs",
        rules: DYNAMIC_SQL,
        reason: "creates per-run schemas, roles, grants, and policies, and runs service-kit's \
                 outbox policy template with the per-run role names",
    },
    Exception {
        path: "crates/outbox/tests/verify_271.rs",
        rules: &[
            Rule::DynamicSql,
            Rule::AssertSqlSafe,
            Rule::DynamicQueryBuilder,
            Rule::LeakedString,
            Rule::SqlBuiltFromStrings,
            Rule::SessionSetting,
        ],
        reason: "independent verification of Laysi/trustiot-v2#271: test handlers run SET ROLE, \
                 RESET ALL, and set_config on the tenant with per-run role names, as attacks \
                 that must fail or as reported findings",
    },
    Exception {
        path: "crates/outbox/tests/failure_paths.rs",
        rules: &[Rule::SessionSetting],
        reason: "a test handler's begin hook moves the delivery's transaction to another tenant \
                 with set_config, as a broken handler would, to test that the consumer \
                 dead-letters the event (Laysi/trustiot-v2#350)",
    },
    Exception {
        path: "services/ingest/tests/clickhouse.rs",
        rules: &[
            Rule::DynamicSql,
            Rule::AssertSqlSafe,
            Rule::DynamicQueryBuilder,
            Rule::LeakedString,
            Rule::SqlBuiltFromStrings,
            Rule::SessionSetting,
        ],
        reason: "ClickHouse, not Postgres: creates per-run users, roles, and tenants, and runs \
                 SET ROLE NONE as an attack that must fail",
    },
    Exception {
        path: "services/identity/tests/adversarial.rs",
        rules: &[Rule::SessionSetting],
        reason: "attacks the identity service role's session: sets valid, unknown, empty and \
                 malformed tenant ids with set_config and runs SET ROLE and SET SESSION \
                 AUTHORIZATION, which row-level security and the database must refuse",
    },
    Exception {
        path: "crates/service-kit/tests/tenant_db.rs",
        rules: &[Rule::SessionSetting],
        reason: "sets a session-level tenant with set_config, as nothing may, to test that the \
                 pool closes such a session on release instead of handing it to the next user \
                 (Laysi/trustiot-v2#271); and sets a tenant as the platform role to test that \
                 row-level security still refuses it a tenant's outbox events and gives it no \
                 extra rows of a table with platform_access, and to record that a table it has \
                 a grant on without platform_access shows it the rows of the tenant it set \
                 (Laysi/trustiot-v2#360); and takes a role the platform role belongs to with \
                 set_config('role', ...) and sets a tenant, to show which rows each setup the \
                 session check refuses for the platform role opens to that tenant \
                 (Laysi/trustiot-v2#366); and takes a role the service or platform role can \
                 become with set_config('role', ...) to show what each setup the session check \
                 refuses for that role reaches without a tenant (Laysi/trustiot-v2#381)",
    },
];

/// sqlx functions whose first argument is SQL.
const QUERY_FUNCTIONS: &[&str] = &[
    "query",
    "query_as",
    "query_as_with",
    "query_scalar",
    "query_scalar_with",
    "query_with",
    "raw_sql",
];

/// Query builder methods whose argument becomes SQL text.
const BUILDER_SQL_METHODS: &[&str] = &["push", "push_unseparated", "separated"];

/// Query builder methods that return the builder (or its `Separated`), for following chains.
const BUILDER_CHAIN_METHODS: &[&str] = &[
    "push",
    "push_bind",
    "push_bind_unseparated",
    "push_tuples",
    "push_unseparated",
    "push_values",
    "separated",
];

/// First words that make a string look like SQL or a SQL fragment. `FROM` and `COPY` are left
/// out because Containerfiles start lines with them; SQL built from strings still has to reach
/// sqlx through the other rules.
const SQL_START_WORDS: &[&str] = &[
    "ALTER",
    "AND",
    "CALL",
    "CREATE",
    "DELETE",
    "DROP",
    "GRANT",
    "GROUP",
    "HAVING",
    "INSERT",
    "JOIN",
    "LIMIT",
    "MERGE",
    "OFFSET",
    "OR",
    "ORDER",
    "RESET",
    "RETURNING",
    "REVOKE",
    "SELECT",
    "SET",
    "TRUNCATE",
    "UPDATE",
    "VALUES",
    "WHERE",
    "WITH",
];

/// A kind of problem the check finds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rule {
    /// SQL passed to a sqlx query function is not static.
    DynamicSql,
    /// `AssertSqlSafe` is used.
    AssertSqlSafe,
    /// A query builder gets SQL text that is not static.
    DynamicQueryBuilder,
    /// A string is leaked in a file that uses sqlx.
    LeakedString,
    /// SQL text is built with formatting or concatenation.
    SqlBuiltFromStrings,
    /// SQL sets or resets the tenant setting or the role.
    SessionSetting,
    /// A file is included that the check cannot read.
    UncheckedInclude,
}

impl Rule {
    /// What to do instead.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::DynamicSql => {
                "SQL passed to sqlx must be a string literal or a constant; bind values with $1 \
                 and .bind()"
            }
            Self::AssertSqlSafe => {
                "AssertSqlSafe turns off sqlx's check that SQL is static; write static SQL and \
                 bind values"
            }
            Self::DynamicQueryBuilder => {
                "QueryBuilder SQL text must be a string literal or a constant; add values with \
                 push_bind"
            }
            Self::LeakedString => {
                "a leaked string passes runtime text to sqlx as &'static str; write static SQL \
                 and bind values"
            }
            Self::SqlBuiltFromStrings => {
                "SQL text is built with format!, concatenation, or push_str; write static SQL \
                 and bind values"
            }
            Self::SessionSetting => {
                "SQL sets or resets a trustiot.* setting or the role, or calls set_config; only \
                 service-kit's tenant transactions set the tenant (decision 29)"
            }
            Self::UncheckedInclude => {
                "include_str!/include_bytes! names a file the check cannot read, so its SQL is \
                 not checked; include a file by a literal path relative to the source file"
            }
        }
    }
}

/// One problem, located in a file.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    /// The file, as given to the check.
    pub path: PathBuf,
    /// 1-based line.
    pub line: usize,
    /// 1-based column.
    pub column: usize,
    /// What is wrong.
    pub rule: Rule,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}: {:?}: {}",
            self.path.display(),
            self.line,
            self.column,
            self.rule,
            self.rule.message()
        )
    }
}

/// Why a file could not be checked.
#[derive(Debug)]
pub enum CheckError {
    /// The file or a directory could not be read.
    Io {
        /// The path.
        path: PathBuf,
        /// The error.
        source: io::Error,
    },
    /// The file is not valid Rust.
    Parse {
        /// The file.
        path: PathBuf,
        /// The parser's message.
        message: String,
    },
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Parse { path, message } => {
                write!(f, "{} is not valid Rust: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for CheckError {}

/// Checks one Rust source file. `path` locates findings; `dir` is the directory of the file on
/// disk, which `include_str!` and `include_bytes!` paths are relative to (with `None`, every
/// include is reported as [`Rule::UncheckedInclude`]); `allow_session_setting` is true only for
/// the files in [`SESSION_SETTING_ALLOWED`].
///
/// # Errors
///
/// When `source` is not valid Rust.
pub fn check_source(
    path: &Path,
    dir: Option<&Path>,
    source: &str,
    allow_session_setting: bool,
) -> Result<Vec<Finding>, CheckError> {
    check_source_allowing(path, dir, source, allow_session_setting, &[])
}

/// [`check_source`], also accepting `set_config` for `settings` (the file's entries in
/// [`SET_CONFIG_ALLOWED`]).
///
/// # Errors
///
/// When `source` is not valid Rust.
pub fn check_source_allowing(
    path: &Path,
    dir: Option<&Path>,
    source: &str,
    allow_session_setting: bool,
    settings: &[&str],
) -> Result<Vec<Finding>, CheckError> {
    let file = syn::parse_file(source).map_err(|error| CheckError::Parse {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    let mut checker = Checker {
        path,
        dir,
        uses_sqlx: source.contains("sqlx"),
        test_file: is_test_path(path),
        test_modules: 0,
        allow_session_setting,
        settings: settings
            .iter()
            .map(|setting| (*setting).to_owned())
            .collect(),
        sqlx_imports: Vec::new(),
        builder_types: BUILDER_TYPES
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
        builders: Vec::new(),
        constants: Vec::new(),
        names: Names::default(),
        mentions_builder: false,
        builder_text: String::new(),
        builder_text_reported: false,
        findings: Vec::new(),
    };
    // Imports and aliases first, so uses before them in the file are recognized too.
    struct Uses<'c>(&'c mut Vec<String>, &'c mut Vec<String>);
    impl<'ast> Visit<'ast> for Uses<'_> {
        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            collect_sqlx_imports(&item.tree, false, self.0, self.1);
        }
    }
    Uses(&mut checker.sqlx_imports, &mut checker.builder_types).visit_file(&file);
    collect_builder_aliases(&file, &mut checker.builder_types);
    checker.constants = collect_text_constants(&file);
    checker.names = Names::collect(&file);
    checker.mentions_builder = checker
        .builder_types
        .iter()
        .any(|name| source.contains(name.as_str()));
    checker.visit_file(&file);
    let mut findings = checker.findings;
    findings.sort();
    findings.dedup();
    Ok(findings)
}

/// Checks every `.rs` file under `root` (the workspace root), skipping [`SKIPPED`], `target`,
/// `node_modules`, and directories whose name starts with `.`. Files in
/// [`EXCEPTIONS`] are not checked for the rules they list.
///
/// # Errors
///
/// When a directory or file cannot be read, or a file is not valid Rust.
pub fn check_tree(root: &Path) -> Result<Vec<Finding>, CheckError> {
    let mut files = Vec::new();
    collect_rust_files(root, root, &mut files)?;
    files.sort();
    let mut findings = Vec::new();
    for file in files {
        let relative = file.strip_prefix(root).unwrap_or(&file).to_owned();
        let source = fs::read_to_string(&file).map_err(|source| CheckError::Io {
            path: file.clone(),
            source,
        })?;
        let allowed = SESSION_SETTING_ALLOWED
            .iter()
            .any(|allowed| relative == Path::new(allowed));
        let waived: &[Rule] = EXCEPTIONS
            .iter()
            .find(|exception| relative == Path::new(exception.path))
            .map_or(&[], |exception| exception.rules);
        let settings: Vec<&str> = SET_CONFIG_ALLOWED
            .iter()
            .filter(|exception| relative == Path::new(exception.path))
            .map(|exception| exception.setting)
            .collect();
        let mut file_findings =
            check_source_allowing(&relative, file.parent(), &source, allowed, &settings)?;
        file_findings.retain(|finding| !waived.contains(&finding.rule));
        findings.extend(file_findings);
    }
    Ok(findings)
}

fn collect_rust_files(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), CheckError> {
    let io_error = |source| CheckError::Io {
        path: dir.to_owned(),
        source,
    };
    for entry in fs::read_dir(dir).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let file_type = entry.file_type().map_err(io_error)?;
        if file_type.is_dir() {
            let relative = path.strip_prefix(root).unwrap_or(&path);
            let skipped = name.starts_with('.')
                || name == "target"
                || name == "node_modules"
                || SKIPPED.iter().any(|skip| relative == Path::new(skip));
            if !skipped {
                collect_rust_files(root, &path, files)?;
            }
        } else if file_type.is_file() && name.ends_with(".rs") {
            files.push(path);
        }
    }
    Ok(())
}

/// The query builder types of sqlx, before renames and aliases.
const BUILDER_TYPES: &[&str] = &["QueryBuilder", "Separated"];

/// Records the names a `use sqlx::...` tree brings in that are query functions (into `names`)
/// or renamed query builder types (into `builder_types`); a glob import brings in every query
/// function.
fn collect_sqlx_imports(
    tree: &UseTree,
    under_sqlx: bool,
    names: &mut Vec<String>,
    builder_types: &mut Vec<String>,
) {
    match tree {
        UseTree::Path(path) => {
            let under = under_sqlx || path.ident == "sqlx";
            collect_sqlx_imports(&path.tree, under, names, builder_types);
        }
        UseTree::Name(name) if under_sqlx => {
            let ident = name.ident.to_string();
            if QUERY_FUNCTIONS.contains(&ident.as_str()) {
                names.push(ident);
            }
        }
        UseTree::Rename(rename) if under_sqlx => {
            let original = rename.ident.to_string();
            if QUERY_FUNCTIONS.contains(&original.as_str()) {
                names.push(rename.rename.to_string());
            } else if BUILDER_TYPES.contains(&original.as_str()) {
                builder_types.push(rename.rename.to_string());
            }
        }
        UseTree::Glob(_) if under_sqlx => {
            names.extend(QUERY_FUNCTIONS.iter().map(|name| (*name).to_owned()));
        }
        UseTree::Group(group) => {
            for tree in &group.items {
                collect_sqlx_imports(tree, under_sqlx, names, builder_types);
            }
        }
        _ => {}
    }
}

/// Adds every `type` alias in `file` whose definition mentions a known builder type, until no
/// new alias appears (aliases of aliases).
fn collect_builder_aliases(file: &syn::File, builder_types: &mut Vec<String>) {
    struct Aliases<'ast>(Vec<(String, &'ast syn::Type)>);
    impl<'ast> Visit<'ast> for Aliases<'ast> {
        fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
            self.0.push((item.ident.to_string(), &item.ty));
            visit::visit_item_type(self, item);
        }
    }
    let mut aliases = Aliases(Vec::new());
    aliases.visit_file(file);
    loop {
        let before = builder_types.len();
        for (name, ty) in &aliases.0 {
            if !builder_types.contains(name) && mentions_builder_type(ty, builder_types) {
                builder_types.push(name.clone());
            }
        }
        if builder_types.len() == before {
            break;
        }
    }
}

struct Checker<'a> {
    path: &'a Path,
    dir: Option<&'a Path>,
    uses_sqlx: bool,
    allow_session_setting: bool,
    /// Settings this file may also change with `set_config`.
    settings: Vec<String>,
    sqlx_imports: Vec<String>,
    /// Names of the query builder types: sqlx's, their renames, and aliases of them.
    builder_types: Vec<String>,
    /// Names known to hold a query builder, one set per function being visited.
    builders: Vec<Vec<String>>,
    /// The file is test code: in a `tests` directory or a [`TEST_SUPPORT`] crate.
    test_file: bool,
    /// How many `#[cfg(test)]` modules enclose the code being visited.
    test_modules: usize,
    /// String constants defined in the file, by name.
    constants: Vec<(String, String)>,
    /// What single `SCREAMING_CASE` names in the file refer to.
    names: Names,
    /// The file names a query builder type (or a rename or alias of one).
    mentions_builder: bool,
    /// The static text the file passes to query builders, in source order, so SQL split
    /// across `push` calls is checked whole.
    builder_text: String,
    builder_text_reported: bool,
    findings: Vec<Finding>,
}

impl Checker<'_> {
    /// Test code may pass SQL through a variable (a single lower-case name): fixtures are
    /// clearer as tables of literals, and sqlx still requires `&'static str`, whose other
    /// sources (`leak`, `AssertSqlSafe`) stay rejected.
    fn is_static_sql(&self, expr: &Expr) -> bool {
        is_static_sql(expr, &self.names)
            || ((self.test_file || self.test_modules > 0) && is_variable(expr))
    }

    fn report(&mut self, span: Span, rule: Rule) {
        let start = span.start();
        self.findings.push(Finding {
            path: self.path.to_owned(),
            line: start.line,
            column: start.column + 1,
            rule,
        });
    }

    fn is_builder_name(&self, name: &str) -> bool {
        self.builders
            .last()
            .is_some_and(|names| names.iter().any(|known| known == name))
    }

    fn add_builder(&mut self, name: String) {
        if let Some(names) = self.builders.last_mut() {
            names.push(name);
        }
    }

    /// Whether `expr` evaluates to a query builder (or its `Separated`) the check knows of.
    fn is_builder_expr(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Call(call) => is_builder_constructor(&call.func, &self.builder_types),
            Expr::MethodCall(call) => {
                BUILDER_CHAIN_METHODS.contains(&call.method.to_string().as_str())
                    && self.is_builder_expr(&call.receiver)
            }
            Expr::Path(path) => path
                .path
                .get_ident()
                .is_some_and(|ident| self.is_builder_name(&ident.to_string())),
            Expr::Paren(inner) => self.is_builder_expr(&inner.expr),
            Expr::Reference(inner) => self.is_builder_expr(&inner.expr),
            Expr::Unary(inner) => self.is_builder_expr(&inner.expr),
            _ => false,
        }
    }

    fn is_query_function(&self, path: &syn::Path) -> bool {
        let Some(last) = path.segments.last() else {
            return false;
        };
        let name = last.ident.to_string();
        match path.segments.len() {
            // Imported from sqlx, possibly renamed.
            1 => self.sqlx_imports.contains(&name),
            _ => {
                QUERY_FUNCTIONS.contains(&name.as_str())
                    && path
                        .segments
                        .first()
                        .is_some_and(|first| first.ident == "sqlx")
            }
        }
    }

    /// Whether the identifier at `index` names a sqlx query function: `sqlx::<name>`, or a name
    /// imported from sqlx.
    fn is_query_function_token(&self, tokens: &[TokenTree], index: usize) -> bool {
        let Some(TokenTree::Ident(ident)) = tokens.get(index) else {
            return false;
        };
        let name = ident.to_string();
        let qualified = index >= 3
            && matches!(&tokens[index - 3], TokenTree::Ident(root) if root == "sqlx")
            && QUERY_FUNCTIONS.contains(&name.as_str());
        qualified || self.sqlx_imports.contains(&name)
    }

    fn check_literal(&mut self, literal: &LitStr) {
        self.check_text(literal.span(), &literal.value());
    }

    fn sets_session(&self, sql: &str) -> bool {
        let settings: Vec<&str> = self.settings.iter().map(String::as_str).collect();
        sets_session_allowing(sql, &settings)
    }

    fn check_text(&mut self, span: Span, sql: &str) {
        if !self.allow_session_setting && self.sets_session(sql) {
            self.report(span, Rule::SessionSetting);
        }
    }

    /// The argument of `include_str!` or `include_bytes!`: the included file's text is checked,
    /// and a file the check cannot read is reported.
    fn check_include(&mut self, span: Span, arguments: &TokenStream) {
        let content = syn::parse2::<LitStr>(arguments.clone())
            .ok()
            .zip(self.dir)
            .and_then(|(file, dir)| fs::read(dir.join(file.value())).ok());
        match content {
            Some(content) => self.check_text(span, &String::from_utf8_lossy(&content)),
            None => self.report(span, Rule::UncheckedInclude),
        }
    }

    /// Static text passed to a query builder: it names no `trustiot` setting, and it does not
    /// complete a session-setting statement with the file's earlier builder text.
    fn check_builder_fragment(&mut self, sql: &Expr) {
        if self.allow_session_setting {
            return;
        }
        let Some(text) = static_text(sql, &self.constants) else {
            return;
        };
        if text.to_ascii_lowercase().contains("trustiot") {
            self.report(span_of(sql), Rule::SessionSetting);
        }
        self.builder_text.push_str(&text);
        if !self.builder_text_reported && self.sets_session(&self.builder_text) {
            self.builder_text_reported = true;
            self.report(span_of(sql), Rule::SessionSetting);
        }
    }

    /// A built-in macro whose expansion the check reads: `concat!`, checked as its joined text
    /// so SQL split across literals is seen whole, and the includes.
    fn check_builtin_macro(&mut self, name: &str, span: Span, arguments: &TokenStream) {
        match name {
            "concat" => self.check_text(span, &concat_text(arguments).0),
            "include_str" | "include_bytes" => self.check_include(span, arguments),
            _ => {}
        }
    }

    /// Checks a token stream that could not be parsed as Rust, such as a `macro_rules!` body:
    /// its string literals, `AssertSqlSafe`, and sqlx query functions called with anything but
    /// static SQL (a `$sql` metavariable included).
    fn check_tokens(&mut self, tokens: TokenStream) {
        let tokens: Vec<TokenTree> = tokens.into_iter().collect();
        for (index, token) in tokens.iter().enumerate() {
            if let TokenTree::Ident(ident) = token
                && self.is_query_function_token(&tokens, index)
                && let Some(arguments) = call_arguments(&tokens, index + 1)
                && !starts_with_static_sql(&arguments, &self.names)
            {
                self.report(ident.span(), Rule::DynamicSql);
            }
            if let TokenTree::Ident(ident) = token
                && self.is_builder_sql_token(&tokens, index)
                && let Some(arguments) = call_arguments(&tokens, index + 1)
                && !starts_with_static_sql(&arguments, &self.names)
            {
                self.report(ident.span(), Rule::DynamicQueryBuilder);
            }
        }
        for (index, token) in tokens.iter().enumerate() {
            if let TokenTree::Ident(ident) = token
                && let Some(TokenTree::Punct(bang)) = tokens.get(index + 1)
                && bang.as_char() == '!'
                && let Some(TokenTree::Group(arguments)) = tokens.get(index + 2)
            {
                self.check_builtin_macro(&ident.to_string(), ident.span(), &arguments.stream());
            }
        }
        for token in tokens {
            match token {
                TokenTree::Group(group) => self.check_tokens(group.stream()),
                TokenTree::Literal(literal) => {
                    if let Some(literal) = string_literal(literal) {
                        self.check_literal(&literal);
                    }
                }
                TokenTree::Ident(ident) => {
                    if ident == "AssertSqlSafe" {
                        self.report(ident.span(), Rule::AssertSqlSafe);
                    }
                }
                TokenTree::Punct(_) => {}
            }
        }
    }

    /// Whether the identifier at `index` of a token stream that is not Rust syntax (a
    /// `macro_rules!` body) takes query builder SQL: a builder constructor (`new` or
    /// `with_arguments` shortly after a builder type name), or a `push`, `push_unseparated`, or
    /// `separated` method call. The receiver of a method call is unknown there, so in a file that
    /// uses sqlx or names a query builder every such call counts.
    fn is_builder_sql_token(&self, tokens: &[TokenTree], index: usize) -> bool {
        let Some(TokenTree::Ident(ident)) = tokens.get(index) else {
            return false;
        };
        let name = ident.to_string();
        let names_builder = |token: &TokenTree| matches!(token, TokenTree::Ident(type_name) if self.builder_types.contains(&type_name.to_string()));
        if name == "new" || name == "with_arguments" {
            return index >= 1
                && matches!(&tokens[index - 1], TokenTree::Punct(colon) if colon.as_char() == ':')
                && tokens[index.saturating_sub(8)..index]
                    .iter()
                    .any(names_builder);
        }
        BUILDER_SQL_METHODS.contains(&name.as_str())
            && index >= 1
            && matches!(&tokens[index - 1], TokenTree::Punct(dot) if dot.as_char() == '.')
            && (self.uses_sqlx || self.mentions_builder)
    }

    fn with_function_scope(&mut self, visit: impl FnOnce(&mut Self)) {
        self.builders.push(Vec::new());
        visit(self);
        self.builders.pop();
    }
}

impl<'ast> Visit<'ast> for Checker<'_> {
    fn visit_attribute(&mut self, _attribute: &'ast syn::Attribute) {
        // Doc comments and other attributes are not SQL.
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let test_module = item.attrs.iter().any(is_cfg_test);
        self.test_modules += usize::from(test_module);
        visit::visit_item_mod(self, item);
        self.test_modules -= usize::from(test_module);
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.with_function_scope(|checker| visit::visit_item_fn(checker, item));
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.with_function_scope(|checker| visit::visit_impl_item_fn(checker, item));
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        self.with_function_scope(|checker| visit::visit_trait_item_fn(checker, item));
    }

    fn visit_pat_type(&mut self, pat: &'ast syn::PatType) {
        if mentions_builder_type(&pat.ty, &self.builder_types)
            && let Pat::Ident(ident) = &*pat.pat
        {
            self.add_builder(ident.ident.to_string());
        }
        visit::visit_pat_type(self, pat);
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        let is_builder = local
            .init
            .as_ref()
            .is_some_and(|init| self.is_builder_expr(&init.expr));
        if is_builder && let Some(name) = pattern_name(&local.pat) {
            self.add_builder(name);
        }
        visit::visit_local(self, local);
    }

    fn visit_ident(&mut self, ident: &'ast proc_macro2::Ident) {
        if ident == "AssertSqlSafe" {
            self.report(ident.span(), Rule::AssertSqlSafe);
        }
    }

    fn visit_lit_str(&mut self, literal: &'ast LitStr) {
        self.check_literal(literal);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(function) = &*call.func {
            let path = &function.path;
            let last = path
                .segments
                .last()
                .map(|segment| segment.ident.to_string());
            if self.is_query_function(path)
                || is_builder_constructor(&call.func, &self.builder_types)
            {
                let rule = if self.is_query_function(path) {
                    Rule::DynamicSql
                } else {
                    Rule::DynamicQueryBuilder
                };
                if let Some(sql) = call.args.first() {
                    if !self.is_static_sql(sql) {
                        self.report(span_of(sql), rule);
                    } else if rule == Rule::DynamicQueryBuilder {
                        self.check_builder_fragment(sql);
                    }
                }
            } else if last.as_deref() == Some("leak") && self.uses_sqlx {
                self.report(span_of(&call.func), Rule::LeakedString);
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        let method = call.method.to_string();
        if method == "leak" && call.args.is_empty() && self.uses_sqlx {
            self.report(call.method.span(), Rule::LeakedString);
        }
        if BUILDER_SQL_METHODS.contains(&method.as_str())
            && self.is_builder_expr(&call.receiver)
            && let Some(sql) = call.args.first()
        {
            if self.is_static_sql(sql) {
                self.check_builder_fragment(sql);
            } else {
                self.report(span_of(sql), Rule::DynamicQueryBuilder);
            }
        }
        if (method == "push_values" || method == "push_tuples")
            && self.is_builder_expr(&call.receiver)
            && let Some(Expr::Closure(closure)) = call.args.iter().nth(1)
            && let Some(first) = closure.inputs.first()
            && let Some(name) = pattern_name(first)
        {
            self.add_builder(name);
        }
        if (method == "push_str" || method == "insert_str")
            && call.args.iter().any(literal_looks_like_sql)
        {
            self.report(call.method.span(), Rule::SqlBuiltFromStrings);
        }
        visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_binary(&mut self, binary: &'ast syn::ExprBinary) {
        if matches!(binary.op, syn::BinOp::Add(_) | syn::BinOp::AddAssign(_))
            && (literal_looks_like_sql(&binary.left) || literal_looks_like_sql(&binary.right))
        {
            self.report(span_of(&binary.right), Rule::SqlBuiltFromStrings);
        }
        visit::visit_expr_binary(self, binary);
    }

    fn visit_macro(&mut self, mac: &'ast Macro) {
        let name = mac
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
            .unwrap_or_default();
        let name_span = mac
            .path
            .segments
            .last()
            .map_or_else(Span::call_site, |segment| segment.ident.span());
        self.check_builtin_macro(&name, name_span, &mac.tokens);
        let formats = matches!(
            name.as_str(),
            "format" | "format_args" | "write" | "writeln"
        );
        if formats
            && macro_literals(&mac.tokens)
                .iter()
                .any(|text| looks_like_sql(text))
        {
            self.report(name_span, Rule::SqlBuiltFromStrings);
        }
        // Macro bodies are not part of the syntax tree; check them as expressions or statements
        // when they parse as such, and their string literals otherwise.
        let expressions =
            Punctuated::<Expr, Token![,]>::parse_terminated.parse2(mac.tokens.clone());
        if let Ok(expressions) = expressions {
            // A macro given a query builder can push its other arguments into it as SQL, so
            // they must be static too.
            if expressions
                .iter()
                .any(|expression| self.is_builder_expr(expression))
            {
                for expression in &expressions {
                    if !self.is_builder_expr(expression) && !self.is_static_sql(expression) {
                        self.report(span_of(expression), Rule::DynamicQueryBuilder);
                    }
                }
            }
            for expression in &expressions {
                self.visit_expr(expression);
            }
            return;
        }
        let statements =
            (|input: ParseStream<'_>| syn::Block::parse_within(input)).parse2(mac.tokens.clone());
        match statements {
            Ok(statements) => {
                for statement in &statements {
                    self.visit_stmt(statement);
                }
            }
            Err(_) => self.check_tokens(mac.tokens.clone()),
        }
    }
}

/// The parenthesized arguments of a call whose name ends just before `start`, skipping a
/// turbofish (`::<...>`); `None` when no call follows.
fn call_arguments(tokens: &[TokenTree], start: usize) -> Option<TokenStream> {
    let mut index = start;
    let mut depth = 0_usize;
    while let Some(token) = tokens.get(index) {
        match token {
            TokenTree::Group(group) if depth == 0 => {
                return (group.delimiter() == proc_macro2::Delimiter::Parenthesis)
                    .then(|| group.stream());
            }
            TokenTree::Punct(punct) => match punct.as_char() {
                ':' => {}
                '<' => depth += 1,
                '>' => depth = depth.checked_sub(1)?,
                _ if depth > 0 => {}
                _ => return None,
            },
            _ if depth > 0 => {}
            _ => return None,
        }
        index += 1;
    }
    None
}

/// Whether call arguments start with static SQL: a string literal, `include_str!`, `concat!` of
/// literals, or a constant name (see [`Names`]).
fn starts_with_static_sql(arguments: &TokenStream, names: &Names) -> bool {
    let mut tokens = arguments.clone().into_iter();
    match tokens.next() {
        Some(TokenTree::Literal(literal)) => string_literal(literal).is_some(),
        Some(TokenTree::Ident(ident)) => {
            let name = ident.to_string();
            if name == "concat" {
                return matches!(
                    (tokens.next(), tokens.next()),
                    (Some(TokenTree::Punct(bang)), Some(TokenTree::Group(arguments)))
                        if bang.as_char() == '!' && concat_text(&arguments.stream()).1
                );
            }
            name == "include_str" || names.is_constant(&name)
        }
        Some(TokenTree::Punct(punct)) if punct.as_char() == '&' => {
            starts_with_static_sql(&tokens.collect(), names)
        }
        _ => false,
    }
}

/// The value of a string literal token; `None` for other literals.
fn string_literal(literal: Literal) -> Option<LitStr> {
    match syn::parse2::<Lit>(TokenStream::from(TokenTree::Literal(literal))) {
        Ok(Lit::Str(literal)) => Some(literal),
        _ => None,
    }
}

fn span_of(expr: &Expr) -> Span {
    use syn::spanned::Spanned;
    expr.span()
}

/// `QueryBuilder::new` or `QueryBuilder::with_arguments` (or through a rename or alias of
/// `QueryBuilder` in `builder_types`), with any type arguments.
fn is_builder_constructor(function: &Expr, builder_types: &[String]) -> bool {
    let Expr::Path(path) = function else {
        return false;
    };
    let segments: Vec<String> = path
        .path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect();
    match segments.as_slice() {
        [.., builder, constructor] => {
            builder != "Separated"
                && builder_types.contains(builder)
                && (constructor == "new" || constructor == "with_arguments")
        }
        _ => false,
    }
}

fn mentions_builder_type(ty: &syn::Type, builder_types: &[String]) -> bool {
    struct Finder<'a>(bool, &'a [String]);
    impl<'ast> Visit<'ast> for Finder<'_> {
        fn visit_path_segment(&mut self, segment: &'ast syn::PathSegment) {
            if self.1.iter().any(|name| segment.ident == name) {
                self.0 = true;
            }
            visit::visit_path_segment(self, segment);
        }
    }
    let mut finder = Finder(false, builder_types);
    finder.visit_type(ty);
    finder.0
}

fn pattern_name(pat: &Pat) -> Option<String> {
    match pat {
        Pat::Ident(ident) => Some(ident.ident.to_string()),
        Pat::Type(typed) => pattern_name(&typed.pat),
        _ => None,
    }
}

/// What single `SCREAMING_CASE` names in a file refer to: constants and statics it declares or
/// imports, and names it binds as variables or parameters (also in macro bodies, after `let`,
/// `mut`, or `ref`), which hold runtime values.
#[derive(Default)]
struct Names {
    declared: Vec<String>,
    bound: Vec<String>,
}

impl Names {
    fn collect(file: &syn::File) -> Self {
        let mut names = Self::default();
        names.visit_file(file);
        names
    }

    fn bind_in_tokens(&mut self, tokens: TokenStream) {
        let mut binding = false;
        for token in tokens {
            match token {
                TokenTree::Group(group) => {
                    self.bind_in_tokens(group.stream());
                    binding = false;
                }
                TokenTree::Ident(ident) => {
                    let name = ident.to_string();
                    if binding {
                        self.bound.push(name.clone());
                    }
                    binding = matches!(name.as_str(), "let" | "mut" | "ref");
                }
                TokenTree::Punct(_) | TokenTree::Literal(_) => binding = false,
            }
        }
    }

    /// Whether a single name is a constant: `SCREAMING_CASE`, declared or imported, and never
    /// bound as a variable.
    fn is_constant(&self, name: &str) -> bool {
        is_constant_name(name)
            && self.declared.iter().any(|declared| declared == name)
            && !self.bound.iter().any(|bound| bound == name)
    }
}

impl<'ast> Visit<'ast> for Names {
    fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
        self.declared.push(item.ident.to_string());
        visit::visit_item_const(self, item);
    }
    fn visit_item_static(&mut self, item: &'ast syn::ItemStatic) {
        self.declared.push(item.ident.to_string());
        visit::visit_item_static(self, item);
    }
    fn visit_impl_item_const(&mut self, item: &'ast syn::ImplItemConst) {
        self.declared.push(item.ident.to_string());
        visit::visit_impl_item_const(self, item);
    }
    fn visit_trait_item_const(&mut self, item: &'ast syn::TraitItemConst) {
        self.declared.push(item.ident.to_string());
        visit::visit_trait_item_const(self, item);
    }
    fn visit_use_name(&mut self, name: &'ast syn::UseName) {
        self.declared.push(name.ident.to_string());
    }
    fn visit_use_rename(&mut self, rename: &'ast syn::UseRename) {
        self.declared.push(rename.rename.to_string());
    }
    fn visit_pat_ident(&mut self, pat: &'ast syn::PatIdent) {
        self.bound.push(pat.ident.to_string());
        visit::visit_pat_ident(self, pat);
    }
    fn visit_macro(&mut self, mac: &'ast Macro) {
        self.bind_in_tokens(mac.tokens.clone());
    }
}

/// SQL fixed at compile time: a string literal, `include_str!`, `concat!` of literals, or a
/// constant, possibly borrowed or parenthesized. A constant is a path with a module whose last
/// segment is `SCREAMING_CASE` (a variable has no module), or a single name that `names` knows
/// as a constant.
fn is_static_sql(expr: &Expr, names: &Names) -> bool {
    match expr {
        Expr::Lit(literal) => matches!(literal.lit, Lit::Str(_)),
        Expr::Macro(mac) => mac.mac.path.segments.last().is_some_and(|segment| {
            segment.ident == "include_str"
                || (segment.ident == "concat" && concat_text(&mac.mac.tokens).1)
        }),
        Expr::Path(path) => {
            let segments = &path.path.segments;
            path.qself.is_none()
                && segments.last().is_some_and(|segment| {
                    let name = segment.ident.to_string();
                    if segments.len() == 1 {
                        names.is_constant(&name)
                    } else {
                        is_constant_name(&name)
                    }
                })
        }
        Expr::Reference(reference) => is_static_sql(&reference.expr, names),
        Expr::Paren(inner) => is_static_sql(&inner.expr, names),
        Expr::Group(inner) => is_static_sql(&inner.expr, names),
        _ => false,
    }
}

/// The text `concat!` produces from `arguments`, and whether the check knows all of it: literals
/// and nested `concat!` and `stringify!` are known; anything else (`env!`, a metavariable)
/// contributes nothing to the text and makes it incomplete.
fn concat_text(arguments: &TokenStream) -> (String, bool) {
    let mut text = String::new();
    let mut complete = true;
    let mut tokens = arguments.clone().into_iter().peekable();
    while let Some(token) = tokens.next() {
        match token {
            TokenTree::Literal(literal) => {
                match syn::parse2::<Lit>(TokenStream::from(TokenTree::Literal(literal))) {
                    Ok(Lit::Str(literal)) => text.push_str(&literal.value()),
                    Ok(Lit::Char(literal)) => text.push(literal.value()),
                    Ok(Lit::Int(literal)) => text.push_str(literal.base10_digits()),
                    Ok(Lit::Float(literal)) => text.push_str(literal.base10_digits()),
                    _ => complete = false,
                }
            }
            TokenTree::Punct(punct) if punct.as_char() == ',' => {}
            TokenTree::Ident(ident) if ident == "true" || ident == "false" => {
                text.push_str(&ident.to_string());
            }
            TokenTree::Ident(ident) if matches!(tokens.peek(), Some(TokenTree::Punct(bang)) if bang.as_char() == '!') =>
            {
                tokens.next();
                match tokens.next() {
                    Some(TokenTree::Group(group)) if ident == "concat" => {
                        let (inner, inner_complete) = concat_text(&group.stream());
                        text.push_str(&inner);
                        complete &= inner_complete;
                    }
                    Some(TokenTree::Group(group)) if ident == "stringify" => {
                        text.push_str(&group.stream().to_string());
                    }
                    _ => complete = false,
                }
            }
            _ => complete = false,
        }
    }
    (text, complete)
}

/// The text of static SQL: a string literal, `concat!` of literals, or a constant defined in the
/// file (`constants`); `None` for anything else, such as a constant from another file.
fn static_text(expr: &Expr, constants: &[(String, String)]) -> Option<String> {
    match expr {
        Expr::Lit(syn::ExprLit {
            lit: Lit::Str(literal),
            ..
        }) => Some(literal.value()),
        Expr::Macro(mac) if mac.mac.path.is_ident("concat") => Some(concat_text(&mac.mac.tokens).0),
        Expr::Path(path) => {
            let name = path.path.segments.last()?.ident.to_string();
            constants
                .iter()
                .find(|(constant, _)| *constant == name)
                .map(|(_, text)| text.clone())
        }
        Expr::Reference(inner) => static_text(&inner.expr, constants),
        Expr::Paren(inner) => static_text(&inner.expr, constants),
        Expr::Group(inner) => static_text(&inner.expr, constants),
        _ => None,
    }
}

/// `const` and `static` items of the file whose value is static text, by name.
fn collect_text_constants(file: &syn::File) -> Vec<(String, String)> {
    struct Constants(Vec<(String, String)>);
    impl<'ast> Visit<'ast> for Constants {
        fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
            if let Some(text) = static_text(&item.expr, &[]) {
                self.0.push((item.ident.to_string(), text));
            }
            visit::visit_item_const(self, item);
        }
        fn visit_item_static(&mut self, item: &'ast syn::ItemStatic) {
            if let Some(text) = static_text(&item.expr, &[]) {
                self.0.push((item.ident.to_string(), text));
            }
            visit::visit_item_static(self, item);
        }
    }
    let mut constants = Constants(Vec::new());
    constants.visit_file(file);
    constants.0
}

/// A single lower-case name, such as a loop variable.
fn is_variable(expr: &Expr) -> bool {
    match expr {
        Expr::Path(path) => path.qself.is_none() && path.path.get_ident().is_some(),
        Expr::Reference(reference) => is_variable(&reference.expr),
        Expr::Paren(inner) => is_variable(&inner.expr),
        _ => false,
    }
}

/// `#[cfg(test)]`.
fn is_cfg_test(attribute: &syn::Attribute) -> bool {
    match &attribute.meta {
        syn::Meta::List(list) => list.path.is_ident("cfg") && list.tokens.to_string() == "test",
        _ => false,
    }
}

fn is_constant_name(name: &str) -> bool {
    name.bytes().any(|byte| byte.is_ascii_uppercase())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn literal_looks_like_sql(expr: &Expr) -> bool {
    match expr {
        Expr::Lit(syn::ExprLit {
            lit: Lit::Str(literal),
            ..
        }) => looks_like_sql(&literal.value()),
        Expr::Reference(reference) => literal_looks_like_sql(&reference.expr),
        Expr::Paren(inner) => literal_looks_like_sql(&inner.expr),
        // `"SELECT ...".to_owned()`, `String::from("SELECT ...")`.
        Expr::MethodCall(call) => literal_looks_like_sql(&call.receiver),
        Expr::Call(call) => call.args.first().is_some_and(literal_looks_like_sql),
        _ => false,
    }
}

/// The string literals among a macro's tokens (not nested groups: a format string is always at
/// the top level).
fn macro_literals(tokens: &TokenStream) -> Vec<String> {
    tokens
        .clone()
        .into_iter()
        .filter_map(|token| match token {
            TokenTree::Literal(literal) => string_literal(literal).map(|literal| literal.value()),
            _ => None,
        })
        .collect()
}

/// Whether text starts like SQL or a SQL fragment: an upper-case SQL word followed by a space,
/// such as `SELECT ` or ` WHERE `.
#[must_use]
pub fn looks_like_sql(text: &str) -> bool {
    let trimmed = text.trim_start();
    let word_end = trimmed
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(trimmed.len());
    let (word, rest) = trimmed.split_at(word_end);
    SQL_START_WORDS.contains(&word) && rest.starts_with(char::is_whitespace)
}

/// Whether SQL text sets or resets a `trustiot.*` setting or the role, calls `set_config` (other
/// than with the literal name of a setting in [`SET_CONFIG_SETTINGS`]), or names `pg_settings`
/// (updating it sets a setting). Identifier quotes, white space around `.`, and comments do not
/// hide a match: the text is checked as it is and with its comments removed.
#[must_use]
pub fn sets_session(sql: &str) -> bool {
    sets_session_allowing(sql, &[])
}

/// [`sets_session`], also accepting `set_config` with the literal name of one of `settings`.
#[must_use]
pub fn sets_session_allowing(sql: &str, settings: &[&str]) -> bool {
    [sql.to_owned(), strip_comments(sql)]
        .iter()
        .any(|text| sets_session_words(text) || calls_unsafe_set_config(text, settings))
}

/// Whether SQL text calls `set_config` other than with the literal name of a setting in
/// [`SET_CONFIG_SETTINGS`] or `settings`; `trustiot.*`, the role, and the session authorization
/// never count as allowed.
fn calls_unsafe_set_config(sql: &str, settings: &[&str]) -> bool {
    let text: Vec<char> = sql
        .chars()
        .filter(|c| *c != '"')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let needle: Vec<char> = "SET_CONFIG".chars().collect();
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut start = 0;
    while let Some(offset) = text[start..]
        .windows(needle.len())
        .position(|window| window == needle.as_slice())
    {
        let at = start + offset;
        start = at + needle.len();
        if at > 0 && is_word(text[at - 1]) || text.get(start).is_some_and(|&c| is_word(c)) {
            continue;
        }
        let mut index = start;
        let skip_space = |index: &mut usize| {
            while text.get(*index).is_some_and(|c| c.is_whitespace()) {
                *index += 1;
            }
        };
        skip_space(&mut index);
        if text.get(index) != Some(&'(') {
            return true;
        }
        index += 1;
        skip_space(&mut index);
        if text.get(index) != Some(&'\'') {
            return true;
        }
        index += 1;
        let name_start = index;
        while text.get(index).is_some_and(|&c| c != '\'') {
            index += 1;
        }
        let name: String = text[name_start..index.min(text.len())].iter().collect();
        index += 1;
        if text.get(index) == Some(&'\'') {
            // A doubled quote: the name goes on, which a setting name never does.
            return true;
        }
        skip_space(&mut index);
        let safe_name = !name.starts_with("TRUSTIOT.")
            && !matches!(name.as_str(), "ROLE" | "SESSION_AUTHORIZATION")
            && SET_CONFIG_SETTINGS
                .iter()
                .chain(settings)
                .any(|setting| setting.eq_ignore_ascii_case(&name));
        if !safe_name || text.get(index) != Some(&',') {
            return true;
        }
    }
    false
}

fn sets_session_words(sql: &str) -> bool {
    let words = sql_words(sql);
    let word = |index: usize| words.get(index).map_or("", String::as_str);
    let is_setting = |word: &str| word.starts_with("TRUSTIOT.");
    for (index, current) in words.iter().enumerate() {
        // Also schema-qualified, such as `pg_catalog.set_config`.
        let name = current.rsplit('.').next().unwrap_or_default();
        if name == "PG_SETTINGS" {
            return true;
        }
        // The last segment too: a stray dot before a keyword joins it to the previous word.
        match name {
            "SET" => {
                let mut next = index + 1;
                if matches!(word(next), "LOCAL" | "SESSION") {
                    next += 1;
                }
                if word(next) == "ROLE" || word(next) == "AUTHORIZATION" || is_setting(word(next)) {
                    return true;
                }
            }
            "RESET" => {
                let next = word(index + 1);
                if matches!(next, "ALL" | "ROLE" | "SESSION") || is_setting(next) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Upper-case words of SQL text. Double quotes are dropped, and a qualified name is one word
/// even with white space around its dots (`"a" . "b"` is `A.B`).
fn sql_words(sql: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    // A dot ended the last word, so the next word continues it.
    let mut after_dot = false;
    for c in sql.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            if current.is_empty() && after_dot {
                current = words.pop().unwrap_or_default();
                current.push('.');
            }
            after_dot = false;
            current.push(c.to_ascii_uppercase());
        } else if c == '"' {
            // Quoted identifiers: the quotes do not split the name.
        } else if c == '.' && !(current.is_empty() && words.is_empty()) {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            after_dot = true;
        } else if c.is_whitespace() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        } else {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            after_dot = false;
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// SQL text with `-- ...` and (nested) `/* ... */` comments replaced by a space. String
/// literals (`'...'`, `E'...'`) and quoted identifiers are kept as they are, so comment markers
/// inside them do not start a comment. Dollar-quoted text is read like SQL, because it is
/// usually a PL/pgSQL body.
fn strip_comments(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len());
    let mut index = 0;
    while let Some(&c) = chars.get(index) {
        let next = chars.get(index + 1).copied();
        if c == '\'' || c == '"' {
            // Copy the quoted text; a doubled quote stays inside it, and so does a
            // backslash-escaped character in an `E'...'` string.
            let escapes = c == '\'' && index > 0 && matches!(chars[index - 1], 'E' | 'e');
            out.push(c);
            index += 1;
            while let Some(&quoted) = chars.get(index) {
                out.push(quoted);
                index += 1;
                if escapes && quoted == '\\' {
                    if let Some(&escaped) = chars.get(index) {
                        out.push(escaped);
                        index += 1;
                    }
                } else if quoted == c {
                    if chars.get(index) == Some(&c) {
                        out.push(c);
                        index += 1;
                    } else {
                        break;
                    }
                }
            }
        } else if c == '-' && next == Some('-') {
            while chars.get(index).is_some_and(|&skipped| skipped != '\n') {
                index += 1;
            }
            out.push(' ');
        } else if c == '/' && next == Some('*') {
            let mut depth = 0_usize;
            while let Some(&skipped) = chars.get(index) {
                let after = chars.get(index + 1).copied();
                if skipped == '/' && after == Some('*') {
                    depth += 1;
                    index += 2;
                } else if skipped == '*' && after == Some('/') {
                    depth -= 1;
                    index += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    index += 1;
                }
            }
            out.push(' ');
        } else {
            out.push(c);
            index += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixtures directory, for `include_str!` in test sources.
    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    fn rules(source: &str) -> Vec<Rule> {
        check_source(Path::new("test.rs"), Some(&fixtures()), source, false)
            .expect("valid Rust")
            .into_iter()
            .map(|finding| finding.rule)
            .collect()
    }

    #[test]
    fn static_sql_passes() {
        let source = r#"
            const LIST: &str = "SELECT name FROM devices WHERE id = $1";
            async fn run(pool: &sqlx::PgPool, id: i64) {
                sqlx::query("SELECT 1").execute(pool).await.unwrap();
                sqlx::query(LIST).bind(id).execute(pool).await.unwrap();
                sqlx::query_as::<_, (i64,)>(concat!("SELECT ", "1")).fetch_one(pool).await.unwrap();
                sqlx::raw_sql(include_str!("schema.sql")).execute(pool).await.unwrap();
                sqlx::query!("SELECT 1 AS one").fetch_one(pool).await.unwrap();
            }
        "#;
        assert_eq!(rules(source), []);
    }

    #[test]
    fn dynamic_sql_is_found() {
        let source = r#"
            use sqlx::{query, query_scalar as scalar};
            async fn run(pool: &sqlx::PgPool, table: &'static str, sql: String) {
                query(table).execute(pool).await.unwrap();
                scalar::<_, i64>(sql.as_str()).fetch_one(pool).await.unwrap();
                sqlx::query(format!("SELECT * FROM {table}").leak()).execute(pool).await.unwrap();
            }
        "#;
        let found = rules(source);
        assert_eq!(
            found
                .iter()
                .filter(|rule| **rule == Rule::DynamicSql)
                .count(),
            3,
            "{found:?}"
        );
        assert!(found.contains(&Rule::LeakedString), "{found:?}");
        assert!(found.contains(&Rule::SqlBuiltFromStrings), "{found:?}");
    }

    #[test]
    fn test_code_may_pass_sql_through_variables() {
        let source = r#"
            async fn run(pool: &sqlx::PgPool) {
                for sql in ["SELECT 1", "SELECT 2"] {
                    sqlx::query(sql).execute(pool).await.unwrap();
                }
            }
            #[cfg(test)]
            mod tests {
                async fn run(pool: &sqlx::PgPool, sql: &'static str) {
                    sqlx::query(sql).execute(pool).await.unwrap();
                    sqlx::query(sql.trim()).execute(pool).await.unwrap();
                }
            }
        "#;
        // Production code: the loop variable. Test module: the method call, not the variable.
        assert_eq!(rules(source), vec![Rule::DynamicSql; 2]);
        let in_tests = check_source(Path::new("crates/x/tests/db.rs"), None, source, false)
            .expect("valid Rust")
            .into_iter()
            .map(|finding| finding.rule)
            .collect::<Vec<_>>();
        assert_eq!(in_tests, [Rule::DynamicSql], "only sql.trim()");
    }

    #[test]
    fn test_support_crates_are_test_code() {
        let source = r#"
            async fn probe(pool: &sqlx::PgPool, sql: &'static str) {
                sqlx::query(sql).execute(pool).await.unwrap();
                let leaked: &'static str = String::from("SELECT 1").leak();
                sqlx::query(leaked).execute(pool).await.unwrap();
            }
        "#;
        let rules_at = |path: &str| {
            check_source(Path::new(path), None, source, false)
                .expect("valid Rust")
                .into_iter()
                .map(|finding| finding.rule)
                .collect::<Vec<_>>()
        };
        // The variable is accepted in a test support crate; leaking a String never is.
        let listed = [Rule::LeakedString];
        let elsewhere = [Rule::DynamicSql, Rule::LeakedString, Rule::DynamicSql];
        assert_eq!(rules_at("crates/testkit/src/isolation/postgres.rs"), listed);
        assert_eq!(
            rules_at("crates/testkits/src/lib.rs"),
            elsewhere,
            "only the listed crate, not a name that starts the same"
        );
        assert_eq!(rules_at("services/assets/src/db.rs"), elsewhere);
    }

    #[test]
    fn query_calls_in_macro_definitions_need_static_sql() {
        let source = r#"
            use sqlx::query_as;
            macro_rules! run {
                ($pool:expr, $sql:expr) => { sqlx::query($sql).execute($pool) };
            }
            macro_rules! typed {
                ($sql:expr) => { query_as::<_, (i64,)>($sql) };
            }
            macro_rules! fixed {
                ($pool:expr) => { sqlx::query("SELECT 1").execute($pool) };
            }
        "#;
        assert_eq!(rules(source), vec![Rule::DynamicSql; 2]);
    }

    #[test]
    fn unrelated_functions_named_query_are_ignored() {
        let source = r#"
            fn query(text: &str) -> usize { text.len() }
            fn run(text: &str) -> usize { query(text) + other::query(text) }
        "#;
        assert_eq!(rules(source), []);
    }

    #[test]
    fn query_builders_take_only_static_text() {
        let source = r#"
            use sqlx::{Postgres, QueryBuilder};
            fn build(names: &[String], column: &str, filter: &mut QueryBuilder<'_, Postgres>) {
                let mut builder = QueryBuilder::<Postgres>::new("SELECT * FROM devices WHERE ");
                builder.push("name = ").push_bind(names[0].clone());
                builder.push(column);
                let mut list = builder.separated(", ");
                list.push_bind(1);
                list.push(column);
                filter.push(column);
                builder.push_values(names, |mut row, name| {
                    row.push_bind(name);
                    row.push(name);
                });
                let mut words = Vec::new();
                words.push(column);
            }
        "#;
        assert_eq!(
            rules(source),
            vec![Rule::DynamicQueryBuilder; 4],
            "builder.push(column), list.push(column), filter.push(column), row.push(name)"
        );
        let dynamic_start = r#"
            fn build(sql: String) { let b = sqlx::QueryBuilder::<sqlx::Postgres>::new(sql); }
        "#;
        assert_eq!(rules(dynamic_start), [Rule::DynamicQueryBuilder]);
    }

    #[test]
    fn renamed_and_aliased_query_builders_are_followed() {
        let source = r#"
            use sqlx::{Postgres, QueryBuilder as Builder};
            type Filter<'a> = Builder<'a, Postgres>;
            type Nested<'a> = Filter<'a>;
            fn build(input: &str, filter: &mut Nested<'_>) {
                let mut builder = Builder::<Postgres>::new("SELECT 1 WHERE ");
                builder.push(input);
                filter.push(input);
                let other = Filter::new(input);
            }
        "#;
        assert_eq!(rules(source), vec![Rule::DynamicQueryBuilder; 3]);
    }

    #[test]
    fn assert_sql_safe_is_always_found() {
        let source = r#"
            use sqlx::AssertSqlSafe;
            async fn run(pool: &sqlx::PgPool, sql: String) {
                sqlx::query(AssertSqlSafe(sql)).execute(pool).await.unwrap();
            }
        "#;
        let found = rules(source);
        assert!(found.contains(&Rule::AssertSqlSafe), "{found:?}");
    }

    #[test]
    fn sql_built_with_strings_is_found() {
        let source = r#"
            fn build(id: &str, mut sql: String) -> String {
                let a = format!("SELECT * FROM devices WHERE id = '{id}'");
                sql.push_str(" WHERE id = ");
                sql += " AND name = ";
                let b = "DELETE FROM devices WHERE id = ".to_owned() + id;
                let message = format!("updated {id}");
                a + &b + &message
            }
        "#;
        assert_eq!(rules(source), vec![Rule::SqlBuiltFromStrings; 4]);
    }

    #[test]
    fn session_settings_are_found_outside_the_kit() {
        for sql in [
            "SET LOCAL trustiot.tenant_id = '00000000-0000-0000-0000-000000000000'",
            "set trustiot.tenant_id to default",
            "SELECT set_config('trustiot.tenant_id', $1, true)",
            "SET ROLE assets_owner",
            "SET SESSION AUTHORIZATION postgres",
            "RESET ALL",
            "reset trustiot.tenant_id",
            "ALTER ROLE assets_app SET trustiot.tenant_id = 'x'",
            "SELECT pg_catalog.set_config('trustiot.tenant_id', $1, true)",
            "SELECT \"set_config\"('trustiot.tenant_id', $1, true)",
            "SELECT \"pg_catalog\" . \"set_config\"('trustiot.tenant_id', $1, true)",
            "SET LOCAL \"trustiot\".tenant_id = '1'",
            "SET LOCAL trustiot . tenant_id = '1'",
            "SET /* tenant */ LOCAL -- b\n trustiot.tenant_id = '1'",
            "SELECT '--', 1; SET LOCAL /* a /* nested */ b */ trustiot.tenant_id = '1'",
            "SELECT E'it\\'s --', 'a''--'; SET /* x */ ROLE owner",
            "UPDATE pg_settings SET setting = '1' WHERE name = 'trustiot.tenant_id'",
        ] {
            assert!(sets_session(sql), "{sql}");
        }
        for sql in [
            "SELECT current_setting('trustiot.tenant_id', true)",
            "UPDATE devices SET name = $1 WHERE id = $2",
            "SET statement_timeout = '5s'",
            "SET statement_timeout = '1.5s'",
            "SELECT t.name FROM devices t WHERE t.settings = $1",
            "SELECT 1",
            "SELECT set_config('statement_timeout', $1, true)",
            "SELECT pg_catalog.set_config ( 'lock_timeout' , $1, true)",
        ] {
            assert!(!sets_session(sql), "{sql}");
        }
        let search_path = "SELECT set_config('search_path', $1, true)";
        assert!(sets_session(search_path), "only reviewed settings");
        assert!(!sets_session_allowing(search_path, &["search_path"]));
        assert!(sets_session_allowing(
            "SELECT set_config('trustiot.tenant_id', $1, true)",
            &["trustiot.tenant_id"]
        ));
        for sql in [
            "SELECT set_config($1, $2, true)",
            "SELECT set_config('trust' || 'iot.tenant_id', $1, true)",
            "SELECT set_config('role', 'owner', false)",
            "SELECT set_config('it''s', $1, true)",
            "SELECT set_config /* x */ ('search_path', $1, true)",
            "SELECT set_config",
        ] {
            assert!(sets_session(sql), "{sql}");
        }
        let source = r#"
            async fn run(pool: &sqlx::PgPool) {
                sqlx::query("SET LOCAL trustiot.tenant_id = '1'").execute(pool).await.unwrap();
                sqlx::query!("SELECT set_config('trustiot.tenant_id', $1, true)", "x");
            }
        "#;
        assert_eq!(rules(source), vec![Rule::SessionSetting; 2]);
        let allowed = check_source(Path::new("tenant.rs"), None, source, true).expect("valid Rust");
        assert_eq!(allowed, []);
    }

    #[test]
    fn only_declared_constants_count_as_static() {
        let source = r#"
            use sqlx::{Postgres, QueryBuilder};
            use crate::queries::LIST;
            const BY_ID: &str = "SELECT 1 WHERE id = $1";
            async fn run(pool: &sqlx::PgPool, input: &str, OTHER: &'static str) {
                sqlx::query(BY_ID).execute(pool);
                sqlx::query(LIST).execute(pool);
                sqlx::query(crate::queries::COUNT).execute(pool);
                sqlx::query(UNKNOWN).execute(pool);
                sqlx::query(OTHER).execute(pool);
                let SQL = input;
                let mut builder = QueryBuilder::<Postgres>::new("SELECT 1 WHERE ");
                builder.push(SQL);
                let BY_ID = input;
            }
            macro_rules! shadow {
                ($input:expr) => {{ let mut LOCAL = $input; sqlx::query(LOCAL) }};
            }
            const LOCAL: &str = "SELECT 1";
        "#;
        // UNKNOWN (not declared), OTHER (a parameter), BY_ID (rebound by let), SQL (a variable),
        // and LOCAL in the macro (bound by let there).
        let mut found = rules(source);
        found.sort();
        let mut expected = vec![Rule::DynamicSql; 4];
        expected.push(Rule::DynamicQueryBuilder);
        assert_eq!(found, expected);
    }

    #[test]
    fn query_builders_in_macros_take_only_static_text() {
        let source = r#"
            use sqlx::{Postgres, QueryBuilder};
            macro_rules! add_filter {
                ($builder:expr, $column:expr) => { $builder.push(" AND ").push($column) };
            }
            macro_rules! start {
                ($sql:expr) => { QueryBuilder::<Postgres>::new($sql) };
            }
            macro_rules! fixed {
                ($builder:expr) => { $builder.push(" AND name = ").push_bind(1) };
            }
            fn filter(builder: &mut QueryBuilder<'_, Postgres>, column: &str) {
                add_filter!(builder, column);
                add_filter!(builder, " AND id = 1");
                fixed!(builder);
                let names = vec![column];
            }
        "#;
        // The definitions of add_filter and start, and the first add_filter! call.
        assert_eq!(rules(source), vec![Rule::DynamicQueryBuilder; 3]);
        let vectors = r#"
            macro_rules! collect {
                ($list:expr, $item:expr) => { $list.push($item) };
            }
        "#;
        assert_eq!(rules(vectors), [], "a file without sqlx or a query builder");
    }

    #[test]
    fn query_builder_text_is_checked_joined() {
        let source = r#"
            use sqlx::{Postgres, QueryBuilder};
            const PREFIX: &str = "SET LOCAL ";
            fn split(builder: &mut QueryBuilder<'_, Postgres>) {
                builder.push("SELECT 1; SET LOCAL trustiot.");
                builder.push("tenant_id = ").push_bind("x");
            }
            fn constant() {
                let mut builder = QueryBuilder::<Postgres>::new(PREFIX);
                builder.push(concat!("tr", "ustiot.tenant_id = 'b'"));
            }
            fn config() {
                let mut builder = QueryBuilder::<Postgres>::new("SELECT set_");
                builder.push("config($1, $2, true)");
            }
        "#;
        // The fragment naming trustiot in each of the first two functions, and the fragment
        // that completes `SET LOCAL trustiot.` in the file's joined builder text.
        assert_eq!(rules(source), vec![Rule::SessionSetting; 3]);
        let config_only = r#"
            use sqlx::{Postgres, QueryBuilder};
            fn config() {
                let mut builder = QueryBuilder::<Postgres>::new("SELECT set_");
                builder.push("config($1, $2, true)");
            }
        "#;
        assert_eq!(rules(config_only), [Rule::SessionSetting]);
        let update = r#"
            use sqlx::{Postgres, QueryBuilder};
            fn update(name: &str) {
                let mut builder = QueryBuilder::<Postgres>::new("UPDATE devices SET ");
                builder.push("name = ").push_bind(name.to_owned());
                builder.push(" WHERE tenant_id = ").push_bind(name.to_owned());
            }
        "#;
        assert_eq!(rules(update), []);
    }

    #[test]
    fn concat_is_checked_as_its_joined_text() {
        let source = r#"
            const SPLIT: &str = concat!("SELECT set_", "config('trustiot.tenant_id', $1, true)");
            const NESTED: &str = concat!("SELECT ", stringify!(set_config), "($1, $2, true)");
            async fn run(pool: &sqlx::PgPool) {
                sqlx::query(concat!("SET LOCAL trustiot.", "tenant_id = '1'")).execute(pool);
                sqlx::query(concat!("SELECT 1 WHERE ", env!("FILTER"))).execute(pool);
                sqlx::query(concat!("SELECT ", concat!("1, ", 2), ", ", 'x')).execute(pool);
            }
            macro_rules! tenant {
                () => { sqlx::query(concat!("SELECT set_", "config($1, $2, true)")) };
                ($filter:expr) => { sqlx::query(concat!("SELECT 1 WHERE ", $filter)) };
            }
        "#;
        // Three splits in code and one in the macro; env! and the metavariable are not static.
        let mut expected = vec![Rule::DynamicSql; 2];
        expected.extend([Rule::SessionSetting; 4]);
        let mut found = rules(source);
        found.sort();
        assert_eq!(found, expected);
        assert_eq!(
            concat_text(
                &"\"a\", concat!(\"b\", 1), 'c', stringify!(d)"
                    .parse()
                    .expect("tokens")
            ),
            ("ab1cd".to_owned(), true)
        );
    }

    #[test]
    fn included_files_are_checked() {
        let source = r#"
            async fn run(pool: &sqlx::PgPool) {
                sqlx::raw_sql(include_str!("schema.sql")).execute(pool);
                sqlx::raw_sql(include_str!("sets-tenant.sql")).execute(pool);
                let bytes = include_bytes!("sets-tenant.sql");
                sqlx::raw_sql(include_str!("missing.sql")).execute(pool);
                sqlx::raw_sql(include_str!(concat!(env!("OUT_DIR"), "/q.sql"))).execute(pool);
            }
            macro_rules! schema {
                () => { sqlx::raw_sql(include_str!("sets-tenant.sql")) };
            }
        "#;
        let mut found = rules(source);
        found.sort();
        let mut expected = vec![Rule::SessionSetting; 3];
        expected.extend([Rule::UncheckedInclude; 2]);
        assert_eq!(found, expected);
        // Without the directory on disk, no include can be read.
        let unread = check_source(Path::new("test.rs"), None, source, false).expect("valid Rust");
        assert!(
            unread
                .iter()
                .all(|finding| finding.rule == Rule::UncheckedInclude),
            "{unread:?}"
        );
    }

    #[test]
    fn doc_comments_are_not_checked() {
        let source = r#"
            /// Runs `SET LOCAL trustiot.tenant_id` and `format!("SELECT {x}")`.
            fn documented() {}
        "#;
        assert_eq!(rules(source), []);
    }

    #[test]
    fn findings_name_the_file_line_and_rule() {
        let findings = check_source(
            Path::new("crates/x/src/lib.rs"),
            None,
            "fn f(s: &'static str) {\n    sqlx::query(s);\n}\n",
            false,
        )
        .expect("valid Rust");
        assert_eq!(findings.len(), 1);
        let text = findings[0].to_string();
        assert!(
            text.starts_with("crates/x/src/lib.rs:2:17: DynamicSql: "),
            "{text}"
        );
    }

    #[test]
    fn sql_like_text_needs_a_sql_word_and_a_space() {
        assert!(looks_like_sql("SELECT * FROM t"));
        assert!(looks_like_sql("  WHERE id = "));
        assert!(!looks_like_sql("Selected {n} rows"));
        assert!(!looks_like_sql("update failed"));
        assert!(!looks_like_sql("SELECTED"));
    }

    #[test]
    fn invalid_rust_is_an_error() {
        assert!(check_source(Path::new("broken.rs"), None, "fn (", false).is_err());
    }
}
