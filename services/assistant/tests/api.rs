//! The conversation threads' API (Laysi/trustiot-v2#645) through the assistant binary as the
//! chart starts it: the real router behind the real identity verifier, which checks tokens
//! signed by a test issuer (`testkit::identity`) against the keys file, over the product's
//! schema applied by the migration Job (`testkit::product`).
//!
//! - Without a valid token for the assistant every endpoint answers 401, without a tenant 403.
//! - A conversation is its owner's alone: another user of the tenant, an administrator
//!   included, lists none of it, and every request by ID gets the same bytes as for an ID that
//!   does not exist; nothing changes.
//! - Reading a conversation gives the questions and the answers' text, nothing of the thinking,
//!   the tool calls and results, the system prompt, the tool definitions or the tenant context.
//! - Renaming, archiving, the list's filter and pages, and the 400s and 422s of each.
//! - Delete, the deleted list and restore; a running question keeps its conversation (409);
//!   past 30 days a conversation cannot be restored, and a purge round removes it with its
//!   messages while a recently deleted and a live one stay.
//! - Writes need the JSON content type; every response carries `cache-control: no-store`.
//! - A database failure answers 503 with nothing of the database's text, and the log has none of
//!   it either: only the SQLSTATE.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

mod service;
mod support;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use assistant::store::{self, RunOutcome};
use assistant::tools::{NoDelegation, Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use chrono::{DateTime, TimeDelta, Utc};
use contract::identity::TenantRole;
use contract::{ServiceName, TenantId};
use reqwest::Method;
use serde_json::{Value, json};
use service::{Reply, Service, User};
use service_kit::tenant::{PlatformDb, TenantDb};
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, Connection, Row};
use support::{StandIn, fixture, ok, respond, settings, tool_step};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};
use uuid::Uuid;

const CONVERSATIONS: &str = "/api/assistant/v1/conversations";
const DELETED: &str = "/api/assistant/v1/deleted-conversations";

/// Everything the database holds of one conversation, as the superuser reads it: the row and
/// every row of its messages, discarded rounds and runs, as JSON.
const SNAPSHOT: &str = "SELECT
    (SELECT row_to_json(c)::text FROM assistant.conversations c
     WHERE c.conversation_id = $1::uuid) AS conversation,
    (SELECT coalesce(json_agg(m ORDER BY m.seq)::text, '[]') FROM assistant.messages m
     WHERE m.conversation_id = $1::uuid) AS messages,
    (SELECT coalesce(json_agg(d ORDER BY d.from_seq)::text, '[]')
     FROM assistant.discarded_rounds d WHERE d.conversation_id = $1::uuid) AS discards,
    (SELECT coalesce(json_agg(r ORDER BY r.started_at, r.run_id)::text, '[]')
     FROM assistant.runs r WHERE r.conversation_id = $1::uuid) AS runs";

/// What the database holds of a conversation as text: its row (the preamble included) and its
/// messages' bodies.
const STORED_TEXT: &str = "SELECT
    (SELECT row_to_json(c)::text FROM assistant.conversations c
     WHERE c.conversation_id = $1::uuid)
    || (SELECT coalesce(string_agg(convert_from(m.body, 'UTF8'), ' ' ORDER BY m.seq), '')
        FROM assistant.messages m WHERE m.conversation_id = $1::uuid)";

/// The superuser moves a deletion back in time, past the clock trigger (which replica mode
/// skips): `$1` conversation, `$2` the new `deleted_at`.
const BACKDATE: &str = "UPDATE assistant.conversations SET deleted_at = $2
WHERE conversation_id = $1::uuid";

/// The superuser gives conversations one `last_activity_at`, so the list's order falls back to
/// the ID, past the clock trigger (which replica mode skips): `$1` the conversations, `$2` the
/// time.
const SAME_ACTIVITY: &str = "UPDATE assistant.conversations SET last_activity_at = $2
WHERE conversation_id::text = ANY ($1)";

/// Strings planted where the chat must not show them.
const THINKING_MARKER: &str = "THINKING-MARKER-5e1f";
const TOOL_INPUT_MARKER: &str = "TOOL-INPUT-MARKER-8a2c";
const TOOL_RESULT_MARKER: &str = "TOOL-RESULT-MARKER-3b7d";
const TOOL_DEFINITION_MARKER: &str = "TOOL-DEFINITION-MARKER-c4e9";
const TENANT_CONTEXT_MARKER: &str = "TENANT-CONTEXT-MARKER-91f0";
const ANSWER_THINKING_MARKER: &str = "ANSWER-THINKING-MARKER-6d3a";
const QUESTION: &str = "How did floor 3 use energy this week?";
const ANSWER: &str = "Floor 3 used 412 kWh this week, 6% less than last week.";

struct Api {
    product: Arc<ProductPostgres>,
    issuer: TestIssuer,
    service: Service,
    /// The service role's pool, for what the API cannot do yet: storing a conversation with
    /// questions (Laysi/trustiot-v2#646) and starting a question's run.
    db: TenantDb,
    /// The purge's platform role, to run a purge round now instead of waiting for the schedule.
    platform: PlatformDb,
}

async fn api() -> Option<Api> {
    let product = product_postgres(&["assistant_app", "assistant_platform"]).await?;
    let issuer = TestIssuer::new();
    let service = Service::start(&product, &issuer.jwks(), &[]).await;
    let db = TenantDb::connect_lazy(
        product.postgres.options("assistant_app", DATABASE),
        PgPoolOptions::new().max_connections(2),
    )
    .expect("the service role's pool verifies the server");
    let platform = PlatformDb::connect_lazy(
        product.postgres.options("assistant_platform", DATABASE),
        PgPoolOptions::new().max_connections(1),
    )
    .expect("the platform role's pool verifies the server");
    Some(Api {
        product,
        issuer,
        service,
        db,
        platform,
    })
}

fn conversation(id: impl std::fmt::Display) -> String {
    format!("{CONVERSATIONS}/{id}")
}

fn restore_path(id: impl std::fmt::Display) -> String {
    format!("{CONVERSATIONS}/{id}/restore")
}

impl Api {
    async fn call(&self, user: &User, method: Method, path: &str, body: Option<&Value>) -> Reply {
        self.service
            .as_user(&self.issuer, user, method, path, body)
            .await
    }

    /// Starts a conversation through the API and returns its ID.
    async fn create(&self, user: &User) -> Uuid {
        let reply = self
            .call(user, Method::POST, CONVERSATIONS, Some(&json!({})))
            .await;
        reply.expect_status(201);
        reply.json()["id"]
            .as_str()
            .and_then(|id| id.parse().ok())
            .unwrap_or_else(|| panic!("no id in {}", reply.body))
    }

    /// The IDs of a list as `user` reads it in one page.
    async fn ids(&self, user: &User, path: &str) -> Vec<Uuid> {
        let reply = self.call(user, Method::GET, path, None).await;
        reply.expect_status(200);
        ids_of(&reply.json())
    }

    /// A conversation of `user` with one answered question, made by the real agent against a
    /// stand-in model and stored as the chat API of Laysi/trustiot-v2#646 will store it.
    async fn with_a_question(&self, user: &User) -> Uuid {
        let stand_in = StandIn::start(respond(|_request, index| {
            if index == 0 {
                ok(tool_step(index))
            } else {
                ok(fixture("answer_end_turn.json"))
            }
        }))
        .await;
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(support::CountReadings)];
        let agent = support::agent(&stand_in, tools, settings());
        let mut conversation = agent.start_conversation(user.principal(), "");
        agent
            .ask(&mut conversation, QUESTION, Arc::new(NoDelegation))
            .await
            .expect("an answer");
        self.store(user, &conversation).await;
        conversation.id()
    }

    async fn store(&self, user: &User, conversation: &assistant::history::Conversation) {
        let mut tx = self
            .db
            .begin(&user.identity(&self.issuer))
            .await
            .expect("a tenant transaction");
        store::create(&mut tx, conversation)
            .await
            .expect("store the conversation");
        tx.commit().await.expect("commit");
    }

    async fn snapshot(&self, id: Uuid) -> String {
        let mut admin = self.product.superuser().await;
        let row = sqlx::query(SNAPSHOT)
            .bind(id.to_string())
            .fetch_one(&mut admin)
            .await
            .expect("read the conversation's rows");
        ["conversation", "messages", "discards", "runs"]
            .map(|column| row.get::<Option<String>, _>(column).unwrap_or_default())
            .join("\n")
    }

    async fn stored_text(&self, id: Uuid) -> String {
        let mut admin = self.product.superuser().await;
        sqlx::query_scalar(STORED_TEXT)
            .bind(id.to_string())
            .fetch_one(&mut admin)
            .await
            .expect("read the stored conversation")
    }

    async fn backdate(&self, id: Uuid, deleted_at: DateTime<Utc>) {
        let mut admin = self.product.superuser().await;
        let mut tx = admin.begin().await.expect("a superuser transaction");
        sqlx::query("SET LOCAL session_replication_role = replica")
            .execute(&mut *tx)
            .await
            .expect("skip the clock trigger");
        let updated = sqlx::query(BACKDATE)
            .bind(id.to_string())
            .bind(deleted_at)
            .execute(&mut *tx)
            .await
            .expect("move the deletion back")
            .rows_affected();
        assert_eq!(updated, 1, "the conversation to move back exists");
        tx.commit().await.expect("commit");
    }
}

fn ids_of(page: &Value) -> Vec<Uuid> {
    page["conversations"]
        .as_array()
        .unwrap_or_else(|| panic!("no conversations in {page}"))
        .iter()
        .map(|item| {
            item["id"]
                .as_str()
                .and_then(|id| id.parse().ok())
                .unwrap_or_else(|| panic!("no id in {item}"))
        })
        .collect()
}

fn time(value: &Value) -> DateTime<Utc> {
    value
        .as_str()
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|time| time.with_timezone(&Utc))
        .unwrap_or_else(|| panic!("not a time: {value}"))
}

/// The parts of a response a client can see, to compare two of them.
fn seen(reply: &Reply) -> (u16, String, Option<String>, Option<String>) {
    (
        reply.status,
        reply.body.clone(),
        reply.header("content-type").map(str::to_owned),
        reply.header("cache-control").map(str::to_owned),
    )
}

/// Every request by ID, as `(method, path, body)` for conversation `id`.
fn requests_by_id(id: impl std::fmt::Display + Copy) -> Vec<(Method, String, Option<Value>)> {
    vec![
        (Method::GET, conversation(id), None),
        (
            Method::PATCH,
            conversation(id),
            Some(json!({"title": "Taken over"})),
        ),
        (
            Method::PATCH,
            conversation(id),
            Some(json!({"archived": true})),
        ),
        (Method::DELETE, conversation(id), None),
        (Method::POST, restore_path(id), Some(json!({}))),
    ]
}

/// Acceptance criterion: no valid token for the assistant, 401; no tenant, 403; on every
/// endpoint, with `cache-control: no-store`, and nothing is written.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_needs_a_token_for_the_assistant_and_a_tenant() {
    let Some(api) = api().await else {
        return;
    };
    let user = User::new(TenantId::generate(), TenantRole::Admin);
    let id = Uuid::now_v7();
    let mut endpoints = vec![
        (Method::GET, CONVERSATIONS.to_owned(), None),
        (Method::POST, CONVERSATIONS.to_owned(), Some(json!({}))),
        (Method::GET, DELETED.to_owned(), None),
    ];
    endpoints.extend(requests_by_id(id));

    let stranger = TestIssuer::new();
    let mut no_tenant = TestIssuer::claims(ServiceName::Assistant, None);
    no_tenant.sub = user.id;
    let tokens = [
        ("no header", None, 401, "unauthenticated"),
        (
            "another issuer's signature",
            Some(stranger.token(&user.claims(ServiceName::Assistant))),
            401,
            "unauthenticated",
        ),
        (
            "a token for the assets service",
            Some(api.issuer.token(&user.claims(ServiceName::Assets))),
            401,
            "unauthenticated",
        ),
        (
            "no tenant",
            Some(api.issuer.token(&no_tenant)),
            403,
            "permission_denied",
        ),
    ];
    for (what, token, status, code) in &tokens {
        for (method, path, body) in &endpoints {
            let body = body.as_ref().map(|body| {
                (
                    "application/json",
                    serde_json::to_vec(body).expect("serialize"),
                )
            });
            let reply = api
                .service
                .call(method.clone(), path, token.as_deref(), body)
                .await;
            assert_eq!(
                reply.status, *status,
                "{what}: {method} {path}: {}",
                reply.body
            );
            assert_eq!(reply.json()["code"], *code, "{what}: {method} {path}");
            assert_eq!(
                reply.header("cache-control"),
                Some("no-store"),
                "{what}: {method} {path}"
            );
        }
    }
    let mut admin = api.product.superuser().await;
    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM assistant.conversations WHERE user_id = $1::uuid")
            .bind(user.id.to_string())
            .fetch_one(&mut admin)
            .await
            .expect("count");
    assert_eq!(stored, 0, "nothing was created");
    println!(
        "{} endpoints: no header, another issuer, the assets audience 401; no tenant 403",
        endpoints.len()
    );
}

/// Acceptance criterion: another user of the tenant, an administrator included, lists none of
/// the owner's conversations, and every request by ID answers exactly what an ID that does not
/// exist gets; nothing changes.
#[tokio::test(flavor = "multi_thread")]
async fn another_user_of_the_tenant_gets_what_a_missing_id_gets() {
    let Some(api) = api().await else {
        return;
    };
    let tenant = TenantId::generate();
    let owner = User::new(tenant, TenantRole::Viewer);
    let live = api.with_a_question(&owner).await;
    api.call(
        &owner,
        Method::PATCH,
        &conversation(live),
        Some(&json!({"title": "Floor 3 energy"})),
    )
    .await
    .expect_status(200);
    let archived = api.create(&owner).await;
    api.call(
        &owner,
        Method::PATCH,
        &conversation(archived),
        Some(&json!({"archived": true})),
    )
    .await
    .expect_status(200);
    let deleted = api.create(&owner).await;
    api.call(&owner, Method::DELETE, &conversation(deleted), None)
        .await
        .expect_status(200);
    let before = [
        api.snapshot(live).await,
        api.snapshot(archived).await,
        api.snapshot(deleted).await,
    ];

    for colleague in [
        User::new(tenant, TenantRole::Viewer),
        User::new(tenant, TenantRole::Editor),
        User::new(tenant, TenantRole::Admin),
    ] {
        let role = colleague.role;
        for list in [
            CONVERSATIONS.to_owned(),
            format!("{CONVERSATIONS}?archived=true"),
            DELETED.to_owned(),
        ] {
            assert!(
                api.ids(&colleague, &list).await.is_empty(),
                "{role:?}: {list}"
            );
        }
        let missing = Uuid::now_v7();
        for id in [live, archived, deleted] {
            for ((method, path, body), (_, missing_path, missing_body)) in
                requests_by_id(id).into_iter().zip(requests_by_id(missing))
            {
                let theirs = api
                    .call(&colleague, method.clone(), &path, body.as_ref())
                    .await;
                let nobodys = api
                    .call(
                        &colleague,
                        method.clone(),
                        &missing_path,
                        missing_body.as_ref(),
                    )
                    .await;
                assert_eq!(theirs.status, 404, "{role:?}: {method} {path}");
                assert_eq!(seen(&theirs), seen(&nobodys), "{role:?}: {method} {path}");
            }
        }
    }
    let after = [
        api.snapshot(live).await,
        api.snapshot(archived).await,
        api.snapshot(deleted).await,
    ];
    assert_eq!(after, before, "nothing changed");
    api.call(&owner, Method::GET, &conversation(live), None)
        .await
        .expect_status(200);
    let body = api
        .call(
            &User::new(tenant, TenantRole::Admin),
            Method::GET,
            &conversation(live),
            None,
        )
        .await
        .body;
    println!("another user's GET of the owner's conversation: 404 {body}");
}

/// The tool of the conversation whose chat is checked: its definition and its result carry
/// markers.
struct MarkerTool;

impl Tool for MarkerTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "energy_report".into(),
            description: format!("Reports a floor's energy use. {TOOL_DEFINITION_MARKER}"),
            input_schema: json!({
                "type": "object",
                "properties": {"floor": {"type": "string"}},
                "required": ["floor"]
            }),
        }
    }

    fn call(
        self: Arc<Self>,
        _context: ToolContext,
        _input: Value,
    ) -> assistant::model::BoxFuture<'static, Result<ToolOutput, ToolError>> {
        Box::pin(async { Ok(ToolOutput::text(format!("412 kWh. {TOOL_RESULT_MARKER}"))) })
    }
}

/// The model's tool step: thinking, then a call of [`MarkerTool`].
fn marker_step() -> String {
    json!({
        "id": "msg_step",
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": format!("Checking the meters. {THINKING_MARKER}"),
             "signature": "c2lnbmF0dXJl"},
            {"type": "tool_use", "id": "toolu_1", "name": "energy_report",
             "input": {"floor": format!("3 {TOOL_INPUT_MARKER}")}}
        ],
        "model": "claude-opus-5-5",
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 300, "output_tokens": 100}
    })
    .to_string()
}

/// The model's answer: thinking, then the text.
fn marker_answer() -> String {
    json!({
        "id": "msg_answer",
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": format!("Summarising. {ANSWER_THINKING_MARKER}"),
             "signature": "c2lnbmF0dXJlMg=="},
            {"type": "text", "text": ANSWER}
        ],
        "model": "claude-opus-5-5",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 400, "output_tokens": 50}
    })
    .to_string()
}

/// Acceptance criterion: reading a conversation gives only the questions and the answers' text.
/// The real agent asks a stand-in model that thinks, calls a tool and answers; the stored
/// history holds every marker, the response none of them.
#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_shows_only_its_questions_and_answers() {
    let Some(api) = api().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let stand_in = StandIn::start(respond(|_request, index| {
        if index == 0 {
            ok(marker_step())
        } else {
            ok(marker_answer())
        }
    }))
    .await;
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(MarkerTool)];
    let agent = support::agent(&stand_in, tools, settings());
    let mut chat = agent.start_conversation(
        owner.principal(),
        &format!("Plant A. {TENANT_CONTEXT_MARKER}"),
    );
    agent
        .ask(&mut chat, QUESTION, Arc::new(NoDelegation))
        .await
        .expect("an answer");
    api.store(&owner, &chat).await;
    let id = chat.id();

    // The markers are stored: the response leaves them out, not the history.
    let stored = api.stored_text(id).await;
    for marker in [
        THINKING_MARKER,
        TOOL_INPUT_MARKER,
        TOOL_RESULT_MARKER,
        TOOL_DEFINITION_MARKER,
        TENANT_CONTEXT_MARKER,
        ANSWER_THINKING_MARKER,
    ] {
        assert!(stored.contains(marker), "{marker} is stored");
    }

    let reply = api.call(&owner, Method::GET, &conversation(id), None).await;
    reply.expect_status(200);
    assert_eq!(reply.header("cache-control"), Some("no-store"));
    let detail = reply.json();
    assert_eq!(
        detail["messages"],
        json!([
            {"kind": "question", "text": QUESTION},
            {"kind": "answer", "text": ANSWER}
        ]),
        "{detail}"
    );
    let system_prompt = assistant::prompt::system_prompt();
    let excerpt: String = system_prompt.text.chars().take(60).collect();
    for absent in [
        THINKING_MARKER,
        TOOL_INPUT_MARKER,
        TOOL_RESULT_MARKER,
        TOOL_DEFINITION_MARKER,
        TENANT_CONTEXT_MARKER,
        ANSWER_THINKING_MARKER,
        "energy_report",
        "signature",
        &excerpt,
        &system_prompt.version,
    ] {
        assert!(!reply.body.contains(absent), "{absent} in {}", reply.body);
    }
    let keys: BTreeSet<&str> = detail
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "id",
            "title",
            "title_source",
            "created_at",
            "last_activity_at",
            "archived_at",
            "messages"
        ])
    );
    println!("GET {}: {}", conversation(id), reply.body);
}

/// Acceptance criterion: renaming (trimmed; 400 and 422 for what the rules refuse), archiving
/// and unarchiving, the list without archived conversations by default and only them with
/// `archived=true`, and walking pages with the cursor across ties without duplicates or gaps;
/// a bad cursor or limit is a 400.
#[tokio::test(flavor = "multi_thread")]
async fn rename_archive_and_the_pages_of_the_list() {
    let Some(api) = api().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let first = api.create(&owner).await;
    let second = api.create(&owner).await;
    let third = api.create(&owner).await;

    // Rename: trimmed, the user's title.
    let renamed = api
        .call(
            &owner,
            Method::PATCH,
            &conversation(first),
            Some(&json!({"title": "  Monthly energy\u{3000} "})),
        )
        .await;
    renamed.expect_status(200);
    assert_eq!(renamed.json()["title"], "Monthly energy");
    assert_eq!(renamed.json()["title_source"], "user");
    assert_eq!(renamed.header("cache-control"), Some("no-store"));

    // What the rules refuse; nothing changes.
    let path = conversation(second);
    for (content_type, body, status, code, key) in [
        (None, None, 400, "invalid_request", "error.invalid_request"),
        (
            Some("text/plain"),
            Some(r#"{"title":"x"}"#),
            400,
            "invalid_request",
            "error.invalid_request",
        ),
        (
            Some("application/json"),
            Some("{}"),
            400,
            "invalid_request",
            "conversation.update_empty",
        ),
        (
            Some("application/json"),
            Some(r#"{"title":"x","pinned":true}"#),
            400,
            "invalid_request",
            "conversation.body_invalid",
        ),
        (
            Some("application/json"),
            Some(r#"{"archived":"yes"}"#),
            400,
            "invalid_request",
            "conversation.body_invalid",
        ),
        (
            Some("application/json"),
            Some("not json"),
            400,
            "invalid_request",
            "conversation.body_invalid",
        ),
    ] {
        let token = owner.token(&api.issuer);
        let body = content_type
            .zip(body)
            .map(|(kind, text)| (kind, text.as_bytes().to_vec()));
        api.service
            .call(Method::PATCH, &path, Some(&token), body)
            .await
            .expect_error(status, code, key);
    }
    let too_long = "字".repeat(201);
    for title in ["", "   ", too_long.as_str(), "line\nbreak", "bell\u{7}"] {
        let reply = api
            .call(
                &owner,
                Method::PATCH,
                &path,
                Some(&json!({"title": title, "archived": true})),
            )
            .await;
        reply.expect_error(422, "validation_failed", "error.validation_failed");
        assert_eq!(
            reply.json()["field_errors"],
            json!([{"field": "/title", "message_key": "conversation.title_invalid",
                    "details": {"max": 200}}]),
            "{title:?}"
        );
    }
    let path = conversation(second);
    let unchanged = api.call(&owner, Method::GET, &path, None).await;
    assert_eq!(unchanged.json()["title"], Value::Null);
    assert_eq!(
        unchanged.json()["archived_at"],
        Value::Null,
        "the refused updates archived nothing"
    );
    api.call(&owner, Method::GET, &conversation("not-a-uuid"), None)
        .await
        .expect_error(404, "not_found", "error.not_found");

    // Archive: out of the default list, in the archived one.
    let archived = api
        .call(
            &owner,
            Method::PATCH,
            &path,
            Some(&json!({"archived": true})),
        )
        .await;
    archived.expect_status(200);
    assert!(archived.json()["archived_at"].is_string());
    assert_eq!(api.ids(&owner, CONVERSATIONS).await, [third, first]);
    assert_eq!(
        api.ids(&owner, &format!("{CONVERSATIONS}?archived=false"))
            .await,
        [third, first]
    );
    assert_eq!(
        api.ids(&owner, &format!("{CONVERSATIONS}?archived=true"))
            .await,
        [second]
    );
    api.call(
        &owner,
        Method::GET,
        &format!("{CONVERSATIONS}?archived=yes"),
        None,
    )
    .await
    .expect_error(400, "invalid_request", "error.invalid_request");
    // Unarchive and rename in one request.
    let back = api
        .call(
            &owner,
            Method::PATCH,
            &path,
            Some(&json!({"archived": false, "title": "Back again"})),
        )
        .await;
    back.expect_status(200);
    assert_eq!(back.json()["archived_at"], Value::Null);
    assert_eq!(back.json()["title"], "Back again");
    assert_eq!(api.ids(&owner, CONVERSATIONS).await, [third, second, first]);
    assert!(
        api.ids(&owner, &format!("{CONVERSATIONS}?archived=true"))
            .await
            .is_empty()
    );

    // Pages: twelve conversations, five of them with the same last activity, read three at a
    // time.
    let mut all = vec![first, second, third];
    for _ in 0..9 {
        all.push(api.create(&owner).await);
    }
    let tied: Vec<String> = all[3..8].iter().map(Uuid::to_string).collect();
    let mut admin = api.product.superuser().await;
    let mut tx = admin.begin().await.expect("a superuser transaction");
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .expect("skip the clock trigger");
    sqlx::query(SAME_ACTIVITY)
        .bind(&tied)
        .bind(Utc::now())
        .execute(&mut *tx)
        .await
        .expect("tie the last activity");
    tx.commit().await.expect("commit");
    let whole = api.ids(&owner, &format!("{CONVERSATIONS}?limit=100")).await;
    assert_eq!(
        whole.iter().collect::<BTreeSet<_>>(),
        all.iter().collect::<BTreeSet<_>>()
    );
    let mut walked = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let path = match &cursor {
            None => format!("{CONVERSATIONS}?limit=5"),
            Some(cursor) => format!("{CONVERSATIONS}?limit=5&cursor={}", encode(cursor)),
        };
        let page = api.call(&owner, Method::GET, &path, None).await;
        page.expect_status(200);
        let page = page.json();
        let ids = ids_of(&page);
        assert!(ids.len() <= 5);
        walked.extend(ids);
        pages += 1;
        match page["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
        assert!(pages < 10, "the walk ends");
    }
    assert_eq!(
        walked, whole,
        "every conversation once, in the list's order"
    );
    assert_eq!(pages, 3);

    // Bad limits and cursors.
    for limit in ["0", "101", "ten", "-1"] {
        let reply = api
            .call(
                &owner,
                Method::GET,
                &format!("{CONVERSATIONS}?limit={limit}"),
                None,
            )
            .await;
        reply.expect_error(400, "invalid_request", "conversation.limit_invalid");
        assert_eq!(reply.json()["details"], json!({"max": 100}));
    }
    let active = api
        .call(
            &owner,
            Method::GET,
            &format!("{CONVERSATIONS}?limit=1"),
            None,
        )
        .await
        .json()["next_cursor"]
        .as_str()
        .expect("a next page")
        .to_owned();
    for extra in [first, second] {
        api.call(&owner, Method::DELETE, &conversation(extra), None)
            .await
            .expect_status(200);
    }
    let deleted = api
        .call(&owner, Method::GET, &format!("{DELETED}?limit=1"), None)
        .await
        .json()["next_cursor"]
        .as_str()
        .expect("a next page of the deleted list")
        .to_owned();
    for (list, cursor) in [
        (CONVERSATIONS.to_owned(), "garbage".to_owned()),
        (CONVERSATIONS.to_owned(), deleted.clone()),
        (format!("{CONVERSATIONS}?archived=true&"), active.clone()),
        (DELETED.to_owned(), active.clone()),
        (CONVERSATIONS.to_owned(), active.replace('Z', "+00:00")),
    ] {
        let separator = if list.ends_with('&') { "" } else { "?" };
        let path = format!("{list}{separator}cursor={}", encode(&cursor));
        api.call(&owner, Method::GET, &path, None)
            .await
            .expect_error(400, "invalid_request", "conversation.cursor_invalid");
    }
    // The cursors themselves work on their own lists.
    api.call(
        &owner,
        Method::GET,
        &format!("{CONVERSATIONS}?cursor={}", encode(&active)),
        None,
    )
    .await
    .expect_status(200);
    api.call(
        &owner,
        Method::GET,
        &format!("{DELETED}?cursor={}", encode(&deleted)),
        None,
    )
    .await
    .expect_status(200);
    // No title reaches the log.
    assert!(!api.service.stdout.text().contains("Monthly energy"));
    assert!(!api.service.stdout.text().contains("Back again"));
    println!(
        "walked {} conversations in {pages} pages of 5 (5 tied); cursor example: {active}",
        walked.len()
    );
}

/// A query value, percent-encoded.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// Acceptance criteria: a deleted conversation leaves the lists and is read as missing, is in
/// the deleted list until 30 days after, and comes back unchanged when restored; a running
/// question keeps its conversation (409, nothing changes); past 30 days it cannot be restored.
#[tokio::test(flavor = "multi_thread")]
async fn delete_the_deleted_list_and_restore() {
    let Some(api) = api().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Editor);
    let id = api.with_a_question(&owner).await;
    let other = api.create(&owner).await;
    let read = api.call(&owner, Method::GET, &conversation(id), None).await;
    read.expect_status(200);
    let before = read.json();
    assert_eq!(before["messages"].as_array().map(Vec::len), Some(2));

    // A running question keeps it.
    let identity = owner.identity(&api.issuer);
    let mut tx = api.db.begin(&identity).await.expect("a tenant transaction");
    let run = store::start_run(
        &mut tx,
        owner.principal(),
        id,
        Uuid::now_v7(),
        Duration::from_secs(600),
    )
    .await
    .expect("a question runs");
    tx.commit().await.expect("commit");
    let running = api.snapshot(id).await;
    api.call(&owner, Method::DELETE, &conversation(id), None)
        .await
        .expect_error(409, "conflict", "conversation.question_running");
    assert_eq!(api.snapshot(id).await, running, "nothing changed");
    let mut tx = api.db.begin(&identity).await.expect("a tenant transaction");
    store::finish_run(&mut tx, owner.principal(), run.run_id, RunOutcome::Answered)
        .await
        .expect("the question ends");
    tx.commit().await.expect("commit");
    let rows_before = api.snapshot(id).await;

    // Deleted: gone from the list, read as missing, in the deleted list.
    let deleted = api
        .call(&owner, Method::DELETE, &conversation(id), None)
        .await;
    deleted.expect_status(200);
    assert_eq!(deleted.header("cache-control"), Some("no-store"));
    let deleted = deleted.json();
    assert_eq!(deleted["id"], id.to_string());
    assert_eq!(
        time(&deleted["restorable_until"]) - time(&deleted["deleted_at"]),
        TimeDelta::days(30)
    );
    assert_eq!(api.ids(&owner, CONVERSATIONS).await, [other]);
    for (method, path, body) in requests_by_id(id).into_iter().take(4) {
        api.call(&owner, method, &path, body.as_ref())
            .await
            .expect_error(404, "not_found", "error.not_found");
    }
    let listed = api.call(&owner, Method::GET, DELETED, None).await;
    listed.expect_status(200);
    assert_eq!(
        listed.json()["conversations"],
        json!([{
            "id": id.to_string(),
            "title": Value::Null,
            "deleted_at": deleted["deleted_at"],
            "restorable_until": deleted["restorable_until"],
        }])
    );

    // Restored: as it was.
    let restored = api
        .call(&owner, Method::POST, &restore_path(id), None)
        .await;
    restored.expect_error(400, "invalid_request", "error.invalid_request");
    let restored = api
        .call(&owner, Method::POST, &restore_path(id), Some(&json!({})))
        .await;
    restored.expect_status(200);
    assert_eq!(restored.json()["id"], id.to_string());
    let after = api
        .call(&owner, Method::GET, &conversation(id), None)
        .await
        .json();
    assert_eq!(after, before, "the conversation is back as it was");
    assert_eq!(api.snapshot(id).await, rows_before, "every row as it was");
    // Back in its place: its last activity is its last question, before `other` started.
    assert_eq!(api.ids(&owner, CONVERSATIONS).await, [other, id]);
    assert!(api.ids(&owner, DELETED).await.is_empty());
    api.call(&owner, Method::POST, &restore_path(id), Some(&json!({})))
        .await
        .expect_error(404, "not_found", "error.not_found");

    // Past 30 days: not in the deleted list, not restorable.
    api.call(&owner, Method::DELETE, &conversation(id), None)
        .await
        .expect_status(200);
    api.backdate(id, Utc::now() - TimeDelta::days(31)).await;
    assert!(api.ids(&owner, DELETED).await.is_empty());
    api.call(&owner, Method::POST, &restore_path(id), Some(&json!({})))
        .await
        .expect_error(404, "not_found", "error.not_found");

    // A purge round removes it with its messages; a recently deleted one and a live one stay.
    // Every service of this test binary also runs a round when it starts, and that may be the
    // one that removes it, so the state is checked, not the round's count.
    let recent = api.create(&owner).await;
    api.call(&owner, Method::DELETE, &conversation(recent), None)
        .await
        .expect_status(200);
    let (recent_before, other_before) = (api.snapshot(recent).await, api.snapshot(other).await);
    assistant::purge::round(&api.platform)
        .await
        .expect("a purge round");
    assert_eq!(
        api.snapshot(id).await,
        "\n[]\n[]\n[]",
        "the conversation and every row of it are gone"
    );
    assert_eq!(api.snapshot(recent).await, recent_before);
    assert_eq!(api.snapshot(other).await, other_before);
    assert_eq!(api.ids(&owner, DELETED).await, [recent]);
    assert_eq!(api.ids(&owner, CONVERSATIONS).await, [other]);
    println!("deleted: {deleted}");
}

/// Starting a conversation: the JSON content type, no body or an empty object; 201 with its
/// metadata and `Location`. The log line names the conversation, never content.
#[tokio::test(flavor = "multi_thread")]
async fn starting_a_conversation() {
    let Some(api) = api().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let token = owner.token(&api.issuer);
    for (content_type, body, key) in [
        (None, None, "error.invalid_request"),
        (Some("text/plain"), Some("{}"), "error.invalid_request"),
        (
            Some("application/x-www-form-urlencoded"),
            Some("a=b"),
            "error.invalid_request",
        ),
        (
            Some("application/json"),
            Some(r#"{"title":"x"}"#),
            "conversation.body_invalid",
        ),
        (
            Some("application/json"),
            Some("[]"),
            "conversation.body_invalid",
        ),
    ] {
        let body = content_type
            .zip(body)
            .map(|(kind, text)| (kind, text.as_bytes().to_vec()));
        api.service
            .call(Method::POST, CONVERSATIONS, Some(&token), body)
            .await
            .expect_error(400, "invalid_request", key);
    }
    assert!(
        api.ids(&owner, CONVERSATIONS).await.is_empty(),
        "nothing started"
    );

    let empty = api
        .service
        .call(
            Method::POST,
            CONVERSATIONS,
            Some(&token),
            Some(("application/json; charset=utf-8", Vec::new())),
        )
        .await;
    empty.expect_status(201);
    let null = api
        .service
        .call(
            Method::POST,
            CONVERSATIONS,
            Some(&token),
            Some(("application/json", b" null ".to_vec())),
        )
        .await;
    null.expect_status(201);
    let created = api
        .call(&owner, Method::POST, CONVERSATIONS, Some(&json!({})))
        .await;
    created.expect_status(201);
    assert_eq!(created.header("cache-control"), Some("no-store"));
    let body = created.json();
    let id = body["id"].as_str().expect("an id");
    assert_eq!(
        created.header("location"),
        Some(conversation(id).as_str()),
        "Location names it"
    );
    assert_eq!(body["title"], Value::Null);
    assert_eq!(body["title_source"], Value::Null);
    assert_eq!(body["archived_at"], Value::Null);
    assert_eq!(body["created_at"], body["last_activity_at"]);
    let read = api.call(&owner, Method::GET, &conversation(id), None).await;
    read.expect_status(200);
    assert_eq!(read.json()["messages"], json!([]));
    assert_eq!(
        api.ids(&owner, CONVERSATIONS).await,
        [&created, &null, &empty].map(|reply| ids_of(&json!({"conversations": [reply.json()]}))[0]),
        "the three started, newest first"
    );

    // The empty tenant context is what the conversation keeps.
    let mut admin = api.product.superuser().await;
    let context: String = sqlx::query_scalar(
        "SELECT tenant_context FROM assistant.conversations WHERE conversation_id = $1::uuid",
    )
    .bind(id)
    .fetch_one(&mut admin)
    .await
    .expect("the stored preamble");
    assert_eq!(context, assistant::prompt::tenant_context_block(""));

    let logged = api.service.wait_logged("conversation started").await;
    assert_eq!(logged["level"], "INFO", "{logged}");
    println!(
        "POST {CONVERSATIONS}: 201 {}; logged {logged}",
        created.body
    );
}

/// What the database says about the failure below, in its message and DETAIL; none of it may
/// reach the response or the log.
const DATABASE_TEXT: [&str; 5] = [
    "duplicate key",
    "violates unique constraint",
    "already exists",
    "Key (",
    LEAK_CANARY_INDEX,
];

/// A unique index the superuser adds for one tenant only, so the other tests of this file are
/// not affected: a user of that tenant can start one conversation, and the second fails with
/// `23505` and the index's name and the key's values in the database's text.
const LEAK_CANARY_INDEX: &str = "leak_canary_one_conversation_per_user";

#[tokio::test(flavor = "multi_thread")]
async fn a_database_failure_shows_nothing_of_the_database() {
    let Some(api) = api().await else { return };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let mut admin = api.product.superuser().await;
    // DDL takes no parameters; the only value is a UUID this test generated.
    sqlx::raw_sql(AssertSqlSafe(format!(
        "CREATE UNIQUE INDEX {LEAK_CANARY_INDEX} ON assistant.conversations (tenant_id, user_id) \
         WHERE tenant_id = '{}'::uuid",
        owner.tenant
    )))
    .execute(&mut admin)
    .await
    .expect("add the canary index");

    // The database's own words for the failure, to prove the strings above are the ones it uses.
    let mut probe = admin.begin().await.expect("a superuser transaction");
    let insert = || {
        sqlx::query(
            "INSERT INTO assistant.conversations
                (tenant_id, conversation_id, user_id, prompt_version, system_prompt,
                 tenant_context, tools, effort, cache_ttl)
             VALUES ($1::uuid, gen_random_uuid(), $2::uuid, 'probe', '', '', '[]', 'medium', '5m')",
        )
        .bind(owner.tenant.to_string())
        .bind(owner.id.to_string())
    };
    insert()
        .execute(&mut *probe)
        .await
        .expect("the first conversation of the user");
    let refused = insert()
        .execute(&mut *probe)
        .await
        .expect_err("the index refuses a second one");
    let sqlx::Error::Database(refused) = refused else {
        panic!("not a database error: {refused}");
    };
    assert_eq!(refused.code().as_deref(), Some("23505"));
    let text = format!("{} {refused:?}", refused.message());
    for expected in DATABASE_TEXT {
        assert!(text.contains(expected), "{expected} not in {text}");
    }
    probe.rollback().await.expect("roll back the probe");

    api.create(&owner).await;
    let failed = api
        .call(&owner, Method::POST, CONVERSATIONS, Some(&json!({})))
        .await;
    failed.expect_error(503, "unavailable", "error.unavailable");
    let logged = api.service.wait_logged("conversation request failed").await;
    assert_eq!(logged["level"], "ERROR", "{logged}");
    assert_eq!(
        logged["fields"]["error"], "database error (SQLSTATE 23505)",
        "{logged}"
    );
    let log = format!(
        "{}\n{}",
        api.service.stdout.text(),
        api.service.stderr.text()
    );
    for text in DATABASE_TEXT {
        assert!(!failed.body.contains(text), "{text} in {}", failed.body);
        assert!(!log.contains(text), "{text} in the log");
    }
    assert_eq!(
        api.ids(&owner, CONVERSATIONS).await.len(),
        1,
        "the failed start stored nothing"
    );

    sqlx::raw_sql(AssertSqlSafe(format!(
        "DROP INDEX assistant.{LEAK_CANARY_INDEX}"
    )))
    .execute(&mut admin)
    .await
    .expect("drop the canary index");
    println!(
        "POST {CONVERSATIONS} after a unique violation: 503 {}; logged {logged}",
        failed.body
    );
}
