//! Sharing a conversation with the tenant (Laysi/trustiot-v2#650) through the assistant binary
//! as the chart starts it: the real router behind the real identity verifier, over the
//! product's schema applied by the migration Job (`testkit::product`). The conversations under
//! test are made by the real main agent against a stand-in model that thinks, calls a tool and
//! answers, with markers planted in the thinking, the tool input and result, the tool
//! definition, the tenant context and the answer's thinking.
//!
//! - Not shared: another member of the tenant (viewer, editor, administrator) does not list it,
//!   and reading or copying it answers byte for byte as for an ID that does not exist.
//! - Shared: the members list it and read only its questions and answers' text; every
//!   owner-only endpoint answers them as for a missing ID and changes nothing; another tenant's
//!   user gets 404 everywhere; the owner reads it only through the owner's endpoints. The list
//!   pages through ties in last activity with its cursor.
//! - Unsharing and deleting take it from the readers at once; a restored conversation is not
//!   shared.
//! - A copy is the reader's own: it carries the questions and answers as text only, the next
//!   question's request to the model holds nothing else, and its run delegation starts as the
//!   reader. Unsharing, deleting and purging the source leave the copy as it was. A copy over
//!   the history limit and a copy over the daily start limit store nothing.
//! - What the answer guard removed from an answer (Laysi/trustiot-v2#646) reaches the readers
//!   as the owner's view shows it, and stays with the answer in a copy, whose next question
//!   still sends the model the answer's text alone.
//! - The share, unshare and copy log lines name no title, question or answer.
//! - The database itself holds the rules: `shared_at` only from its clock, set once per sharing,
//!   and never on a deleted conversation.
//!
//! The conversations under test are answered by the real agent in the test and stored through the
//! store, as the question API (Laysi/trustiot-v2#646) stores a question's round, so the markers
//! are where a real question puts them. A copy's next question is asked both ways: by the real
//! agent in the test, on the copy as the store loads it for the reader, and through the question
//! API of a service whose AI gateway and identity service are stand-ins
//! (`a_copy_asked_through_the_question_api_runs_as_the_copier`); each run delegation starts at
//! a stand-in identity service, which keeps the token it was started with.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

mod service;
mod support;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use assistant::api::questions::QUESTIONS_PATH;
use assistant::api::sharing::COPY_TOO_LARGE;
use assistant::delegation::QuestionRuns;
use assistant::store::{self, MAX_HISTORY_BYTES, MAX_STARTS_PER_DAY};
use assistant::tools::{NoDelegation, Tool, ToolContext, ToolError, ToolOutput, ToolSpec};
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::post;
use chrono::{DateTime, TimeDelta, Utc};
use contract::api::identity::CreateRunDelegation;
use contract::client::{Endpoint, InternalClient};
use contract::identity::TenantRole;
use contract::{INTERNAL_IDENTITY_HEADER, RunId, ServiceName, TenantId, service as services};
use reqwest::Method;
use serde_json::{Value, json};
use service::{Reply, Service, User, wait_for};
use service_kit::tenant::{PlatformDb, TenantDb};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, Row};
use support::{StandIn, WAIT, fixture, ok, respond, settings};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};
use tokio::net::TcpListener;
use uuid::Uuid;

const CONVERSATIONS: &str = "/api/assistant/v1/conversations";
const DELETED: &str = "/api/assistant/v1/deleted-conversations";
const SHARED: &str = "/api/assistant/v1/shared-conversations";

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

/// The bodies of a conversation's messages, in order, as the superuser reads them.
const MESSAGE_BODIES: &str = "SELECT convert_from(body, 'UTF8') FROM assistant.messages
WHERE conversation_id = $1::uuid ORDER BY seq";

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

/// How many conversations user `$2` has stored in tenant `$1`, deleted ones included.
const STARTED: &str = "SELECT count(*) FROM assistant.conversations
WHERE tenant_id = $1::uuid AND user_id = $2::uuid";

/// The superuser stores `$3` conversations of user `$2` in tenant `$1`, started 30 minutes ago.
const SEED_STARTS: &str = "INSERT INTO assistant.conversations
    (tenant_id, conversation_id, user_id, prompt_version, system_prompt, tenant_context, tools,
     effort, cache_ttl, created_at, last_activity_at)
SELECT $1::uuid, gen_random_uuid(), $2::uuid, 'seed', '', '', '[]', 'medium', '5m',
    now() - interval '30 minutes', now() - interval '30 minutes'
FROM generate_series(1, $3)";

/// The superuser stores conversation `$2` of user `$3` in tenant `$1`, without messages.
const SEED_CONVERSATION: &str = "INSERT INTO assistant.conversations
    (tenant_id, conversation_id, user_id, prompt_version, system_prompt, tenant_context, tools,
     effort, cache_ttl)
VALUES ($1::uuid, $2::uuid, $3::uuid, 'seed', '', '', '[]', 'medium', '5m')";

/// The superuser stores message `$3` of kind `$4` with body `$5` in conversation `$2` of tenant
/// `$1`.
const SEED_MESSAGE: &str = "INSERT INTO assistant.messages
    (tenant_id, conversation_id, seq, kind, format, body)
VALUES ($1::uuid, $2::uuid, $3, $4, 'anthropic_messages', $5)";

/// Strings planted where no reader, copy or log may show them.
const THINKING_MARKER: &str = "THINKING-MARKER-0a71";
const TOOL_INPUT_MARKER: &str = "TOOL-INPUT-MARKER-5c22";
const TOOL_RESULT_MARKER: &str = "TOOL-RESULT-MARKER-e913";
const TOOL_DEFINITION_MARKER: &str = "TOOL-DEFINITION-MARKER-74bd";
const TENANT_CONTEXT_MARKER: &str = "TENANT-CONTEXT-MARKER-2f06";
const ANSWER_THINKING_MARKER: &str = "ANSWER-THINKING-MARKER-b58e";
const QUESTION: &str = "How did floor 3 use energy this week? QUESTION-MARKER-61d0";
const ANSWER: &str = "Floor 3 used 412 kWh this week. ANSWER-MARKER-9c3e";
const TITLE: &str = "Floor 3 energy TITLE-MARKER-d4a7";
const NEXT_QUESTION: &str = "And the week before?";
/// The text of the recorded answer (`tests/fixtures/answer_end_turn.json`).
const RECORDED_ANSWER: &str = "Yes. All twelve meters on floor 3 reported in the last hour.";
/// An answer the answer guard filters: it holds a code block and HTML.
const FILTERED_ANSWER: &str = "Floor 3 used 412 kWh this week.\n\n\
     ```sql\nSELECT sum(kwh) FROM readings\n```\n\n\
     Most of it was <b>daytime</b> use.";

/// Every marker of the source conversation that only its owner's history holds.
const HIDDEN_MARKERS: [&str; 6] = [
    THINKING_MARKER,
    TOOL_INPUT_MARKER,
    TOOL_RESULT_MARKER,
    TOOL_DEFINITION_MARKER,
    TENANT_CONTEXT_MARKER,
    ANSWER_THINKING_MARKER,
];

struct Api {
    product: Arc<ProductPostgres>,
    issuer: TestIssuer,
    service: Service,
    /// The service role's pool, for storing a conversation the real agent answered in the test
    /// as the question API (Laysi/trustiot-v2#646) stores it.
    db: TenantDb,
    /// The purge's platform role, to run a purge round now.
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

fn share_path(id: impl std::fmt::Display) -> String {
    format!("{CONVERSATIONS}/{id}/share")
}

fn copy_path(id: impl std::fmt::Display) -> String {
    format!("{CONVERSATIONS}/{id}/copy")
}

fn shared(id: impl std::fmt::Display) -> String {
    format!("{SHARED}/{id}")
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

/// A request by conversation ID: method, path and JSON body.
type Request = (Method, String, Option<Value>);

/// The reader's requests by ID: the read-only view and a copy.
fn reader_requests(id: Uuid) -> Vec<Request> {
    vec![
        (Method::GET, shared(id), None),
        (Method::POST, copy_path(id), Some(json!({}))),
    ]
}

/// Every request by ID that only the owner may make: read, rename, archive, delete, restore,
/// share and unshare.
fn owner_requests(id: Uuid) -> Vec<Request> {
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
        (
            Method::POST,
            format!("{CONVERSATIONS}/{id}/restore"),
            Some(json!({})),
        ),
        (Method::PUT, share_path(id), None),
        (Method::DELETE, share_path(id), None),
    ]
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

fn keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {value}"))
        .keys()
        .cloned()
        .collect()
}

/// The tool of the conversations under test: its definition and its result carry markers.
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

/// The model's answer [`FILTERED_ANSWER`], after the recorded answer's thinking.
fn filtered_answer() -> String {
    let mut answer: Value =
        serde_json::from_str(&fixture("answer_end_turn.json")).expect("the fixture is JSON");
    let thinking = answer["content"][0].clone();
    answer["content"] = json!([thinking, {"type": "text", "text": FILTERED_ANSWER}]);
    answer.to_string()
}

impl Api {
    async fn call(&self, user: &User, method: Method, path: &str, body: Option<&Value>) -> Reply {
        self.service
            .as_user(&self.issuer, user, method, path, body)
            .await
    }

    /// A conversation of `owner` titled [`TITLE`] with one answered question, made by the real
    /// agent against a stand-in model that thinks, calls [`MarkerTool`] and answers, and stored
    /// as the question API of Laysi/trustiot-v2#646 stores it. Every marker is in the database.
    async fn with_markers(&self, owner: &User) -> Uuid {
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
        let mut tx = self
            .db
            .begin(&owner.identity(&self.issuer))
            .await
            .expect("a tenant transaction");
        store::create(&mut tx, &chat)
            .await
            .expect("store the conversation");
        tx.commit().await.expect("commit");
        let id = chat.id();
        self.call(
            owner,
            Method::PATCH,
            &conversation(id),
            Some(&json!({"title": TITLE})),
        )
        .await
        .expect_status(200);
        let stored = self.stored_text(id).await;
        for marker in HIDDEN_MARKERS {
            assert!(stored.contains(marker), "{marker} is stored");
        }
        id
    }

    /// A conversation of `owner` with one question, whose answer the answer guard filtered, made
    /// by the real agent and stored as the question API stores it; with what the guard removed,
    /// as the API gives it.
    async fn with_filtered_answer(&self, owner: &User) -> (Uuid, Value) {
        let stand_in = StandIn::start(respond(|_request, _index| ok(filtered_answer()))).await;
        let agent = support::agent(&stand_in, Vec::new(), settings());
        let mut chat = agent.start_conversation(owner.principal(), "");
        let answer = agent
            .ask(&mut chat, QUESTION, Arc::new(NoDelegation))
            .await
            .expect("an answer");
        assert!(!answer.removed.is_empty(), "the guard removed something");
        let mut tx = self
            .db
            .begin(&owner.identity(&self.issuer))
            .await
            .expect("a tenant transaction");
        store::create(&mut tx, &chat)
            .await
            .expect("store the conversation");
        tx.commit().await.expect("commit");
        let removed = serde_json::to_value(answer.removed).expect("JSON");
        (chat.id(), removed)
    }

    /// Shares `id` as `owner` and returns the response's conversation.
    async fn share(&self, owner: &User, id: Uuid) -> Value {
        let reply = self.call(owner, Method::PUT, &share_path(id), None).await;
        reply.expect_status(200);
        assert_eq!(reply.header("cache-control"), Some("no-store"));
        reply.json()
    }

    async fn unshare(&self, owner: &User, id: Uuid) -> Value {
        let reply = self
            .call(owner, Method::DELETE, &share_path(id), None)
            .await;
        reply.expect_status(200);
        reply.json()
    }

    /// The IDs of a list as `user` reads it in one page.
    async fn ids(&self, user: &User, path: &str) -> Vec<Uuid> {
        let reply = self.call(user, Method::GET, path, None).await;
        reply.expect_status(200);
        ids_of(&reply.json())
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

    async fn message_bodies(&self, id: Uuid) -> Vec<String> {
        let mut admin = self.product.superuser().await;
        sqlx::query_scalar(MESSAGE_BODIES)
            .bind(id.to_string())
            .fetch_all(&mut admin)
            .await
            .expect("read the messages")
    }

    async fn started(&self, user: &User) -> i64 {
        let mut admin = self.product.superuser().await;
        sqlx::query_scalar(STARTED)
            .bind(user.tenant.to_string())
            .bind(user.id.to_string())
            .fetch_one(&mut admin)
            .await
            .expect("count the user's conversations")
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

    /// Fails unless `request` answers `user` byte for byte what the same request for an ID that
    /// does not exist gets: the one 404.
    async fn expect_as_missing(
        &self,
        user: &User,
        (method, path, body): &Request,
        (missing_method, missing_path, missing_body): &Request,
    ) {
        assert_eq!(method, missing_method, "the same request");
        assert_eq!(body, missing_body, "the same request");
        let reply = self.call(user, method.clone(), path, body.as_ref()).await;
        let nobodys = self
            .call(user, method.clone(), missing_path, body.as_ref())
            .await;
        assert_eq!(reply.status, 404, "{method} {path}: {}", reply.body);
        assert_eq!(seen(&reply), seen(&nobodys), "{method} {path}");
    }

    /// Every request of `requests(id)` answers `user` as for a missing ID.
    async fn expect_all_missing(&self, user: &User, id: Uuid, requests: fn(Uuid) -> Vec<Request>) {
        for (request, missing) in requests(id).iter().zip(requests(Uuid::now_v7()).iter()) {
            self.expect_as_missing(user, request, missing).await;
        }
    }
}

/// Acceptance criterion 1: a conversation that is not shared is invisible to the other members
/// of the tenant, its administrator included: none lists it, and the read-only view and a copy
/// answer as for an ID that does not exist. Nothing is written.
#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_not_shared_is_nobody_elses() {
    let Some(api) = api().await else {
        return;
    };
    let tenant = TenantId::generate();
    let owner = User::new(tenant, TenantRole::Viewer);
    let id = api.with_markers(&owner).await;
    let before = api.snapshot(id).await;
    assert_eq!(
        api.call(&owner, Method::GET, &conversation(id), None)
            .await
            .json()["shared_at"],
        Value::Null,
        "a conversation starts unshared"
    );

    for member in [
        User::new(tenant, TenantRole::Viewer),
        User::new(tenant, TenantRole::Editor),
        User::new(tenant, TenantRole::Admin),
    ] {
        assert!(
            api.ids(&member, SHARED).await.is_empty(),
            "{:?}",
            member.role
        );
        api.expect_all_missing(&member, id, reader_requests).await;
        assert_eq!(api.started(&member).await, 0, "no copy was made");
    }
    assert_eq!(api.snapshot(id).await, before, "nothing changed");
    let refused = api.call(&owner, Method::GET, &shared(id), None).await;
    println!(
        "an unshared conversation's view for another member: 404 {}",
        refused.body
    );
}

/// Acceptance criterion 2: once shared, every member lists it and reads its questions and
/// answers' text, nothing else; the owner-only endpoints answer a member as for a missing ID and
/// change nothing; another tenant's user gets 404 everywhere. The owner reads it through their
/// own endpoints only, and their own shared conversation is not in their shared list.
#[tokio::test(flavor = "multi_thread")]
async fn a_shared_conversation_shows_its_readers_only_the_questions_and_answers() {
    let Some(api) = api().await else {
        return;
    };
    let tenant = TenantId::generate();
    let owner = User::new(tenant, TenantRole::Viewer);
    let id = api.with_markers(&owner).await;

    let shared_once = api.share(&owner, id).await;
    let shared_at = time(&shared_once["shared_at"]);
    assert_eq!(shared_once["id"], id.to_string());
    assert_eq!(shared_once["title"], TITLE);
    // Sharing again keeps the first time.
    assert_eq!(api.share(&owner, id).await, shared_once);
    let owners = api.call(&owner, Method::GET, &conversation(id), None).await;
    owners.expect_status(200);
    assert_eq!(time(&owners.json()["shared_at"]), shared_at);
    let listed = api.call(&owner, Method::GET, CONVERSATIONS, None).await;
    assert_eq!(
        time(&listed.json()["conversations"][0]["shared_at"]),
        shared_at,
        "the owner's list shows it shared"
    );
    assert!(
        api.ids(&owner, SHARED).await.is_empty(),
        "the owner's own conversation is not shared with the owner"
    );
    api.expect_all_missing(&owner, id, |id| vec![(Method::GET, shared(id), None)])
        .await;
    let before = api.snapshot(id).await;

    // The agent that stored the conversation runs with the answer tool (`support::settings`).
    let system_prompt = assistant::prompt::system_prompt();
    let excerpt: String = system_prompt.text.chars().take(60).collect();
    // The version is a date: as a JSON string of its own, so the timestamps of a conversation
    // started that day do not count as it.
    let version = format!("\"{}\"", system_prompt.version);
    for member in [
        User::new(tenant, TenantRole::Viewer),
        User::new(tenant, TenantRole::Editor),
        User::new(tenant, TenantRole::Admin),
    ] {
        let role = member.role;
        let list = api.call(&member, Method::GET, SHARED, None).await;
        list.expect_status(200);
        assert_eq!(list.header("cache-control"), Some("no-store"));
        let list = list.json();
        assert_eq!(
            list["conversations"],
            json!([{
                "id": id.to_string(),
                "title": TITLE,
                "owner_user_id": owner.id.to_string(),
                "shared_at": shared_once["shared_at"],
                "last_activity_at": shared_once["last_activity_at"],
            }]),
            "{role:?}"
        );
        assert_eq!(list["next_cursor"], Value::Null);

        let view = api.call(&member, Method::GET, &shared(id), None).await;
        view.expect_status(200);
        assert_eq!(view.header("cache-control"), Some("no-store"));
        let detail = view.json();
        assert_eq!(
            keys(&detail),
            BTreeSet::from(
                [
                    "id",
                    "title",
                    "owner_user_id",
                    "shared_at",
                    "last_activity_at",
                    "messages"
                ]
                .map(String::from)
            ),
            "{role:?}"
        );
        assert_eq!(detail["owner_user_id"], owner.id.to_string());
        assert_eq!(
            detail["messages"],
            json!([
                {"kind": "question", "text": QUESTION},
                {"kind": "answer", "text": ANSWER, "draft_versions": []}
            ]),
            "{role:?}"
        );
        for absent in HIDDEN_MARKERS.into_iter().chain([
            "energy_report",
            "signature",
            excerpt.as_str(),
            version.as_str(),
        ]) {
            assert!(
                !view.body.contains(absent),
                "{role:?}: {absent} in {}",
                view.body
            );
        }

        // Every owner-only endpoint: as for a missing ID, and nothing changes.
        api.expect_all_missing(&member, id, owner_requests).await;
        assert_eq!(api.snapshot(id).await, before, "{role:?}: nothing changed");
    }

    // Another tenant's user, even the same person, an administrator there: 404 everywhere.
    for stranger in [
        User::new(TenantId::generate(), TenantRole::Admin),
        User {
            tenant: TenantId::generate(),
            ..owner
        },
    ] {
        assert!(api.ids(&stranger, SHARED).await.is_empty());
        api.expect_all_missing(&stranger, id, reader_requests).await;
        api.expect_all_missing(&stranger, id, owner_requests).await;
        assert_eq!(api.started(&stranger).await, 0, "no copy was made");
    }
    assert_eq!(api.snapshot(id).await, before, "nothing changed");
    let view = api
        .call(
            &User::new(tenant, TenantRole::Admin),
            Method::GET,
            &shared(id),
            None,
        )
        .await;
    println!("GET {}: {}", shared(id), view.body);
}

/// What the answer guard removed from an answer (Laysi/trustiot-v2#646): each reader of the
/// shared conversation sees the messages exactly as its owner does, `removed` included.
#[tokio::test(flavor = "multi_thread")]
async fn readers_see_what_the_answer_guard_removed() {
    let Some(api) = api().await else {
        return;
    };
    let tenant = TenantId::generate();
    let owner = User::new(tenant, TenantRole::Viewer);
    let (id, removed) = api.with_filtered_answer(&owner).await;
    api.share(&owner, id).await;
    let owners = api.call(&owner, Method::GET, &conversation(id), None).await;
    owners.expect_status(200);
    let messages = owners.json()["messages"].clone();
    assert_eq!(messages[0]["kind"], "question");
    assert_eq!(messages[0].get("removed"), None, "a question has none");
    assert_eq!(messages[1]["kind"], "answer");
    assert_eq!(messages[1]["removed"], removed, "the owner's view");
    for reason in ["```", "SELECT", "<b>"] {
        let text = messages[1]["text"].as_str().expect("the answer's text");
        assert!(!text.contains(reason), "{reason} in {text}");
    }
    for role in [TenantRole::Viewer, TenantRole::Editor, TenantRole::Admin] {
        let reader = User::new(tenant, role);
        let view = api.call(&reader, Method::GET, &shared(id), None).await;
        view.expect_status(200);
        assert_eq!(view.json()["messages"], messages, "{role:?}");
    }
    println!("removed: {removed}");
}

/// Acceptance criterion 4 through the question API (Laysi/trustiot-v2#650 left it to
/// Laysi/trustiot-v2#646): a member copies the owner's shared conversation and asks in the copy
/// with `POST .../questions`, on a service whose AI gateway and identity service are stand-ins.
/// The model gets the earlier question and answer texts, the note that they were copied and the
/// new question, nothing of the source's thinking, tool calls, results, tool definitions or
/// tenant context; the run delegation starts as the member, never as the owner; the copy holds
/// the copied texts and the new round, and the source is as it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_copy_asked_through_the_question_api_runs_as_the_copier() {
    let Some(api) = api().await else {
        return;
    };
    let tenant = TenantId::generate();
    let owner = User::new(tenant, TenantRole::Admin);
    let reader = User::new(tenant, TenantRole::Viewer);
    let id = api.with_markers(&owner).await;
    api.share(&owner, id).await;
    let source_before = api.snapshot(id).await;

    let gateway = StandIn::start(respond(|_request, _index| {
        ok(fixture("answer_end_turn.json"))
    }))
    .await;
    let seen = Delegations::default();
    let identity = identity_stand_in(seen.clone()).await;
    let service = Service::start(
        &api.product,
        &api.issuer.jwks(),
        &[
            ("TRUSTIOT__AI_GATEWAY__URL", gateway.url.as_str()),
            ("TRUSTIOT__IDENTITY__EXCHANGE_URL", identity.as_str()),
        ],
    )
    .await;

    let copied = service
        .as_user(
            &api.issuer,
            &reader,
            Method::POST,
            &copy_path(id),
            Some(&json!({})),
        )
        .await;
    copied.expect_status(201);
    let copy_id: Uuid = copied.json()["id"]
        .as_str()
        .and_then(|id| id.parse().ok())
        .expect("the copy's ID");
    let asked = service
        .as_user(
            &api.issuer,
            &reader,
            Method::POST,
            &QUESTIONS_PATH.replace("{conversation_id}", &copy_id.to_string()),
            Some(&json!({"question": NEXT_QUESTION})),
        )
        .await;
    asked.expect_status(202);
    let run = asked.json()["id"]
        .as_str()
        .expect("the run's ID")
        .to_owned();
    let what = format!("the question {run} in the copy reading as answered");
    let detail = wait_for(&what, WAIT, || async {
        let read = service
            .as_user(
                &api.issuer,
                &reader,
                Method::GET,
                &conversation(copy_id),
                None,
            )
            .await;
        read.expect_status(200);
        let detail = read.json();
        let latest = &detail["latest_run"];
        (latest["id"] == run.as_str() && latest["status"] == "answered").then_some(detail)
    })
    .await;
    assert_eq!(
        detail["messages"],
        json!([
            {"kind": "question", "text": QUESTION},
            {"kind": "answer", "text": ANSWER, "draft_versions": []},
            {"kind": "question", "text": NEXT_QUESTION},
            {"kind": "answer", "text": RECORDED_ANSWER, "draft_versions": []},
        ])
    );

    // The one model call: the copied texts, the note and the new question, nothing else.
    let requests = gateway.requests();
    let [request] = requests.as_slice() else {
        panic!("one model call: {requests:?}");
    };
    let body = request.json();
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": QUESTION},
            {"role": "system", "content": assistant::prompt::COPIED_ROUNDS},
            {"role": "assistant", "content": [{"type": "text", "text": ANSWER}]},
            {"role": "user", "content": NEXT_QUESTION},
        ]),
        "only the earlier question and answer texts, marked as copied"
    );
    for absent in HIDDEN_MARKERS.into_iter().chain(["energy_report"]) {
        assert!(
            !request.body.contains(absent),
            "{absent} in the request: {}",
            request.body
        );
    }
    let messages = body["messages"].to_string();
    for absent in [
        "\"thinking\"",
        "redacted_thinking",
        "tool_use",
        "tool_result",
        "signature",
    ] {
        assert!(!messages.contains(absent), "{absent} in {messages}");
    }
    assert_eq!(request.tenant.as_deref(), Some(tenant.to_string().as_str()));

    // The run delegation started as the reader, once.
    let tokens = seen
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let [token] = tokens.as_slice() else {
        panic!("one delegation: {tokens:?}");
    };
    let started_as = api
        .issuer
        .verifier(ServiceName::Assistant)
        .verify(token)
        .expect("a token of the issuer");
    assert_eq!(started_as.user_id(), reader.id, "started as the reader");
    assert_ne!(started_as.user_id(), owner.id, "never as the owner");
    assert_eq!(started_as.tenant_id(), Some(tenant));

    // Stored: the copied texts, then the new round; the source is as it was.
    let bodies: Vec<Value> = api
        .message_bodies(copy_id)
        .await
        .iter()
        .map(|body| serde_json::from_str(body).expect("a message"))
        .collect();
    let [question, note, answer, next, reply] = bodies.as_slice() else {
        panic!("the copied round and the new one: {bodies:?}");
    };
    assert_eq!(
        [question, note, answer, next],
        [
            &json!({"role": "user", "content": QUESTION}),
            &json!({"role": "system", "content": assistant::prompt::COPIED_ROUNDS}),
            &json!({"role": "assistant", "content": [{"type": "text", "text": ANSWER}]}),
            &json!({"role": "user", "content": NEXT_QUESTION}),
        ]
    );
    assert_eq!(reply["role"], "assistant");
    assert!(reply.to_string().contains(RECORDED_ANSWER), "{reply}");
    let stored = api.stored_text(copy_id).await;
    for marker in HIDDEN_MARKERS {
        assert!(!stored.contains(marker), "{marker} in the copy: {stored}");
    }
    assert_eq!(
        api.snapshot(id).await,
        source_before,
        "the source is unchanged"
    );
    println!("the copy's question ran as the reader through POST {QUESTIONS_PATH}");
}

/// What the answer guard removed from an answer (Laysi/trustiot-v2#646) stays with the answer in
/// a reader's copy: the copy's chat shows the source's messages, `removed` included. The copy's
/// next question still sends the model the texts alone (criterion 4).
#[tokio::test(flavor = "multi_thread")]
async fn a_copy_keeps_what_the_answer_guard_removed() {
    let Some(api) = api().await else {
        return;
    };
    let tenant = TenantId::generate();
    let owner = User::new(tenant, TenantRole::Viewer);
    let reader = User::new(tenant, TenantRole::Viewer);
    let (id, removed) = api.with_filtered_answer(&owner).await;
    api.share(&owner, id).await;
    let source = api.call(&owner, Method::GET, &conversation(id), None).await;
    source.expect_status(200);
    let messages = source.json()["messages"].clone();
    assert_eq!(messages[1]["removed"], removed, "the source's answer");

    let copied = api
        .call(&reader, Method::POST, &copy_path(id), Some(&json!({})))
        .await;
    copied.expect_status(201);
    let copy_id: Uuid = copied.json()["id"]
        .as_str()
        .expect("the copy's ID")
        .parse()
        .expect("a UUID");
    let copy = api
        .call(&reader, Method::GET, &conversation(copy_id), None)
        .await;
    copy.expect_status(200);
    assert_eq!(
        copy.json()["messages"],
        messages,
        "the copy's chat shows what the source's does"
    );

    // The copy's next question, by the real agent on the copy as the store loads it.
    let gateway = StandIn::start(respond(|_request, _index| {
        ok(fixture("answer_end_turn.json"))
    }))
    .await;
    let agent = support::agent(&gateway, Vec::new(), settings());
    let mut tx = api
        .db
        .begin(&reader.identity(&api.issuer))
        .await
        .expect("a tenant transaction");
    let mut loaded = store::load(&mut tx, reader.principal(), copy_id)
        .await
        .expect("load the copy")
        .expect("the reader's copy")
        .conversation;
    tx.commit().await.expect("commit");
    agent
        .ask(&mut loaded, NEXT_QUESTION, Arc::new(NoDelegation))
        .await
        .expect("an answer in the copy");
    let requests = gateway.requests();
    let [request] = requests.as_slice() else {
        panic!("one model call: {requests:?}");
    };
    let text = messages[1]["text"].as_str().expect("the answer's text");
    assert_eq!(
        request.json()["messages"],
        json!([
            {"role": "user", "content": QUESTION},
            {"role": "system", "content": assistant::prompt::COPIED_ROUNDS},
            {"role": "assistant", "content": [{"type": "text", "text": text}]},
            {"role": "user", "content": NEXT_QUESTION},
        ]),
        "the answer's text alone"
    );
    println!("the copy's answer keeps removed: {removed}");
}

/// Acceptance criterion 3: unsharing takes the conversation from its readers at once; deleting
/// does too, and a restored conversation is not shared until the owner shares it again.
#[tokio::test(flavor = "multi_thread")]
async fn unsharing_and_deleting_take_a_conversation_from_its_readers() {
    let Some(api) = api().await else {
        return;
    };
    let tenant = TenantId::generate();
    let owner = User::new(tenant, TenantRole::Editor);
    let reader = User::new(tenant, TenantRole::Viewer);
    let id = api.with_markers(&owner).await;

    api.share(&owner, id).await;
    api.call(&reader, Method::GET, &shared(id), None)
        .await
        .expect_status(200);
    let unshared = api.unshare(&owner, id).await;
    assert_eq!(unshared["shared_at"], Value::Null);
    // Unsharing again changes nothing.
    assert_eq!(api.unshare(&owner, id).await, unshared);
    assert!(api.ids(&reader, SHARED).await.is_empty());
    api.expect_all_missing(&reader, id, reader_requests).await;

    // Shared again, then deleted: gone for the reader, and unshared in the database.
    let again = api.share(&owner, id).await;
    assert_ne!(again["shared_at"], Value::Null);
    assert_eq!(api.ids(&reader, SHARED).await, [id]);
    api.call(&owner, Method::DELETE, &conversation(id), None)
        .await
        .expect_status(204);
    assert!(api.ids(&reader, SHARED).await.is_empty());
    api.expect_all_missing(&reader, id, reader_requests).await;
    let snapshot: Value =
        serde_json::from_str(api.snapshot(id).await.lines().next().expect("the row"))
            .expect("the row as JSON");
    assert_eq!(snapshot["shared_at"], Value::Null, "deleting unshares");
    assert_ne!(snapshot["deleted_at"], Value::Null);
    // Sharing or unsharing a deleted conversation is the one 404 for its owner too.
    api.expect_all_missing(&owner, id, |id| {
        vec![
            (Method::PUT, share_path(id), None),
            (Method::DELETE, share_path(id), None),
            (Method::POST, copy_path(id), Some(json!({}))),
        ]
    })
    .await;

    // Restored within 30 days: back for its owner, not shared.
    let restored = api
        .call(
            &owner,
            Method::POST,
            &format!("{CONVERSATIONS}/{id}/restore"),
            Some(&json!({})),
        )
        .await;
    restored.expect_status(200);
    assert_eq!(restored.json()["shared_at"], Value::Null);
    assert!(api.ids(&reader, SHARED).await.is_empty());
    api.expect_all_missing(&reader, id, reader_requests).await;
    assert_eq!(api.ids(&owner, CONVERSATIONS).await, [id]);
    println!("restored: {}", restored.body);
}

/// The superuser stores conversation `$2` of user `$3` in tenant `$1`, last active and shared at
/// `$4` (shared when `$5`), past the clock triggers (which replica mode skips).
const SEED_ACTIVE: &str = "INSERT INTO assistant.conversations
    (tenant_id, conversation_id, user_id, prompt_version, system_prompt, tenant_context, tools,
     effort, cache_ttl, created_at, last_activity_at, shared_at)
VALUES ($1::uuid, $2::uuid, $3::uuid, 'seed', '', '', '[]', 'medium', '5m', $4, $4,
    CASE WHEN $5 THEN $4 END)";

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

/// Acceptance criterion 2, the "shared with me" list over several pages: walking it with each
/// page's `next_cursor` gives every conversation shared with the reader once, most recently
/// active first and by ID among those last active at the same time, and never the reader's own,
/// an unshared one or another tenant's. The first page and the pages after a cursor are
/// different statements (`store/sharing.rs`), so both are walked.
#[tokio::test(flavor = "multi_thread")]
async fn the_shared_list_pages_in_activity_order() {
    let Some(api) = api().await else {
        return;
    };
    let tenant = TenantId::generate();
    let (first, second) = (
        User::new(tenant, TenantRole::Editor),
        User::new(tenant, TenantRole::Viewer),
    );
    let reader = User::new(tenant, TenantRole::Viewer);
    let stranger = User::new(TenantId::generate(), TenantRole::Editor);
    let base = DateTime::from_timestamp(Utc::now().timestamp() - 3600, 0).expect("a time");
    let at = |minutes: i64| base + TimeDelta::minutes(minutes);
    // (owner, last active, shared): ties at minutes 0, 10 and 20 cross a page boundary of 2.
    let seeds = [
        (&first, at(0), true),
        (&first, at(0), true),
        (&first, at(10), true),
        (&second, at(10), true),
        (&second, at(20), true),
        (&second, at(20), true),
        (&second, at(30), true),
        (&reader, at(10), true),
        (&first, at(10), false),
        (&stranger, at(10), true),
    ];
    let mut expected = Vec::new();
    let mut admin = api.product.superuser().await;
    let mut tx = admin.begin().await.expect("a superuser transaction");
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .expect("skip the clock triggers");
    for (owner, last_active, shared) in seeds {
        let id = Uuid::now_v7();
        sqlx::query(SEED_ACTIVE)
            .bind(owner.tenant.to_string())
            .bind(id.to_string())
            .bind(owner.id.to_string())
            .bind(last_active)
            .bind(shared)
            .execute(&mut *tx)
            .await
            .expect("store a conversation");
        if shared && owner.tenant == tenant && owner.id != reader.id {
            expected.push((last_active, id));
        }
    }
    tx.commit().await.expect("commit");
    expected.sort_by(|left, right| right.cmp(left));
    let expected: Vec<Uuid> = expected.into_iter().map(|(_, id)| id).collect();
    assert_eq!(expected.len(), 7);

    assert_eq!(
        api.ids(&reader, &format!("{SHARED}?limit=100")).await,
        expected,
        "one page"
    );
    let mut walked = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let path = match &cursor {
            None => format!("{SHARED}?limit=2"),
            Some(cursor) => format!("{SHARED}?limit=2&cursor={}", encode(cursor)),
        };
        let page = api.call(&reader, Method::GET, &path, None).await;
        page.expect_status(200);
        let page = page.json();
        let ids = ids_of(&page);
        assert!(ids.len() <= 2, "{page}");
        walked.extend(ids);
        pages += 1;
        match page["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
        assert!(pages < 10, "the walk ends");
    }
    assert_eq!(walked, expected, "every shared conversation once, in order");
    assert_eq!(pages, 4);
}

/// What the stand-in identity service saw of the run delegations started.
#[derive(Clone, Default)]
struct Delegations(Arc<Mutex<Vec<String>>>);

async fn create_delegation(
    State(seen): State<Delegations>,
    headers: HeaderMap,
) -> axum::response::Response {
    let token = headers
        .get(INTERNAL_IDENTITY_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    seen.0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(token);
    let expires_at = Utc::now().timestamp() + 900;
    axum::Json(json!({"delegation": "delegation.for.copy", "expires_at": expires_at}))
        .into_response()
}

/// A stand-in identity service that starts run delegations and keeps the request token of each;
/// returns its origin.
async fn identity_stand_in(seen: Delegations) -> String {
    let router = Router::new()
        .route(CreateRunDelegation::ROUTE, post(create_delegation))
        .with_state(seen);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let address = listener.local_addr().expect("the local address");
    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("serve the stand-in identity service");
    });
    format!("http://{address}")
}

/// Acceptance criterion 4: a member copies a shared conversation. The copy is theirs, with the
/// source's title as their own, the questions and answers as text and nothing else stored; the
/// owner cannot see it. The member's next question in it sends the model only the earlier
/// question and answer texts, with the note that they were copied (security review L1), and its
/// run delegation starts as the member. Afterwards the owner
/// unshares, deletes, and the purge removes the source; the copy stays as it was. The owner can
/// copy their own conversation too.
#[tokio::test(flavor = "multi_thread")]
async fn a_copy_is_the_readers_own_and_carries_only_the_texts() {
    let Some(api) = api().await else {
        return;
    };
    let tenant = TenantId::generate();
    let owner = User::new(tenant, TenantRole::Admin);
    let reader = User::new(tenant, TenantRole::Viewer);
    let id = api.with_markers(&owner).await;
    api.share(&owner, id).await;

    // The copy needs the JSON content type, and no settings.
    let token = reader.token(&api.issuer);
    api.service
        .call(Method::POST, &copy_path(id), Some(&token), None)
        .await
        .expect_error(400, "invalid_request", "error.invalid_request");
    api.call(
        &reader,
        Method::POST,
        &copy_path(id),
        Some(&json!({"title": "x"})),
    )
    .await
    .expect_error(400, "invalid_request", "conversation.body_invalid");
    assert_eq!(api.started(&reader).await, 0, "nothing was copied");

    let copied = api
        .call(&reader, Method::POST, &copy_path(id), Some(&json!({})))
        .await;
    copied.expect_status(201);
    assert_eq!(copied.header("cache-control"), Some("no-store"));
    let copy = copied.json();
    let copy_id: Uuid = copy["id"]
        .as_str()
        .and_then(|id| id.parse().ok())
        .expect("an id");
    assert_ne!(copy_id, id);
    assert_eq!(
        copied.header("location"),
        Some(conversation(copy_id).as_str())
    );
    assert_eq!(copy["title"], TITLE);
    assert_eq!(copy["title_source"], "user");
    assert_eq!(copy["shared_at"], Value::Null);
    assert_eq!(copy["archived_at"], Value::Null);
    assert_eq!(api.ids(&reader, CONVERSATIONS).await, [copy_id]);
    assert!(!api.ids(&owner, CONVERSATIONS).await.contains(&copy_id));
    api.expect_all_missing(&owner, copy_id, owner_requests)
        .await;
    api.expect_all_missing(&owner, copy_id, reader_requests)
        .await;

    // The reader reads it as their own conversation: the same questions and answers.
    let read = api
        .call(&reader, Method::GET, &conversation(copy_id), None)
        .await;
    read.expect_status(200);
    assert_eq!(
        read.json()["messages"],
        json!([
            {"kind": "question", "text": QUESTION},
            {"kind": "answer", "text": ANSWER, "draft_versions": []}
        ])
    );
    // Shared in turn, the copy shows its readers the same two messages, never the note.
    api.share(&reader, copy_id).await;
    let shared_copy = api.call(&owner, Method::GET, &shared(copy_id), None).await;
    shared_copy.expect_status(200);
    assert_eq!(shared_copy.json()["messages"], read.json()["messages"]);
    api.unshare(&reader, copy_id).await;
    // Stored: one user message with the question, the note that the turns were copied, one
    // assistant message with the answer's text; nothing of the source's thinking, tool calls or
    // results. The chat above shows no note.
    let bodies: Vec<Value> = api
        .message_bodies(copy_id)
        .await
        .iter()
        .map(|body| serde_json::from_str(body).expect("a message"))
        .collect();
    assert_eq!(
        bodies,
        [
            json!({"role": "user", "content": QUESTION}),
            json!({"role": "system", "content": assistant::prompt::COPIED_ROUNDS}),
            json!({"role": "assistant", "content": [{"type": "text", "text": ANSWER}]}),
        ]
    );
    let stored = api.stored_text(copy_id).await;
    for marker in HIDDEN_MARKERS {
        assert!(!stored.contains(marker), "{marker} in the copy: {stored}");
    }
    let mut admin = api.product.superuser().await;
    let row = sqlx::query(
        "SELECT user_id::text AS user_id, tenant_context, tools FROM assistant.conversations
         WHERE conversation_id = $1::uuid",
    )
    .bind(copy_id.to_string())
    .fetch_one(&mut admin)
    .await
    .expect("the copy's row");
    assert_eq!(row.get::<String, _>("user_id"), reader.id.to_string());
    assert_eq!(
        row.get::<String, _>("tenant_context"),
        assistant::prompt::tenant_context_block(""),
        "the copy's preamble is its own, as a new conversation's"
    );
    drop(admin);

    // The reader asks the next question in the copy: the real agent on the copy as the store
    // loads it for the reader, with the reader's run delegation. Through the question API
    // (Laysi/trustiot-v2#646) itself: `a_copy_asked_through_the_question_api_runs_as_the_copier`.
    let seen = Delegations::default();
    let identity = identity_stand_in(seen.clone()).await;
    let runs = QuestionRuns::new(
        InternalClient::<services::Identity>::new(&identity, Duration::from_secs(30))
            .expect("an identity client"),
        Duration::from_secs(900),
    );
    let gateway = StandIn::start(respond(|_request, _index| {
        ok(fixture("answer_end_turn.json"))
    }))
    .await;
    let agent = support::agent(&gateway, Vec::new(), settings());
    let reader_identity = reader.identity(&api.issuer);
    let mut tx = api
        .db
        .begin(&reader_identity)
        .await
        .expect("a tenant transaction");
    let mut loaded = store::load(&mut tx, reader.principal(), copy_id)
        .await
        .expect("load the copy")
        .expect("the reader's copy")
        .conversation;
    tx.commit().await.expect("commit");
    let run = runs
        .start(&reader_identity, loaded.principal(), RunId::generate())
        .await
        .expect("the reader's run delegation");
    assert_eq!(
        run.principal(),
        reader.principal(),
        "the run is the reader's"
    );
    agent
        .ask(&mut loaded, NEXT_QUESTION, Arc::new(run))
        .await
        .expect("an answer in the copy");
    let requests = gateway.requests();
    let [request] = requests.as_slice() else {
        panic!("one model call: {requests:?}");
    };
    let body = request.json();
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": QUESTION},
            {"role": "system", "content": assistant::prompt::COPIED_ROUNDS},
            {"role": "assistant", "content": [{"type": "text", "text": ANSWER}]},
            {"role": "user", "content": NEXT_QUESTION},
        ]),
        "only the earlier question and answer texts, marked as copied"
    );
    // Nothing of the source anywhere in the request (its preamble is the copy's own: the
    // source's tool definition and tenant context are not in it either), and no block in the
    // messages but text.
    for absent in HIDDEN_MARKERS.into_iter().chain(["energy_report"]) {
        assert!(
            !request.body.contains(absent),
            "{absent} in the request: {}",
            request.body
        );
    }
    let messages = body["messages"].to_string();
    for absent in [
        "\"thinking\"",
        "redacted_thinking",
        "tool_use",
        "tool_result",
        "signature",
    ] {
        assert!(!messages.contains(absent), "{absent} in {messages}");
    }
    assert_eq!(request.tenant.as_deref(), Some(tenant.to_string().as_str()));
    let tokens = seen
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let [token] = tokens.as_slice() else {
        panic!("one delegation: {tokens:?}");
    };
    let started_as = api
        .issuer
        .verifier(ServiceName::Assistant)
        .verify(token)
        .expect("the reader's own token");
    assert_eq!(
        started_as.user_id(),
        reader.id,
        "the delegation starts as the reader"
    );
    assert_eq!(started_as.tenant_id(), Some(tenant));

    // The owner unshares, deletes, and the purge removes the source: the copy stays.
    let copy_before = api.snapshot(copy_id).await;
    let read_before = read.json();
    api.unshare(&owner, id).await;
    api.call(&owner, Method::DELETE, &conversation(id), None)
        .await
        .expect_status(204);
    api.backdate(id, Utc::now() - TimeDelta::days(31)).await;
    assistant::purge::round(&api.platform)
        .await
        .expect("a purge round");
    assert_eq!(
        api.snapshot(id).await,
        "\n[]\n[]\n[]",
        "the source and every row of it are gone"
    );
    assert_eq!(
        api.snapshot(copy_id).await,
        copy_before,
        "the copy is as it was"
    );
    let read_after = api
        .call(&reader, Method::GET, &conversation(copy_id), None)
        .await;
    read_after.expect_status(200);
    assert_eq!(read_after.json(), read_before);

    // The owner copies their own conversation: a conversation of theirs.
    let mine = api.with_markers(&owner).await;
    let own_copy = api
        .call(&owner, Method::POST, &copy_path(mine), Some(&json!({})))
        .await;
    own_copy.expect_status(201);
    let own_copy_id = own_copy.json()["id"].as_str().expect("an id").to_owned();
    let read = api
        .call(&owner, Method::GET, &conversation(&own_copy_id), None)
        .await;
    assert_eq!(
        read.json()["messages"],
        json!([
            {"kind": "question", "text": QUESTION},
            {"kind": "answer", "text": ANSWER, "draft_versions": []}
        ])
    );
    println!("POST {}: 201 {}", copy_path(id), copied.body);
}

/// A copy whose history would be over `MAX_HISTORY_BYTES` answers 409 with its key and stores
/// nothing; a copy over the daily start limit answers 429 like starting a conversation.
#[tokio::test(flavor = "multi_thread")]
async fn a_copy_over_a_limit_stores_nothing() {
    let Some(api) = api().await else {
        return;
    };
    let tenant = TenantId::generate();
    let owner = User::new(tenant, TenantRole::Viewer);
    let reader = User::new(tenant, TenantRole::Viewer);

    // Two rounds whose answers are 9 MiB of text each: 18 MiB, over the 16 MiB limit, each
    // message under the 32 MiB one. After them, a message out of place (position 5, not 4):
    // reading on would answer 503 for a corrupt history, so the 409 shows the store stopped
    // reading where the texts went over the limit.
    let big = Uuid::now_v7();
    let mut admin = api.product.superuser().await;
    sqlx::query(SEED_CONVERSATION)
        .bind(tenant.to_string())
        .bind(big.to_string())
        .bind(owner.id.to_string())
        .execute(&mut admin)
        .await
        .expect("store the conversation");
    let text = "a".repeat(9 << 20);
    for (seq, kind, body) in [
        (0, "question", json!({"role": "user", "content": "First?"})),
        (
            1,
            "answer",
            json!({"role": "assistant", "content": [{"type": "text", "text": text}]}),
        ),
        (2, "question", json!({"role": "user", "content": "Second?"})),
        (
            3,
            "answer",
            json!({"role": "assistant", "content": [{"type": "text", "text": text}]}),
        ),
        (
            5,
            "question",
            json!({"role": "user", "content": "Out of place?"}),
        ),
    ] {
        sqlx::query(SEED_MESSAGE)
            .bind(tenant.to_string())
            .bind(big.to_string())
            .bind(seq)
            .bind(kind)
            .bind(body.to_string().into_bytes())
            .execute(&mut admin)
            .await
            .expect("store a message");
    }
    drop(admin);
    api.share(&owner, big).await;
    let refused = api
        .call(&reader, Method::POST, &copy_path(big), Some(&json!({})))
        .await;
    refused.expect_error(409, "conflict", COPY_TOO_LARGE.as_str());
    assert_eq!(
        refused.json()["details"],
        json!({"max_bytes": MAX_HISTORY_BYTES})
    );
    assert_eq!(api.started(&reader).await, 0, "nothing was stored");

    // Texts under the limit whose copy is over it: one answer of 9 MiB of quotation marks, which
    // the copy's message writes as 18 MiB (each one escaped).
    let quoted = Uuid::now_v7();
    let mut admin = api.product.superuser().await;
    sqlx::query(SEED_CONVERSATION)
        .bind(tenant.to_string())
        .bind(quoted.to_string())
        .bind(owner.id.to_string())
        .execute(&mut admin)
        .await
        .expect("store the conversation");
    let quotes = "\"".repeat(9 << 20);
    for (seq, kind, body) in [
        (0, "question", json!({"role": "user", "content": "Quoted?"})),
        (
            1,
            "answer",
            json!({"role": "assistant", "content": [{"type": "text", "text": quotes}]}),
        ),
    ] {
        sqlx::query(SEED_MESSAGE)
            .bind(tenant.to_string())
            .bind(quoted.to_string())
            .bind(seq)
            .bind(kind)
            .bind(body.to_string().into_bytes())
            .execute(&mut admin)
            .await
            .expect("store a message");
    }
    drop(admin);
    api.share(&owner, quoted).await;
    api.call(&reader, Method::POST, &copy_path(quoted), Some(&json!({})))
        .await
        .expect_error(409, "conflict", COPY_TOO_LARGE.as_str());
    assert_eq!(api.started(&reader).await, 0, "nothing was stored");
    // The reader still reads it: the limit is the copy's.
    let view = api.call(&reader, Method::GET, &shared(quoted), None).await;
    view.expect_status(200);
    assert_eq!(view.json()["messages"][1]["text"], quotes.as_str());

    // The daily start limit counts copies like starts.
    let small = api.with_markers(&owner).await;
    api.share(&owner, small).await;
    let mut admin = api.product.superuser().await;
    sqlx::query(SEED_STARTS)
        .bind(tenant.to_string())
        .bind(reader.id.to_string())
        .bind(i32::try_from(MAX_STARTS_PER_DAY).expect("a count"))
        .execute(&mut admin)
        .await
        .expect("store a day's starts");
    drop(admin);
    let limited = api
        .call(&reader, Method::POST, &copy_path(small), Some(&json!({})))
        .await;
    limited.expect_error(429, "rate_limited", "conversation.start_limited");
    assert!(limited.header("retry-after").is_some());
    assert_eq!(
        api.started(&reader).await,
        MAX_STARTS_PER_DAY,
        "nothing was stored"
    );
    println!("an 18 MiB copy: {} {}", refused.status, refused.body);
}

/// Acceptance criterion 6: sharing, unsharing and copying each log one INFO line with the user,
/// tenant and conversation (and the copy's new one), and no log line of the service names the
/// title, the question or the answer.
#[tokio::test(flavor = "multi_thread")]
async fn sharing_logs_carry_no_content() {
    let Some(api) = api().await else {
        return;
    };
    let tenant = TenantId::generate();
    let owner = User::new(tenant, TenantRole::Viewer);
    let reader = User::new(tenant, TenantRole::Editor);
    let id = api.with_markers(&owner).await;
    api.share(&owner, id).await;
    let copied = api
        .call(&reader, Method::POST, &copy_path(id), Some(&json!({})))
        .await;
    copied.expect_status(201);
    let copy_id = copied.json()["id"].as_str().expect("an id").to_owned();
    api.unshare(&owner, id).await;

    let line = |message: &'static str, user: &'static str| {
        let api = &api;
        let expected = if user == "owner" { owner.id } else { reader.id };
        async move {
            wait_for(
                &format!("the assistant logging `{message}` for {id}"),
                service::LOG_LIMIT,
                || async {
                    api.service
                        .stdout
                        .messages(message)
                        .into_iter()
                        .find(|line| {
                            line["fields"]["conversation_id"] == id.to_string()
                                && line["fields"]["user_id"] == expected.to_string()
                        })
                },
            )
            .await
        }
    };
    let shared_line = line("conversation shared", "owner").await;
    let unshared_line = line("conversation unshared", "owner").await;
    let copied_line = line("conversation copied", "reader").await;
    for (logged, outcome) in [
        (&shared_line, "shared"),
        (&unshared_line, "unshared"),
        (&copied_line, "copied"),
    ] {
        assert_eq!(logged["level"], "INFO", "{logged}");
        assert_eq!(
            logged["fields"]["tenant_id"],
            tenant.to_string(),
            "{logged}"
        );
        assert_eq!(logged["fields"]["outcome"], outcome, "{logged}");
    }
    assert_eq!(copied_line["fields"]["new_conversation_id"], copy_id);

    let output = format!(
        "{}\n{}",
        api.service.stdout.text(),
        api.service.stderr.text()
    );
    for content in [
        TITLE,
        QUESTION,
        ANSWER,
        "TITLE-MARKER",
        "QUESTION-MARKER",
        "ANSWER-MARKER",
    ] {
        assert!(!output.contains(content), "{content} in the log");
    }
    println!("logged: {shared_line}\n{unshared_line}\n{copied_line}");
}

/// The previous release's `store::delete` (main `5d7bcd55`, before Laysi/trustiot-v2#650), word
/// for word: it sets `deleted_at` alone. `$1` tenant, `$2` conversation, `$3` user, `$4` the
/// retention in days.
const PREVIOUS_RELEASE_DELETE: &str = "UPDATE assistant.conversations
SET deleted_at = now()
WHERE tenant_id = $1::uuid AND conversation_id = $2::uuid AND user_id = $3::uuid
    AND deleted_at IS NULL
RETURNING conversation_id::text AS conversation_id, title, title_source, deleted_at,
    deleted_at + make_interval(days => $4) AS restorable_until";

/// The migration (the database's part of criteria 1 and 3): the service role may set `shared_at`
/// only to the transaction's now or NULL, only once per sharing, and never on a deleted
/// conversation; a delete that names `deleted_at` alone, as the previous release's does,
/// unshares it.
#[tokio::test(flavor = "multi_thread")]
async fn the_database_holds_the_sharing_rules() {
    let Some(api) = api().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = api.with_markers(&owner).await;
    let identity = owner.identity(&api.issuer);
    let attempt = |statement: &'static str| {
        let (api, identity) = (&api, &identity);
        async move {
            let mut tx = api.db.begin(identity).await.expect("a tenant transaction");
            let result = sqlx::query(statement)
                .bind(id.to_string())
                .execute(&mut *tx)
                .await;
            match result {
                Ok(done) => {
                    tx.commit().await.expect("commit");
                    Ok(done.rows_affected())
                }
                Err(error) => {
                    tx.rollback().await.expect("roll back");
                    Err(store::StoreError::from(error)
                        .sqlstate()
                        .unwrap_or("none")
                        .to_owned())
                }
            }
        }
    };
    let set = |at: &'static str| -> &'static str {
        match at {
            "past" => {
                "UPDATE assistant.conversations SET shared_at = now() - interval '1 hour'
                 WHERE conversation_id = $1::uuid"
            }
            "future" => {
                "UPDATE assistant.conversations SET shared_at = now() + interval '1 hour'
                 WHERE conversation_id = $1::uuid"
            }
            "now" => {
                "UPDATE assistant.conversations SET shared_at = now()
                 WHERE conversation_id = $1::uuid"
            }
            "null" => {
                "UPDATE assistant.conversations SET shared_at = NULL
                 WHERE conversation_id = $1::uuid"
            }
            _ => unreachable!("a known time"),
        }
    };
    assert_eq!(
        attempt(set("past")).await,
        Err("42501".into()),
        "back-dated"
    );
    assert_eq!(attempt(set("future")).await, Err("42501".into()), "future");
    assert_eq!(attempt(set("now")).await, Ok(1), "shared now");
    assert_eq!(
        attempt(set("now")).await,
        Err("42501".into()),
        "a later now() would move the time of a shared conversation"
    );
    // The previous release's delete, which sets `deleted_at` alone (a helm rollback, or its pods
    // beside this release's in a rolling update), deletes a shared conversation and unshares it.
    let mut tx = api.db.begin(&identity).await.expect("a tenant transaction");
    let deleted = sqlx::query(PREVIOUS_RELEASE_DELETE)
        .bind(owner.tenant.to_string())
        .bind(id.to_string())
        .bind(owner.id.to_string())
        .bind(30_i32)
        .fetch_optional(&mut *tx)
        .await
        .expect("the previous release's delete of a shared conversation");
    assert!(deleted.is_some(), "it deleted the conversation");
    tx.commit().await.expect("commit");
    let mut admin = api.product.superuser().await;
    let (unshared, gone): (bool, bool) = sqlx::query_as(
        "SELECT shared_at IS NULL, deleted_at IS NOT NULL FROM assistant.conversations
         WHERE conversation_id = $1::uuid",
    )
    .bind(id.to_string())
    .fetch_one(&mut admin)
    .await
    .expect("read the row");
    drop(admin);
    assert_eq!((unshared, gone), (true, true), "deleted and unshared");
    // A direct write of `shared_at` on the deleted conversation is still refused.
    assert_eq!(
        attempt(set("now")).await,
        Err("23514".into()),
        "a deleted conversation cannot be shared"
    );
    // The service role still cannot change anything else of a conversation's preamble.
    assert_eq!(
        attempt(
            "UPDATE assistant.conversations SET user_id = gen_random_uuid()
             WHERE conversation_id = $1::uuid"
        )
        .await,
        Err("42501".into()),
        "the owner cannot be changed"
    );
    assert!(api.ids(&owner, DELETED).await.contains(&id));
    println!("shared_at: the database's clock only, once per sharing, never when deleted");
}
