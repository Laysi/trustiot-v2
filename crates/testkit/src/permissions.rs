//! The permission matrix (A2, Laysi/trustiot-v2#584): what every operation of a service's client
//! API requires of the caller, declared once in the service's tests and held to two sources.
//!
//! - The service's OpenAPI document: every operation in it must be declared, and every
//!   declaration must name one of them, so a new route fails the test until someone decides who
//!   may call it.
//! - The matrix document, [`MATRIX`] (`docs/architecture/permissions.md`): the service's section
//!   lists the same operations with the same requirement, so the document cannot drift from the
//!   code, and the code cannot drift from the document.
//!
//! The service's table in the matrix document is generated from the declarations: with
//! [`REWRITE_ENV`] set to `1`, [`Suite::check_or_rewrite_declarations`] writes it
//! ([`Suite::rewrite_matrix`]) instead of only comparing it, so a merge conflict in the table is
//! resolved by regenerating it, never by hand (Laysi/trustiot-v2#1095). It rewrites only when
//! every declaration is well formed and matches the OpenAPI document and the rewritten section
//! passes the comparison, and refuses to run with `CI` or `GITHUB_ACTIONS` set, where the
//! committed document is what is checked.
//!
//! [`Suite::run`] then calls each operation over HTTP as every kind of caller its requirement
//! has to tell apart ([`Access`] lists them) and expects exactly 401 for an invalid or missing
//! token, exactly 403 for a caller the requirement refuses, and neither for the caller it
//! admits. The requests carry no body: a refused caller must be refused from the token alone,
//! before the service reads anything, and the admitted caller's request then fails validation
//! (400, 422) or touches nothing (an empty page, 404 for a made-up ID), so the probes change no
//! data. Path parameters are filled with new random UUIDs.
//!
//! ```no_run
//! # async fn example(openapi: serde_json::Value, base_url: &str) {
//! use contract::{ServiceName, TenantRole};
//! use testkit::identity::TestIssuer;
//! use testkit::permissions::{Access, MATRIX, Operation, Suite};
//!
//! let issuer = TestIssuer::new(); // the service under test trusts its keys (write_jwks)
//! Suite::new(ServiceName::Assets, &issuer)
//!     .openapi(openapi)
//!     .matrix(MATRIX)
//!     .operation(Operation::new("GET /api/assets/v1/devices", Access::Tenant(TenantRole::Viewer)))
//!     .operation(Operation::new("POST /api/assets/v1/devices", Access::Tenant(TenantRole::Editor)))
//!     .run(base_url)
//!     .await
//!     .assert_passed("the permission matrix");
//! # }
//! ```

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use contract::{Actor, InternalClaims, ServiceName, TenantGrant, TenantId, TenantRole};
use http::Method;
use serde_json::Value;
use uuid::Uuid;

use crate::identity::TestIssuer;
use crate::isolation::Report;
use crate::isolation::api::operations;

/// The matrix document, `docs/architecture/permissions.md`, as of this build. Pass it to
/// [`Suite::matrix`].
pub const MATRIX: &str = include_str!("../../../docs/architecture/permissions.md");

/// Where [`MATRIX`] lives, for failure messages.
pub const MATRIX_PATH: &str = "docs/architecture/permissions.md";

/// The matrix document's file in this checkout, which [`Suite::rewrite_matrix`] writes. Pass it
/// to [`Suite::check_or_rewrite_declarations`].
pub const MATRIX_FILE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/architecture/permissions.md"
);

/// The environment variable that turns [`Suite::check_or_rewrite_declarations`] from comparing
/// the service's table in the matrix document into rewriting it: `1` rewrites; unset compares;
/// any other value fails.
pub const REWRITE_ENV: &str = "TRUSTIOT_REWRITE_PERMISSIONS";

/// The variables that mark a CI run, where [`REWRITE_ENV`] is refused whatever their value.
const CI_VARIABLES: [&str; 2] = ["CI", "GITHUB_ACTIONS"];

/// The header row every service's table in the matrix document starts with.
pub const MATRIX_HEADER: [&str; 3] = ["操作", "要求", "已知落差"];

/// What the [`Operation::accepts_exchanged_tokens`] gap is called in the matrix document's
/// 已知落差 column.
pub const EXCHANGED_GAP_LABEL: &str = "接受換發的 token";

/// How long one probe's request may take before it counts as a failure. A bound against a
/// stuck service on a loaded machine, not a performance requirement.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Who may call an operation. The probes of each requirement are in the matrix document,
/// section 「探測」.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Anyone, without a token (logging in).
    Public,
    /// Any signed-in user, with or without a tenant, also through a token another service
    /// exchanged on the user's behalf (an `act` claim).
    SignedIn,
    /// Any signed-in user's own request: a token with `act` gets 403. The user's own account
    /// (the session, preferences).
    OwnRequest,
    /// A member of the token's tenant with at least this role (admin includes editor, which
    /// includes viewer), also through an exchanged token. A token without a tenant, with a lower
    /// role or a role the build does not know gets 403, and so does a platform administrator
    /// without a tenant role (decision 4 of Laysi/trustiot-v2#104: no implicit tenant rights).
    Tenant(TenantRole),
    /// A platform administrator's own request: a token with `act` gets 403 (Laysi/trustiot-v2#104,
    /// 2026-10-23), and so does every tenant role.
    PlatformAdmin,
}

impl Access {
    /// The requirement as the matrix document's 要求 column writes it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Public => "公開",
            Self::SignedIn => "已登入",
            Self::OwnRequest => "已登入，本人",
            Self::Tenant(TenantRole::Viewer) => "租戶 viewer",
            Self::Tenant(TenantRole::Editor) => "租戶 editor",
            Self::Tenant(TenantRole::Admin) => "租戶 admin",
            Self::Tenant(TenantRole::Unrecognized) => "租戶 unrecognized",
            Self::PlatformAdmin => "平台管理員，本人",
        }
    }

    /// Whether the requirement refuses tokens another service exchanged.
    fn own_only(self) -> bool {
        matches!(self, Self::OwnRequest | Self::PlatformAdmin)
    }
}

/// One operation of the service's OpenAPI document and what it requires.
#[derive(Clone, Debug)]
pub struct Operation {
    /// `METHOD /path`, as declared.
    name: String,
    /// `None` when the declaration is not `METHOD /path`.
    parsed: Option<(Method, String)>,
    access: Access,
    headers: Vec<(&'static str, String)>,
    exchanged_gap: Option<&'static str>,
}

impl Operation {
    /// `operation` is written `METHOD /path` as in the OpenAPI document, for example
    /// `GET /api/identity/v1/event-log`.
    #[must_use]
    pub fn new(operation: &str, access: Access) -> Self {
        let parsed = operation.split_once(' ').and_then(|(method, path)| {
            let method = Method::from_bytes(method.as_bytes()).ok()?;
            (path.starts_with('/') && !path.contains(' ')).then(|| (method, path.to_owned()))
        });
        Self {
            name: operation.to_owned(),
            parsed,
            access,
            headers: Vec::new(),
            exchanged_gap: None,
        }
    }

    /// A header every probe of this operation sends, for an operation that needs more than the
    /// token to get past authorization (logging out needs a session cookie). Never a credential
    /// that would let a refused caller in.
    #[must_use]
    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    /// A known gap between the code and the decision that only a user's own request may call
    /// this [`Access::OwnRequest`] or [`Access::PlatformAdmin`] operation: today it accepts a
    /// token another service exchanged. The exchanged probe then expects the request to get
    /// through, and fails once the service refuses it, so whoever fixes the gap also removes
    /// this and the matrix document's 已知落差 entry. `reason` names the issue that tracks it.
    #[must_use]
    pub fn accepts_exchanged_tokens(mut self, reason: &'static str) -> Self {
        self.exchanged_gap = Some(reason);
        self
    }

    /// `METHOD /path`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What the operation requires.
    #[must_use]
    pub fn access(&self) -> Access {
        self.access
    }

    /// The 已知落差 column of the operation's row.
    fn gap_label(&self) -> &'static str {
        if self.exchanged_gap.is_some() {
            EXCHANGED_GAP_LABEL
        } else {
            ""
        }
    }
}

/// The permission matrix of one service: its operations, its OpenAPI document and the matrix
/// document.
#[derive(Debug)]
pub struct Suite<'a> {
    service: ServiceName,
    issuer: &'a TestIssuer,
    openapi: Option<Value>,
    matrix: Option<&'a str>,
    operations: Vec<Operation>,
}

impl<'a> Suite<'a> {
    /// The matrix of `service`, whose tokens `issuer` signs.
    #[must_use]
    pub fn new(service: ServiceName, issuer: &'a TestIssuer) -> Self {
        Self {
            service,
            issuer,
            openapi: None,
            matrix: None,
            operations: Vec::new(),
        }
    }

    /// The service's OpenAPI document (`serde_json::to_value(service::openapi())`).
    #[must_use]
    pub fn openapi(mut self, document: Value) -> Self {
        self.openapi = Some(document);
        self
    }

    /// The matrix document to hold the declarations to: [`MATRIX`] for a real service; the
    /// section `## <service>` (the service's wire name, for example `## identity`) is compared.
    #[must_use]
    pub fn matrix(mut self, markdown: &'a str) -> Self {
        self.matrix = Some(markdown);
        self
    }

    /// Declares an operation.
    #[must_use]
    pub fn operation(mut self, operation: Operation) -> Self {
        self.operations.push(operation);
        self
    }

    /// The declarations against the OpenAPI document and the matrix document, without calling
    /// the service.
    #[must_use]
    pub fn check_declarations(&self) -> Report {
        let mut report = Report::default();
        self.check_operations(&mut report);
        self.check_coverage(&mut report);
        self.check_matrix(&mut report);
        report
    }

    /// [`Suite::check_declarations`], or [`Suite::rewrite_matrix`] of `file` (normally
    /// [`MATRIX_FILE`]) when [`REWRITE_ENV`] is `1`. The rewrite is refused (a failure) with
    /// `CI` or `GITHUB_ACTIONS` set, to any value: CI checks the committed document, so a
    /// rewrite there would hide a stale one.
    #[must_use]
    pub fn check_or_rewrite_declarations(&self, file: impl AsRef<Path>) -> Report {
        let ci = CI_VARIABLES
            .iter()
            .any(|variable| std::env::var_os(variable).is_some());
        match rewrite_requested(std::env::var_os(REWRITE_ENV), ci) {
            Ok(false) => self.check_declarations(),
            Ok(true) => self.rewrite_matrix(file.as_ref()),
            Err(problem) => {
                let mut report = Report::default();
                report.fail("permissions", "rewrite", problem);
                report
            }
        }
    }

    /// Writes the service's table in the matrix document at `file` from the declarations, then
    /// compares the declarations with what it wrote, like [`Suite::check_declarations`]. Nothing
    /// is written while a declaration is malformed or does not match the OpenAPI document: those
    /// failures are reported as they are, so the rewrite cannot make up for a missing
    /// declaration. Only the table is written, from its header to its last row: a merge conflict
    /// inside it is replaced as a whole, and one that reaches past the table, or conflict
    /// markers left elsewhere in the section, fail for a person to resolve.
    #[must_use]
    pub fn rewrite_matrix(&self, file: &Path) -> Report {
        let mut report = Report::default();
        self.check_operations(&mut report);
        self.check_coverage(&mut report);
        if self.matrix.is_none() {
            report.fail(
                "permissions",
                "matrix",
                format!("call matrix(MATRIX) to hold the declarations to {MATRIX_PATH}"),
            );
        }
        let shown = file.display();
        if report.failure_count() > 0 {
            report.fail(
                "permissions",
                "rewrite",
                format!("{shown} not rewritten: fix the declarations first"),
            );
            return report;
        }
        let service = self.service.as_str();
        let rewritten = std::fs::read_to_string(file)
            .map_err(|error| format!("cannot read {shown}: {error}"))
            .and_then(|current| {
                let table = self.matrix_table()?;
                Ok((rewrite_table(&current, service, &table)?, current))
            });
        let (rewritten, current) = match rewritten {
            Ok(texts) => texts,
            Err(problem) => {
                report.fail("permissions", "rewrite", problem);
                return report;
            }
        };
        // What would be written must pass before anything is.
        self.check_matrix_of(&rewritten, &mut report);
        if report.failure_count() > 0 {
            report.fail(
                "permissions",
                "rewrite",
                format!("{shown} not rewritten: the rewritten section would not match"),
            );
            return report;
        }
        if rewritten == current {
            report.pass(
                "permissions",
                "rewrite",
                format!("{shown} section `## {service}`: unchanged"),
            );
        } else if let Err(problem) = replace_file(file, &rewritten) {
            report.fail("permissions", "rewrite", problem);
        } else {
            report.pass(
                "permissions",
                "rewrite",
                format!("{shown} section `## {service}`: rewritten"),
            );
        }
        report
    }

    /// The service's table as the matrix document writes it: the header, then one row per
    /// declaration in the order declared.
    ///
    /// # Errors
    ///
    /// A declaration whose name would break the table (a `|`, a backtick or a line break).
    pub fn matrix_table(&self) -> Result<String, String> {
        let mut table = table_line(&MATRIX_HEADER);
        table.push_str(&table_line(&["---"; MATRIX_HEADER.len()]));
        for operation in &self.operations {
            if operation.name.contains(['|', '`', '\n', '\r']) {
                return Err(format!(
                    "{}: a `|`, backtick or line break cannot be written in the table",
                    operation.name
                ));
            }
            let name = format!("`{}`", operation.name);
            table.push_str(&table_line(&[
                name.as_str(),
                operation.access.label(),
                operation.gap_label(),
            ]));
        }
        Ok(table)
    }

    /// [`Suite::check_declarations`], then every probe of every operation against the service
    /// listening at `base_url` (for example `http://127.0.0.1:8080`). Start the service with the
    /// issuer's [`TestIssuer::jwks`].
    pub async fn run(&self, base_url: &str) -> Report {
        let mut report = self.check_declarations();
        let client = match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
            Ok(client) => client,
            Err(error) => {
                report.fail("permissions", "client", error.to_string());
                return report;
            }
        };
        let forger = TestIssuer::new();
        for operation in &self.operations {
            let Some((method, path)) = &operation.parsed else {
                continue;
            };
            for probe in probes(operation) {
                let token = self.token(probe.caller, &forger);
                let target = format!("{}{}", base_url.trim_end_matches('/'), fill(path));
                let mut request = client.request(method.clone(), target);
                if let Some(token) = token {
                    request = request.header(contract::INTERNAL_IDENTITY_HEADER, token);
                }
                for (name, value) in &operation.headers {
                    request = request.header(*name, value);
                }
                match request.send().await {
                    Ok(response) => {
                        let status = response.status().as_u16();
                        let body = response.bytes().await.unwrap_or_default();
                        let code = serde_json::from_slice::<Value>(&body)
                            .ok()
                            .and_then(|body| {
                                body.get("code").and_then(Value::as_str).map(str::to_owned)
                            });
                        judge(&mut report, operation, &probe, status, code.as_deref());
                    }
                    Err(error) => report.fail(
                        operation.name(),
                        probe.label,
                        format!("the request failed: {error}"),
                    ),
                }
            }
        }
        report
    }

    /// Records a failure for every declaration that is malformed, repeated, or whose options do
    /// not fit its requirement.
    fn check_operations(&self, report: &mut Report) {
        for (index, operation) in self.operations.iter().enumerate() {
            if operation.parsed.is_none() {
                report.fail(
                    operation.name(),
                    "declaration",
                    "not `METHOD /path` as the OpenAPI document names operations",
                );
            }
            if self.operations[..index]
                .iter()
                .any(|earlier| earlier.name == operation.name)
            {
                report.fail(operation.name(), "declaration", "declared more than once");
            }
            if operation.access == Access::Tenant(TenantRole::Unrecognized) {
                report.fail(
                    operation.name(),
                    "declaration",
                    "Tenant(Unrecognized) grants nothing; declare viewer, editor or admin",
                );
            }
            if operation.exchanged_gap.is_some() && !operation.access.own_only() {
                report.fail(
                    operation.name(),
                    "declaration",
                    format!(
                        "accepts_exchanged_tokens is a gap of own-request operations only, and \
                         `{}` accepts exchanged tokens by definition",
                        operation.access.label()
                    ),
                );
            }
        }
    }

    /// Every operation of the OpenAPI document must be declared, and every declaration must
    /// name one.
    fn check_coverage(&self, report: &mut Report) {
        let Some(document) = &self.openapi else {
            report.fail(
                "permissions",
                "coverage",
                "call openapi(document) with the service's OpenAPI document",
            );
            return;
        };
        let documented = match operations(document) {
            Ok(documented) => documented,
            Err(problem) => {
                report.fail("permissions", "coverage", problem);
                return;
            }
        };
        let declared: BTreeSet<&str> = self.operations.iter().map(Operation::name).collect();
        for operation in &documented {
            if declared.contains(operation.as_str()) {
                report.pass(operation, "coverage", "declared");
            } else {
                report.fail(
                    operation,
                    "coverage",
                    "in the OpenAPI document but not declared: decide who may call it, declare \
                     it, and add it to the matrix document",
                );
            }
        }
        for operation in &declared {
            if !documented.iter().any(|documented| documented == operation) {
                report.fail(
                    *operation,
                    "coverage",
                    "declared but not in the OpenAPI document (removed or misspelled)",
                );
            }
        }
    }

    /// The service's section of the matrix document must list exactly the declarations.
    fn check_matrix(&self, report: &mut Report) {
        let Some(markdown) = self.matrix else {
            report.fail(
                "permissions",
                "matrix",
                format!("call matrix(MATRIX) to hold the declarations to {MATRIX_PATH}"),
            );
            return;
        };
        self.check_matrix_of(markdown, report);
    }

    /// The service's section of `markdown` must list exactly the declarations.
    fn check_matrix_of(&self, markdown: &str, report: &mut Report) {
        let service = self.service.as_str();
        let rows = match matrix_rows(markdown, service) {
            Ok(rows) => rows,
            Err(problem) => {
                report.fail("permissions", "matrix", problem);
                return;
            }
        };
        for (index, row) in rows.iter().enumerate() {
            if rows[..index]
                .iter()
                .any(|earlier| earlier.operation == row.operation)
            {
                report.fail(
                    &row.operation,
                    "matrix",
                    format!("{MATRIX_PATH} line {}: listed more than once", row.line),
                );
            }
        }
        for operation in &self.operations {
            let Some(row) = rows.iter().find(|row| row.operation == operation.name) else {
                report.fail(
                    operation.name(),
                    "matrix",
                    format!("declared but missing from {MATRIX_PATH}, section `## {service}`"),
                );
                continue;
            };
            let expected = (operation.access.label(), operation.gap_label());
            if (row.access.as_str(), row.gap.as_str()) == expected {
                report.pass(
                    operation.name(),
                    "matrix",
                    format!("line {}: {}", row.line, row.access),
                );
            } else {
                report.fail(
                    operation.name(),
                    "matrix",
                    format!(
                        "{MATRIX_PATH} line {} says `{}` / `{}`, the declaration `{}` / `{}`",
                        row.line, row.access, row.gap, expected.0, expected.1
                    ),
                );
            }
        }
        for row in &rows {
            if !self
                .operations
                .iter()
                .any(|operation| operation.name == row.operation)
            {
                report.fail(
                    &row.operation,
                    "matrix",
                    format!(
                        "{MATRIX_PATH} line {} lists it, but the service does not declare it",
                        row.line
                    ),
                );
            }
        }
    }

    /// The token of `caller`, or none.
    fn token(&self, caller: Caller, forger: &TestIssuer) -> Option<String> {
        match caller {
            Caller::Anonymous => None,
            Caller::Forged => Some(forger.token(&TestIssuer::claims(self.service, None))),
            Caller::OtherAudience => {
                let other = ServiceName::ALL
                    .into_iter()
                    .find(|service| *service != self.service)
                    .unwrap_or(ServiceName::Identity);
                Some(self.issuer.token(&TestIssuer::claims(other, None)))
            }
            Caller::User {
                tenant,
                platform_admin,
                exchanged,
            } => {
                let grant = tenant.map(|role| TenantGrant {
                    id: TenantId::generate(),
                    role,
                });
                let mut claims: InternalClaims = TestIssuer::claims(self.service, grant);
                claims.platform_admin = platform_admin;
                if exchanged {
                    // As the identity service's exchange signs it: the actor, and no session.
                    let actor = ServiceName::ALL
                        .into_iter()
                        .find(|service| {
                            *service != self.service && *service != ServiceName::Identity
                        })
                        .unwrap_or(ServiceName::Assistant);
                    claims.act = Some(Actor { sub: actor });
                    claims.sid = None;
                }
                Some(self.issuer.token(&claims))
            }
        }
    }
}

/// Who a probe calls as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Caller {
    /// No token at all.
    Anonymous,
    /// A well-formed token signed by a key the service does not trust: a forged identity header
    /// sent to the pod directly, past the ingress.
    Forged,
    /// A valid token addressed to another service.
    OtherAudience,
    /// A valid token for the service.
    User {
        tenant: Option<TenantRole>,
        platform_admin: bool,
        exchanged: bool,
    },
}

/// What a probe must get.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    /// Exactly 401.
    Unauthenticated,
    /// Exactly 403.
    Refused,
    /// Neither 401 nor 403: authorization let the caller through.
    Admitted,
    /// Neither 401 nor 403 today, a known gap ([`Operation::accepts_exchanged_tokens`]).
    AdmittedByGap(&'static str),
}

#[derive(Clone, Debug)]
struct Probe {
    label: String,
    caller: Caller,
    expect: Expect,
}

impl Probe {
    fn new(label: impl Into<String>, caller: Caller, expect: Expect) -> Self {
        Self {
            label: label.into(),
            caller,
            expect,
        }
    }
}

const fn user(tenant: Option<TenantRole>, platform_admin: bool, exchanged: bool) -> Caller {
    Caller::User {
        tenant,
        platform_admin,
        exchanged,
    }
}

/// The roles a tenant token can carry, from least to most, then the one the build does not
/// know.
const ROLES: [(TenantRole, &str); 4] = [
    (TenantRole::Viewer, "viewer"),
    (TenantRole::Editor, "editor"),
    (TenantRole::Admin, "admin"),
    (TenantRole::Unrecognized, "unrecognized role"),
];

/// The role as the probes name it.
fn role_name(role: TenantRole) -> &'static str {
    ROLES
        .iter()
        .find(|(known, _)| *known == role)
        .map_or("unrecognized role", |(_, name)| name)
}

/// Every probe of `operation`, in the order they run.
fn probes(operation: &Operation) -> Vec<Probe> {
    use Expect::{Admitted, Refused, Unauthenticated};
    let access = operation.access;
    if access == Access::Public {
        return vec![Probe::new("without a token", Caller::Anonymous, Admitted)];
    }
    let mut probes = vec![
        Probe::new("without a token", Caller::Anonymous, Unauthenticated),
        Probe::new("forged token", Caller::Forged, Unauthenticated),
        Probe::new(
            "token for another service",
            Caller::OtherAudience,
            Unauthenticated,
        ),
    ];
    let exchanged = match (access.own_only(), operation.exchanged_gap) {
        (true, Some(reason)) => Expect::AdmittedByGap(reason),
        (true, None) => Refused,
        (false, _) => Admitted,
    };
    match access {
        Access::Public => {}
        Access::SignedIn | Access::OwnRequest => {
            probes.push(Probe::new(
                "signed in, no tenant",
                user(None, false, false),
                Admitted,
            ));
            probes.push(Probe::new(
                "exchanged token",
                user(None, false, true),
                exchanged,
            ));
        }
        Access::Tenant(required) => {
            probes.push(Probe::new("no tenant", user(None, false, false), Refused));
            probes.push(Probe::new(
                "platform administrator without a tenant",
                user(None, true, false),
                Refused,
            ));
            for (role, name) in ROLES {
                let expect = if role.includes(required) {
                    Admitted
                } else {
                    Refused
                };
                probes.push(Probe::new(
                    format!("tenant {name}"),
                    user(Some(role), false, false),
                    expect,
                ));
            }
            probes.push(Probe::new(
                format!("tenant {}, exchanged token", role_name(required)),
                user(Some(required), false, true),
                exchanged,
            ));
        }
        Access::PlatformAdmin => {
            probes.push(Probe::new("no tenant", user(None, false, false), Refused));
            for (role, name) in ROLES {
                probes.push(Probe::new(
                    format!("tenant {name}"),
                    user(Some(role), false, false),
                    Refused,
                ));
            }
            probes.push(Probe::new(
                "platform administrator, exchanged token",
                user(None, true, true),
                exchanged,
            ));
            probes.push(Probe::new(
                "platform administrator",
                user(None, true, false),
                Admitted,
            ));
        }
    }
    probes
}

/// The error code of a request without a valid token.
const UNAUTHENTICATED: &str = "unauthenticated";

/// The error code of a valid token whose caller the operation refuses. A 403 with another code
/// (a cross-site refusal, a membership check) is some other check answering, so it does not
/// count as the refusal the probe expects.
const PERMISSION_DENIED: &str = "permission_denied";

/// Records the probe's outcome: a refusal must carry its exact status and error code.
fn judge(
    report: &mut Report,
    operation: &Operation,
    probe: &Probe,
    status: u16,
    code: Option<&str>,
) {
    let got = match code {
        Some(code) => format!("{status} `{code}`"),
        None => status.to_string(),
    };
    let subject = operation.name();
    let label = probe.label.as_str();
    let authorized = !matches!(status, 401 | 403);
    match probe.expect {
        Expect::Unauthenticated if status == 401 && code == Some(UNAUTHENTICATED) => {
            report.pass(subject, label, got);
        }
        Expect::Unauthenticated => report.fail(
            subject,
            label,
            format!("expected 401 `{UNAUTHENTICATED}`, got {got}"),
        ),
        Expect::Refused if status == 403 && code == Some(PERMISSION_DENIED) => {
            report.pass(subject, label, got);
        }
        Expect::Refused => report.fail(
            subject,
            label,
            format!(
                "expected 403 `{PERMISSION_DENIED}` (`{}` refuses this caller), got {got}",
                operation.access.label()
            ),
        ),
        Expect::Admitted if authorized => {
            report.pass(subject, label, format!("{got}: past authorization"));
        }
        Expect::Admitted => report.fail(
            subject,
            label,
            format!(
                "expected the request past authorization (`{}` admits this caller), got {got}",
                operation.access.label()
            ),
        ),
        Expect::AdmittedByGap(reason) if authorized => report.pass(
            subject,
            label,
            format!("{got}: known gap, accepts exchanged tokens ({reason})"),
        ),
        Expect::AdmittedByGap(_) => report.fail(
            subject,
            label,
            format!(
                "got {got}: the known gap is fixed; remove accepts_exchanged_tokens and the \
                 row's 已知落差 in {MATRIX_PATH}"
            ),
        ),
    }
}

/// `path` with each `{...}` parameter replaced by a new random UUID.
fn fill(path: &str) -> String {
    let mut filled = String::with_capacity(path.len());
    let mut rest = path;
    while let Some((before, parameter)) = rest.split_once('{') {
        let Some((_, after)) = parameter.split_once('}') else {
            break;
        };
        filled.push_str(before);
        filled.push_str(&Uuid::new_v4().to_string());
        rest = after;
    }
    filled.push_str(rest);
    filled
}

/// One row of a service's table in the matrix document.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    /// 1-based line number in the document.
    line: usize,
    operation: String,
    access: String,
    gap: String,
}

/// The rows of `service`'s table: the first table after the heading `## <service>`, before the
/// next heading of level 1 or 2.
fn matrix_rows(markdown: &str, service: &str) -> Result<Vec<Row>, String> {
    let heading = format!("## {service}");
    let mut lines = markdown.lines().enumerate();
    if !lines.any(|(_, line)| line.trim_end() == heading) {
        return Err(format!("{MATRIX_PATH} has no section `{heading}`"));
    }
    let mut rows = Vec::new();
    let mut header_seen = false;
    let mut table_over = false;
    for (index, line) in lines {
        let number = index + 1;
        if line.starts_with("# ") || line.starts_with("## ") {
            break;
        }
        let Some(cells) = table_cells(line) else {
            if header_seen {
                table_over = true;
            }
            continue;
        };
        if table_over {
            return Err(format!(
                "{MATRIX_PATH} line {number}: a second table in section `{heading}`; keep one"
            ));
        }
        if !header_seen {
            if cells != MATRIX_HEADER {
                return Err(format!(
                    "{MATRIX_PATH} line {number}: the table's header must be `| {} |`",
                    MATRIX_HEADER.join(" | ")
                ));
            }
            header_seen = true;
            continue;
        }
        if cells
            .iter()
            .all(|cell| !cell.is_empty() && cell.chars().all(|c| matches!(c, '-' | ':')))
        {
            continue;
        }
        let [operation, access, gap] = cells.as_slice() else {
            return Err(format!(
                "{MATRIX_PATH} line {number}: expected {} cells",
                MATRIX_HEADER.len()
            ));
        };
        let Some(operation) = operation
            .strip_prefix('`')
            .and_then(|rest| rest.strip_suffix('`'))
        else {
            return Err(format!(
                "{MATRIX_PATH} line {number}: write the operation in backticks, `METHOD /path`"
            ));
        };
        rows.push(Row {
            line: number,
            operation: operation.to_owned(),
            access: (*access).to_owned(),
            gap: (*gap).to_owned(),
        });
    }
    if !header_seen {
        return Err(format!("{MATRIX_PATH} section `{heading}` has no table"));
    }
    Ok(rows)
}

/// What keeps the matrix document from covering exactly the operations of the whole API spec
/// (`api/openapi.json`, every service's `openapi()` merged): an operation of the spec that no
/// service's section lists, one that two sections list, and one a section lists that the spec
/// does not have. Empty when the document covers the spec. Each service's own test holds its
/// section to its declarations; this holds the document to every service registered in
/// `tools/openapi`, so a service with client operations cannot skip the matrix
/// (Laysi/trustiot-v2#585).
#[cfg(test)]
fn spec_coverage(spec: &Value, markdown: &str) -> Vec<String> {
    let documented = match operations(spec) {
        Ok(documented) => documented,
        Err(problem) => return vec![format!("api/openapi.json: {problem}")],
    };
    let mut problems = Vec::new();
    let mut listed: Vec<(String, &'static str)> = Vec::new();
    for service in ServiceName::ALL {
        let name = service.as_str();
        let heading = format!("## {name}");
        if !markdown.lines().any(|line| line.trim_end() == heading) {
            continue;
        }
        match matrix_rows(markdown, name) {
            Ok(rows) => listed.extend(rows.into_iter().map(|row| (row.operation, name))),
            Err(problem) => problems.push(problem),
        }
    }
    for operation in &documented {
        let sections: Vec<&str> = listed
            .iter()
            .filter(|(listed, _)| listed == operation)
            .map(|(_, service)| *service)
            .collect();
        match sections.as_slice() {
            [_] => {}
            [] => problems.push(format!(
                "{operation} is in api/openapi.json but in no section of {MATRIX_PATH}: declare \
                 it in its service's tests/permissions.rs and list it in the service's section"
            )),
            several => problems.push(format!(
                "{operation} is listed in more than one section of {MATRIX_PATH}: {}",
                several.join(", ")
            )),
        }
    }
    for (operation, service) in &listed {
        if !documented.contains(operation) {
            problems.push(format!(
                "{MATRIX_PATH} section `## {service}` lists {operation}, which api/openapi.json \
                 does not have"
            ));
        }
    }
    problems
}

/// Whether [`REWRITE_ENV`] (`requested`) asks for a rewrite; `ci` when one of
/// [`CI_VARIABLES`] is set.
///
/// # Errors
///
/// A value other than `1`, or a rewrite in CI.
fn rewrite_requested(requested: Option<OsString>, ci: bool) -> Result<bool, String> {
    let Some(requested) = requested else {
        return Ok(false);
    };
    if requested != "1" {
        return Err(format!(
            "{REWRITE_ENV}={}: set it to 1 to rewrite the matrix document, or unset it to compare",
            requested.to_string_lossy()
        ));
    }
    if ci {
        return Err(format!(
            "{REWRITE_ENV} is set together with CI or GITHUB_ACTIONS: CI compares the committed \
             matrix document; rewrite it locally and commit the result"
        ));
    }
    Ok(true)
}

/// Writes `content` to a temporary file next to `file`, then renames it over `file`, so an
/// interrupted rewrite leaves the old document whole.
///
/// # Errors
///
/// The write or the rename failed; the temporary file is removed.
fn replace_file(file: &Path, content: &str) -> Result<(), String> {
    let name = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| format!("{} is not a file", file.display()))?;
    let temporary = file.with_file_name(format!(".{name}.{}.rewrite", std::process::id()));
    let written = std::fs::write(&temporary, content)
        .and_then(|()| std::fs::rename(&temporary, file))
        .map_err(|error| format!("cannot write {}: {error}", file.display()));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// `cells` as one Markdown table row, an empty cell written `| |`, with its line break.
fn table_line(cells: &[&str]) -> String {
    let mut line = String::from("|");
    for cell in cells {
        if cell.is_empty() {
            line.push_str(" |");
        } else {
            line.push(' ');
            line.push_str(cell);
            line.push_str(" |");
        }
    }
    line.push('\n');
    line
}

/// A line git writes around a merge conflict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Marker {
    /// `<<<<<<< ours`
    Ours,
    /// `||||||| base` (diff3 and zdiff3 styles)
    Base,
    /// `=======`
    Theirs,
    /// `>>>>>>> theirs`
    End,
}

/// The conflict marker `line` is, if any.
fn conflict_marker(line: &str) -> Option<Marker> {
    let line = line.trim_end_matches(['\n', '\r']);
    let marker = |prefix: &str| {
        line.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    };
    if marker("<<<<<<<") {
        Some(Marker::Ours)
    } else if marker("|||||||") {
        Some(Marker::Base)
    } else if line == "=======" {
        Some(Marker::Theirs)
    } else if marker(">>>>>>>") {
        Some(Marker::End)
    } else {
        None
    }
}

/// `markdown` with the table of `service`'s section (the first table after `## <service>`,
/// from its first row to its last, conflict markers between its rows included) replaced by
/// `table`. Everything else is kept byte for byte.
///
/// # Errors
///
/// No such section or table; a conflict that starts in the table and does not end in it; or
/// conflict markers left in the section after the table was replaced.
fn rewrite_table(markdown: &str, service: &str, table: &str) -> Result<String, String> {
    let heading = format!("## {service}");
    let lines: Vec<&str> = markdown.split_inclusive('\n').collect();
    let Some(heading_at) = lines.iter().position(|line| line.trim_end() == heading) else {
        return Err(format!("{MATRIX_PATH} has no section `{heading}`"));
    };
    let section_end = lines[heading_at + 1..]
        .iter()
        .position(|line| line.starts_with("# ") || line.starts_with("## "))
        .map_or(lines.len(), |offset| heading_at + 1 + offset);
    let is_row = |line: &str| conflict_marker(line).is_none() && table_cells(line).is_some();
    let Some(start) = (heading_at + 1..section_end).find(|&index| is_row(lines[index])) else {
        return Err(format!("{MATRIX_PATH} section `{heading}` has no table"));
    };
    if table_cells(lines[start]).as_deref() != Some(MATRIX_HEADER.as_slice()) {
        return Err(format!(
            "{MATRIX_PATH} line {}: the first table of `{heading}` must start with `| {} |`; \
             nothing was rewritten",
            start + 1,
            MATRIX_HEADER.join(" | ")
        ));
    }
    let mut end = start;
    let mut open: Option<(usize, Marker)> = None;
    while end < section_end {
        let line = lines[end];
        if let Some(marker) = conflict_marker(line) {
            let expected = matches!(
                (open.map(|(_, last)| last), marker),
                (None, Marker::Ours)
                    | (Some(Marker::Ours), Marker::Base | Marker::Theirs)
                    | (Some(Marker::Base), Marker::Theirs)
                    | (Some(Marker::Theirs), Marker::End)
            );
            if !expected {
                return Err(format!(
                    "{MATRIX_PATH} line {}: a conflict marker out of order in the table of \
                     `{heading}`; resolve the conflict by hand",
                    end + 1
                ));
            }
            open = match marker {
                Marker::End => None,
                _ => Some((open.map_or(end, |(first, _)| first), marker)),
            };
        } else if !is_row(line) {
            break;
        }
        end += 1;
    }
    if let Some((first, _)) = open {
        return Err(format!(
            "{MATRIX_PATH} line {}: the merge conflict that starts in the table of `{heading}` \
             goes on past it; resolve it by hand, then rewrite",
            first + 1
        ));
    }
    let mut rewritten = String::with_capacity(markdown.len());
    rewritten.extend(lines[..start].iter().copied());
    rewritten.push_str(table);
    rewritten.extend(lines[end..].iter().copied());
    let left = (heading_at..start)
        .chain(end..section_end)
        .find(|&index| conflict_marker(lines[index]).is_some());
    if let Some(index) = left {
        return Err(format!(
            "{MATRIX_PATH} line {}: a conflict marker in section `{heading}` outside its table; \
             the rewrite regenerates the table only, resolve the rest by hand",
            index + 1
        ));
    }
    Ok(rewritten)
}

/// The trimmed cells of a Markdown table row, or `None` for a line that is not one.
fn table_cells(line: &str) -> Option<Vec<&str>> {
    let inner = line.trim().strip_prefix('|')?.strip_suffix('|')?;
    Some(inner.split('|').map(str::trim).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_parameters_become_uuids() {
        let filled = fill("/v1/a/{a_id}/b/{b}");
        let parts: Vec<&str> = filled.split('/').collect();
        assert_eq!(parts[1..3], ["v1", "a"]);
        assert!(parts[3].parse::<Uuid>().is_ok(), "{filled}");
        assert_eq!(parts[4], "b");
        assert!(parts[5].parse::<Uuid>().is_ok(), "{filled}");
        assert_eq!(fill("/v1/plain"), "/v1/plain");
    }

    #[test]
    fn declarations_parse_method_and_path() {
        assert!(
            Operation::new("GET /v1/items", Access::SignedIn)
                .parsed
                .is_some()
        );
        for malformed in ["GET", "/v1/items", "GET v1/items", "GET /v1/a b", "G?T /v1"] {
            assert!(
                Operation::new(malformed, Access::SignedIn).parsed.is_none(),
                "{malformed}"
            );
        }
    }

    #[test]
    fn each_requirement_has_its_probes() {
        let labels = |access: Access| -> Vec<(String, Expect)> {
            probes(&Operation::new("GET /x", access))
                .into_iter()
                .map(|probe| (probe.label, probe.expect))
                .collect()
        };
        assert_eq!(
            labels(Access::Public),
            [("without a token".to_owned(), Expect::Admitted)]
        );
        let editor = labels(Access::Tenant(TenantRole::Editor));
        let expect = |label: &str| {
            editor
                .iter()
                .find(|(name, _)| name == label)
                .map(|(_, expect)| *expect)
        };
        assert_eq!(expect("tenant viewer"), Some(Expect::Refused));
        assert_eq!(expect("tenant editor"), Some(Expect::Admitted));
        assert_eq!(expect("tenant admin"), Some(Expect::Admitted));
        assert_eq!(expect("tenant unrecognized role"), Some(Expect::Refused));
        assert_eq!(expect("no tenant"), Some(Expect::Refused));
        assert_eq!(
            expect("platform administrator without a tenant"),
            Some(Expect::Refused)
        );
        assert_eq!(
            expect("tenant editor, exchanged token"),
            Some(Expect::Admitted)
        );
        assert_eq!(expect("forged token"), Some(Expect::Unauthenticated));

        let platform = labels(Access::PlatformAdmin);
        assert!(platform.contains(&(
            "platform administrator, exchanged token".to_owned(),
            Expect::Refused
        )));
        assert!(platform.contains(&("tenant admin".to_owned(), Expect::Refused)));
        assert!(platform.contains(&("platform administrator".to_owned(), Expect::Admitted)));
        let own = labels(Access::OwnRequest);
        assert!(own.contains(&("exchanged token".to_owned(), Expect::Refused)));
        let signed_in = labels(Access::SignedIn);
        assert!(signed_in.contains(&("exchanged token".to_owned(), Expect::Admitted)));
    }

    #[test]
    fn a_known_gap_expects_the_exchanged_token_through() {
        let operation = Operation::new("POST /x", Access::PlatformAdmin)
            .accepts_exchanged_tokens("tracked elsewhere");
        let exchanged = probes(&operation)
            .into_iter()
            .find(|probe| probe.label == "platform administrator, exchanged token")
            .expect("the probe");
        assert_eq!(exchanged.expect, Expect::AdmittedByGap("tracked elsewhere"));
        let mut report = Report::default();
        judge(&mut report, &operation, &exchanged, 201, None);
        assert!(report.is_isolated(), "admitted: the gap still holds");
        judge(
            &mut report,
            &operation,
            &exchanged,
            403,
            Some("permission_denied"),
        );
        assert_eq!(report.failure_count(), 1, "refused: the gap is fixed");
        assert!(report.failures()[0].contains("remove accepts_exchanged_tokens"));
    }

    #[test]
    fn judging_is_exact_about_401_and_403() {
        let operation = Operation::new("GET /x", Access::Tenant(TenantRole::Admin));
        let refused = Probe::new(
            "tenant viewer",
            user(Some(TenantRole::Viewer), false, false),
            Expect::Refused,
        );
        let anonymous = Probe::new(
            "without a token",
            Caller::Anonymous,
            Expect::Unauthenticated,
        );
        let admitted = Probe::new(
            "tenant admin",
            user(Some(TenantRole::Admin), false, false),
            Expect::Admitted,
        );
        let denied = Some(PERMISSION_DENIED);
        let unauthenticated = Some(UNAUTHENTICATED);
        let cases = [
            (&refused, 403, denied, true),
            (&refused, 403, None, false),
            (&refused, 403, Some("conflict"), false),
            (&refused, 401, unauthenticated, false),
            (&refused, 404, None, false),
            (&refused, 200, None, false),
            (&anonymous, 401, unauthenticated, true),
            (&anonymous, 401, None, false),
            (&anonymous, 403, denied, false),
            (&admitted, 200, None, true),
            (&admitted, 400, Some("invalid_request"), true),
            (&admitted, 503, Some("unavailable"), true),
            (&admitted, 403, denied, false),
            (&admitted, 401, unauthenticated, false),
        ];
        for (probe, status, code, passes) in cases {
            let mut report = Report::default();
            judge(&mut report, &operation, probe, status, code);
            assert_eq!(
                report.is_isolated(),
                passes,
                "{} with {status} {code:?}",
                probe.label
            );
        }
    }

    const DOCUMENT: &str = "# 權限矩陣

## assets

說明。

| 操作 | 要求 | 已知落差 |
| --- | --- | --- |
| `GET /v1/items` | 租戶 viewer | |
| `POST /v1/items` | 平台管理員，本人 | 接受換發的 token |

## certs

| 操作 | 要求 | 已知落差 |
| --- | --- | --- |
| `GET /v1/other` | 公開 | |
";

    #[test]
    fn a_section_reads_its_own_table_only() {
        let rows = matrix_rows(DOCUMENT, "assets").expect("rows");
        assert_eq!(
            rows,
            [
                Row {
                    line: 9,
                    operation: "GET /v1/items".to_owned(),
                    access: "租戶 viewer".to_owned(),
                    gap: String::new(),
                },
                Row {
                    line: 10,
                    operation: "POST /v1/items".to_owned(),
                    access: "平台管理員，本人".to_owned(),
                    gap: EXCHANGED_GAP_LABEL.to_owned(),
                },
            ]
        );
        assert_eq!(matrix_rows(DOCUMENT, "certs").expect("rows").len(), 1);
        assert!(matrix_rows(DOCUMENT, "identity").is_err(), "no section");
        assert!(
            matrix_rows(DOCUMENT, "asset").is_err(),
            "a heading matches whole"
        );
    }

    #[test]
    fn a_malformed_table_is_refused() {
        for (broken, problem) in [
            ("## assets\n\n| 操作 | 要求 |\n| --- | --- |\n", "header"),
            ("## assets\n\nno table\n", "has no table"),
            (
                "## assets\n\n| 操作 | 要求 | 已知落差 |\n| --- | --- | --- |\n| GET /x | 公開 | |\n",
                "backticks",
            ),
            (
                "## assets\n\n| 操作 | 要求 | 已知落差 |\n| --- | --- | --- |\n| `GET /x` | 公開 |\n",
                "cells",
            ),
            (
                "## assets\n\n| 操作 | 要求 | 已知落差 |\n| --- | --- | --- |\n| `GET /x` | 公開 | |\n\n| 操作 | 要求 | 已知落差 |\n",
                "second table",
            ),
        ] {
            let error = matrix_rows(broken, "assets").expect_err(problem);
            assert!(error.contains(problem), "{problem}: {error}");
        }
    }

    #[test]
    fn the_repository_matrix_has_the_header_in_every_section() {
        // Every `## <service>` section of the real document parses.
        for line in MATRIX.lines() {
            if let Some(name) = line.strip_prefix("## ")
                && name.parse::<ServiceName>().is_ok()
            {
                matrix_rows(MATRIX, name).unwrap_or_else(|error| panic!("{error}"));
            }
        }
    }

    /// The assets table of [`DOCUMENT`] with `rows` in place of its rows.
    fn assets_table(rows: &str) -> String {
        format!("| 操作 | 要求 | 已知落差 |\n| --- | --- | --- |\n{rows}")
    }

    const ASSETS_ROWS: &str = "| `GET /v1/items` | 租戶 viewer | |
| `POST /v1/items` | 平台管理員，本人 | 接受換發的 token |
";

    #[test]
    fn the_table_is_written_as_the_document_writes_it() {
        let issuer = TestIssuer::new();
        let suite = Suite::new(ServiceName::Assets, &issuer)
            .operation(Operation::new(
                "GET /v1/items",
                Access::Tenant(TenantRole::Viewer),
            ))
            .operation(
                Operation::new("POST /v1/items", Access::PlatformAdmin)
                    .accepts_exchanged_tokens("tracked elsewhere"),
            );
        assert_eq!(suite.matrix_table(), Ok(assets_table(ASSETS_ROWS)));
        // Rewriting the section with its own table changes nothing.
        assert_eq!(
            rewrite_table(DOCUMENT, "assets", &assets_table(ASSETS_ROWS)).as_deref(),
            Ok(DOCUMENT)
        );
        let broken = Suite::new(ServiceName::Assets, &issuer)
            .operation(Operation::new("GET /v1/a|b", Access::Public));
        assert!(broken.matrix_table().is_err(), "a pipe would split the row");
    }

    #[test]
    fn a_rewrite_replaces_the_sections_table_only() {
        let table =
            assets_table("| `GET /v1/items` | 租戶 editor | |\n| `PUT /v1/items` | 公開 | |\n");
        let rewritten = rewrite_table(DOCUMENT, "assets", &table).expect("rewritten");
        assert_eq!(
            rewritten,
            DOCUMENT.replace(&assets_table(ASSETS_ROWS), &table),
            "the prose, the heading and the certs section stay as they are"
        );
        assert_eq!(
            matrix_rows(&rewritten, "certs").expect("certs"),
            matrix_rows(DOCUMENT, "certs").expect("certs")
        );
        assert!(
            rewrite_table(DOCUMENT, "identity", &table).is_err(),
            "no section"
        );
        let other_table = DOCUMENT.replace("說明。\n", "| 欄 | 值 |\n| --- | --- |\n| a | b |\n");
        let error = rewrite_table(&other_table, "assets", &table).expect_err("another table");
        assert!(error.contains("must start with"), "{error}");
        let no_table = "# 權限矩陣\n\n## assets\n\n說明。\n\n## certs\n";
        let error = rewrite_table(no_table, "assets", &table).expect_err("no table");
        assert!(error.contains("has no table"), "{error}");
    }

    #[test]
    fn a_rewrite_resolves_a_conflict_inside_the_table_only() {
        let table = assets_table(ASSETS_ROWS);
        for conflict in [
            // Both sides added a row at the end of the table.
            "| `GET /v1/items` | 租戶 viewer | |\n| `POST /v1/items` | 平台管理員，本人 | 接受換發的 token |\n<<<<<<< HEAD\n| `GET /v1/a` | 公開 | |\n=======\n| `GET /v1/b` | 公開 | |\n>>>>>>> other\n",
            // The diff3 style, with the base between.
            "<<<<<<< HEAD\n| `GET /v1/items` | 租戶 viewer | |\n||||||| base\n=======\n| `GET /v1/b` | 公開 | |\n>>>>>>> other\n| `POST /v1/items` | 平台管理員，本人 | 接受換發的 token |\n",
        ] {
            let conflicted = DOCUMENT.replace(ASSETS_ROWS, conflict);
            assert_ne!(conflicted, DOCUMENT);
            assert!(
                matrix_rows(&conflicted, "assets").is_err(),
                "the comparison refuses a conflicted table"
            );
            assert_eq!(
                rewrite_table(&conflicted, "assets", &table).as_deref(),
                Ok(DOCUMENT),
                "{conflict}"
            );
        }
        for (broken, problem) in [
            // The conflict goes on into the prose after the table.
            (
                DOCUMENT.replace(
                    "| `POST /v1/items` | 平台管理員，本人 | 接受換發的 token |\n\n",
                    "<<<<<<< HEAD\n| `POST /v1/items` | 平台管理員，本人 | 接受換發的 token |\n\n=======\n\n>>>>>>> other\n",
                ),
                "goes on past it",
            ),
            // A conflict in the prose before the table.
            (
                DOCUMENT.replace("說明。\n", "<<<<<<< HEAD\n說明。\n=======\n說明二。\n>>>>>>> other\n"),
                "outside its table",
            ),
            // Markers out of order.
            (
                DOCUMENT.replace(
                    "| `GET /v1/items` | 租戶 viewer | |\n",
                    "| `GET /v1/items` | 租戶 viewer | |\n=======\n",
                ),
                "out of order",
            ),
        ] {
            let error = rewrite_table(&broken, "assets", &table).expect_err(problem);
            assert!(error.contains(problem), "{problem}: {error}");
        }
    }

    #[test]
    fn only_one_rewrites_and_never_in_ci() {
        let value = |text: &str| Some(OsString::from(text));
        assert_eq!(rewrite_requested(None, false), Ok(false));
        assert_eq!(rewrite_requested(None, true), Ok(false));
        assert_eq!(rewrite_requested(value("1"), false), Ok(true));
        assert!(rewrite_requested(value("1"), true).is_err(), "never in CI");
        for requested in ["true", "0", "", "yes", " 1"] {
            assert!(
                rewrite_requested(value(requested), false).is_err(),
                "{requested:?}"
            );
        }
    }

    /// The API spec every service's `openapi()` is merged into (`tools/openapi` keeps it equal
    /// to what the code generates), as of this build.
    const API_SPEC: &str = include_str!("../../../api/openapi.json");

    #[test]
    fn the_matrix_covers_every_operation_of_the_api_spec() {
        let spec: Value = serde_json::from_str(API_SPEC).expect("api/openapi.json is JSON");
        let problems = spec_coverage(&spec, MATRIX);
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn the_spec_coverage_finds_what_is_missing_twice_or_gone() {
        let spec = serde_json::json!({"paths": {
            "/v1/items": {"get": {}, "post": {}},
            "/v1/other": {"get": {}},
        }});
        // DOCUMENT lists GET /v1/items and POST /v1/items under assets, GET /v1/other under certs.
        assert_eq!(spec_coverage(&spec, DOCUMENT), Vec::<String>::new());

        let missing = serde_json::json!({"paths": {
            "/v1/items": {"get": {}, "post": {}, "delete": {}},
            "/v1/other": {"get": {}},
        }});
        let problems = spec_coverage(&missing, DOCUMENT);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("DELETE /v1/items is in api/openapi.json but in no section"));

        let gone = serde_json::json!({"paths": {"/v1/items": {"get": {}, "post": {}}}});
        let problems = spec_coverage(&gone, DOCUMENT);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("`## certs` lists GET /v1/other"));

        let twice = format!(
            "{DOCUMENT}\n## identity\n\n| 操作 | 要求 | 已知落差 |\n| --- | --- | --- |\n| `GET /v1/other` | 公開 | |\n"
        );
        let problems = spec_coverage(&spec, &twice);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("GET /v1/other is listed in more than one section"));
    }
}
