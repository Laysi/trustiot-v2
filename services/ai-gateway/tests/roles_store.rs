//! The model roles' stored settings (README.md, 「資料庫」 and 「多副本」) against a real
//! Postgres with the gateway's migration: what a change writes and refuses, the versions, what
//! the service role may do to the change log, and what a forwarded request does with the
//! settings: it uses a change made before it started, keeps its own snapshot while it runs,
//! keeps a model's concurrency count across reloads, and is refused with 503 without calling a
//! provider when the database cannot be read.
//!
//! The two-process and restart cases run the real binary (tests/process.rs). These tests skip
//! without podman or docker, and fail without them when `CI` is set.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ai_gateway::config::{Protocol, RoleRules};
use ai_gateway::store::{Changed, Store, StoreError};
use ai_gateway::{CONFIGURED_ONLY, MODEL_HEADER, TENANT_HEADER};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sqlx::{Connection, PgConnection};
use support::database::{Database, models, set_models};
use support::{
    DECISIONS, MESSAGES, Server, Step, TENANT, Vars, WAIT, fixture, json_reply, post, sse_head,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;
use tower::ServiceExt;

const BODY: &str = r#"{"max_tokens":16,"messages":[{"role":"user","content":"Hi"}]}"#;
const STREAM_BODY: &str = r#"{"stream":true,"max_tokens":16,"messages":[]}"#;

/// SQLSTATE `insufficient_privilege`.
const INSUFFICIENT_PRIVILEGE: &str = "42501";

const COUNT_ROLES: &str = "SELECT count(*) FROM ai_gateway.role_models";
const COUNT_CHANGES: &str = "SELECT count(*) FROM ai_gateway.role_model_changes";
const READ_GENERATION: &str = "SELECT generation FROM ai_gateway.settings_generation";
const READ_CHANGES: &str = "SELECT version, role, action, models, changed_by
FROM ai_gateway.role_model_changes ORDER BY version";
const UPDATE_CHANGES: &str = "UPDATE ai_gateway.role_model_changes SET changed_by = 'someone'";
const DELETE_CHANGES: &str = "DELETE FROM ai_gateway.role_model_changes";
const TRUNCATE_CHANGES: &str = "TRUNCATE ai_gateway.role_model_changes";
/// A reset keeps the role's row, so the service role may not delete one either.
const DELETE_ROLES: &str = "DELETE FROM ai_gateway.role_models";
const TRUNCATE_ROLES: &str = "TRUNCATE ai_gateway.role_models";

/// A row of the change log: version, role, action, models, operator.
type ChangeRecord = (i64, String, String, Option<Vec<String>>, String);

fn message_reply(request_id: &str) -> Vec<Step> {
    vec![Step::Send(json_reply(
        &fixture("anthropic/message_end_turn.json"),
        "request-id",
        request_id,
    ))]
}

fn decision_reply() -> Vec<Step> {
    vec![Step::Send(json_reply(
        &fixture("decisions/router.response.json"),
        "x-request-id",
        "req_decision",
    ))]
}

fn model_of(server: &Server, index: usize) -> String {
    server.request(index).json()["model"]
        .as_str()
        .expect("a model in the request body")
        .to_owned()
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

/// Providers `first` and `second` (Anthropic Messages) and `openai` (OpenAI Decisions); role
/// `main` on `first/model-one` and role `router` on `openai/gpt-6-luna`.
fn rules() -> RoleRules {
    RoleRules {
        providers: [
            ("first".to_owned(), Protocol::AnthropicMessages),
            ("second".to_owned(), Protocol::AnthropicMessages),
            ("openai".to_owned(), Protocol::OpenaiDecisions),
        ]
        .into(),
        roles: [
            ("main".to_owned(), Protocol::AnthropicMessages),
            ("router".to_owned(), Protocol::OpenaiDecisions),
        ]
        .into(),
    }
}

#[tokio::test]
async fn refused_changes_write_nothing_and_each_change_writes_one_record() {
    let Some(database) = Database::new(
        "CREATE DATABASE ai_gateway_refused OWNER ai_gateway_owner",
        "ai_gateway_refused",
    )
    .await
    else {
        return;
    };
    let store = database.store();
    let rules = rules();
    let mut app = database.connect("ai_gateway_app").await;

    let refused = [
        // Another protocol's model.
        ("main", models(&["openai/gpt-6-luna"]), "test-admin"),
        // A provider that is not configured.
        ("main", models(&["missing/model-one"]), "test-admin"),
        // Too many, twice the same, a malformed one, a model ID with a comma.
        ("main", models(&["first/a"; 9]), "test-admin"),
        ("main", models(&["first/a", "first/a"]), "test-admin"),
        ("main", models(&["first"]), "test-admin"),
        ("main", models(&["first/a,b"]), "test-admin"),
        // A role the configuration does not have.
        ("other", models(&["first/model-one"]), "test-admin"),
        // An operator that cannot be recorded.
        ("main", models(&["second/model-two"]), ""),
    ];
    for (role, list, operator) in &refused {
        let error = store
            .set_models(&rules, role, list, 0, operator)
            .await
            .expect_err("the change is refused");
        assert!(
            matches!(
                error,
                StoreError::InvalidModels(_)
                    | StoreError::UnknownRole
                    | StoreError::InvalidOperator
            ),
            "{role} {list:?}: {error}"
        );
        assert_eq!(counts(&mut app).await, (0, 0, 0), "{role} {list:?}");
    }
    let wrong = store
        .set_models(&rules, "main", &models(&["openai/gpt-6-luna"]), 0, "a")
        .await
        .expect_err("another protocol");
    assert!(wrong.to_string().contains("anthropic-messages"), "{wrong}");

    let version = set_models(&store, &rules, "main", &["second/model-two"], 0).await;
    assert_eq!(version, 1);
    assert_eq!(counts(&mut app).await, (1, 1, 1));

    // A stale version is a conflict, and writes nothing either.
    let conflict = store
        .set_models(
            &rules,
            "main",
            &models(&["first/model-one"]),
            0,
            "test-admin",
        )
        .await
        .expect_err("a conflict");
    assert!(
        matches!(
            conflict,
            StoreError::Conflict {
                expected: 0,
                current: 1
            }
        ),
        "{conflict}"
    );
    assert_eq!(counts(&mut app).await, (1, 1, 1));

    assert_eq!(
        store.reset("main", 1, "other-admin").await.expect("reset"),
        Changed::Written { version: 2 }
    );
    // The row stays, without models, at the reset's version.
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
                "test-admin".to_owned()
            ),
            (
                2,
                "main".to_owned(),
                "reset".to_owned(),
                None,
                "other-admin".to_owned()
            ),
        ]
    );

    // Resetting a role without stored models (reset already, or never set) changes nothing and
    // records nothing; the version check still applies.
    assert_eq!(
        store.reset("main", 2, "test-admin").await.expect("reset"),
        Changed::Unchanged
    );
    assert_eq!(
        store.reset("router", 0, "test-admin").await.expect("reset"),
        Changed::Unchanged
    );
    assert!(matches!(
        store.reset("main", 0, "test-admin").await,
        Err(StoreError::Conflict {
            expected: 0,
            current: 2
        })
    ));
    assert_eq!(counts(&mut app).await, (2, 1, 2));
    assert!(matches!(
        store.reset("Not A Role", 0, "test-admin").await,
        Err(StoreError::InvalidRole)
    ));
}

#[tokio::test]
async fn versions_keep_rising_after_a_reset() {
    let Some(database) = Database::new(
        "CREATE DATABASE ai_gateway_versions OWNER ai_gateway_owner",
        "ai_gateway_versions",
    )
    .await
    else {
        return;
    };
    let store = database.store();
    let rules = rules();

    let before_reset = set_models(&store, &rules, "main", &["second/model-two"], 0).await;
    // Another role's change moves the generation too.
    set_models(&store, &rules, "router", &["openai/gpt-6-sol"], 0).await;
    let Changed::Written { version: reset } = store
        .reset("main", before_reset, "test-admin")
        .await
        .expect("reset")
    else {
        panic!("the reset wrote nothing");
    };
    assert!(reset > before_reset);

    // After the reset the role has no stored models but keeps the reset's version: a change that
    // still carries the version from before the reset is a conflict, and so is one based on a
    // read from before the role was ever set (version 0), which would otherwise undo the set and
    // the reset unnoticed.
    let stored = store.load().await.expect("load");
    let main = stored
        .roles
        .iter()
        .find(|role| role.role == "main")
        .expect("main keeps its row");
    assert_eq!((main.models.as_ref(), main.version), (None, reset));
    for expected in [before_reset, 0] {
        let stale = store
            .set_models(
                &rules,
                "main",
                &models(&["first/model-three"]),
                expected,
                "late",
            )
            .await
            .expect_err("a conflict");
        assert!(
            matches!(stale, StoreError::Conflict { current, .. } if current == reset),
            "{expected}: {stale}"
        );
    }
    let after_reset = set_models(&store, &rules, "main", &["first/model-three"], reset).await;
    assert!(
        after_reset > reset && after_reset > before_reset,
        "{before_reset} {reset} {after_reset}"
    );
    let stale = store
        .set_models(
            &rules,
            "main",
            &models(&["second/model-two"]),
            before_reset,
            "late",
        )
        .await
        .expect_err("a conflict");
    assert!(
        matches!(stale, StoreError::Conflict { current, .. } if current == after_reset),
        "{stale}"
    );
    let stale_reset = store
        .reset("main", before_reset, "late")
        .await
        .expect_err("a conflict");
    assert!(
        matches!(stale_reset, StoreError::Conflict { .. }),
        "{stale_reset}"
    );

    let stored = store.load().await.expect("load");
    assert_eq!(stored.generation, after_reset);
    let main = stored
        .roles
        .iter()
        .find(|role| role.role == "main")
        .expect("main is stored");
    assert_eq!(main.version, after_reset);
    assert_eq!(main.models, Some(models(&["first/model-three"])));
    assert_eq!(main.updated_by, "test-admin");
    assert!(main.updated_at.ends_with('Z'), "{}", main.updated_at);
}

#[tokio::test]
async fn the_service_role_cannot_rewrite_or_remove_the_change_log() {
    let Some(database) = Database::new(
        "CREATE DATABASE ai_gateway_log OWNER ai_gateway_owner",
        "ai_gateway_log",
    )
    .await
    else {
        return;
    };
    let store = database.store();
    set_models(&store, &rules(), "main", &["second/model-two"], 0).await;

    let mut app = database.connect("ai_gateway_app").await;
    for statement in [
        UPDATE_CHANGES,
        DELETE_CHANGES,
        TRUNCATE_CHANGES,
        DELETE_ROLES,
        TRUNCATE_ROLES,
    ] {
        let error = sqlx::query(statement)
            .execute(&mut app)
            .await
            .expect_err("the service role may not do this");
        let code = error
            .as_database_error()
            .and_then(|error| error.code().map(|code| code.into_owned()));
        assert_eq!(
            code.as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{statement}: {error}"
        );
    }
    assert_eq!(counts(&mut app).await, (1, 1, 1));
}

/// UPDATEs that would move the generation or a role's version back, keep them, or rename a role.
const NOT_RISING: [&str; 5] = [
    "UPDATE ai_gateway.settings_generation SET generation = generation",
    "UPDATE ai_gateway.settings_generation SET generation = generation - 1",
    "UPDATE ai_gateway.role_models SET version = version WHERE role = 'main'",
    "UPDATE ai_gateway.role_models SET version = version - 1, models = NULL WHERE role = 'main'",
    "UPDATE ai_gateway.role_models SET models = NULL WHERE role = 'main'",
];
// `role` comes second: sql-check reads `SET role` as switching the session role.
const RENAME_ROLE: &str =
    "UPDATE ai_gateway.role_models SET version = version + 1, role = 'router' WHERE role = 'main'";
const READ_MAIN_VERSION: &str = "SELECT version FROM ai_gateway.role_models WHERE role = 'main'";

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|error| error.code().map(|code| code.into_owned()))
}

#[tokio::test]
async fn the_generation_and_the_versions_only_rise_for_every_role() {
    let Some(database) = Database::new(
        "CREATE DATABASE ai_gateway_rising OWNER ai_gateway_owner",
        "ai_gateway_rising",
    )
    .await
    else {
        return;
    };
    let store = database.store();
    let version = set_models(&store, &rules(), "main", &["second/model-two"], 0).await;

    // The triggers hold the owner to the same order as the service role.
    for role in ["ai_gateway_app", "ai_gateway_owner"] {
        let mut connection = database.connect(role).await;
        for statement in NOT_RISING {
            let error = sqlx::query(statement)
                .execute(&mut connection)
                .await
                .expect_err("an UPDATE that does not raise the number");
            assert_eq!(
                sqlstate(&error).as_deref(),
                Some(INSUFFICIENT_PRIVILEGE),
                "{role}: {statement}: {error}"
            );
        }
        // The service role may not change the role column at all; the owner may not either,
        // because the trigger keeps it.
        let error = sqlx::query(RENAME_ROLE)
            .execute(&mut connection)
            .await
            .expect_err("renaming a role");
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{role}: {error}"
        );
        assert_eq!(counts(&mut connection).await, (version, 1, 1), "{role}");
        let kept: i64 = sqlx::query_scalar(READ_MAIN_VERSION)
            .fetch_one(&mut connection)
            .await
            .expect("read the version");
        assert_eq!(kept, version, "{role}");
    }

    // What the store does still works: the next change raises both.
    let next = set_models(&store, &rules(), "main", &["first/model-one"], version).await;
    assert!(next > version);
}

/// What a migration's lock does to every reader of the settings.
const LOCK_GENERATION: &str = "LOCK TABLE ai_gateway.settings_generation IN ACCESS EXCLUSIVE MODE";
/// The longest `database.timeout`, which a read bounded by an earlier deadline must not wait for.
const LONGEST_DATABASE_TIMEOUT: Duration = Duration::from_secs(60);
/// The deadline the reads below are given: far shorter than `LONGEST_DATABASE_TIMEOUT`.
const READ_DEADLINE: Duration = Duration::from_millis(500);
/// The server's own timeouts, which are equal, so either may end the wait first: SQLSTATE 55P03
/// (lock_not_available, `lock_timeout`) or 57014 (query_canceled, `statement_timeout`).
const SERVER_TIMEOUTS: [&str; 2] = ["55P03", "57014"];
/// Well before `LONGEST_DATABASE_TIMEOUT`, so that a read the server bounded by that timeout
/// instead of the deadline fails here rather than racing it.
const ENDS_WITHIN: Duration = Duration::from_secs(50);

#[tokio::test]
async fn the_server_stops_a_blocked_read_at_the_calls_deadline() {
    let Some(database) = Database::new(
        "CREATE DATABASE ai_gateway_deadline OWNER ai_gateway_owner",
        "ai_gateway_deadline",
    )
    .await
    else {
        return;
    };
    let store = Store::with_timeout(database.app_pool(), LONGEST_DATABASE_TIMEOUT);
    let mut owner = database.connect("ai_gateway_owner").await;
    let mut lock = owner.begin().await.expect("begin the lock's transaction");
    sqlx::query(LOCK_GENERATION)
        .execute(&mut *lock)
        .await
        .expect("lock the generation");

    // Nothing on this side cancels either read before `ENDS_WITHIN`: only the server's own
    // timeouts, set from the deadline, can end them that early.
    let read = tokio::time::timeout(
        ENDS_WITHIN,
        store.generation_by(Instant::now() + READ_DEADLINE),
    )
    .await
    .expect("the generation read ended before database.timeout")
    .expect_err("the generation is locked");
    assert!(
        sqlstate(&read).is_some_and(|code| SERVER_TIMEOUTS.contains(&code.as_str())),
        "{read}"
    );
    let load = tokio::time::timeout(ENDS_WITHIN, store.load_by(Instant::now() + READ_DEADLINE))
        .await
        .expect("the load ended before database.timeout")
        .expect_err("the generation is locked");
    assert!(
        sqlstate(&load).is_some_and(|code| SERVER_TIMEOUTS.contains(&code.as_str())),
        "{load}"
    );

    lock.rollback().await.expect("release the lock");
    assert_eq!(store.generation().await.expect("read after the lock"), 0);
}

#[tokio::test]
async fn a_change_is_used_by_the_next_request_and_only_then_reloaded() {
    let Some(database) = Database::new(
        "CREATE DATABASE ai_gateway_next OWNER ai_gateway_owner",
        "ai_gateway_next",
    )
    .await
    else {
        return;
    };
    let first = Server::start(vec![message_reply("req_1"), message_reply("req_2")]).await;
    let second = Server::start(vec![message_reply("req_3")]).await;
    let (gateway, router) = Vars::new()
        .database(&database)
        .provider("first", "anthropic-messages", &first)
        .provider("second", "anthropic-messages", &second)
        .role("main", "first/model-one")
        .live();
    assert_eq!(gateway.current().generation(), CONFIGURED_ONLY);

    let reply = post(&router, MESSAGES, &[], BODY).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let loaded = gateway.current();
    assert_eq!(loaded.generation(), 0);
    let reply = post(&router, MESSAGES, &[], BODY).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    // The same generation: the snapshot in memory, not a new one.
    assert!(Arc::ptr_eq(&loaded, &gateway.current()));
    assert_eq!(model_of(&first, 1), "model-one");

    let version = set_models(
        &database.store(),
        gateway.rules(),
        "main",
        &["second/model-two"],
        0,
    )
    .await;
    let reply = post(&router, MESSAGES, &[], BODY).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.header(MODEL_HEADER), Some("second/model-two"));
    assert_eq!(model_of(&second, 0), "model-two");
    assert_eq!(first.request_count(), 2);
    let snapshot = gateway.current();
    assert_eq!(snapshot.generation(), version);
    assert_eq!(
        snapshot.route("main").expect("main").stored_version,
        Some(version)
    );
}

/// A TCP proxy in front of Postgres that can be cut: every connection it carries is closed and
/// its port then refuses new ones, as when the database goes away.
struct Proxy {
    port: u16,
    cut: watch::Sender<bool>,
}

impl Proxy {
    async fn start(upstream: std::net::SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the proxy");
        let port = listener.local_addr().expect("the proxy address").port();
        let (cut, cut_seen) = watch::channel(false);
        let mut accept_cut = cut_seen.clone();
        tokio::spawn(async move {
            loop {
                let client = tokio::select! {
                    accepted = listener.accept() => match accepted {
                        Ok((client, _)) => client,
                        Err(_) => return,
                    },
                    _ = accept_cut.wait_for(|cut| *cut) => return,
                };
                let mut connection_cut = cut_seen.clone();
                tokio::spawn(async move {
                    let Ok(mut server) = TcpStream::connect(upstream).await else {
                        return;
                    };
                    let mut client = client;
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut client, &mut server) => {}
                        _ = connection_cut.wait_for(|cut| *cut) => {}
                    }
                });
            }
        });
        Self { port, cut }
    }

    /// Closes every connection and stops listening, so the port refuses new connections.
    fn cut(&self) {
        let _ = self.cut.send(true);
    }
}

#[tokio::test]
async fn no_provider_is_called_while_the_database_cannot_be_read() {
    let Some(database) = Database::new(
        "CREATE DATABASE ai_gateway_down OWNER ai_gateway_owner",
        "ai_gateway_down",
    )
    .await
    else {
        return;
    };
    let anthropic = Server::start(vec![message_reply("req_1")]).await;
    let openai = Server::start(vec![decision_reply()]).await;
    let proxy = Proxy::start(database.postgres.address).await;
    let (_gateway, router) = Vars::new()
        .database(&database)
        .set("AI_GATEWAY_POSTGRES_PORT", proxy.port.to_string())
        // The settings check gives up after this; the test configures it, and the calls below
        // must be answered after it, never before the database was tried.
        .set("AI_GATEWAY__DATABASE__TIMEOUT", "3s")
        .provider("first", "anthropic-messages", &anthropic)
        .provider("openai", "openai-decisions", &openai)
        .role("main", "first/model-one")
        .role("router", "openai/gpt-6-luna")
        .live();
    let decision = fixture("decisions/router.request.json");

    // Both protocols work while the database answers.
    let reply = post(&router, MESSAGES, &[], BODY).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let reply = post(&router, DECISIONS, &[], &decision).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());

    proxy.cut();
    for _ in 0..2 {
        let reply = post(&router, MESSAGES, &[], BODY).await;
        assert_eq!(
            reply.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            reply.text()
        );
        let body = reply.json();
        assert_eq!(body["type"], "error", "{body}");
        assert_eq!(body["error"]["type"], "api_error", "{body}");

        let reply = post(&router, DECISIONS, &[], &decision).await;
        assert_eq!(
            reply.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            reply.text()
        );
        let body = reply.json();
        assert_eq!(body["error"]["type"], "server_error", "{body}");
        assert!(body["error"]["param"].is_null(), "{body}");
    }
    assert_eq!(anthropic.request_count(), 1);
    assert_eq!(openai.request_count(), 1);

    // A gateway that never reached its database refuses too.
    let unreachable = Server::start(vec![message_reply("req_never")]).await;
    let (_gateway, router) = Vars::new()
        .provider("first", "anthropic-messages", &unreachable)
        .role("main", "first/model-one")
        .live();
    let reply = post(&router, MESSAGES, &[], BODY).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.text()
    );
    assert_eq!(unreachable.request_count(), 0);
}

/// A provider that streams the first part of a response, waits until the test releases it, and
/// then streams the rest: a stream that is in flight while the test changes the settings.
struct HeldStream {
    addr: std::net::SocketAddr,
    release: Option<oneshot::Sender<()>>,
    body: Arc<Mutex<Option<serde_json::Value>>>,
}

impl HeldStream {
    async fn start(first: String, rest: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("the address");
        let (release, released) = oneshot::channel::<()>();
        let body = Arc::new(Mutex::new(None));
        let recorded = Arc::clone(&body);
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = Vec::new();
            let head_end = loop {
                if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                    break end + 4;
                }
                let mut chunk = [0u8; 4096];
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                }
            };
            let head = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut request = buffer[head_end..].to_vec();
            while request.len() < length {
                let mut chunk = [0u8; 4096];
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => request.extend_from_slice(&chunk[..n]),
                }
            }
            *recorded.lock().expect("the body lock") = serde_json::from_slice(&request).ok();
            let mut out = sse_head();
            out.extend_from_slice(first.as_bytes());
            if socket.write_all(&out).await.is_err() {
                return;
            }
            let _ = released.await;
            let _ = socket.write_all(rest.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        Self {
            addr,
            release: Some(release),
            body,
        }
    }
}

#[tokio::test]
async fn a_stream_keeps_its_model_after_a_change() {
    let Some(database) = Database::new(
        "CREATE DATABASE ai_gateway_stream OWNER ai_gateway_owner",
        "ai_gateway_stream",
    )
    .await
    else {
        return;
    };
    let text = support::sse_without_comments(&fixture("anthropic/stream_text.sse"));
    let split = text
        .find("event: content_block_stop\n")
        .expect("the fixture has a content_block_stop");
    let (first_part, rest) = text.split_at(split);
    let mut held = HeldStream::start(first_part.to_owned(), rest.to_owned()).await;
    let second = Server::start(vec![message_reply("req_after")]).await;
    let (_gateway, router) = Vars::new()
        .database(&database)
        .set(
            "AI_GATEWAY__PROVIDERS__FIRST__PROTOCOL",
            "anthropic-messages",
        )
        .set("AI_GATEWAY__PROVIDERS__FIRST__KEY_ENV", "FIRST_TEST_KEY")
        .set(
            "AI_GATEWAY__PROVIDERS__FIRST__BASE_URL",
            format!("http://{}", held.addr),
        )
        .provider("second", "anthropic-messages", &second)
        .role("main", "first/model-one")
        .live();

    let request = Request::post(MESSAGES)
        .header("content-type", "application/json")
        .header(TENANT_HEADER, TENANT)
        .body(Body::from(STREAM_BODY))
        .expect("a request");
    let response = tokio::time::timeout(WAIT, router.clone().oneshot(request))
        .await
        .unwrap_or_else(|_| panic!("the gateway did not answer within {WAIT:?}"))
        .expect("the router is infallible");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(MODEL_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("first/model-one")
    );
    let mut body = response.into_body();
    let mut received = Vec::new();
    // Read until the part sent before the change has arrived.
    while received.len() < first_part.len() {
        let frame = tokio::time::timeout(WAIT, body.frame())
            .await
            .unwrap_or_else(|_| panic!("no stream data within {WAIT:?}"))
            .expect("the stream goes on")
            .expect("a frame");
        if let Ok(data) = frame.into_data() {
            received.extend_from_slice(&data);
        }
    }

    // The change, and a new request that uses it, while the stream is in flight.
    set_models(
        &database.store(),
        &rules(),
        "main",
        &["second/model-two"],
        0,
    )
    .await;
    let reply = post(&router, MESSAGES, &[], BODY).await;
    assert_eq!(reply.header(MODEL_HEADER), Some("second/model-two"));

    // The stream goes on from its own model to its end.
    let _ = held.release.take().expect("not released yet").send(());
    let rest_of_stream = tokio::time::timeout(WAIT, body.collect())
        .await
        .unwrap_or_else(|_| panic!("the stream did not end within {WAIT:?}"))
        .expect("the rest of the stream")
        .to_bytes();
    received.extend_from_slice(&rest_of_stream);
    assert_eq!(String::from_utf8(received).expect("UTF-8"), text);
    let sent = held.body.lock().expect("the body lock").clone();
    assert_eq!(
        sent.as_ref().and_then(|body| body["model"].as_str()),
        Some("model-one")
    );
    assert_eq!(model_of(&second, 0), "model-two");
}

#[tokio::test]
async fn a_reload_keeps_the_concurrency_count_of_a_model_that_stays() {
    let Some(database) = Database::new(
        "CREATE DATABASE ai_gateway_slots OWNER ai_gateway_owner",
        "ai_gateway_slots",
    )
    .await
    else {
        return;
    };
    // `busy` takes the one call it may have in flight and never answers it.
    let busy = Server::start(vec![vec![Step::Hang]]).await;
    let spare = Server::start(vec![message_reply("req_spare")]).await;
    let configured = Server::start(Vec::new()).await;
    let (gateway, router) = Vars::new()
        .database(&database)
        .provider("busy", "anthropic-messages", &busy)
        .provider("spare", "anthropic-messages", &spare)
        .provider("configured", "anthropic-messages", &configured)
        .role("main", "configured/model-zero")
        .set("AI_GATEWAY__ROLES__MAIN__MAX_CONCURRENT", "1")
        .live();
    let store = database.store();
    let first = set_models(&store, gateway.rules(), "main", &["busy/model-one"], 0).await;

    let hanging = {
        let router = router.clone();
        tokio::spawn(async move { post(&router, MESSAGES, &[], BODY).await })
    };
    busy.wait_for_requests(1).await;

    // The new list keeps `busy/model-one`: its client, with its one slot taken, is reused, so
    // the call falls back to `spare` at once instead of reaching `busy` again.
    set_models(
        &store,
        gateway.rules(),
        "main",
        &["busy/model-one", "spare/model-two"],
        first,
    )
    .await;
    let reply = post(&router, MESSAGES, &[], BODY).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.header(MODEL_HEADER), Some("spare/model-two"));
    assert_eq!(busy.request_count(), 1);
    assert_eq!(configured.request_count(), 0);

    hanging.abort();
    let _ = hanging.await;
}
