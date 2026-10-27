//! The admin API (README.md, 「管理 API」) on the gateway's routers, against a real Postgres with
//! the gateway's migration: a change and a reset with their versions, what `GET` shows, that the
//! next forwarded request uses the change, that every refused request (a model of another
//! protocol, a provider that is not configured, a malformed list, a stale version, a missing or
//! malformed actor, a malformed body) answers its code and changes nothing, that a database that
//! cannot be reached answers 503, and that the admin and forwarding routers do not share a route.
//!
//! The two-process, restart, log and port cases run the real binary (tests/process.rs). The
//! database tests skip without podman or docker, and fail without them when `CI` is set.

mod support;

use std::sync::Arc;

use ai_gateway::MODEL_HEADER;
use ai_gateway_api::{ACTOR_HEADER, MAX_ACTOR_CHARS, ROLES_PATH};
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgConnection;
use support::database::{Database, models};
use support::{MESSAGES, Reply, Server, Step, Vars, fixture, json_reply, post, send};

const BODY: &str = r#"{"max_tokens":16,"messages":[{"role":"user","content":"Hi"}]}"#;

const ACTOR: &str = "user:0f3a-admin";
const MAIN: &str = "/internal/admin/v1/roles/main";

const COUNT_ROLES: &str = "SELECT count(*) FROM ai_gateway.role_models";
const COUNT_CHANGES: &str = "SELECT count(*) FROM ai_gateway.role_model_changes";
const READ_GENERATION: &str = "SELECT generation FROM ai_gateway.settings_generation";
const READ_CHANGES: &str = "SELECT version, role, action, models, changed_by
FROM ai_gateway.role_model_changes ORDER BY version";

/// A row of the change log: version, role, action, models, actor.
type ChangeRecord = (i64, String, String, Option<Vec<String>>, String);

fn message_reply(request_id: &str) -> Vec<Step> {
    vec![Step::Send(json_reply(
        &fixture("anthropic/message_end_turn.json"),
        "request-id",
        request_id,
    ))]
}

/// What the database holds: the generation, the stored roles and the change records.
async fn counts(connection: &mut PgConnection) -> (i64, i64, i64) {
    let generation: i64 = sqlx::query_scalar(READ_GENERATION)
        .fetch_one(&mut *connection)
        .await
        .expect("read the generation");
    let roles: i64 = sqlx::query_scalar(COUNT_ROLES)
        .fetch_one(&mut *connection)
        .await
        .expect("count the roles");
    let changes: i64 = sqlx::query_scalar(COUNT_CHANGES)
        .fetch_one(&mut *connection)
        .await
        .expect("count the changes");
    (generation, roles, changes)
}

/// A request to the admin router with each of `actors` as an actor header.
async fn admin_with(
    router: &Router,
    method: &str,
    path: &str,
    actors: &[HeaderValue],
    body: Option<String>,
) -> Reply {
    let mut request = Request::builder().method(method).uri(path);
    for actor in actors {
        request = request.header(ACTOR_HEADER, actor.clone());
    }
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(body)),
        None => request.body(Body::empty()),
    };
    send(router, request.expect("a request")).await
}

/// A request to the admin router as [`ACTOR`].
async fn admin(router: &Router, method: &str, path: &str, body: Option<Value>) -> Reply {
    admin_with(
        router,
        method,
        path,
        &[HeaderValue::from_static(ACTOR)],
        body.map(|body| body.to_string()),
    )
    .await
}

/// `role` in a `GET` answer.
fn role_in(listing: &Value, role: &str) -> Value {
    listing["roles"]
        .as_array()
        .expect("a list of roles")
        .iter()
        .find(|entry| entry["role"] == role)
        .unwrap_or_else(|| panic!("no role {role} in {listing}"))
        .clone()
}

/// An error answer with `code`: JSON, not cached, and its message quotes none of `quoted`.
fn assert_refused(reply: &Reply, status: StatusCode, code: &str, quoted: &[&str], what: &str) {
    assert_eq!(reply.status, status, "{what}: {}", reply.text());
    assert_eq!(reply.header("cache-control"), Some("no-store"), "{what}");
    assert_eq!(
        reply.header("content-type"),
        Some("application/json"),
        "{what}"
    );
    let body = reply.json();
    assert_eq!(body["error"]["code"], code, "{what}: {body}");
    let message = body["error"]["message"].as_str().expect("a message");
    for text in quoted {
        assert!(
            !message.contains(text),
            "{what}: the message quotes the request: {message}"
        );
    }
}

/// Providers `first` and `second` (Anthropic Messages) and `openai` (OpenAI Decisions); role
/// `main` on `first/model-one`, role `router` on `openai/gpt-6-luna`.
fn vars(database: &Database, first: &Server, second: &Server, openai: &Server) -> Vars {
    Vars::new()
        .database(database)
        .provider("first", "anthropic-messages", first)
        .provider("second", "anthropic-messages", second)
        .provider("openai", "openai-decisions", openai)
        .role("main", "first/model-one")
        .role("router", "openai/gpt-6-luna")
}

#[tokio::test]
async fn a_change_and_a_reset_through_the_admin_api() {
    let Some(database) = Database::new(
        "CREATE DATABASE ai_gateway_admin OWNER ai_gateway_owner",
        "ai_gateway_admin",
    )
    .await
    else {
        return;
    };
    let first = Server::start(vec![message_reply("req_1"), message_reply("req_3")]).await;
    let second = Server::start(vec![message_reply("req_2")]).await;
    let openai = Server::start(Vec::new()).await;
    let (gateway, router) = vars(&database, &first, &second, &openai).live();
    let admin_router = ai_gateway::admin_routes(Arc::clone(&gateway));
    let mut app = database.connect("ai_gateway_app").await;

    let reply = admin(&admin_router, "GET", ROLES_PATH, None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.header("cache-control"), Some("no-store"));
    let listing = reply.json();
    assert_eq!(
        role_in(&listing, "main"),
        json!({
            "role": "main",
            "protocol": "anthropic-messages",
            "models": ["first/model-one"],
            "configured_models": ["first/model-one"],
            "source": "configuration",
            "version": 0,
            "updated_at": null,
            "updated_by": null,
        })
    );
    assert_eq!(role_in(&listing, "router")["protocol"], "openai-decisions");
    assert_eq!(
        listing["providers"],
        json!([
            {"name": "first", "protocol": "anthropic-messages"},
            {"name": "openai", "protocol": "openai-decisions"},
            {"name": "second", "protocol": "anthropic-messages"},
        ])
    );
    let reply = post(&router, MESSAGES, &[], BODY).await;
    assert_eq!(reply.header(MODEL_HEADER), Some("first/model-one"));

    // A change: the answer is the role as it is now, and the next forwarded request uses it.
    let reply = admin(
        &admin_router,
        "PUT",
        MAIN,
        Some(json!({"models": ["second/model-two"], "expected_version": 0})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let changed = reply.json();
    assert_eq!(changed["models"], json!(["second/model-two"]), "{changed}");
    assert_eq!(changed["configured_models"], json!(["first/model-one"]));
    assert_eq!(changed["source"], "database");
    assert_eq!(changed["version"], 1);
    assert_eq!(changed["updated_by"], ACTOR);
    assert!(changed["updated_at"].is_string(), "{changed}");
    let reply = post(&router, MESSAGES, &[], BODY).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.header(MODEL_HEADER), Some("second/model-two"));
    assert_eq!(counts(&mut app).await, (1, 1, 1));

    // A stale version changes nothing and says where the role is.
    let reply = admin(
        &admin_router,
        "PUT",
        MAIN,
        Some(json!({"models": ["first/model-three"], "expected_version": 0})),
    )
    .await;
    assert_refused(
        &reply,
        StatusCode::CONFLICT,
        "version_conflict",
        &["model-three"],
        "a stale change",
    );
    assert_eq!(reply.json()["error"]["current_version"], 1);
    let reply = admin(
        &admin_router,
        "DELETE",
        &format!("{MAIN}?expected_version=0"),
        None,
    )
    .await;
    assert_refused(
        &reply,
        StatusCode::CONFLICT,
        "version_conflict",
        &[],
        "a stale reset",
    );
    assert_eq!(reply.json()["error"]["current_version"], 1);
    assert_eq!(counts(&mut app).await, (1, 1, 1));

    // A reset goes back to the configured models, with a new version.
    let reply = admin(
        &admin_router,
        "DELETE",
        &format!("{MAIN}?expected_version=1"),
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let reset = reply.json();
    assert_eq!(reset["models"], json!(["first/model-one"]), "{reset}");
    assert_eq!(reset["source"], "configuration");
    assert_eq!(reset["version"], 2);
    let listing = admin(&admin_router, "GET", ROLES_PATH, None).await.json();
    let main = role_in(&listing, "main");
    assert_eq!(main["models"], json!(["first/model-one"]), "{main}");
    assert_eq!(main["source"], "configuration");
    assert_eq!(main["version"], 2);
    assert_eq!(main["updated_by"], ACTOR);
    let reply = post(&router, MESSAGES, &[], BODY).await;
    assert_eq!(reply.header(MODEL_HEADER), Some("first/model-one"));
    assert_eq!(first.request_count(), 2);
    assert_eq!(second.request_count(), 1);

    // Resetting it again writes nothing.
    let reply = admin(
        &admin_router,
        "DELETE",
        &format!("{MAIN}?expected_version=2"),
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["version"], 2);
    assert_eq!(counts(&mut app).await, (2, 1, 2));

    let records: Vec<ChangeRecord> = sqlx::query_as(READ_CHANGES)
        .fetch_all(&mut app)
        .await
        .expect("read the change log");
    assert_eq!(
        records,
        [
            (
                1,
                "main".to_owned(),
                "set".to_owned(),
                Some(models(&["second/model-two"])),
                ACTOR.to_owned()
            ),
            (
                2,
                "main".to_owned(),
                "reset".to_owned(),
                None,
                ACTOR.to_owned()
            ),
        ]
    );
}

#[tokio::test]
async fn refused_requests_answer_their_code_and_change_nothing() {
    let Some(database) = Database::new(
        "CREATE DATABASE ai_gateway_admin_refused OWNER ai_gateway_owner",
        "ai_gateway_admin_refused",
    )
    .await
    else {
        return;
    };
    let first = Server::start(vec![message_reply("req_1")]).await;
    let second = Server::start(Vec::new()).await;
    let openai = Server::start(Vec::new()).await;
    let (gateway, router) = vars(&database, &first, &second, &openai).live();
    let admin_router = ai_gateway::admin_routes(Arc::clone(&gateway));
    let mut app = database.connect("ai_gateway_app").await;

    let change = |models: Value| Some(json!({"models": models, "expected_version": 0}));
    let nine: Vec<String> = (0..9).map(|index| format!("first/model-{index}")).collect();
    let malformed_query = format!("{MAIN}?expected_version=x7");
    let refused = [
        // Another protocol's model, either way round.
        (
            "PUT",
            MAIN,
            change(json!(["openai/gpt-6-luna"])),
            422,
            "protocol_mismatch",
            "gpt-6-luna",
        ),
        (
            "PUT",
            "/internal/admin/v1/roles/router",
            change(json!(["first/model-one"])),
            422,
            "protocol_mismatch",
            "model-one",
        ),
        (
            "PUT",
            MAIN,
            change(json!(["first/model-one", "openai/gpt-6-luna"])),
            422,
            "protocol_mismatch",
            "gpt-6-luna",
        ),
        // A provider that is not configured.
        (
            "PUT",
            MAIN,
            change(json!(["missing/model-x"])),
            422,
            "unknown_provider",
            "model-x",
        ),
        // Malformed lists: none, too many, twice the same, not <provider>/<model>, a space.
        (
            "PUT",
            MAIN,
            change(json!([])),
            422,
            "invalid_models",
            "first/",
        ),
        (
            "PUT",
            MAIN,
            change(json!(nine)),
            422,
            "invalid_models",
            "model-8",
        ),
        (
            "PUT",
            MAIN,
            change(json!(["first/model-y", "first/model-y"])),
            422,
            "invalid_models",
            "model-y",
        ),
        (
            "PUT",
            MAIN,
            change(json!(["model-z"])),
            422,
            "invalid_models",
            "model-z",
        ),
        (
            "PUT",
            MAIN,
            change(json!(["first/model w"])),
            422,
            "invalid_models",
            "model w",
        ),
        // A role the configuration does not have.
        (
            "PUT",
            "/internal/admin/v1/roles/other",
            change(json!(["first/model-one"])),
            404,
            "role_not_found",
            "other",
        ),
        (
            "DELETE",
            "/internal/admin/v1/roles/other?expected_version=0",
            None,
            404,
            "role_not_found",
            "other",
        ),
        // Malformed bodies and queries.
        (
            "PUT",
            MAIN,
            Some(json!(["first/model-one"])),
            400,
            "invalid_request",
            "model-one",
        ),
        (
            "PUT",
            MAIN,
            Some(json!({"models": ["first/model-one"], "expected_version": 0, "role": "x-extra"})),
            400,
            "invalid_request",
            "x-extra",
        ),
        (
            "PUT",
            MAIN,
            Some(json!({"models": ["first/model-one"], "expected_version": "zero"})),
            400,
            "invalid_request",
            "zero",
        ),
        (
            "PUT",
            MAIN,
            Some(json!({"models": ["first/model-one"], "expected_version": -1})),
            400,
            "invalid_request",
            "-1",
        ),
        ("DELETE", MAIN, None, 400, "invalid_request", "main"),
        (
            "DELETE",
            malformed_query.as_str(),
            None,
            400,
            "invalid_request",
            "x7",
        ),
        // No such admin route.
        (
            "GET",
            "/internal/admin/v1/settings",
            None,
            404,
            "not_found",
            "settings",
        ),
    ];
    for (method, path, body, status, code, quoted) in refused {
        let what = format!("{method} {path} {body:?}");
        let reply = admin(&admin_router, method, path, body).await;
        let status = StatusCode::from_u16(status).expect("a status");
        assert_refused(&reply, status, code, &[quoted], &what);
        assert_eq!(counts(&mut app).await, (0, 0, 0), "{what}");
    }

    // A body over the limit.
    let long = "m".repeat(ai_gateway::admin::MAX_BODY_BYTES);
    let reply = admin(
        &admin_router,
        "PUT",
        MAIN,
        Some(json!({"models": [format!("first/{long}")], "expected_version": 0})),
    )
    .await;
    assert_refused(
        &reply,
        StatusCode::PAYLOAD_TOO_LARGE,
        "request_too_large",
        &["mmm"],
        "a large body",
    );

    // The actor header: required on every request, once, 1 to 128 printable ASCII characters.
    let valid = json!({"models": ["second/model-two"], "expected_version": 0}).to_string();
    let actors: [(&str, Vec<HeaderValue>); 6] = [
        ("no actor", Vec::new()),
        ("an empty actor", vec![HeaderValue::from_static("")]),
        (
            "two actors",
            vec![
                HeaderValue::from_static("user:1"),
                HeaderValue::from_static("user:2"),
            ],
        ),
        (
            "a long actor",
            vec![HeaderValue::from_str(&"a".repeat(MAX_ACTOR_CHARS + 1)).expect("a value")],
        ),
        (
            "a non-ASCII actor",
            vec![HeaderValue::from_bytes(b"caf\xc3\xa9").expect("a value")],
        ),
        ("a tab", vec![HeaderValue::from_static("user\t1")]),
    ];
    for (what, actors) in &actors {
        for (method, path, body) in [
            ("PUT", MAIN.to_owned(), Some(valid.clone())),
            ("DELETE", format!("{MAIN}?expected_version=0"), None),
            ("GET", ROLES_PATH.to_owned(), None),
        ] {
            let reply = admin_with(&admin_router, method, &path, actors, body).await;
            assert_refused(
                &reply,
                StatusCode::BAD_REQUEST,
                "invalid_actor",
                &["user", "caf"],
                &format!("{method} with {what}"),
            );
        }
        assert_eq!(counts(&mut app).await, (0, 0, 0), "{what}");
    }

    // Nothing changed: GET and the next forwarded request still use the configured model.
    let listing = admin(&admin_router, "GET", ROLES_PATH, None).await.json();
    let main = role_in(&listing, "main");
    assert_eq!(main["models"], json!(["first/model-one"]), "{main}");
    assert_eq!(main["source"], "configuration");
    assert_eq!(main["version"], 0);
    let reply = post(&router, MESSAGES, &[], BODY).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.header(MODEL_HEADER), Some("first/model-one"));
    assert_eq!(second.request_count(), 0);
    assert_eq!(counts(&mut app).await, (0, 0, 0));
}

#[tokio::test]
async fn the_admin_api_answers_503_while_the_database_cannot_be_reached() {
    // The database settings point at a closed port: nothing is ever written.
    let first = Server::start(Vec::new()).await;
    let (gateway, _router) = Vars::new()
        .provider("first", "anthropic-messages", &first)
        .role("main", "first/model-one")
        .live();
    let admin_router = ai_gateway::admin_routes(gateway);
    for (method, path, body) in [
        ("GET", ROLES_PATH.to_owned(), None),
        (
            "PUT",
            MAIN.to_owned(),
            Some(json!({"models": ["first/model-two"], "expected_version": 0})),
        ),
        ("DELETE", format!("{MAIN}?expected_version=0"), None),
    ] {
        let reply = admin(&admin_router, method, &path, body).await;
        assert_refused(
            &reply,
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            &["model-two"],
            method,
        );
    }
    assert_eq!(first.request_count(), 0);
}

#[tokio::test]
async fn the_admin_and_forwarding_routers_share_no_route() {
    let first = Server::start(Vec::new()).await;
    let vars = Vars::new()
        .provider("first", "anthropic-messages", &first)
        .role("main", "first/model-one");
    let (gateway, router) = vars.live();
    let admin_router = ai_gateway::admin_routes(gateway);

    // The forwarding routes are not on the admin router.
    let reply = post(&admin_router, MESSAGES, &[], BODY).await;
    assert_refused(
        &reply,
        StatusCode::NOT_FOUND,
        "not_found",
        &["Hi"],
        "messages",
    );
    let reply = post(
        &admin_router,
        "/internal/roles/main/v1/decisions",
        &[],
        "{}",
    )
    .await;
    assert_refused(&reply, StatusCode::NOT_FOUND, "not_found", &[], "decisions");

    // The admin routes are not on the forwarding router.
    for (method, path) in [("GET", ROLES_PATH), ("PUT", MAIN), ("DELETE", MAIN)] {
        let reply = admin_with(
            &router,
            method,
            path,
            &[HeaderValue::from_static(ACTOR)],
            Some(json!({"models": ["first/model-two"], "expected_version": 0}).to_string()),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {path}");
    }
    assert_eq!(first.request_count(), 0);
}
