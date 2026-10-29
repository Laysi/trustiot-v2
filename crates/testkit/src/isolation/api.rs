//! Endpoint probes: the service's HTTP API called as one tenant against the other's resources,
//! first as tenant A, then as tenant B (the table says A and B for the first round).
//!
//! | Kind | Attack (tenant A's admin) | Without a tenant | Positive control |
//! | --- | --- | --- | --- |
//! | [`Endpoint::list`] | the response holds none of B's markers | holds no tenant's markers | 2xx, every row of A seeded in the object appears |
//! | [`Endpoint::get`], [`Endpoint::item`] | B's row id in the path: 403 or 404, no other B marker in the response | A's row: not 2xx, no markers | A's row: 2xx (a `GET` also shows the row) |
//! | [`Endpoint::create`] | a body naming tenant B (`{tenant}`): no B marker in the response | not 2xx, no markers | the same body naming A: 2xx |
//!
//! After each round, the victim's stored rows must be exactly as before it, so a write that
//! reached the other tenant through any endpoint fails the run.
//!
//! Bodies are JSON; in their strings `{id}` becomes the row id of the request, `{tenant}` the
//! tenant id the request targets (B in attacks, A in positive controls), and `{canary}` a new
//! canary. Reads run before writes, so the positive controls of lists see every seeded row, and
//! each write to a single row uses a seeded row no other probe uses.

use std::time::Duration;

/// The HTTP method of an [`Endpoint`], re-exported so services need no `http` dependency.
pub use http::Method;
use serde_json::Value;
use uuid::Uuid;

use super::{ApiDeclaration, Attack, Report, SeededRow, Side, canary, shows};

/// Methods an OpenAPI path item can hold, lower case as written there.
const OPENAPI_METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// How long one request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    List,
    Item,
    Create,
}

/// A tenant endpoint of the service under test. `object` is the declared table (or other
/// store object) whose seeded rows the endpoint serves.
#[derive(Clone, Debug)]
pub struct Endpoint {
    method: Method,
    path: &'static str,
    object: &'static str,
    kind: Kind,
    body: Option<Value>,
}

impl Endpoint {
    /// `GET path` lists the caller's tenant's rows of `object`. The path has no parameter.
    #[must_use]
    pub fn list(path: &'static str, object: &'static str) -> Self {
        Self::new(Method::GET, path, object, Kind::List)
    }

    /// `GET path` returns one row of `object`; the path's one parameter (for example
    /// `/v1/devices/{id}`) is the row id.
    #[must_use]
    pub fn get(path: &'static str, object: &'static str) -> Self {
        Self::new(Method::GET, path, object, Kind::Item)
    }

    /// `method path` acts on one row of `object` (update, delete); the path's one parameter is
    /// the row id. Give a request body with [`Endpoint::body`].
    #[must_use]
    pub fn item(method: Method, path: &'static str, object: &'static str) -> Self {
        Self::new(method, path, object, Kind::Item)
    }

    /// `method path` creates a row of `object`; the path has no parameter. Give the body with
    /// [`Endpoint::body`], naming the target tenant as `{tenant}` wherever the API could take
    /// one.
    #[must_use]
    pub fn create(method: Method, path: &'static str, object: &'static str) -> Self {
        Self::new(method, path, object, Kind::Create)
    }

    fn new(method: Method, path: &'static str, object: &'static str, kind: Kind) -> Self {
        Self {
            method,
            path,
            object,
            kind,
            body: None,
        }
    }

    /// The JSON request body; see the module documentation for `{id}`, `{tenant}`, and
    /// `{canary}`.
    #[must_use]
    pub fn body(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }

    /// `METHOD /path`, as the OpenAPI coverage check names operations.
    #[must_use]
    pub fn operation(&self) -> String {
        format!("{} {}", self.method, self.path)
    }

    fn reads(&self) -> bool {
        self.method == Method::GET
    }

    /// A write that targets one row: it gets a seeded row of its own.
    fn writes_a_row(&self) -> bool {
        self.kind == Kind::Item && !self.reads()
    }
}

/// How many endpoints write a single row.
fn row_writes(endpoints: &[Endpoint]) -> usize {
    endpoints
        .iter()
        .filter(|endpoint| endpoint.writes_a_row())
        .count()
}

/// How many rows a store must seed per tenant and object for `endpoints`: one that every read
/// uses, plus one for each write to a single row and attacker, so a write's positive control (a
/// `DELETE`, say) never changes a row another probe relies on, and each tenant's attacks target
/// rows the other tenant's own probes left alone.
pub(super) fn rows_needed(endpoints: &[Endpoint]) -> usize {
    1 + 2 * row_writes(endpoints)
}

/// The number of `{...}` parameters in a path.
fn parameters(path: &str) -> usize {
    path.matches('{').count()
}

/// `path` with its one `{...}` parameter replaced by `id`.
fn fill_path(path: &str, id: Uuid) -> String {
    match (path.find('{'), path.find('}')) {
        (Some(start), Some(end)) if start < end => {
            let (before, rest) = path.split_at(start);
            let after = rest.get(end - start + 1..).unwrap_or_default();
            format!("{before}{id}{after}")
        }
        _ => path.to_owned(),
    }
}

/// `value` with `{id}`, `{tenant}`, and `{canary}` replaced in every string.
fn fill_body(value: &Value, id: Uuid, tenant: Uuid, canary: &str) -> Value {
    match value {
        Value::String(text) => Value::String(
            text.replace("{id}", &id.to_string())
                .replace("{tenant}", &tenant.to_string())
                .replace("{canary}", canary),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| fill_body(item, id, tenant, canary))
                .collect(),
        ),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, item)| (key.clone(), fill_body(item, id, tenant, canary)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The operations of an OpenAPI document, as `METHOD /path`.
pub(crate) fn operations(document: &Value) -> Result<Vec<String>, &'static str> {
    let paths = document
        .get("paths")
        .and_then(Value::as_object)
        .ok_or("the OpenAPI document has no paths object")?;
    let mut operations = Vec::new();
    for (path, item) in paths {
        let item = item
            .as_object()
            .ok_or("an OpenAPI path item is not an object")?;
        for method in OPENAPI_METHODS {
            if item.contains_key(method) {
                operations.push(format!("{} {path}", method.to_uppercase()));
            }
        }
    }
    operations.sort();
    Ok(operations)
}

/// Every operation of the API must be declared or exempt, every declaration must name an
/// operation, and every endpoint's path must have the parameters its kind needs.
pub(super) fn check_coverage(
    api: &ApiDeclaration,
    endpoints: &[Endpoint],
    exempt: &[(String, &'static str)],
    report: &mut Report,
) {
    for endpoint in endpoints {
        let expected = match endpoint.kind {
            Kind::Item => 1,
            Kind::List | Kind::Create => 0,
        };
        if parameters(endpoint.path) != expected {
            report.fail(
                endpoint.operation(),
                "declaration",
                format!(
                    "a {:?} endpoint needs exactly {expected} path parameter(s); nested paths \
                     need a hand-written test and exempt_endpoint",
                    endpoint.kind
                ),
            );
        }
    }
    let declared: Vec<String> = endpoints
        .iter()
        .map(Endpoint::operation)
        .chain(exempt.iter().map(|(operation, _)| operation.clone()))
        .collect();
    for (index, operation) in declared.iter().enumerate() {
        if declared[..index].contains(operation) {
            report.fail(operation, "coverage", "declared more than once");
        }
    }
    let document = match api {
        ApiDeclaration::Undeclared => {
            report.fail(
                "api",
                "coverage",
                "call openapi(document) with the service's OpenAPI document, or without_api(reason)",
            );
            return;
        }
        ApiDeclaration::None(reason) => {
            if declared.is_empty() {
                report.pass("api", "coverage", format!("no HTTP API: {reason}"));
            } else {
                report.fail(
                    "api",
                    "coverage",
                    "without_api() is set but endpoints are declared",
                );
            }
            return;
        }
        ApiDeclaration::OpenApi(document) => document,
    };
    let operations = match operations(document) {
        Ok(operations) => operations,
        Err(problem) => {
            report.fail("api", "coverage", problem);
            return;
        }
    };
    for operation in &operations {
        if let Some((_, reason)) = exempt.iter().find(|(name, _)| name == operation) {
            report.pass(operation, "coverage", format!("exempt: {reason}"));
        } else if !declared.contains(operation) {
            report.fail(
                operation,
                "coverage",
                "in the OpenAPI document but not in the suite: declare it with endpoint(), or \
                 exempt_endpoint() with the reason it needs no cross-tenant probe",
            );
        }
    }
    for operation in &declared {
        if !operations.contains(operation) {
            report.fail(
                operation,
                "coverage",
                "declared but not in the OpenAPI document (removed or misspelled)",
            );
        }
    }
}

/// A response: status and body text, and the ids the request itself carried.
struct Answer {
    status: u16,
    body: String,
    /// Ids put into the request (the row id of the path or body, the tenant id of the body); a
    /// response may echo them without leaking anything.
    supplied: Vec<Uuid>,
}

impl Answer {
    fn success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Calls the service; an error is the reason the request did not complete.
async fn send(
    client: &reqwest::Client,
    base_url: &str,
    method: &Method,
    path: &str,
    token: &str,
    body: Option<&Value>,
) -> Result<Answer, String> {
    let url = format!("{}{path}", base_url.trim_end_matches('/'));
    let mut request = client
        .request(method.clone(), url)
        .header(contract::INTERNAL_IDENTITY_HEADER, token);
    if let Some(body) = body {
        request = request
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("request failed: {error}"))?;
    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .map_err(|error| format!("reading the response failed: {error}"))?;
    Ok(Answer {
        status,
        body,
        supplied: Vec::new(),
    })
}

/// One probe's request.
struct Call<'e> {
    endpoint: &'e Endpoint,
    /// The token's tenant; `None` for a user without one.
    caller: Option<Side>,
    /// The row in the path, if any.
    row: Option<&'e SeededRow>,
    /// The tenant `{tenant}` stands for.
    target: Side,
}

/// Runs every endpoint's probes as `attack.attacker`, reads first.
pub(super) async fn attack(
    base_url: &str,
    endpoints: &[Endpoint],
    attack: &Attack<'_>,
    report: &mut Report,
) {
    let client = match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
        Ok(client) => client,
        Err(error) => {
            report.fail("api", "client", error.to_string());
            return;
        }
    };
    // Reads first, on row 0; each write to a single row then on a row of its own: 1, 2, ... when
    // tenant A attacks, the next ones when tenant B does.
    let reads = endpoints.iter().filter(|endpoint| endpoint.reads());
    let writes = endpoints.iter().filter(|endpoint| !endpoint.reads());
    let mut next_row = match attack.attacker {
        Side::A => 1,
        Side::B => 1 + row_writes(endpoints),
    };
    for endpoint in reads.chain(writes) {
        let row = if endpoint.writes_a_row() {
            next_row += 1;
            next_row - 1
        } else {
            0
        };
        Probe {
            client: &client,
            base_url,
            attack,
            endpoint,
            row,
        }
        .run(report)
        .await;
    }
}

struct Probe<'p> {
    client: &'p reqwest::Client,
    base_url: &'p str,
    attack: &'p Attack<'p>,
    endpoint: &'p Endpoint,
    /// Which of the object's seeded rows (per tenant) the probes use.
    row: usize,
}

impl Probe<'_> {
    async fn run(&self, report: &mut Report) {
        let endpoint = self.endpoint;
        let subject = endpoint.operation();
        let attacker = self.attack.attacker;
        let victim = self.attack.victim();
        let seeded = self.attack.seeded;
        let own = seeded.rows(endpoint.object, attacker);
        let theirs = seeded.rows(endpoint.object, victim);
        let index = self.row;
        let (Some(own_row), Some(their_row)) =
            (own.get(index).copied(), theirs.get(index).copied())
        else {
            report.fail(
                &subject,
                "declaration",
                format!(
                    "`{}` has no seeded row {index}; name a declared table that has a seed",
                    endpoint.object
                ),
            );
            return;
        };
        match endpoint.kind {
            Kind::List => {
                self.attack_list(&subject, &own, report).await;
                let call = self.call(None, None, attacker);
                self.without_tenant(&subject, call, false, report).await;
            }
            Kind::Item => {
                self.attack_item(&subject, their_row, report).await;
                let call = self.call(None, Some(own_row), attacker);
                self.without_tenant(&subject, call, true, report).await;
                self.own_item(&subject, own_row, report).await;
            }
            Kind::Create => {
                self.attack_create(&subject, report).await;
                let call = self.call(None, None, attacker);
                self.without_tenant(&subject, call, true, report).await;
                self.own_create(&subject, report).await;
            }
        }
    }

    fn call<'e>(
        &'e self,
        caller: Option<Side>,
        row: Option<&'e SeededRow>,
        target: Side,
    ) -> Call<'e> {
        Call {
            endpoint: self.endpoint,
            caller,
            row,
            target,
        }
    }

    async fn send(&self, call: &Call<'_>) -> Result<Answer, String> {
        let world = self.attack.seeded.world();
        let row_id = call.row.map_or_else(Uuid::new_v4, |row| row.id);
        let path = match call.row {
            Some(row) => fill_path(call.endpoint.path, row.id),
            None => call.endpoint.path.to_owned(),
        };
        let marker = canary(self.attack.attacker);
        let body =
            call.endpoint.body.as_ref().map(|body| {
                fill_body(body, row_id, Uuid::from(world.tenant(call.target)), &marker)
            });
        let token = self.attack.token(call.caller);
        let mut answer = send(
            self.client,
            self.base_url,
            &call.endpoint.method,
            &path,
            &token,
            body.as_ref(),
        )
        .await?;
        let template = call
            .endpoint
            .body
            .as_ref()
            .map(Value::to_string)
            .unwrap_or_default();
        if call.row.is_some() || template.contains("{id}") {
            answer.supplied.push(row_id);
        }
        if template.contains("{tenant}") {
            answer.supplied.push(Uuid::from(world.tenant(call.target)));
        }
        Ok(answer)
    }

    async fn attack_list(&self, subject: &str, own: &[&SeededRow], report: &mut Report) {
        let attacker = self.attack.attacker;
        let victim = self.attack.victim();
        let probe = format!("list as {}", attacker.label());
        let call = self.call(Some(attacker), None, attacker);
        let answer = match self.send(&call).await {
            Ok(answer) => answer,
            Err(error) => return report.fail(subject, probe, error),
        };
        let leaked = self
            .attack
            .seeded
            .leaks(&answer.body, victim, &answer.supplied);
        let missing = own.iter().filter(|row| !shows(&answer.body, row)).count();
        if !leaked.is_empty() {
            report.fail(
                subject,
                probe,
                format!(
                    "{} shows {}'s data: {leaked:?}",
                    answer.status,
                    victim.label()
                ),
            );
        } else if !answer.success() || missing > 0 {
            report.fail(
                subject,
                probe,
                format!(
                    "positive control: expected 2xx with all {} own rows, got {} with {missing} \
                     missing",
                    own.len(),
                    answer.status
                ),
            );
        } else {
            report.pass(
                subject,
                probe,
                format!(
                    "{}: all {} own rows, nothing of {}",
                    answer.status,
                    own.len(),
                    victim.label()
                ),
            );
        }
    }

    async fn attack_item(&self, subject: &str, their_row: &SeededRow, report: &mut Report) {
        let attacker = self.attack.attacker;
        let victim = self.attack.victim();
        let probe = format!("{}'s row as {}", victim.label(), attacker.label());
        let call = self.call(Some(attacker), Some(their_row), victim);
        let answer = match self.send(&call).await {
            Ok(answer) => answer,
            Err(error) => return report.fail(subject, probe, error),
        };
        let leaked = self
            .attack
            .seeded
            .leaks(&answer.body, victim, &answer.supplied);
        if !leaked.is_empty() {
            report.fail(
                subject,
                probe,
                format!(
                    "{} shows {}'s data: {leaked:?}",
                    answer.status,
                    victim.label()
                ),
            );
        } else if !matches!(answer.status, 403 | 404) {
            report.fail(
                subject,
                probe,
                format!(
                    "expected 403 or 404 for another tenant's resource, got {}",
                    answer.status
                ),
            );
        } else {
            report.pass(subject, probe, format!("{}, nothing shown", answer.status));
        }
    }

    async fn attack_create(&self, subject: &str, report: &mut Report) {
        let attacker = self.attack.attacker;
        let victim = self.attack.victim();
        let probe = format!("create for {} as {}", victim.label(), attacker.label());
        let call = self.call(Some(attacker), None, victim);
        let answer = match self.send(&call).await {
            Ok(answer) => answer,
            Err(error) => return report.fail(subject, probe, error),
        };
        let leaked = self
            .attack
            .seeded
            .leaks(&answer.body, victim, &answer.supplied);
        if leaked.is_empty() {
            report.pass(
                subject,
                probe,
                format!(
                    "{}, nothing of {} shown (stored rows are compared after all probes)",
                    answer.status,
                    victim.label()
                ),
            );
        } else {
            report.fail(
                subject,
                probe,
                format!(
                    "{} shows {}'s data: {leaked:?}",
                    answer.status,
                    victim.label()
                ),
            );
        }
    }

    /// A user without a tenant: never any tenant's data, and no success where `refuse` is set.
    async fn without_tenant(
        &self,
        subject: &str,
        call: Call<'_>,
        refuse: bool,
        report: &mut Report,
    ) {
        let probe = "without a tenant";
        let answer = match self.send(&call).await {
            Ok(answer) => answer,
            Err(error) => return report.fail(subject, probe, error),
        };
        let seeded = self.attack.seeded;
        let mut leaked = seeded.leaks(&answer.body, Side::A, &answer.supplied);
        leaked.extend(seeded.leaks(&answer.body, Side::B, &answer.supplied));
        if !leaked.is_empty() {
            report.fail(
                subject,
                probe,
                format!("{} shows tenant data: {leaked:?}", answer.status),
            );
        } else if refuse && answer.success() {
            report.fail(
                subject,
                probe,
                format!("expected a refusal, got {}", answer.status),
            );
        } else {
            report.pass(subject, probe, format!("{}, nothing shown", answer.status));
        }
    }

    /// The same create for the attacker's own tenant must succeed, or the attack proved
    /// nothing (a malformed body, a broken endpoint).
    async fn own_create(&self, subject: &str, report: &mut Report) {
        let attacker = self.attack.attacker;
        let victim = self.attack.victim();
        let probe = format!("positive control: create for {}", attacker.label());
        let call = self.call(Some(attacker), None, attacker);
        let answer = match self.send(&call).await {
            Ok(answer) => answer,
            Err(error) => return report.fail(subject, probe, error),
        };
        let leaked = self
            .attack
            .seeded
            .leaks(&answer.body, victim, &answer.supplied);
        if !leaked.is_empty() {
            report.fail(
                subject,
                probe,
                format!(
                    "{} shows {}'s data: {leaked:?}",
                    answer.status,
                    victim.label()
                ),
            );
        } else if answer.success() {
            report.pass(subject, probe, answer.status.to_string());
        } else {
            report.fail(
                subject,
                probe,
                format!(
                    "expected 2xx, got {}: without a working create the attack proves nothing",
                    answer.status
                ),
            );
        }
    }

    async fn own_item(&self, subject: &str, own_row: &SeededRow, report: &mut Report) {
        let attacker = self.attack.attacker;
        let probe = format!("positive control: own row as {}", attacker.label());
        let call = self.call(Some(attacker), Some(own_row), attacker);
        let answer = match self.send(&call).await {
            Ok(answer) => answer,
            Err(error) => return report.fail(subject, probe, error),
        };
        let shown = !self.endpoint.reads() || shows(&answer.body, own_row);
        if answer.success() && shown {
            report.pass(subject, probe, answer.status.to_string());
        } else {
            report.fail(
                subject,
                probe,
                format!(
                    "expected 2xx{}, got {}",
                    if self.endpoint.reads() {
                        " showing the row"
                    } else {
                        ""
                    },
                    answer.status
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn document() -> Value {
        json!({"openapi": "3.1.0", "paths": {
            "/v1/items": {"get": {}, "post": {}},
            "/v1/items/{id}": {"get": {}, "put": {}, "parameters": []},
            "/v1/whoami": {"get": {}}
        }})
    }

    fn checked(endpoints: &[Endpoint], exempt: &[(String, &'static str)]) -> Report {
        let mut report = Report::default();
        check_coverage(
            &ApiDeclaration::OpenApi(document()),
            endpoints,
            exempt,
            &mut report,
        );
        report
    }

    fn all_endpoints() -> Vec<Endpoint> {
        vec![
            Endpoint::list("/v1/items", "items"),
            Endpoint::create(Method::POST, "/v1/items", "items"),
            Endpoint::get("/v1/items/{id}", "items"),
            Endpoint::item(Method::PUT, "/v1/items/{id}", "items"),
        ]
    }

    fn whoami_exempt() -> Vec<(String, &'static str)> {
        vec![("GET /v1/whoami".to_owned(), "no tenant data")]
    }

    #[test]
    fn every_operation_declared_or_exempt_passes() {
        let report = checked(&all_endpoints(), &whoami_exempt());
        assert_eq!(report.failures(), Vec::<String>::new());
    }

    #[test]
    fn an_undeclared_operation_fails() {
        let mut endpoints = all_endpoints();
        endpoints.pop();
        let failures = checked(&endpoints, &whoami_exempt()).failures();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].starts_with("PUT /v1/items/{id} | coverage"));
    }

    #[test]
    fn a_stale_or_repeated_declaration_fails() {
        let mut endpoints = all_endpoints();
        endpoints.push(Endpoint::list("/v1/gone", "items"));
        endpoints.push(Endpoint::list("/v1/items", "items"));
        let failures = checked(&endpoints, &whoami_exempt()).failures();
        assert!(
            failures
                .iter()
                .any(|f| f.starts_with("GET /v1/gone | coverage"))
        );
        assert!(
            failures
                .iter()
                .any(|f| f.contains("GET /v1/items | coverage | declared more than once"))
        );
    }

    #[test]
    fn paths_must_match_the_kind() {
        let endpoints = [
            Endpoint::get("/v1/items", "items"),
            Endpoint::list("/v1/items/{id}", "items"),
        ];
        let mut report = Report::default();
        check_coverage(&ApiDeclaration::None("test"), &endpoints, &[], &mut report);
        let failures = report.failures();
        assert_eq!(
            failures
                .iter()
                .filter(|f| f.contains("| declaration |"))
                .count(),
            2,
            "{failures:?}"
        );
    }

    #[test]
    fn an_undeclared_api_fails() {
        let mut report = Report::default();
        check_coverage(&ApiDeclaration::Undeclared, &[], &[], &mut report);
        assert_eq!(report.failure_count(), 1);
        let mut report = Report::default();
        check_coverage(
            &ApiDeclaration::None("consumes events only"),
            &[],
            &[],
            &mut report,
        );
        assert!(report.is_isolated());
    }

    #[test]
    fn each_write_to_a_row_gets_its_own_row() {
        assert_eq!(rows_needed(&[Endpoint::list("/v1/items", "items")]), 1);
        assert_eq!(rows_needed(&all_endpoints()), 3, "one PUT per attacker");
        let mut endpoints = all_endpoints();
        endpoints.push(Endpoint::item(Method::DELETE, "/v1/items/{id}", "items"));
        endpoints.push(Endpoint::item(Method::PATCH, "/v1/items/{id}", "items"));
        assert_eq!(
            rows_needed(&endpoints),
            7,
            "PUT, DELETE, PATCH per attacker, and the reads"
        );
    }

    #[test]
    fn placeholders_are_filled() {
        let id = Uuid::new_v4();
        let tenant = Uuid::new_v4();
        assert_eq!(
            fill_path("/v1/items/{id}/x", id),
            format!("/v1/items/{id}/x")
        );
        let body = fill_body(
            &json!({"tenant_id": "{tenant}", "names": ["{canary}"], "ref": "item {id}", "n": 1}),
            id,
            tenant,
            "canary-a-1",
        );
        assert_eq!(
            body,
            json!({"tenant_id": tenant.to_string(), "names": ["canary-a-1"], "ref": format!("item {id}"), "n": 1})
        );
    }
}
