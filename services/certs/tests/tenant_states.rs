//! Each tenant's state from the identity service's events (Laysi/trustiot-v2#778; README.md,
//! "租戶狀態"), with the real binary on the product's schema (`testkit::product`), in a database of
//! its own: the service starts after the tenants exist, so its subscription replays the identity
//! service's log from the start.
//!
//! - Tenants are created by the identity service's own code (`identity::tenants::TenantStore`,
//!   which writes `identity.tenant_created`), and their states changed the way its state API
//!   changes them: the row's next version and `identity.tenant_state_changed` with the snapshot,
//!   in one transaction of the tenant (`services/identity/src/tenant_state.rs`, `change`). A
//!   tenant is suspended, resumed, given another reason, terminated and deleted, and suspended
//!   and resumed by concurrent changes.
//! - After the replay the service holds every tenant's state at the identity service's version
//!   (acceptance 3); a deleted tenant, a state this build does not know and a tenant without any
//!   event read as suspended (acceptance 3).
//! - Suspensions and resumptions written out of version order (as a replayed dead letter or a
//!   late event would arrive) end at the state of the highest version, the last operation
//!   (acceptance 2).
//! - A late copy of an old state and a replay of the whole subscription (`Replay::Reprocess`)
//!   store nothing and change no row (acceptance 1); nothing is dead-lettered.
//!
//! The lag of this subscription refusing renewals is `tests/tenant_state_lag.rs`; a refused
//! renewal for each state is `tests/renewal.rs`.
//!
//! Skipped without podman or docker unless `CI` is set. Run with `--nocapture` to see the
//! evidence lines the test report quotes.

mod support;

use std::collections::BTreeMap;

use certs::bindings::Projection;
use certs::tenant_states::SUBSCRIPTION;
use chrono::{DateTime, Utc};
use contract::events::identity::{
    SuspensionReason, TENANT_AGGREGATE, TenantCreated, TenantState, TenantStateChanged,
};
use contract::{Event, ServiceName, TenantId};
use identity::tenants::{TenantName, TenantStore};
use outbox::{Aggregate, Position, Replay};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use service_kit::identity::{ServiceIdentity, UserIdentity};
use service_kit::tenant::{BackgroundTenantDb, PlatformDb, TenantDb, TenantTransaction};
use sqlx::Connection as _;
use sqlx::postgres::PgPoolOptions;
use support::{Service, TestCa, wait_for};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};

/// The roles this test logs in with: the identity service's, which writes the tenants and their
/// events, and the certs service's two.
const LOGINS: &[&str] = &["identity_app", "certs_app", "certs_platform"];

/// The handler's line for every tenant state event it applied.
const APPLIED: &str = "applied a tenant state";

/// The lock the identity service's state change takes on the tenant's row.
const LOCK_STATE: &str = "SELECT state, suspension_reason FROM identity.tenants
WHERE tenant_id = $1::uuid
FOR NO KEY UPDATE";
/// The identity service's state change: the next version (its trigger refuses any other).
const UPDATE_STATE: &str = "UPDATE identity.tenants
SET state = $2, suspension_reason = $3, state_version = state_version + 1
WHERE tenant_id = $1::uuid
RETURNING state_version";

/// Every tenant of the identity service with the certs service's copy of its state.
const BOTH: &str = "SELECT tenants.tenant_id::text, tenants.state, tenants.state_version,
    statuses.status, statuses.version
FROM identity.tenants AS tenants
LEFT JOIN certs.tenant_statuses AS statuses USING (tenant_id)
ORDER BY 1";
/// The certs service's rows, whole.
const STATUSES: &str =
    "SELECT tenant_id::text, status, version, updated_at FROM certs.tenant_statuses";
/// Whether the subscription processed an event.
const PROCESSED: &str = "SELECT count(*) FROM certs.outbox_inbox
WHERE subscription = $1 AND event_id = $2::uuid";
/// The subscription's dead letters.
const DEAD_LETTERS: &str = "SELECT count(*) FROM certs.outbox_dead_letter WHERE subscription = $1";

/// `BOTH`'s rows.
type Both = (String, String, i64, Option<String>, Option<i64>);
/// `STATUSES`' rows by tenant.
type Rows = BTreeMap<String, (String, i64, DateTime<Utc>)>;

/// `identity.tenant_state_changed` as a newer identity service could write it, with a state this
/// build does not know.
#[derive(Serialize, Deserialize)]
struct FutureStateChanged {
    state: String,
    reason: Option<String>,
    state_version: i64,
}

impl Event for FutureStateChanged {
    const TYPE: &'static str = TenantStateChanged::TYPE;
    const VERSION: u32 = 1;
    const PRODUCER: ServiceName = ServiceName::Identity;
}

/// The identity service's role: its tenant transactions.
fn identity_writer(product: &ProductPostgres) -> BackgroundTenantDb {
    TenantDb::connect_lazy_with_background(
        product.postgres.options("identity_app", DATABASE),
        PgPoolOptions::new().max_connections(4),
        ServiceIdentity::new(ServiceName::Identity),
    )
    .expect("the identity service role's pool")
    .1
}

/// A platform administrator's verified identity, as the identity service's API receives it.
fn platform_admin(issuer: &TestIssuer) -> UserIdentity {
    let mut claims = TestIssuer::claims(ServiceName::Identity, None);
    claims.platform_admin = true;
    issuer.user(ServiceName::Identity, &claims)
}

/// Creates a tenant with the identity service's own code: its row and `identity.tenant_created`.
async fn create_tenant(store: &TenantStore, admin: &UserIdentity, name: &str) -> TenantId {
    store
        .create(admin, &TenantName::parse(name).expect("a valid name"))
        .await
        .expect("create the tenant")
        .tenant_id
}

/// Changes `tenant`'s state as the identity service's state API does: the row locked first, then,
/// unless it already has this state and reason, its next version and its event, in one
/// transaction of the tenant. Returns the new version; `None` when nothing changed.
async fn change_state(
    identity: &BackgroundTenantDb,
    tenant: TenantId,
    state: TenantState,
    reason: Option<SuspensionReason>,
) -> Option<i64> {
    let as_text = |value: Value| value.as_str().expect("a string").to_owned();
    let state_text = as_text(serde_json::to_value(state).expect("a state"));
    let reason_text = reason.map(|reason| as_text(serde_json::to_value(reason).expect("reason")));
    let mut tx = identity
        .begin(tenant, "test-change-tenant-state")
        .await
        .expect("a transaction of the tenant");
    let current: (String, Option<String>) = sqlx::query_as(LOCK_STATE)
        .bind(tenant.to_string())
        .fetch_one(&mut *tx)
        .await
        .expect("lock the tenant");
    if current == (state_text.clone(), reason_text.clone()) {
        tx.rollback().await.expect("roll back");
        return None;
    }
    let state_version: i64 = sqlx::query_scalar(UPDATE_STATE)
        .bind(tenant.to_string())
        .bind(&state_text)
        .bind(reason_text)
        .fetch_one(&mut *tx)
        .await
        .expect("change the state");
    publish(
        tx,
        tenant,
        &TenantStateChanged {
            state,
            reason,
            state_version,
        },
    )
    .await;
    Some(state_version)
}

/// Writes `event` to the identity service's log for `tenant` in `tx` and commits; returns the
/// event's ID.
async fn publish<E: Event>(mut tx: TenantTransaction, tenant: TenantId, event: &E) -> String {
    let aggregate = Aggregate::new(TENANT_AGGREGATE, tenant.to_string()).expect("an aggregate");
    let published = outbox::publish(tx.transaction(), &aggregate, event, Some(tenant))
        .await
        .expect("write the event");
    tx.commit().await.expect("commit the event");
    published.event_id.to_string()
}

/// Writes only an event for `tenant` (no row of the identity service), as a replayed dead letter
/// or a newer producer would leave it in the log.
async fn publish_only<E: Event>(
    identity: &BackgroundTenantDb,
    tenant: TenantId,
    event: &E,
) -> String {
    let tx = identity
        .begin(tenant, "test-tenant-event")
        .await
        .expect("a transaction of the tenant");
    publish(tx, tenant, event).await
}

/// A state change of a tenant that has no identity row, at `state_version`.
fn snapshot(state: TenantState, state_version: i64) -> TenantStateChanged {
    TenantStateChanged {
        state,
        reason: (state != TenantState::Active).then_some(SuspensionReason::NonPayment),
        state_version,
    }
}

async fn rows(product: &ProductPostgres) -> Rows {
    let rows: Vec<(String, String, i64, DateTime<Utc>)> = sqlx::query_as(STATUSES)
        .fetch_all(&mut product.superuser().await)
        .await
        .expect("read the statuses");
    rows.into_iter()
        .map(|(tenant, status, version, updated_at)| (tenant, (status, version, updated_at)))
        .collect()
}

async fn wait_processed(product: &ProductPostgres, event: &str) {
    wait_for(
        &format!("the certs service processing event {event}"),
        || async {
            let processed: i64 = sqlx::query_scalar(PROCESSED)
                .bind(SUBSCRIPTION)
                .bind(event)
                .fetch_one(&mut product.superuser().await)
                .await
                .expect("read the inbox");
            (processed == 1).then_some(())
        },
    )
    .await;
}

/// The `stored` field of the service's applied lines for `tenant`, in order.
fn applied(service: &Service, tenant: TenantId) -> Vec<(String, i64, bool)> {
    service
        .stdout
        .messages(APPLIED)
        .iter()
        .filter(|line| line["fields"]["tenant_id"] == tenant.to_string())
        .map(|line| {
            (
                line["fields"]["status"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                line["fields"]["version"].as_i64().unwrap_or_default(),
                line["fields"]["stored"].as_bool().unwrap_or_default(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario on one database: the replay covers every tenant made before it"
)]
async fn a_replay_from_the_start_holds_every_tenant_at_its_newest_state() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let admin = platform_admin(&issuer);
    let store = TenantStore::connect_lazy(product.postgres.options("identity_app", DATABASE))
        .expect("verify-full options");
    let identity = identity_writer(&product);

    // 1. Before the certs service runs: tenants and their changes, by the identity service's code.
    let active = create_tenant(&store, &admin, "Active").await;
    let suspended = create_tenant(&store, &admin, "Suspended").await;
    change_state(
        &identity,
        suspended,
        TenantState::Suspended,
        Some(SuspensionReason::NonPayment),
    )
    .await;
    let resumed = create_tenant(&store, &admin, "Resumed").await;
    for (state, reason) in [
        (
            TenantState::Suspended,
            Some(SuspensionReason::SecurityOrAbuse),
        ),
        (TenantState::Suspended, Some(SuspensionReason::NonPayment)),
        (TenantState::Active, None),
    ] {
        change_state(&identity, resumed, state, reason).await;
    }
    let deleted = create_tenant(&store, &admin, "Deleted").await;
    for state in [TenantState::Suspended, TenantState::Deleted] {
        change_state(
            &identity,
            deleted,
            state,
            Some(SuspensionReason::ContractTerminated),
        )
        .await;
    }
    // Concurrent suspensions and resumptions of one tenant, each with a reason of its own so
    // every one is a change: the row lock orders them, each one version more, and the last one
    // decides.
    let contested = create_tenant(&store, &admin, "Contested").await;
    let reasons = [
        SuspensionReason::NonPayment,
        SuspensionReason::SecurityOrAbuse,
    ];
    let tasks: Vec<_> = (0..8_usize)
        .map(|n| {
            let identity = identity.clone();
            // Alternate between the two reasons, resuming every fourth: consecutive changes
            // may land in any order, so no two neighbours may ask for the same state and reason.
            let (state, reason) = match n % 4 {
                3 => (TenantState::Active, None),
                k => (TenantState::Suspended, Some(reasons[k % 2])),
            };
            tokio::spawn(async move { change_state(&identity, contested, state, reason).await })
        })
        .collect();
    let mut versions = Vec::new();
    for task in tasks {
        versions.extend(task.await.expect("a state change"));
    }
    versions.sort_unstable();
    let contested_version = 1 + i64::try_from(versions.len()).expect("a count");
    assert_eq!(
        versions,
        (2..=contested_version).collect::<Vec<_>>(),
        "one version per change"
    );
    println!("evidence: 8 concurrent changes of one tenant: versions {versions:?}");

    // Event-only tenants, as a newer producer or a replayed dead letter leaves the log: a state
    // this build does not know, and suspensions and resumptions written out of version order.
    let future = TenantId::generate();
    publish_only(&identity, future, &TenantCreated {}).await;
    publish_only(
        &identity,
        future,
        &FutureStateChanged {
            state: "archived".to_owned(),
            reason: Some("audit_hold".to_owned()),
            state_version: 2,
        },
    )
    .await;
    // Odd versions suspend, even ones resume; the last operation is the highest version.
    let shuffled = [5_i64, 2, 9, 3, 7, 4, 8, 6];
    let state_of = |version: i64| {
        if version % 2 == 1 {
            TenantState::Suspended
        } else {
            TenantState::Active
        }
    };
    let (ends_suspended, ends_active) = (TenantId::generate(), TenantId::generate());
    for (tenant, versions) in [
        (ends_suspended, &shuffled[..]),
        (ends_active, &[4, 7, 2, 10, 3, 6][..]),
    ] {
        publish_only(&identity, tenant, &TenantCreated {}).await;
        for version in versions {
            publish_only(&identity, tenant, &snapshot(state_of(*version), *version)).await;
        }
    }
    let never = TenantId::generate();

    // 2. The service starts and replays the identity service's log from the start.
    let ca = TestCa::new(&[1]);
    let service = Service::start(&product, &ca, &[]).await;
    let both: Vec<Both> = wait_for("every identity tenant's state in certs", || async {
        let both: Vec<Both> = sqlx::query_as(BOTH)
            .fetch_all(&mut product.superuser().await)
            .await
            .expect("read both");
        let caught_up = both.len() == 5
            && both.iter().all(|(_, state, version, status, stored)| {
                status.as_deref() == Some(state.as_str()) && *stored == Some(*version)
            });
        caught_up.then_some(both)
    })
    .await;
    println!("evidence: identity tenants and certs' copies after the replay: {both:?}");
    let expected: BTreeMap<TenantId, (&str, i64)> = [
        (active, ("active", 1)),
        (suspended, ("suspended", 2)),
        (resumed, ("active", 4)),
        (deleted, ("deleted", 3)),
        (contested, ("decided by the last change", contested_version)),
        (future, ("unrecognized", 2)),
        (ends_suspended, ("suspended", 9)),
        (ends_active, ("active", 10)),
    ]
    .into_iter()
    .collect();
    // The contested tenant's last change decides; the identity service's row says which.
    let contested_state = both
        .iter()
        .find(|(tenant, ..)| *tenant == contested.to_string())
        .map(|(_, state, ..)| state.clone())
        .expect("the contested tenant");
    let stored = wait_for("the event-only tenants' states", || async {
        let stored = rows(&product).await;
        (stored.len() == expected.len()
            && stored.get(&ends_active.to_string()).map(|row| row.1) == Some(10)
            && stored.get(&ends_suspended.to_string()).map(|row| row.1) == Some(9)
            && stored.get(&future.to_string()).map(|row| row.1) == Some(2))
        .then_some(stored)
    })
    .await;
    for (tenant, (status, version)) in &expected {
        let row = stored.get(&tenant.to_string()).expect("a row");
        let status = if *tenant == contested {
            contested_state.as_str()
        } else {
            status
        };
        assert_eq!((row.0.as_str(), row.1), (status, *version), "{tenant}");
    }
    println!(
        "evidence: out-of-order suspensions and resumptions: arrivals {shuffled:?} end at \
         {:?}; arrivals [4, 7, 2, 10, 3, 6] end at {:?}",
        stored[&ends_suspended.to_string()],
        stored[&ends_active.to_string()]
    );
    // Arrivals that were older than the stored version were not stored.
    let lines = applied(&service, ends_suspended);
    let skipped: Vec<i64> = lines
        .iter()
        .filter(|(_, _, stored)| !stored)
        .map(|(_, version, _)| *version)
        .collect();
    assert_eq!(skipped, [2, 3, 7, 4, 8, 6], "{lines:?}");

    // 3. What the renewal reads: only an active tenant is not suspended.
    let projection = Projection::new(
        PlatformDb::connect_lazy(
            product.postgres.options("certs_platform", DATABASE),
            PgPoolOptions::new().max_connections(1),
        )
        .expect("the certs platform role's pool"),
    );
    let mut read = BTreeMap::new();
    for (name, tenant) in [
        ("active", active),
        ("suspended", suspended),
        ("resumed", resumed),
        ("deleted", deleted),
        ("unrecognized", future),
        ("ends_suspended", ends_suspended),
        ("ends_active", ends_active),
        ("never_created", never),
    ] {
        let refused = projection
            .tenant_suspended(tenant, "test-tenant-states")
            .await
            .expect("read");
        read.insert(name, refused);
    }
    assert_eq!(
        read,
        [
            ("active", false),
            ("deleted", true),
            ("ends_active", false),
            ("ends_suspended", true),
            ("never_created", true),
            ("resumed", false),
            ("suspended", true),
            ("unrecognized", true),
        ]
        .into_iter()
        .collect()
    );
    println!("evidence: read as suspended: {read:?}");

    // 4. A late copy of an older state (a delayed event): processed, nothing stored.
    let before = rows(&product).await;
    let late = publish_only(&identity, resumed, &snapshot(TenantState::Suspended, 3)).await;
    wait_processed(&product, &late).await;
    assert_eq!(rows(&product).await, before, "a late event changed a row");

    // 5. The whole subscription again: every event is delivered once more and nothing changes.
    let applied_before = service.stdout.messages(APPLIED).len();
    let mut certs = product.postgres.connect("certs_app", DATABASE).await;
    let mut tx = certs.begin().await.expect("begin");
    outbox::replay_from(&mut tx, SUBSCRIPTION, Position::START, Replay::Reprocess)
        .await
        .expect("replay the subscription");
    tx.commit().await.expect("commit the replay");
    // A tenant created after the replay: once its state is there, every event before it was
    // delivered again (one subscription delivers in log order).
    let sentinel = create_tenant(&store, &admin, "Sentinel").await;
    wait_for("the sentinel tenant's state after the replay", || async {
        rows(&product)
            .await
            .contains_key(&sentinel.to_string())
            .then_some(())
    })
    .await;
    let mut after = rows(&product).await;
    after.remove(&sentinel.to_string());
    assert_eq!(after, before, "the replay changed a row");
    let replayed: Vec<Value> = service.stdout.messages(APPLIED)[applied_before..].to_vec();
    let stored_again = replayed
        .iter()
        .filter(|line| line["fields"]["stored"] == true)
        .count();
    assert_eq!(
        stored_again, 1,
        "only the sentinel's creation is stored during the replay: {replayed:?}"
    );
    let dead: i64 = sqlx::query_scalar(DEAD_LETTERS)
        .bind(SUBSCRIPTION)
        .fetch_one(&mut product.superuser().await)
        .await
        .expect("count the dead letters");
    assert_eq!(dead, 0, "nothing dead-lettered");
    println!(
        "evidence: late event and full replay: {} events delivered again, {} stored (the \
         sentinel), every other row unchanged (status, version, updated_at); 0 dead letters",
        replayed.len(),
        stored_again
    );
}
