//! A renewal that fails on the service's side gives its turn of the hardware ID's own rate limit
//! back (Laysi/trustiot-v2#801, `Limiter::refund_verified`; Laysi/trustiot-v2#846;
//! services/certs/README.md, "換發", "規則與取捨"), with the real binary on the product's schema
//! (`testkit::product`) and software P-256 keys standing in for the gateways' TPM keys.
//!
//! The database fails after the signature held, in the issuance transaction of the gateway's
//! tenant, by a privilege the service's role loses for a while (an `UPDATE` of nothing else, so
//! the failure is exactly "the statement is refused", every time):
//!
//! - **the ledger insert is refused**: the first statement that writes;
//! - **the event insert is refused**: the ledger row is already written in the transaction, so
//!   this is the half-written case, which must leave neither the row nor the event.
//!
//! Each answer is 503 `unavailable` with `Retry-After: 5`, and the hardware ID's limit is as if
//! nothing had been asked: more faulted redemptions than its burst do not run into
//! `gateway_rate_limited`, and once the privileges are back it renews as often as its burst
//! says, no more (the refund never exceeds the burst).
//!
//! This test binary must keep a single test: a failed assertion between a `REVOKE` and its
//! `GRANT` leaves the role without the privilege in the shared database.
//!
//! Skipped without podman or docker unless `CI` is set. Run with `--nocapture` to see the
//! evidence lines the test report quotes.

mod support;

use std::time::Duration;

use certs::bindings::TenantStatus;
use certs::renewal::{CHALLENGE_UNKNOWN, SIGNATURE_REFUSED};
use contract::TenantId;
use protocol::renewal::RenewedCertificate;
use serde_json::Value;
use sqlx::PgConnection;
use support::renewal::{
    Answer, FAST_CRLS, Factory, LOGINS, REFUSED_EVENTS, RENEWED_EVENTS, assets_writer, bind,
    challenge, counters, ledger, refusal_lines, renew_request, send, wait_for_bindings_lag,
};
use support::{FakeCollector, Service, TestCa, store_status, wait_for};
use testkit::product::product_postgres;

/// Per hardware ID 2 redemptions whose signature held, then one an hour: more than the faulted
/// redemptions below would leave if they were not given back.
const GATEWAY_BURST: u64 = 2;

/// Redemptions that fail on the service's side for each fault: more than [`GATEWAY_BURST`], so
/// without the refund the hardware ID's own limit would be used up.
const FAULTED: u64 = 3;

/// `Retry-After` after a database failure (`certs::renewal`).
const RETRY_DATABASE_SECS: u64 = 5;

/// What a fault takes from the service's role, and gives back. `$1` is not usable in `GRANT` or
/// `REVOKE`, so each fault names its table in a static statement.
struct Fault {
    name: &'static str,
    /// Whether the issuance transaction gets as far as inserting the ledger row, which the
    /// failure then rolls back.
    writes_ledger_row: bool,
    revoke: &'static str,
    grant: &'static str,
}

const LEDGER_INSERT: Fault = Fault {
    name: "the ledger insert is refused",
    writes_ledger_row: false,
    revoke: "REVOKE INSERT ON certs.issued_certificates FROM certs_app",
    grant: "GRANT INSERT ON certs.issued_certificates TO certs_app",
};

const EVENT_INSERT: Fault = Fault {
    name: "the event insert is refused after the ledger row was written",
    writes_ledger_row: true,
    revoke: "REVOKE INSERT ON certs.outbox_event FROM certs_app",
    grant: "GRANT INSERT ON certs.outbox_event TO certs_app",
};

/// The tenant events of renewals of an aggregate, as their count.
async fn renewal_events(admin: &mut PgConnection, aggregate: &str) -> usize {
    let events: Vec<(String, String, String)> = sqlx::query_as(RENEWED_EVENTS)
        .bind(aggregate)
        .fetch_all(admin)
        .await
        .expect("read the renewal events");
    events.len()
}

/// Rows ever inserted into the ledger table by any transaction, committed or not (the statistics
/// are flushed by the backends within moments, so a caller waits for the number it expects).
const LEDGER_INSERTS: &str = "SELECT n_tup_ins FROM pg_stat_all_tables
    WHERE schemaname = 'certs' AND relname = 'issued_certificates'";

/// One full redemption: a challenge, then the signed redemption.
async fn redeem(service: &Service, hardware_id: &str, key: &rcgen::KeyPair) -> Answer {
    let challenge = challenge(service, hardware_id).await;
    send(service, &renew_request(key, &challenge)).await
}

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario on one replica: each step uses the turns of the one before"
)]
async fn a_database_failure_while_issuing_gives_the_turn_back() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let collector = FakeCollector::start();
    let endpoint = collector.url();
    let mut env = FAST_CRLS.to_vec();
    env.extend([
        ("TRUSTIOT__RENEWAL__GATEWAY_BURST", "2"),
        ("TRUSTIOT__RENEWAL__GATEWAY_INTERVAL", "1h"),
        ("TRUSTIOT__TELEMETRY__OTLP_ENDPOINT", endpoint.as_str()),
        ("OTEL_METRIC_EXPORT_INTERVAL", "300"),
    ]);
    let service = Service::start(&product, &ca, &env).await;
    wait_for("the published CRLs", || async {
        let (status, _) = service.get(certs::http::CRL_PATH).await;
        (status == 200).then_some(())
    })
    .await;
    let factory = Factory::new(&service).await;
    let key = factory.register("fi-ok").await;
    let tenant = TenantId::generate();
    store_status(&product, tenant, TenantStatus::Active, 1).await;
    let assets = assets_writer(&product);
    bind(&product, &assets, tenant, "fi-ok").await;
    wait_for_bindings_lag(&collector).await;
    let mut admin = product.superuser().await;
    let aggregate = format!("{tenant}:fi-ok");
    let ledger_before = ledger(&product, "fi-ok").await;
    let mut faulted = 0_u64;

    for (n, fault) in [LEDGER_INSERT, EVENT_INSERT].iter().enumerate() {
        let revoke = fault.revoke;
        sqlx::query(revoke)
            .execute(&mut admin)
            .await
            .expect("take the privilege away");
        let inserts_before: i64 = sqlx::query_scalar(LEDGER_INSERTS)
            .fetch_one(&mut admin)
            .await
            .expect("read the statistics");
        let mut answers = Vec::new();
        for attempt in 0..FAULTED {
            if attempt == 1 {
                // As the gateway does: wait what `Retry-After` said before the next try.
                tokio::time::sleep(Duration::from_secs(RETRY_DATABASE_SECS)).await;
            }
            let answer = redeem(&service, "fi-ok", &key).await;
            assert_eq!(answer.status, 503, "{}: {answer:?}", fault.name);
            assert_eq!(answer.body["code"], "unavailable", "{answer:?}");
            assert_eq!(answer.retry_after, Some(RETRY_DATABASE_SECS), "{answer:?}");
            assert_eq!(answer.cache_control.as_deref(), Some("no-store"));
            answers.push(answer.status);
            faulted += 1;
        }
        if fault.writes_ledger_row {
            // The ledger row was inserted in each transaction and rolled back with it.
            let wanted = inserts_before + i64::try_from(FAULTED).expect("a count");
            wait_for(
                "the statistics of the rolled back ledger inserts",
                || async {
                    let inserts: i64 = sqlx::query_scalar(LEDGER_INSERTS)
                        .fetch_one(&mut product.superuser().await)
                        .await
                        .expect("read the statistics");
                    (inserts >= wanted).then_some(())
                },
            )
            .await;
        }
        // Nothing half written: no ledger row (the second fault wrote one in the transaction,
        // which rolled back), no tenant event.
        assert_eq!(
            ledger(&product, "fi-ok").await,
            ledger_before,
            "{}: the ledger",
            fault.name
        );
        assert_eq!(
            renewal_events(&mut admin, &aggregate).await,
            0,
            "{}: the events",
            fault.name
        );
        let grant = fault.grant;
        sqlx::query(grant)
            .execute(&mut admin)
            .await
            .expect("give the privilege back");
        println!(
            "evidence: fault {} ({}): {} redemptions of fi-ok (burst {GATEWAY_BURST}) answered \
             {answers:?} with Retry-After {RETRY_DATABASE_SECS}; ledger rows {} before, {} after \
             (ledger row inserted in the transaction and rolled back: {}); renewal events 0",
            n + 1,
            fault.name,
            answers.len(),
            ledger_before,
            ledger(&product, "fi-ok").await,
            if fault.writes_ledger_row {
                "yes"
            } else {
                "no, the insert itself is refused"
            },
        );
    }
    assert!(faulted > 2 * GATEWAY_BURST);

    // The database is back: the gateway renews, as often as its burst says and no more.
    for renewed in 1..=GATEWAY_BURST {
        let answer = redeem(&service, "fi-ok", &key).await;
        assert_eq!(answer.status, 201, "renewal {renewed}: {answer:?}");
        let _: RenewedCertificate =
            serde_json::from_value(answer.body).expect("the renewed certificate");
        let renewed = i64::try_from(renewed).expect("a count");
        assert_eq!(ledger(&product, "fi-ok").await, ledger_before + renewed);
        assert_eq!(
            renewal_events(&mut admin, &aggregate).await,
            usize::try_from(renewed).expect("a count")
        );
    }
    let limited = redeem(&service, "fi-ok", &key).await;
    assert_eq!(limited.status, 429, "{limited:?}");
    assert!(limited.retry_after.is_some(), "{limited:?}");
    println!(
        "evidence: after {faulted} faulted redemptions and the privileges back: {GATEWAY_BURST} \
         renewals 201 (ledger +{GATEWAY_BURST}, {GATEWAY_BURST} events in the ledger rows' \
         transactions), the next one 429 Retry-After {:?}: the refunds did not go past the burst",
        limited.retry_after
    );

    // The log, the counts and the metrics agree: every failure is `unavailable`, which is not
    // counted for the refusal event; the only `gateway_rate_limited` is the last one.
    let lines = refusal_lines(&service, "fi-ok");
    let unavailable = lines
        .iter()
        .filter(|reason| *reason == "unavailable")
        .count();
    let rate_limited = lines
        .iter()
        .filter(|reason| *reason == "gateway_rate_limited")
        .count();
    assert_eq!(
        (unavailable, rate_limited, lines.len()),
        (
            usize::try_from(faulted).expect("a count"),
            1,
            usize::try_from(faulted).expect("a count") + 1
        ),
        "{lines:?}"
    );
    let counted: Vec<String> = sqlx::query_scalar(
        "SELECT reasons::text FROM certs.renewal_refusals WHERE hardware_id = 'fi-ok'",
    )
    .fetch_all(&mut admin)
    .await
    .expect("read the refusal counts");
    assert_eq!(
        counted,
        ["{\"gateway_rate_limited\": 1}"],
        "an `unavailable` is not counted; the one 429 is"
    );
    let events: Vec<String> = sqlx::query_scalar(REFUSED_EVENTS)
        .bind("fi-ok")
        .fetch_all(&mut admin)
        .await
        .expect("read the refusal events");
    assert!(
        events.iter().all(|envelope| {
            let envelope: Value = serde_json::from_str(envelope).expect("an envelope");
            envelope["payload"]["reasons"].get("unavailable").is_none()
        }),
        "{events:?}"
    );
    let metric = |reason: &str| ("certs.renewal.refused".to_owned(), reason.to_owned());
    wait_for("the refusal counters of fi-ok", || async {
        let totals = counters(&collector);
        (totals.get(&metric("unavailable")) == Some(&faulted)
            && totals.get(&metric("gateway_rate_limited")) == Some(&1))
        .then_some(())
    })
    .await;
    println!(
        "evidence: log lines {unavailable} `unavailable`, {rate_limited} `gateway_rate_limited`; \
         refusal counts {counted:?}; certs.renewal.refused unavailable={faulted}, \
         gateway_rate_limited=1"
    );

    // After the faults, requests that name fi-ok without its key are still answered as before
    // (that they never use its turns is `tests/renewal.rs`, Laysi/trustiot-v2#801).
    let made_up = send(&service, &support::renewal::made_up_renewal("fi-ok", 7)).await;
    assert!(made_up.is(409, &CHALLENGE_UNKNOWN), "{made_up:?}");
    let forger = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("a key");
    let forged = redeem(&service, "fi-ok", &forger).await;
    assert!(forged.is(401, &SIGNATURE_REFUSED), "{forged:?}");
}
