//! Proof that the permission matrix (`testkit::permissions`) catches what it is for: a small
//! example service with one operation of each requirement passes with every role check in place,
//! and fails, on exactly the operation and caller concerned, when one check is removed or
//! loosened. The matrix document comparison and the coverage check fail on every kind of drift.
//! Needs no container: the example service runs in the test process.

use axum::extract::{FromRef, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use contract::{ErrorBody, ErrorCode, ServiceName, TenantRole};
use serde_json::{Value, json};
use service_kit::identity::{IdentityVerifier, UserIdentity};
use testkit::identity::TestIssuer;
use testkit::isolation::Report;
use testkit::permissions::{Access, Operation, Suite};

const SERVICE: ServiceName = ServiceName::Assets;

/// A role check the example service can lose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flaw {
    /// Every check in place.
    None,
    /// `GET /v1/status` wants a token although it is public.
    PublicRouteLocked,
    /// `PUT /v1/me/settings` takes a token another service exchanged.
    OwnCheckRemoved,
    /// `POST /v1/items` checks for a viewer instead of an editor.
    EditorCheckLowered,
    /// `DELETE /v1/items/{id}` checks nothing but the token.
    TenantCheckRemoved,
    /// `GET /v1/platform/items` takes a token another service exchanged.
    PlatformOwnCheckRemoved,
    /// `GET /v1/platform/items` also lets tenant administrators in.
    PlatformCheckWidened,
    /// `POST /v1/platform/legacy` refuses exchanged tokens now: its known gap is fixed.
    GapFixed,
}

#[derive(Clone)]
struct App {
    verifier: IdentityVerifier,
    flaw: Flaw,
}

impl FromRef<App> for IdentityVerifier {
    fn from_ref(state: &App) -> Self {
        state.verifier.clone()
    }
}

fn has_role(user: &UserIdentity, role: TenantRole) -> bool {
    user.tenant().is_some_and(|grant| grant.role.includes(role))
}

fn exchanged(user: &UserIdentity) -> bool {
    user.claims().act.is_some()
}

/// `admitted` if the check lets the caller through, otherwise 403 with the M1 body
/// `permission_denied`, as the services answer.
fn decide(allowed: bool, admitted: StatusCode) -> Response {
    if allowed {
        admitted.into_response()
    } else {
        let body = ErrorBody::new(ErrorCode::PermissionDenied);
        (body.http_status(), Json(body)).into_response()
    }
}

async fn status() -> StatusCode {
    StatusCode::OK
}

async fn status_locked(_user: UserIdentity) -> StatusCode {
    StatusCode::OK
}

async fn me(_user: UserIdentity) -> StatusCode {
    StatusCode::OK
}

/// Past the check, a request without a body is malformed.
async fn settings(State(app): State<App>, user: UserIdentity) -> Response {
    let own = app.flaw == Flaw::OwnCheckRemoved || !exchanged(&user);
    decide(own, StatusCode::BAD_REQUEST)
}

async fn list_items(user: UserIdentity) -> Response {
    decide(has_role(&user, TenantRole::Viewer), StatusCode::OK)
}

async fn create_item(State(app): State<App>, user: UserIdentity) -> Response {
    let required = if app.flaw == Flaw::EditorCheckLowered {
        TenantRole::Viewer
    } else {
        TenantRole::Editor
    };
    decide(has_role(&user, required), StatusCode::BAD_REQUEST)
}

/// Past the check, the made-up id names nothing.
async fn delete_item(State(app): State<App>, user: UserIdentity) -> Response {
    let allowed = app.flaw == Flaw::TenantCheckRemoved || has_role(&user, TenantRole::Admin);
    decide(allowed, StatusCode::NOT_FOUND)
}

async fn platform_items(State(app): State<App>, user: UserIdentity) -> Response {
    let platform = user.is_platform_admin()
        || (app.flaw == Flaw::PlatformCheckWidened && has_role(&user, TenantRole::Admin));
    let own = app.flaw == Flaw::PlatformOwnCheckRemoved || !exchanged(&user);
    decide(platform && own, StatusCode::OK)
}

async fn platform_legacy(State(app): State<App>, user: UserIdentity) -> Response {
    let own = app.flaw != Flaw::GapFixed || !exchanged(&user);
    decide(user.is_platform_admin() && own, StatusCode::BAD_REQUEST)
}

/// Starts the example service with `flaw`; its base URL.
async fn start(issuer: &TestIssuer, flaw: Flaw) -> String {
    let public = if flaw == Flaw::PublicRouteLocked {
        get(status_locked)
    } else {
        get(status)
    };
    let router = Router::new()
        .route("/v1/status", public)
        .route("/v1/me", get(me))
        .route("/v1/me/settings", put(settings))
        .route("/v1/items", get(list_items).post(create_item))
        .route("/v1/items/{id}", delete(delete_item))
        .route("/v1/platform/items", get(platform_items))
        .route("/v1/platform/legacy", post(platform_legacy))
        .with_state(App {
            verifier: issuer.verifier(SERVICE),
            flaw,
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a local port");
    let address = listener.local_addr().expect("local address");
    tokio::spawn(async move { axum::serve(listener, router).await });
    format!("http://{address}")
}

fn openapi() -> Value {
    json!({"openapi": "3.1.0", "paths": {
        "/v1/status": {"get": {}},
        "/v1/me": {"get": {}},
        "/v1/me/settings": {"put": {}},
        "/v1/items": {"get": {}, "post": {}},
        "/v1/items/{id}": {"delete": {}, "parameters": []},
        "/v1/platform/items": {"get": {}},
        "/v1/platform/legacy": {"post": {}}
    }})
}

const MATRIX: &str = "# 權限矩陣

## assets

| 操作 | 要求 | 已知落差 |
| --- | --- | --- |
| `GET /v1/status` | 公開 | |
| `GET /v1/me` | 已登入 | |
| `PUT /v1/me/settings` | 已登入，本人 | |
| `GET /v1/items` | 租戶 viewer | |
| `POST /v1/items` | 租戶 editor | |
| `DELETE /v1/items/{id}` | 租戶 admin | |
| `GET /v1/platform/items` | 平台管理員，本人 | |
| `POST /v1/platform/legacy` | 平台管理員，本人 | 接受換發的 token |

## certs

| 操作 | 要求 | 已知落差 |
| --- | --- | --- |
| `GET /v1/elsewhere` | 公開 | |
";

fn operations() -> Vec<Operation> {
    vec![
        Operation::new("GET /v1/status", Access::Public),
        Operation::new("GET /v1/me", Access::SignedIn),
        Operation::new("PUT /v1/me/settings", Access::OwnRequest),
        Operation::new("GET /v1/items", Access::Tenant(TenantRole::Viewer)),
        Operation::new("POST /v1/items", Access::Tenant(TenantRole::Editor)),
        Operation::new("DELETE /v1/items/{id}", Access::Tenant(TenantRole::Admin)),
        Operation::new("GET /v1/platform/items", Access::PlatformAdmin),
        Operation::new("POST /v1/platform/legacy", Access::PlatformAdmin)
            .accepts_exchanged_tokens("the example's known gap"),
    ]
}

fn suite<'a>(issuer: &'a TestIssuer, matrix: &'a str, operations: Vec<Operation>) -> Suite<'a> {
    operations.into_iter().fold(
        Suite::new(SERVICE, issuer)
            .openapi(openapi())
            .matrix(matrix),
        Suite::operation,
    )
}

async fn run(flaw: Flaw) -> Report {
    let issuer = TestIssuer::new();
    let base_url = start(&issuer, flaw).await;
    suite(&issuer, MATRIX, operations()).run(&base_url).await
}

#[tokio::test]
async fn the_example_passes_with_every_check_in_place() {
    let report = run(Flaw::None).await;
    report.assert_passed("the example's permission matrix");
    let text = report.to_string();
    for probe in [
        "pass | GET /v1/status | without a token | 200: past authorization",
        "pass | PUT /v1/me/settings | exchanged token | 403",
        "pass | POST /v1/items | tenant viewer | 403",
        "pass | POST /v1/items | tenant editor | 400: past authorization",
        "pass | DELETE /v1/items/{id} | platform administrator without a tenant | 403",
        "pass | DELETE /v1/items/{id} | tenant unrecognized role | 403",
        "pass | GET /v1/platform/items | forged token | 401",
        "pass | GET /v1/platform/items | token for another service | 401",
        "pass | GET /v1/platform/items | platform administrator, exchanged token | 403",
        "pass | POST /v1/platform/legacy | platform administrator, exchanged token | 400: known gap",
    ] {
        assert!(text.contains(probe), "missing `{probe}` in\n{text}");
    }
}

#[tokio::test]
async fn each_removed_check_fails_on_its_operation_and_caller() {
    let cases: [(Flaw, &str, &[&str]); 7] = [
        (
            Flaw::PublicRouteLocked,
            "GET /v1/status",
            &["without a token"],
        ),
        (
            Flaw::OwnCheckRemoved,
            "PUT /v1/me/settings",
            &["exchanged token"],
        ),
        (
            Flaw::EditorCheckLowered,
            "POST /v1/items",
            &["tenant viewer"],
        ),
        (
            Flaw::TenantCheckRemoved,
            "DELETE /v1/items/{id}",
            &[
                "no tenant",
                "platform administrator without a tenant",
                "tenant viewer",
                "tenant editor",
                "tenant unrecognized role",
            ],
        ),
        (
            Flaw::PlatformOwnCheckRemoved,
            "GET /v1/platform/items",
            &["platform administrator, exchanged token"],
        ),
        (
            Flaw::PlatformCheckWidened,
            "GET /v1/platform/items",
            &["tenant admin"],
        ),
        (
            Flaw::GapFixed,
            "POST /v1/platform/legacy",
            &["platform administrator, exchanged token"],
        ),
    ];
    for (flaw, operation, callers) in cases {
        let report = run(flaw).await;
        let failures = report.failures();
        let expected: Vec<String> = callers
            .iter()
            .map(|caller| format!("{operation} | {caller} | "))
            .collect();
        assert_eq!(
            failures.len(),
            expected.len(),
            "{flaw:?}: exactly the callers the flaw lets in or locks out\n{report}"
        );
        for prefix in &expected {
            assert!(
                failures.iter().any(|failure| failure.starts_with(prefix)),
                "{flaw:?}: no failure `{prefix}…` in {failures:#?}"
            );
        }
    }
}

#[tokio::test]
async fn a_fixed_gap_says_what_to_remove() {
    let failures = run(Flaw::GapFixed).await.failures();
    assert!(failures[0].contains("got 403"), "{failures:?}");
    assert!(
        failures[0].contains("remove accepts_exchanged_tokens"),
        "{failures:?}"
    );
}

/// The declarations against `matrix`: the failures.
fn matrix_failures(matrix: &str, operations: Vec<Operation>) -> Vec<String> {
    suite(&TestIssuer::new(), matrix, operations)
        .check_declarations()
        .failures()
}

#[test]
fn the_matrix_document_must_list_exactly_the_declarations() {
    assert_eq!(matrix_failures(MATRIX, operations()), Vec::<String>::new());

    let cases: [(&str, String, &str); 5] = [
        (
            "a row with another requirement",
            MATRIX.replace(
                "| `POST /v1/items` | 租戶 editor |",
                "| `POST /v1/items` | 租戶 viewer |",
            ),
            "POST /v1/items | matrix | docs/architecture/permissions.md line 11 says `租戶 viewer`",
        ),
        (
            "a missing row",
            MATRIX.replace("| `GET /v1/me` | 已登入 | |\n", ""),
            "GET /v1/me | matrix | declared but missing",
        ),
        (
            "a row nobody declares",
            MATRIX.replace(
                "| `GET /v1/me` | 已登入 | |\n",
                "| `GET /v1/me` | 已登入 | |\n| `GET /v1/ghost` | 公開 | |\n",
            ),
            "GET /v1/ghost | matrix | ",
        ),
        (
            "a known gap the document leaves out",
            MATRIX.replace(
                "| 平台管理員，本人 | 接受換發的 token |",
                "| 平台管理員，本人 | |",
            ),
            "POST /v1/platform/legacy | matrix | ",
        ),
        (
            "a row listed twice",
            MATRIX.replace(
                "| `GET /v1/me` | 已登入 | |\n",
                "| `GET /v1/me` | 已登入 | |\n| `GET /v1/me` | 已登入 | |\n",
            ),
            "GET /v1/me | matrix | docs/architecture/permissions.md line 9: listed more than once",
        ),
    ];
    for (what, matrix, expected) in cases {
        let failures = matrix_failures(&matrix, operations());
        assert_eq!(failures.len(), 1, "{what}: {failures:#?}");
        assert!(failures[0].starts_with(expected), "{what}: {failures:#?}");
    }

    // A declaration the document has another requirement for.
    let mut changed = operations();
    changed[1] = Operation::new("GET /v1/me", Access::OwnRequest);
    let failures = matrix_failures(MATRIX, changed);
    assert_eq!(failures.len(), 1, "{failures:#?}");
    assert!(
        failures[0].contains("the declaration `已登入，本人`"),
        "{failures:#?}"
    );

    // No section for the service at all.
    let failures = matrix_failures(&MATRIX.replace("## assets", "## alerts"), operations());
    assert_eq!(
        failures,
        ["permissions | matrix | docs/architecture/permissions.md has no section `## assets`"]
    );
}

#[test]
fn every_operation_must_be_declared_once_and_exist() {
    let issuer = TestIssuer::new();
    let mut without_one = operations();
    without_one.remove(1);
    let failures = suite(&issuer, MATRIX, without_one)
        .check_declarations()
        .failures();
    assert!(
        failures
            .iter()
            .any(|failure| failure.starts_with("GET /v1/me | coverage | in the OpenAPI document")),
        "{failures:#?}"
    );

    let mut extra = operations();
    extra.push(Operation::new("GET /v1/gone", Access::Public));
    let failures = suite(&issuer, MATRIX, extra)
        .check_declarations()
        .failures();
    assert!(
        failures
            .iter()
            .any(|failure| failure.starts_with("GET /v1/gone | coverage | declared but not")),
        "{failures:#?}"
    );

    let mut repeated = operations();
    repeated.push(Operation::new("GET /v1/me", Access::SignedIn));
    let failures = suite(&issuer, MATRIX, repeated)
        .check_declarations()
        .failures();
    assert!(
        failures.contains(&"GET /v1/me | declaration | declared more than once".to_owned()),
        "{failures:#?}"
    );

    let mut odd = operations();
    odd.push(Operation::new("GET /v1/me/settings", Access::SignedIn).accepts_exchanged_tokens("x"));
    odd.push(Operation::new(
        "PATCH /v1/x",
        Access::Tenant(TenantRole::Unrecognized),
    ));
    odd.push(Operation::new("/v1/no-method", Access::Public));
    let failures = suite(&issuer, MATRIX, odd).check_declarations().failures();
    for expected in [
        "GET /v1/me/settings | declaration | accepts_exchanged_tokens is a gap of own-request",
        "PATCH /v1/x | declaration | Tenant(Unrecognized) grants nothing",
        "/v1/no-method | declaration | not `METHOD /path`",
    ] {
        assert!(
            failures.iter().any(|failure| failure.starts_with(expected)),
            "missing `{expected}` in {failures:#?}"
        );
    }

    let bare =
        Suite::new(SERVICE, &issuer).operation(Operation::new("GET /v1/me", Access::SignedIn));
    let failures = bare.check_declarations().failures();
    assert!(
        failures
            .iter()
            .any(|f| f.starts_with("permissions | coverage | call openapi")),
        "{failures:#?}"
    );
    assert!(
        failures
            .iter()
            .any(|f| f.starts_with("permissions | matrix | call matrix")),
        "{failures:#?}"
    );
}
