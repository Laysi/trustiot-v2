//! The cross-tenant negative test suite (decision 29): proves that a user of tenant A cannot
//! read or write tenant B's data, through the database and through the service's API.
//!
//! A [`Suite`] holds one [`Store`] (the Postgres tables of the service, [`postgres::PgStore`])
//! and the service's tenant endpoints ([`api::Endpoint`]). [`Suite::run`]:
//!
//! 1. Coverage: every table or view the service's database role can use, and every operation
//!    in the service's OpenAPI document, must be declared, or exempted with a reason. A new table
//!    or endpoint that nobody added to the suite fails the run instead of going untested.
//! 2. Seeds rows for each tenant in every declared table, bypassing isolation: one per endpoint
//!    that writes a single row plus one for reads, at least two. Each row carries a random id
//!    and a canary string; together with the tenant ids these are the *markers* of a
//!    tenant ([`Seeded::markers`]).
//! 3. Attacks the store as tenant A against tenant B and as B against A: reads, writes that
//!    try to reach or create the other tenant's rows, and reads without a tenant.
//! 4. Calls each endpoint as tenant A (a tenant admin, the most privileged tenant user), then
//!    as tenant B: the other tenant's markers must never appear in a response, its resources
//!    must answer 403 or 404, and its stored rows must be unchanged afterwards.
//!
//! Every probe also has a positive control (tenant A sees its own rows, its own resources
//! answer 2xx), so a table or endpoint that returns nothing at all cannot pass by accident.
//!
//! Extension: ClickHouse row policies (Laysi/trustiot-v2#59) and AGE graph roles (MS4) become
//! further [`Store`] implementations that seed, attack, and snapshot their objects with the
//! same [`World`], markers, and [`Report`]; the endpoint probes work with any store.

pub mod api;
pub mod postgres;

use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;

use contract::{ServiceName, TenantGrant, TenantId, TenantRole};
use service_kit::identity::UserIdentity;
use uuid::Uuid;

use crate::identity::TestIssuer;

/// One of the two tenants of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Side {
    /// Tenant A.
    A,
    /// Tenant B.
    B,
}

impl Side {
    /// The other tenant.
    #[must_use]
    pub fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }

    /// `tenant A` or `tenant B`, for reports.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::A => "tenant A",
            Self::B => "tenant B",
        }
    }

    fn letter(self) -> char {
        match self {
            Self::A => 'a',
            Self::B => 'b',
        }
    }
}

/// The two tenants of one run, new for every run, so a database shared between runs works.
#[derive(Clone, Copy, Debug)]
pub struct World {
    a: TenantId,
    b: TenantId,
}

impl World {
    /// Two new tenants.
    #[must_use]
    pub fn new() -> Self {
        Self {
            a: TenantId::generate(),
            b: TenantId::generate(),
        }
    }

    /// The tenant of `side`.
    #[must_use]
    pub fn tenant(&self, side: Side) -> TenantId {
        match side {
            Side::A => self.a,
            Side::B => self.b,
        }
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

/// A random marker string for `side`, `canary-a-<32 hex digits>` or `canary-b-...`.
#[must_use]
pub fn canary(side: Side) -> String {
    format!("canary-{}-{}", side.letter(), Uuid::new_v4().simple())
}

/// A row a [`Store`] seeded for one tenant: its object (table), id, and canary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeededRow {
    /// The table or other object the row is in, as declared.
    pub object: &'static str,
    /// Whose row it is.
    pub side: Side,
    /// The row's id.
    pub id: Uuid,
    /// A string unique to the row.
    pub canary: String,
}

impl SeededRow {
    /// A row of `object` for `side`, with a new id and canary.
    #[must_use]
    pub fn new(object: &'static str, side: Side) -> Self {
        Self {
            object,
            side,
            id: Uuid::new_v4(),
            canary: canary(side),
        }
    }
}

/// The ways an id can appear in text: hyphenated and simple, lower case.
fn id_forms(id: Uuid) -> [String; 2] {
    [id.hyphenated().to_string(), id.simple().to_string()]
}

/// Whether `text` shows `row`: contains its id or its canary (case-insensitive).
#[must_use]
pub fn shows(text: &str, row: &SeededRow) -> bool {
    let text = text.to_lowercase();
    id_forms(row.id)
        .iter()
        .chain([&row.canary])
        .any(|marker| text.contains(marker.as_str()))
}

/// The seeded rows of a run and its two tenants.
#[derive(Clone, Debug)]
pub struct Seeded {
    world: World,
    rows: Vec<SeededRow>,
}

impl Seeded {
    /// The rows a store seeded for `world`.
    #[must_use]
    pub fn new(world: World, rows: Vec<SeededRow>) -> Self {
        Self { world, rows }
    }

    /// The run's tenants.
    #[must_use]
    pub fn world(&self) -> World {
        self.world
    }

    /// The rows of `object` seeded for `side`, in seeding order.
    #[must_use]
    pub fn rows(&self, object: &str, side: Side) -> Vec<&SeededRow> {
        self.rows
            .iter()
            .filter(|row| row.object == object && row.side == side)
            .collect()
    }

    /// The objects that have seeded rows.
    #[must_use]
    pub fn objects(&self) -> BTreeSet<&'static str> {
        self.rows.iter().map(|row| row.object).collect()
    }

    /// Every marker of `side`: its tenant id, and the ids and canaries of its rows, lower case.
    #[must_use]
    pub fn markers(&self, side: Side) -> Vec<String> {
        let tenant = Uuid::from(self.world.tenant(side));
        let mut markers: Vec<String> = id_forms(tenant).into();
        for row in self.rows.iter().filter(|row| row.side == side) {
            markers.extend(id_forms(row.id));
            markers.push(row.canary.clone());
        }
        markers
    }

    /// The markers of `side` that appear in `text` (case-insensitive). `except` are ids the
    /// caller itself put into the request, such as the row id in the path or the tenant id in
    /// the body of a request aimed at the other tenant; a response that echoes them is not a
    /// leak, while any other marker (a canary, another row's id) still is.
    #[must_use]
    pub fn leaks(&self, text: &str, side: Side, except: &[Uuid]) -> Vec<String> {
        let text = text.to_lowercase();
        let excepted: Vec<String> = except.iter().copied().flat_map(id_forms).collect();
        self.markers(side)
            .into_iter()
            .filter(|marker| !excepted.contains(marker))
            .filter(|marker| text.contains(marker.as_str()))
            .collect()
    }
}

/// One attack run: who attacks, and the seeded data.
pub struct Attack<'a> {
    /// The seeded rows of both tenants.
    pub seeded: &'a Seeded,
    /// The tenant the attacker belongs to.
    pub attacker: Side,
    issuer: &'a TestIssuer,
    service: ServiceName,
}

impl<'a> Attack<'a> {
    /// An attack as `attacker` against the other tenant of `seeded`, with identities for
    /// `service` signed by `issuer`.
    #[must_use]
    pub fn new(
        seeded: &'a Seeded,
        attacker: Side,
        issuer: &'a TestIssuer,
        service: ServiceName,
    ) -> Self {
        Self {
            seeded,
            attacker,
            issuer,
            service,
        }
    }

    /// The tenant under attack.
    #[must_use]
    pub fn victim(&self) -> Side {
        self.attacker.other()
    }

    /// The service under test.
    #[must_use]
    pub fn service(&self) -> ServiceName {
        self.service
    }

    /// Claims for an admin of `side`'s tenant (the most privileged tenant user), or for a user
    /// without a tenant.
    #[must_use]
    pub fn claims(&self, side: Option<Side>) -> contract::InternalClaims {
        let grant = side.map(|side| TenantGrant {
            id: self.seeded.world().tenant(side),
            role: TenantRole::Admin,
        });
        TestIssuer::claims(self.service, grant)
    }

    /// The verified identity of an admin of the attacker's tenant.
    #[must_use]
    pub fn user(&self) -> UserIdentity {
        self.issuer
            .user(self.service, &self.claims(Some(self.attacker)))
    }

    /// A signed token for `side`'s admin, or for a user without a tenant.
    #[must_use]
    pub fn token(&self, side: Option<Side>) -> String {
        self.issuer.token(&self.claims(side))
    }
}

impl fmt::Debug for Attack<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attack")
            .field("attacker", &self.attacker)
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

/// A store of tenant data whose isolation the suite tests: Postgres today
/// ([`postgres::PgStore`]); ClickHouse and AGE implement it when their services arrive.
pub trait Store {
    /// Records a failure for every object the service can reach that is not declared, and for
    /// every declaration that matches no such object.
    fn check_coverage(&self, report: &mut Report) -> impl Future<Output = ()> + Send;

    /// Writes `rows_per_tenant` rows into every declared object for each tenant of `world`,
    /// bypassing isolation, and checks that each row can be found again by its id or canary.
    /// Records a failure for every row that cannot be seeded.
    fn seed(
        &self,
        world: World,
        rows_per_tenant: usize,
        report: &mut Report,
    ) -> impl Future<Output = Vec<SeededRow>> + Send;

    /// Attacks every declared object as `attack.attacker`, through the same path the service
    /// uses, and records each probe.
    fn attack(&self, attack: &Attack<'_>, report: &mut Report) -> impl Future<Output = ()> + Send;

    /// The stored rows that carry `side`'s markers, as text, sorted, read bypassing isolation;
    /// the suite compares the victim's rows before and after the endpoint probes.
    fn snapshot(
        &self,
        seeded: &Seeded,
        side: Side,
        report: &mut Report,
    ) -> impl Future<Output = Vec<String>> + Send;
}

/// Rows seeded per tenant and object at least, so the database probes' positive controls see
/// more than one row.
pub const MIN_ROWS_PER_TENANT: usize = 2;

/// What the suite knows about the service's HTTP API.
#[derive(Clone, Debug)]
enum ApiDeclaration {
    /// Neither [`Suite::openapi`] nor [`Suite::without_api`] was called: the run fails.
    Undeclared,
    /// The service's OpenAPI document; every operation must be declared or exempt.
    OpenApi(serde_json::Value),
    /// The service has no HTTP API, for this reason.
    None(&'static str),
}

/// The cross-tenant negative test suite of one service.
///
/// ```no_run
/// # async fn example(owner: sqlx::postgres::PgConnectOptions,
/// #     app: sqlx::postgres::PgConnectOptions, openapi: serde_json::Value, base_url: &str) {
/// use contract::ServiceName;
/// use testkit::identity::TestIssuer;
/// use testkit::isolation::Suite;
/// use testkit::isolation::api::Endpoint;
/// use testkit::isolation::postgres::PgStore;
/// use testkit::pg_table;
///
/// let issuer = TestIssuer::new(); // write_jwks() writes the keys file of the service under test
/// let store = PgStore::new(owner, app)
///     .table(pg_table!(
///         "devices",
///         seed = "INSERT INTO devices (tenant_id, id, name) VALUES ($1, $2, $3)"
///     ))
///     .exempt("public.device_models", "the shared model catalogue has no tenant data");
/// let report = Suite::new(ServiceName::Assets, &issuer, store)
///     .openapi(openapi)
///     .endpoint(Endpoint::list("/v1/devices", "devices"))
///     .endpoint(Endpoint::get("/v1/devices/{id}", "devices"))
///     .run(Some(base_url))
///     .await;
/// report.assert_isolated();
/// # }
/// ```
#[derive(Debug)]
pub struct Suite<'a, S> {
    service: ServiceName,
    issuer: &'a TestIssuer,
    store: S,
    endpoints: Vec<api::Endpoint>,
    exempt_endpoints: Vec<(String, &'static str)>,
    api: ApiDeclaration,
    without_tenant_data: Option<&'static str>,
}

impl<'a, S: Store> Suite<'a, S> {
    /// The suite of `service`, whose tokens `issuer` signs, over its tables in `store`.
    #[must_use]
    pub fn new(service: ServiceName, issuer: &'a TestIssuer, store: S) -> Self {
        Self {
            service,
            issuer,
            store,
            endpoints: Vec::new(),
            exempt_endpoints: Vec::new(),
            api: ApiDeclaration::Undeclared,
            without_tenant_data: None,
        }
    }

    /// Adds a tenant endpoint to attack.
    #[must_use]
    pub fn endpoint(mut self, endpoint: api::Endpoint) -> Self {
        self.endpoints.push(endpoint);
        self
    }

    /// The service's OpenAPI document (`serde_json::to_value(service::openapi())`): every
    /// operation in it must be declared with [`Suite::endpoint`] or [`Suite::exempt_endpoint`].
    #[must_use]
    pub fn openapi(mut self, document: serde_json::Value) -> Self {
        self.api = ApiDeclaration::OpenApi(document);
        self
    }

    /// Leaves an operation, written `METHOD /path` as in the OpenAPI document (for example
    /// `GET /v1/whoami`), out of the attacks, for `reason`: it serves no tenant data, or a
    /// hand-written test covers it.
    #[must_use]
    pub fn exempt_endpoint(mut self, operation: &str, reason: &'static str) -> Self {
        self.exempt_endpoints.push((operation.to_owned(), reason));
        self
    }

    /// The service has no HTTP API (for example it only consumes events), for `reason`.
    #[must_use]
    pub fn without_api(mut self, reason: &'static str) -> Self {
        self.api = ApiDeclaration::None(reason);
        self
    }

    /// The service keeps and serves no tenant data, for `reason`: every relation is exempt
    /// ([`postgres::PgStore::exempt`]) and every operation too ([`Suite::exempt_endpoint`]), so
    /// only the coverage checks apply. Without this, such a suite fails with "nothing is
    /// declared"; with it, a tenant table or an endpoint to attack fails the run, because the
    /// declaration no longer holds.
    #[must_use]
    pub fn without_tenant_data(mut self, reason: &'static str) -> Self {
        self.without_tenant_data = Some(reason);
        self
    }

    /// Runs the suite; `base_url` is where the service under test listens (for example
    /// `http://127.0.0.1:8080`), required when endpoints are declared. Start the service with
    /// the issuer's [`TestIssuer::jwks`] and the same database as the store.
    pub async fn run(&self, base_url: Option<&str>) -> Report {
        let mut report = Report::default();
        self.store.check_coverage(&mut report).await;
        api::check_coverage(
            &self.api,
            &self.endpoints,
            &self.exempt_endpoints,
            &mut report,
        );

        let world = World::new();
        let failed_before_seeding = report.failure_count();
        let rows_per_tenant = api::rows_needed(&self.endpoints).max(MIN_ROWS_PER_TENANT);
        let rows = self.store.seed(world, rows_per_tenant, &mut report).await;
        if report.failure_count() > failed_before_seeding {
            report.fail(
                "suite",
                "seed",
                "stopped: probes against unseeded objects would prove nothing",
            );
            return report;
        }
        let seeded = Seeded::new(world, rows);
        let nothing_declared = seeded.objects().is_empty() && self.endpoints.is_empty();
        match (self.without_tenant_data, nothing_declared) {
            (None, true) => {
                report.fail(
                    "suite",
                    "declarations",
                    "nothing is declared, so nothing was tested",
                );
                return report;
            }
            (Some(reason), true) => {
                report.pass(
                    "suite",
                    "declarations",
                    format!("no tenant data, every relation and operation exempt: {reason}"),
                );
                return report;
            }
            (Some(reason), false) => report.fail(
                "suite",
                "declarations",
                format!(
                    "declared without tenant data ({reason}), but tenant tables or endpoints \
                     are declared"
                ),
            ),
            (None, false) => {}
        }

        for attacker in [Side::A, Side::B] {
            let attack = Attack::new(&seeded, attacker, self.issuer, self.service);
            self.store.attack(&attack, &mut report).await;
        }

        if self.endpoints.is_empty() {
            return report;
        }
        let Some(base_url) = base_url else {
            report.fail(
                "api",
                "base URL",
                "endpoints are declared but run() got no base URL",
            );
            return report;
        };
        for attacker in [Side::A, Side::B] {
            let attack = Attack::new(&seeded, attacker, self.issuer, self.service);
            let victim = attack.victim();
            let before = self.store.snapshot(&seeded, victim, &mut report).await;
            api::attack(base_url, &self.endpoints, &attack, &mut report).await;
            let after = self.store.snapshot(&seeded, victim, &mut report).await;
            compare_snapshots(&before, &after, victim, &mut report);
        }
        report
    }
}

/// Records whether the victim's stored rows survived the attacker's endpoint probes unchanged.
fn compare_snapshots(before: &[String], after: &[String], victim: Side, report: &mut Report) {
    let removed: Vec<&String> = before.iter().filter(|row| !after.contains(row)).collect();
    let added: Vec<&String> = after.iter().filter(|row| !before.contains(row)).collect();
    let probe = format!(
        "{} rows after {}'s endpoint probes",
        victim.label(),
        victim.other().label()
    );
    if removed.is_empty() && added.is_empty() {
        report.pass(
            "api",
            probe,
            format!("{} rows, all unchanged", before.len()),
        );
    } else {
        report.fail(
            "api",
            probe,
            format!("changed by another tenant's requests: before {removed:?}, after {added:?}"),
        );
    }
}

/// One probe and its outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    subject: String,
    probe: String,
    outcome: String,
    failed: bool,
}

/// The outcome of every probe of a run. Print it (`{report}`) as test evidence; it never
/// contains tokens.
#[derive(Clone, Debug, Default)]
pub struct Report {
    entries: Vec<Entry>,
}

impl Report {
    /// Records a probe that passed. `subject` is what was probed (a table, an operation).
    pub fn pass(
        &mut self,
        subject: impl Into<String>,
        probe: impl Into<String>,
        outcome: impl Into<String>,
    ) {
        self.push(subject.into(), probe.into(), outcome.into(), false);
    }

    /// Records a probe that failed: a leak, a missing positive control, a coverage gap.
    pub fn fail(
        &mut self,
        subject: impl Into<String>,
        probe: impl Into<String>,
        outcome: impl Into<String>,
    ) {
        self.push(subject.into(), probe.into(), outcome.into(), true);
    }

    fn push(&mut self, subject: String, probe: String, outcome: String, failed: bool) {
        self.entries.push(Entry {
            subject,
            probe,
            outcome,
            failed,
        });
    }

    /// How many probes failed.
    #[must_use]
    pub fn failure_count(&self) -> usize {
        self.entries.iter().filter(|entry| entry.failed).count()
    }

    /// Each failed probe as `subject | probe | outcome`.
    #[must_use]
    pub fn failures(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|entry| entry.failed)
            .map(|entry| format!("{} | {} | {}", entry.subject, entry.probe, entry.outcome))
            .collect()
    }

    /// Whether at least one probe ran and none failed.
    #[must_use]
    pub fn is_isolated(&self) -> bool {
        !self.entries.is_empty() && self.failure_count() == 0
    }

    /// Prints the report and fails the test unless [`Report::is_isolated`].
    ///
    /// # Panics
    ///
    /// When a probe failed or none ran.
    #[allow(clippy::print_stdout, reason = "the report is the test evidence")]
    pub fn assert_isolated(&self) {
        println!("{self}");
        assert!(
            self.is_isolated(),
            "cross-tenant isolation failed ({} of {} probes):\n{}",
            self.failure_count(),
            self.entries.len(),
            self.failures().join("\n")
        );
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "result | subject | probe | outcome")?;
        for entry in &self.entries {
            let result = if entry.failed { "FAIL" } else { "pass" };
            writeln!(
                f,
                "{result} | {} | {} | {}",
                entry.subject, entry.probe, entry.outcome
            )?;
        }
        write!(
            f,
            "{} probes, {} failed",
            self.entries.len(),
            self.failure_count()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded() -> Seeded {
        let rows = vec![
            SeededRow::new("items", Side::A),
            SeededRow::new("items", Side::B),
        ];
        Seeded::new(World::new(), rows)
    }

    #[test]
    fn markers_cover_the_tenant_and_each_row() {
        let seeded = seeded();
        let markers = seeded.markers(Side::B);
        let row = seeded.rows("items", Side::B)[0].clone();
        let tenant = Uuid::from(seeded.world().tenant(Side::B));
        for expected in [
            tenant.hyphenated().to_string(),
            tenant.simple().to_string(),
            row.id.hyphenated().to_string(),
            row.id.simple().to_string(),
            row.canary.clone(),
        ] {
            assert!(markers.contains(&expected), "missing {expected}");
        }
        assert!(
            !markers
                .iter()
                .any(|marker| seeded.markers(Side::A).contains(marker)),
            "the tenants share no marker"
        );
    }

    #[test]
    fn leaks_find_any_marker_in_any_case() {
        let seeded = seeded();
        let b = seeded.rows("items", Side::B)[0].clone();
        let tenant = Uuid::from(seeded.world().tenant(Side::B));
        assert!(seeded.leaks("nothing here", Side::B, &[]).is_empty());
        assert_eq!(
            seeded.leaks(&format!("{{\"name\":\"{}\"}}", b.canary), Side::B, &[]),
            std::slice::from_ref(&b.canary)
        );
        let upper = b.id.hyphenated().to_string().to_uppercase();
        assert!(!seeded.leaks(&upper, Side::B, &[]).is_empty());
        let simple = tenant.simple().to_string();
        assert!(!seeded.leaks(&simple, Side::B, &[]).is_empty());
        // Tenant A's rows are not tenant B's leak.
        let a = seeded.rows("items", Side::A)[0].clone();
        assert!(seeded.leaks(&a.canary, Side::B, &[]).is_empty());
    }

    #[test]
    fn the_requested_id_is_not_a_leak_but_its_canary_is() {
        let seeded = seeded();
        let b = seeded.rows("items", Side::B)[0].clone();
        let echo = format!("no item {}", b.id);
        assert!(seeded.leaks(&echo, Side::B, &[b.id]).is_empty());
        assert_eq!(
            seeded.leaks(&format!("{echo} {}", b.canary), Side::B, &[b.id]),
            std::slice::from_ref(&b.canary)
        );
    }

    #[test]
    fn a_tenant_id_sent_in_the_request_is_not_a_leak() {
        let seeded = seeded();
        let tenant = Uuid::from(seeded.world().tenant(Side::B));
        let b = seeded.rows("items", Side::B)[0].clone();
        let echo = format!("{{\"tenant_id\":\"{tenant}\"}}");
        assert!(seeded.leaks(&echo, Side::B, &[tenant]).is_empty());
        assert!(!seeded.leaks(&echo, Side::B, &[]).is_empty());
        assert!(
            !seeded
                .leaks(&format!("{echo} {}", b.id), Side::B, &[tenant])
                .is_empty()
        );
    }

    #[test]
    fn shows_finds_a_row_by_id_or_canary() {
        let row = SeededRow::new("items", Side::A);
        assert!(shows(&row.id.to_string(), &row));
        assert!(shows(&row.canary, &row));
        assert!(!shows("something else", &row));
    }

    #[test]
    fn an_empty_report_is_not_isolated() {
        let mut report = Report::default();
        assert!(!report.is_isolated());
        report.pass("items", "read", "ok");
        assert!(report.is_isolated());
        report.fail("items", "write", "leaked");
        assert!(!report.is_isolated());
        assert_eq!(report.failures(), ["items | write | leaked"]);
    }

    #[test]
    fn snapshot_changes_fail() {
        let mut report = Report::default();
        let before = vec!["items: (b, 1)".to_owned()];
        compare_snapshots(&before, &before, Side::B, &mut report);
        assert!(report.is_isolated());
        compare_snapshots(&before, &[], Side::B, &mut report);
        assert_eq!(report.failure_count(), 1);
    }
}
