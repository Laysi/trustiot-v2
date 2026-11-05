//! The draft versions an answer lists (Laysi/trustiot-v2#1043; docs/architecture/ai-assistant.md,
//! 「儀表板」, 「回答帶出的元件」): every version of a draft the `dashboard` tool wrote while the
//! question was answered, in the order written, as `draft_versions` of the answer in
//! `GET /conversations/{id}` and a shared reader's `GET /shared-conversations/{id}`.
//!
//! - End to end, through the service binary: a stand-in AI gateway scripts the model, which
//!   calls the real `dashboard` tool (components only, so no check service), whose datasets run
//!   at a stand-in telemetry-query as the user through the question's run delegation (a stand-in
//!   identity service issues and exchanges it). One question writes draft A, then draft B, then
//!   a second version of A, and its answer lists A 1, B 1, A 2; a question that writes nothing
//!   lists none; a question that writes a draft and then fails leaves no answer, and no later
//!   answer lists its version, which stays in the conversation's drafts. A shared reader sees the
//!   same lists and can open each listed version; a copy lists none.
//! - Seeded as the superuser, through the service binary: answers stored before answers recorded
//!   their run's position list none; a cancelled run's and a discarded round's versions are
//!   listed nowhere; a version another conversation's draft records under this conversation's
//!   run, and this conversation's version under another conversation's run, are never listed.
//!
//! The cross-tenant probe is `answer_drafts_stay_in_their_tenant` in tests/cross_tenant.rs; the
//! database's rules for `answer_seq` are in tests/question_store.rs.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

mod dashboard_world;
mod service;
mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use assistant::api::conversations::{CONVERSATION_PATH, CONVERSATIONS_PATH};
use assistant::api::dashboards::{CONVERSATION_DASHBOARDS_PATH, DASHBOARD_PATH};
use assistant::api::questions::QUESTIONS_PATH;
use assistant::api::sharing::{COPY_PATH, SHARE_PATH, SHARED_CONVERSATION_PATH};
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use contract::api::identity::{CreateRunDelegation, ExchangeRunDelegation};
use contract::client::Endpoint as _;
use contract::identity::TenantRole;
use contract::{
    Actor, ErrorBody, ErrorCode, INTERNAL_IDENTITY_HEADER, InternalClaims, ServiceName, TenantId,
};
use dashboard_world::{Peers, spec};
use reqwest::Method;
use serde_json::{Value, json};
use service::{Reply, Service, User, wait_for};
use support::{Recorded, StandIn, WAIT, fixture, ok, respond, step};
use testkit::identity::TestIssuer;
use testkit::product::{ProductPostgres, product_postgres};
use tokio::net::TcpListener;
use uuid::Uuid;

/// A question with this writes draft A, then draft B, then a second version of A.
const THREE_WRITES: &str = "[three writes]";

/// A question with this writes draft C, then its model call fails.
const WRITE_THEN_FAIL: &str = "[write, then fail]";

/// The answer the recorded fixture gives.
const ANSWER: &str = "Yes. All twelve meters on floor 3 reported in the last hour.";

fn unix_now() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch");
    i64::try_from(elapsed.as_secs()).expect("seconds fit")
}

/// The stand-in identity service: issues a run delegation to the user whose token the assistant
/// forwards, and exchanges it for that user's tokens, as the identity service does (the run's
/// proof is not checked here).
struct Identity {
    issuer: Arc<TestIssuer>,
    /// The user of each delegation issued.
    delegations: Mutex<BTreeMap<String, InternalClaims>>,
}

fn refuse(code: ErrorCode) -> Response {
    let body = ErrorBody::new(code);
    (body.http_status(), axum::Json(body)).into_response()
}

fn token_of(headers: &HeaderMap) -> String {
    headers
        .get(INTERNAL_IDENTITY_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

async fn create_delegation(
    State(identity): State<Arc<Identity>>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    let Ok(user) = identity
        .issuer
        .verifier(ServiceName::Assistant)
        .verify(&token_of(&headers))
    else {
        return refuse(ErrorCode::Unauthenticated);
    };
    let run = body["run_id"].as_str().unwrap_or_default().replace('-', "");
    let delegation = format!("delegation.{run}.question");
    identity
        .delegations
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(delegation.clone(), user.claims().clone());
    let lifetime = body["lifetime_secs"].as_i64().unwrap_or_default();
    axum::Json(json!({"delegation": delegation, "expires_at": unix_now() + lifetime}))
        .into_response()
}

async fn exchange_delegation(
    State(identity): State<Arc<Identity>>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    let user = identity
        .delegations
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&token_of(&headers))
        .cloned();
    let Some(user) = user else {
        return refuse(ErrorCode::Unauthenticated);
    };
    let Ok(audience) = serde_json::from_value::<ServiceName>(body["audience"].clone()) else {
        return refuse(ErrorCode::InvalidRequest);
    };
    let iat = unix_now();
    let claims = InternalClaims {
        iss: ServiceName::Identity,
        sub: user.sub,
        aud: audience,
        iat,
        exp: iat + 60,
        tenant: user.tenant,
        platform_admin: false,
        act: Some(Actor {
            sub: ServiceName::Assistant,
        }),
        sid: None,
        tenant_suspended: false,
    };
    axum::Json(json!({"token": identity.issuer.token(&claims)})).into_response()
}

/// Serves the stand-in identity service; returns its origin.
async fn identity_stand_in(issuer: Arc<TestIssuer>) -> String {
    let identity = Arc::new(Identity {
        issuer,
        delegations: Mutex::default(),
    });
    let router = Router::new()
        .route(CreateRunDelegation::ROUTE, post(create_delegation))
        .route(ExchangeRunDelegation::ROUTE, post(exchange_delegation))
        .with_state(identity);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let address = listener.local_addr().expect("the local address");
    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("serve the identity stand-in");
    });
    format!("http://{address}")
}

/// The `daily` dataset of [`spec`] alone, which telemetry-query's stand-in answers with a `day`
/// and a `kwh` column.
fn daily() -> Value {
    let daily = spec()
        .datasets
        .into_iter()
        .find(|dataset| dataset.name.as_str() == "daily")
        .expect("the spec has `daily`");
    serde_json::to_value([daily]).expect("the dataset serializes")
}

/// A new draft of components only, titled `title`.
fn new_draft(title: &str) -> Value {
    json!({
        "action": "write",
        "title": title,
        "datasets": {"upsert": daily()},
        "components": [
            {"kind": "bar", "title": "Energy per day", "dataset": "daily", "x": "day",
             "y": ["kwh"], "unit": "kWh"},
        ],
    })
}

/// The next version of draft `id`, after its version `base`, titled `title`.
fn retitled(id: &str, base: u32, title: &str) -> Value {
    json!({
        "action": "write",
        "dashboard_id": id,
        "base_version": base,
        "title": title,
    })
}

/// The JSON of each tool result in this question's turn of `request`, in order: the messages
/// after the question (the last user message whose content is text).
fn turn(request: &Recorded) -> (String, Vec<Value>) {
    let body = request.json();
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let start = messages
        .iter()
        .rposition(|message| message["role"] == "user" && message["content"].is_string())
        .expect("a question");
    let question = messages[start]["content"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let results = messages[start + 1..]
        .iter()
        .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
        .filter(|block| block["type"] == "tool_result")
        .map(|block| {
            let text: String = block["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part["text"].as_str())
                .collect();
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        })
        .collect();
    (question, results)
}

/// The scripted model: each step of a question with [`THREE_WRITES`] or [`WRITE_THEN_FAIL`]
/// from the tool results the question got so far; the recorded answer otherwise.
fn model(request: &Recorded) -> support::Reply {
    let (question, results) = turn(request);
    let write = |index: usize, input: Value| ok(step(index, &[("dashboard", input)]));
    if question.contains(THREE_WRITES) {
        match results.as_slice() {
            [] => write(1, new_draft("Energy A")),
            [_a] => write(2, new_draft("Energy B")),
            [a, _b] => write(
                3,
                retitled(
                    a["dashboard_id"].as_str().unwrap_or_default(),
                    1,
                    "Energy A, weekly",
                ),
            ),
            _ => ok(fixture("answer_end_turn.json")),
        }
    } else if question.contains(WRITE_THEN_FAIL) {
        match results.as_slice() {
            [] => write(4, new_draft("Energy C")),
            _ => support::Reply::Json(
                400,
                json!({"type": "error", "error": {"type": "invalid_request_error",
                       "message": "refused"}})
                .to_string(),
            ),
        }
    } else {
        ok(fixture("answer_end_turn.json"))
    }
}

struct World {
    issuer: Arc<TestIssuer>,
    service: Service,
    gateway: StandIn,
    peers: Peers,
}

impl World {
    async fn call(&self, user: &User, method: Method, path: &str, body: Option<&Value>) -> Reply {
        self.service
            .as_user(&self.issuer, user, method, path, body)
            .await
    }

    async fn conversation(&self, user: &User) -> String {
        let created = self
            .call(user, Method::POST, CONVERSATIONS_PATH, Some(&json!({})))
            .await;
        created.expect_status(201).json()["id"]
            .as_str()
            .expect("an id")
            .to_owned()
    }

    /// Asks `question` and waits until its run ended `status`; the conversation then.
    async fn ask(&self, user: &User, id: &str, question: &str, status: &str) -> Value {
        let asked = self
            .call(
                user,
                Method::POST,
                &QUESTIONS_PATH.replace("{conversation_id}", id),
                Some(&json!({"question": question})),
            )
            .await;
        asked.expect_status(202);
        let run = asked.json()["id"]
            .as_str()
            .expect("the run's ID")
            .to_owned();
        let path = CONVERSATION_PATH.replace("{conversation_id}", id);
        let what = format!("the question {run} ending");
        let detail = wait_for(&what, WAIT, || async {
            let read = self.call(user, Method::GET, &path, None).await;
            read.expect_status(200);
            let detail = read.json();
            let latest = &detail["latest_run"];
            (latest["id"] == run.as_str() && latest["status"] != "running").then_some(detail)
        })
        .await;
        assert_eq!(detail["latest_run"]["status"], status, "{detail}");
        detail
    }

    /// The conversation's drafts: each title's ID.
    async fn drafts(&self, user: &User, id: &str) -> BTreeMap<String, String> {
        let listed = self
            .call(
                user,
                Method::GET,
                &CONVERSATION_DASHBOARDS_PATH.replace("{conversation_id}", id),
                None,
            )
            .await;
        listed.expect_status(200);
        listed.json()["dashboards"]
            .as_array()
            .expect("dashboards")
            .iter()
            .map(|draft| {
                (
                    draft["title"].as_str().expect("a title").to_owned(),
                    draft["dashboard_id"].as_str().expect("an ID").to_owned(),
                )
            })
            .collect()
    }
}

async fn world(product: &ProductPostgres) -> World {
    let issuer = Arc::new(TestIssuer::new());
    let peers = Peers::start(Arc::clone(&issuer)).await;
    let identity = identity_stand_in(Arc::clone(&issuer)).await;
    let gateway = StandIn::start(respond(|request, _index| model(request))).await;
    let service = Service::start(
        product,
        &issuer.jwks(),
        &[
            ("TRUSTIOT__AI_GATEWAY__URL", gateway.url.as_str()),
            ("TRUSTIOT__IDENTITY__EXCHANGE_URL", identity.as_str()),
            ("TRUSTIOT__TELEMETRY_QUERY__URL", peers.url.as_str()),
        ],
    )
    .await;
    World {
        issuer,
        service,
        gateway,
        peers,
    }
}

/// `(kind, text, draft_versions)` of each message of a conversation as read.
fn shown(detail: &Value) -> Vec<Value> {
    detail["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|message| {
            json!([
                message["kind"],
                message["text"],
                message.get("draft_versions")
            ])
        })
        .collect()
}

fn version(id: &str, version: u32) -> Value {
    json!({"dashboard_id": id, "version": version})
}

/// Acceptance criterion 1, end to end: the answer of a question that wrote draft A, draft B and
/// A's second version lists A 1, B 1, A 2, in that order; the answer of a question that wrote
/// nothing lists none, and so does the answer after a question that wrote a draft and failed.
/// A shared reader sees the same, and can open each listed version; a copy lists none.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_lists_the_draft_versions_its_question_wrote_in_order() {
    let Some(product) = product_postgres(&["assistant_app", "assistant_platform"]).await else {
        return;
    };
    let world = world(&product).await;
    let tenant = TenantId::generate();
    let owner = User::new(tenant, TenantRole::Viewer);
    let id = world.conversation(&owner).await;

    let question = format!("Show the energy per day {THREE_WRITES}");
    let first = world.ask(&owner, &id, &question, "answered").await;
    let drafts = world.drafts(&owner, &id).await;
    let (a, b) = (&drafts["Energy A, weekly"], &drafts["Energy B"]);
    assert_ne!(a, b);
    assert_eq!(
        shown(&first),
        [
            json!(["question", question, null]),
            json!([
                "answer",
                ANSWER,
                [version(a, 1), version(b, 1), version(a, 2)]
            ]),
        ],
        "{first}"
    );
    assert!(
        !first["messages"][1]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("kwh"),
        "the answer's text carries no spec"
    );
    // The dataset ran (once for the question, which reuses its result) as the owner, through
    // the run delegation.
    assert!(!world.peers.calls().is_empty(), "the dataset ran");
    assert!(
        world
            .peers
            .calls()
            .iter()
            .all(|call| call.tenant() == Some(tenant)),
        "every dataset ran in the owner's tenant"
    );
    assert!(
        world.peers.unexpected().is_empty(),
        "{:?}",
        world.peers.unexpected()
    );

    let second = world
        .ask(&owner, &id, "Did every meter report?", "answered")
        .await;
    assert_eq!(
        shown(&second)[3],
        json!(["answer", ANSWER, []]),
        "a question that wrote nothing: {second}"
    );

    let failing = format!("Make one more {WRITE_THEN_FAIL}");
    let failed = world.ask(&owner, &id, &failing, "failed").await;
    assert_eq!(
        failed["messages"], second["messages"],
        "a failed question left no answer"
    );
    let drafts = world.drafts(&owner, &id).await;
    let c = drafts
        .get("Energy C")
        .expect("the failed question's draft stays")
        .clone();
    let after = world
        .ask(&owner, &id, "And the meters on floor 4?", "answered")
        .await;
    let messages = shown(&after);
    assert_eq!(messages.len(), 6, "{after}");
    assert_eq!(messages[5], json!(["answer", ANSWER, []]), "{after}");
    assert!(
        !after.to_string().contains(&c),
        "no answer lists the failed question's version: {after}"
    );

    // A shared reader: the same lists, and each listed version opens.
    world
        .call(
            &owner,
            Method::PUT,
            &SHARE_PATH.replace("{conversation_id}", &id),
            None,
        )
        .await
        .expect_status(200);
    let reader = User::new(tenant, TenantRole::Viewer);
    let view = world
        .call(
            &reader,
            Method::GET,
            &SHARED_CONVERSATION_PATH.replace("{conversation_id}", &id),
            None,
        )
        .await;
    view.expect_status(200);
    assert_eq!(view.json()["messages"], after["messages"], "{}", view.body);
    for listed in after["messages"][1]["draft_versions"]
        .as_array()
        .expect("the first answer's versions")
    {
        let dashboard = listed["dashboard_id"].as_str().expect("an ID");
        let opened = world
            .call(
                &reader,
                Method::GET,
                &format!(
                    "{}?version={}",
                    DASHBOARD_PATH.replace("{dashboard_id}", dashboard),
                    listed["version"]
                ),
                None,
            )
            .await;
        opened.expect_status(200);
    }

    // A copy has no drafts: its answers list none.
    let copied = world
        .call(
            &reader,
            Method::POST,
            &COPY_PATH.replace("{conversation_id}", &id),
            Some(&json!({})),
        )
        .await;
    copied.expect_status(201);
    let copy_id = copied.json()["id"]
        .as_str()
        .expect("the copy's ID")
        .to_owned();
    let copy = world
        .call(
            &reader,
            Method::GET,
            &CONVERSATION_PATH.replace("{conversation_id}", &copy_id),
            None,
        )
        .await;
    copy.expect_status(200);
    let copy = copy.json();
    let answers: Vec<&Value> = copy["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|message| message["kind"] == "answer")
        .collect();
    assert_eq!(answers.len(), 3, "{copy}");
    for answer in answers {
        assert_eq!(answer["draft_versions"], json!([]), "{copy}");
    }
    assert!(
        world.gateway.requests().len() >= 7,
        "the model was asked every step"
    );
    println!("first answer: {}", first["messages"][1]);
}

/// Seeds, as the superuser, conversation `$2` of user `$3` in tenant `$1` (`$4` another
/// conversation of the same user), each as a question API stored it, with runs and draft
/// versions:
///
/// | position | conversation `$2` | its run |
/// | --- | --- | --- |
/// | 0, 1 | question, answer | `$5`, answered, `answer_seq` 1 |
/// | 2, 3 | question, answer | `$6`, answered before answers recorded it (`answer_seq` NULL) |
/// | 4, 5 | question, answer, discarded | `$7`, answered, `answer_seq` 5 |
/// | 6, 7 | question, answer | `$8`, answered, `answer_seq` 7 |
/// | - | - | `$9`, cancelled |
///
/// Draft `$10` of `$2` has versions 1 to 5 by `$5`, `$6`, `$7`, `$9` and `$8`, then a restore
/// (6). Conversation `$4` has a question and its answer (run `$11`, `answer_seq` 1) and draft
/// `$12` with version 1 by `$5` (`$2`'s run) and version 2 by `$11`; `$2`'s draft `$13` has
/// version 1 by `$11` (`$4`'s run).
const SEED: &str = "WITH conversations AS (
    INSERT INTO assistant.conversations
        (tenant_id, conversation_id, user_id, prompt_version, system_prompt, tenant_context,
         tools, effort, cache_ttl, title, title_source)
    SELECT $1::uuid, id, $3::uuid, 'seed', 'seed', '', '[]', 'medium', '5m', 'Seeded', 'user'
    FROM unnest(ARRAY[$2::uuid, $4::uuid]) AS id
    RETURNING tenant_id, conversation_id
), messages AS (
    INSERT INTO assistant.messages (tenant_id, conversation_id, seq, kind, format, body)
    SELECT $1::uuid, m.conversation_id, m.seq, m.kind, 'anthropic_messages',
        convert_to(CASE m.kind
            WHEN 'question' THEN '{\"role\":\"user\",\"content\":\"Question ' || m.seq || '\"}'
            ELSE '{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Answer '
                || m.seq || '\"}]}'
        END, 'UTF8')
    FROM conversations c
    JOIN (SELECT $2::uuid AS conversation_id, n AS seq,
                 CASE WHEN n % 2 = 0 THEN 'question' ELSE 'answer' END AS kind
          FROM generate_series(0, 7) AS n
          UNION ALL
          SELECT $4::uuid, n, CASE WHEN n = 0 THEN 'question' ELSE 'answer' END
          FROM generate_series(0, 1) AS n) m ON m.conversation_id = c.conversation_id
    RETURNING conversation_id, seq
), discarded AS (
    INSERT INTO assistant.discarded_rounds (tenant_id, conversation_id, from_seq, to_seq)
    SELECT $1::uuid, $2::uuid, 4, 5
    WHERE (SELECT count(*) FROM messages) = 10
    RETURNING from_seq
), runs AS (
    INSERT INTO assistant.runs
        (tenant_id, run_id, conversation_id, user_id, status, deadline_at, ended_at, answer_seq)
    SELECT $1::uuid, r.run_id, r.conversation_id, $3::uuid, r.status,
        now() + interval '10 minutes', now(), r.answer_seq
    FROM (VALUES ($5::uuid, $2::uuid, 'answered', 1), ($6::uuid, $2::uuid, 'answered', NULL),
                 ($7::uuid, $2::uuid, 'answered', 5), ($8::uuid, $2::uuid, 'answered', 7),
                 ($9::uuid, $2::uuid, 'cancelled', NULL), ($11::uuid, $4::uuid, 'answered', 1))
        AS r (run_id, conversation_id, status, answer_seq)
    WHERE (SELECT count(*) FROM discarded) = 1
    RETURNING run_id
), dashboards AS (
    INSERT INTO assistant.dashboards (tenant_id, dashboard_id, kind, conversation_id, created_by)
    SELECT $1::uuid, d.dashboard_id, 'draft', d.conversation_id, $3::uuid
    FROM (VALUES ($10::uuid, $2::uuid), ($12::uuid, $4::uuid), ($13::uuid, $2::uuid))
        AS d (dashboard_id, conversation_id)
    WHERE (SELECT count(*) FROM runs) = 6
    RETURNING dashboard_id
), contents AS (
    INSERT INTO assistant.dashboard_versions
        (tenant_id, dashboard_id, version, origin, runtime, title, spec, page, created_by,
         run_id)
    SELECT $1::uuid, v.dashboard_id, v.version, 'agent', 1, 'Seeded ' || v.version,
        '{\"datasets\":[{\"name\":\"seed\",\"title\":\"Seed\",\"description\":\"A seeded dataset.\",\"max_rows\":10,\"source\":{\"kind\":\"sql\",\"sql\":\"SELECT 1 AS one\"}}]}',
        '<main><h1>seed</h1></main>', $3::uuid, v.run_id
    FROM (VALUES ($10::uuid, 1, $5::uuid), ($10::uuid, 2, $6::uuid), ($10::uuid, 3, $7::uuid),
                 ($10::uuid, 4, $9::uuid), ($10::uuid, 5, $8::uuid),
                 ($12::uuid, 1, $5::uuid), ($12::uuid, 2, $11::uuid), ($13::uuid, 1, $11::uuid))
        AS v (dashboard_id, version, run_id)
    WHERE (SELECT count(*) FROM dashboards) = 3
    RETURNING dashboard_id
)
INSERT INTO assistant.dashboard_versions
    (tenant_id, dashboard_id, version, origin, restored_from, content_version, created_by)
SELECT $1::uuid, $10::uuid, 6, 'restore', 1, 1, $3::uuid
WHERE (SELECT count(*) FROM contents) = 8";

/// Acceptance criteria 1 and 2 on seeded rows: an answer lists only versions its own run wrote
/// into this conversation's drafts. An answer stored before answers recorded their run lists
/// none; a cancelled run's and a discarded round's versions are under no answer; a version that
/// names this conversation's run in another conversation's draft, and one in this
/// conversation's draft that names another conversation's run, are never listed. The shared
/// reader sees the same.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_lists_only_its_own_runs_versions_of_this_conversations_drafts() {
    let Some(product) = product_postgres(&["assistant_app", "assistant_platform"]).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let service = Service::start(&product, &issuer.jwks(), &[]).await;
    let tenant = TenantId::generate();
    let owner = User::new(tenant, TenantRole::Viewer);
    let [
        x,
        y,
        r1,
        r_old,
        r_discarded,
        r3,
        r_cancelled,
        r_y,
        draft,
        draft_y,
        draft_x2,
    ] = std::array::from_fn::<Uuid, 11, _>(|_| Uuid::now_v7());
    let mut admin = product.superuser().await;
    sqlx::query(SEED)
        .bind(tenant.to_string())
        .bind(x.to_string())
        .bind(owner.id.to_string())
        .bind(y.to_string())
        .bind(r1.to_string())
        .bind(r_old.to_string())
        .bind(r_discarded.to_string())
        .bind(r3.to_string())
        .bind(r_cancelled.to_string())
        .bind(draft.to_string())
        .bind(r_y.to_string())
        .bind(draft_y.to_string())
        .bind(draft_x2.to_string())
        .execute(&mut admin)
        .await
        .expect("seed the conversations");
    let read = |user: User, path: String| {
        let (service, issuer) = (&service, &issuer);
        async move {
            let reply = service
                .as_user(issuer, &user, Method::GET, &path, None)
                .await;
            reply.expect_status(200);
            reply.json()
        }
    };
    let path = |id: Uuid| CONVERSATION_PATH.replace("{conversation_id}", &id.to_string());
    let draft = draft.to_string();
    let draft_y = draft_y.to_string();

    let detail = read(owner, path(x)).await;
    assert_eq!(
        shown(&detail),
        [
            json!(["question", "Question 0", null]),
            json!(["answer", "Answer 1", [version(&draft, 1)]]),
            json!(["question", "Question 2", null]),
            json!(["answer", "Answer 3", []]),
            json!(["question", "Question 6", null]),
            json!(["answer", "Answer 7", [version(&draft, 5)]]),
        ],
        "{detail}"
    );
    let other = read(owner, path(y)).await;
    assert_eq!(
        shown(&other),
        [
            json!(["question", "Question 0", null]),
            json!(["answer", "Answer 1", [version(&draft_y, 2)]]),
        ],
        "{other}"
    );

    // The shared reader sees the same lists.
    service
        .as_user(
            &issuer,
            &owner,
            Method::PUT,
            &SHARE_PATH.replace("{conversation_id}", &x.to_string()),
            None,
        )
        .await
        .expect_status(200);
    let reader = User::new(tenant, TenantRole::Admin);
    let view = read(
        reader,
        SHARED_CONVERSATION_PATH.replace("{conversation_id}", &x.to_string()),
    )
    .await;
    assert_eq!(view["messages"], detail["messages"], "{view}");
    // Not shared: the reader gets the one 404, and `y`'s lists stay its owner's.
    let refused = service
        .as_user(
            &issuer,
            &reader,
            Method::GET,
            &SHARED_CONVERSATION_PATH.replace("{conversation_id}", &y.to_string()),
            None,
        )
        .await;
    refused.expect_error(404, "not_found", "error.not_found");
    println!("seeded answers: {}", detail["messages"]);
}
