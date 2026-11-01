//! Measurements of the renewal endpoint under load (Laysi/trustiot-v2#846; services/certs/README.md,
//! "換發", "限流與資源上限" and "規則與取捨"), with the real binary on the product's schema
//! (`testkit::product`). They are measurements, not regression tests, so they are `#[ignore]`d
//! and every number they print is host-dependent; what they assert is structural.
//!
//! - `concurrency_slots_under_load`: with the default `renewal.max_concurrent` (2) and global
//!   limits that are not reached, redemptions that all do database work (a signed redemption
//!   reads the registration, verifies and issues) are sent at once in growing numbers (20 to 600), with a
//!   gateway that follows `Retry-After` among them. It prints, per level, how many answered 201
//!   and how many 503 `busy`, how long each kind took, and what the gateway got. Asserted:
//!   every answer is 201 or a `busy` 503 (`Retry-After: 1`), the ledger has a row for each 201
//!   and for nothing else, `busy` writes no refusal count, and the gateway renews.
//! - `refusal_event_volume`: many registered gateways each refused many times by requests that
//!   need no key (a made-up nonce, another key's signature) and by redemptions of their own key
//!   after which the tenant turned out suspended. It prints, per `renewal.refusal_window`, the
//!   rows of `renewal_refusals` and `renewal_unverified_refusals` and the events
//!   `certs.gateway_renewal_refused` that come out of them. Asserted: the counts add up to the
//!   refusals the client saw, at most one row and one event per registered hardware ID, category
//!   and window, and the window lengths.
//!
//! Run (on a build host, with podman):
//! `CI=1 cargo test -p certs --test renewal_load -- --ignored --nocapture --test-threads=1`.
//! The lines that start with `measure:` are the data of the test report.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use certs::bindings::TenantStatus;
use certs::renewal::CHALLENGE_UNKNOWN;
use contract::TenantId;
use protocol::renewal::Challenge;
use rcgen::{KeyPair, PKCS_ECDSA_P256_SHA256};
use serde_json::{Value, json};
use sqlx::PgConnection;
use support::renewal::{
    Answer, FAST_CRLS, Factory, LOGINS, assets_writer, bind, challenge, challenge_request,
    counters, made_up_renewal, renew_request, send, wait_for_bindings_lag,
};
use support::{FakeCollector, LIMIT, Service, TestCa, store_status, wait_for};
use testkit::product::{ProductPostgres, product_postgres};

/// The most a flood of one gateway puts on the wire: below its burst of 100.
const PER_GATEWAY: usize = 50;

/// How many redemptions are sent at once at each level.
const LEVELS: [usize; 5] = [20, 100, 300, 450, 600];

/// `Retry-After` of a `busy` answer (`certs::renewal`).
const RETRY_BUSY_SECS: u64 = 1;

/// Sends `body` and measures how long the answer took.
async fn timed(service: &Service, body: &Value) -> (Answer, Duration) {
    let started = Instant::now();
    let answer = send(service, body).await;
    (answer, started.elapsed())
}

/// The `p`th percentile (0 to 100, nearest rank) of `sorted`, in milliseconds.
fn percentile(sorted: &[Duration], p: usize) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p * sorted.len()).div_ceil(100).clamp(1, sorted.len());
    sorted[rank - 1].as_millis()
}

/// Count, median, 95th percentile and maximum of `durations`, in milliseconds.
fn summary(mut durations: Vec<Duration>) -> Value {
    durations.sort();
    json!({
        "count": durations.len(),
        "p50_ms": percentile(&durations, 50),
        "p95_ms": percentile(&durations, 95),
        "max_ms": durations.last().map_or(0, Duration::as_millis),
    })
}

/// What a gateway that follows `Retry-After` went through to renew.
struct Gateway {
    /// The first thing that happened to it, for example `201` or `503 busy at challenge`.
    first: String,
    /// Complete tries (a challenge and a redemption) and the 503 `busy` answers on the way, by step.
    tries: u64,
    busy_at_challenge: u64,
    busy_at_redemption: u64,
    /// From the first request to the certificate.
    total: Duration,
}

/// Renews as the agent does: every try starts at the challenge, a 503 waits what `Retry-After`
/// says; anything else than 201 or 503 `busy` fails the measurement. Gives up only after
/// [`LIMIT`] and then says so.
async fn follow_retry_after(service: Arc<Service>, hardware_id: String, key: KeyPair) -> Gateway {
    let started = Instant::now();
    let (mut tries, mut busy_at_challenge, mut busy_at_redemption) = (0, 0, 0);
    let mut first = None;
    loop {
        assert!(
            started.elapsed() < LIMIT,
            "{hardware_id} did not renew within {LIMIT:?} of following Retry-After ({tries} tries)"
        );
        tries += 1;
        let asked = send(&service, &challenge_request(&hardware_id)).await;
        if asked.status == 503 {
            busy_at_challenge += 1;
            first.get_or_insert_with(|| "503 busy at challenge".to_owned());
            tokio::time::sleep(Duration::from_secs(asked.retry_after.unwrap_or(1))).await;
            continue;
        }
        assert_eq!(asked.status, 200, "{hardware_id}: {asked:?}");
        let challenge: Challenge = serde_json::from_value(asked.body).expect("a challenge");
        let answer = send(&service, &renew_request(&key, &challenge)).await;
        match answer.status {
            201 => {
                first.get_or_insert_with(|| "201".to_owned());
                return Gateway {
                    first: first.unwrap_or_default(),
                    tries,
                    busy_at_challenge,
                    busy_at_redemption,
                    total: started.elapsed(),
                };
            }
            503 => {
                busy_at_redemption += 1;
                first.get_or_insert_with(|| "503 busy at redemption".to_owned());
                assert_eq!(answer.retry_after, Some(RETRY_BUSY_SECS), "{answer:?}");
                tokio::time::sleep(Duration::from_secs(RETRY_BUSY_SECS)).await;
            }
            status => panic!("{hardware_id}: unexpected {status}: {answer:?}"),
        }
    }
}

/// Registers `hardware_ids` and binds them to a new active tenant; returns their keys.
async fn registered(
    product: &ProductPostgres,
    factory: &Factory<'_>,
    collector: &FakeCollector,
    hardware_ids: &[String],
    status: TenantStatus,
) -> Vec<KeyPair> {
    let tenant = TenantId::generate();
    store_status(product, tenant, TenantStatus::Active, 1).await;
    if !matches!(status, TenantStatus::Active) {
        store_status(product, tenant, status, 2).await;
    }
    let assets = assets_writer(product);
    let mut keys = Vec::new();
    for hardware_id in hardware_ids {
        keys.push(factory.register(hardware_id).await);
        bind(product, &assets, tenant, hardware_id).await;
    }
    wait_for_bindings_lag(collector).await;
    keys
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement: run it as the module documentation says"]
#[expect(
    clippy::too_many_lines,
    reason = "one measurement on one replica, level after level"
)]
async fn concurrency_slots_under_load() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let collector = FakeCollector::start();
    let endpoint = collector.url();
    let mut env = FAST_CRLS.to_vec();
    // The default `renewal.max_concurrent` (2) is what is measured. The limits are far above what
    // the levels send, so only the slots decide.
    env.extend([
        ("TRUSTIOT__RENEWAL__GATEWAY_BURST", "100"),
        ("TRUSTIOT__RENEWAL__GLOBAL_BURST", "10000"),
        ("TRUSTIOT__RENEWAL__GLOBAL_INTERVAL", "1ms"),
        ("TRUSTIOT__TELEMETRY__OTLP_ENDPOINT", endpoint.as_str()),
        ("OTEL_METRIC_EXPORT_INTERVAL", "300"),
    ]);
    let service = Arc::new(Service::start(&product, &ca, &env).await);
    wait_for("the published CRLs", || async {
        let (status, _) = service.get(certs::http::CRL_PATH).await;
        (status == 200).then_some(())
    })
    .await;

    // Gateways: one that renews alone for the baseline, and for each level the ones whose
    // challenges make the flood plus one that follows `Retry-After` through it.
    let mut names = vec!["ld-base".to_owned()];
    for level in LEVELS {
        names.push(format!("ld-{level}-real"));
        names.extend((0..level.div_ceil(PER_GATEWAY)).map(|n| format!("ld-{level}-{n}")));
    }
    let factory = Factory::new(&service).await;
    let mut keys: BTreeMap<String, KeyPair> = names
        .iter()
        .cloned()
        .zip(registered(&product, &factory, &collector, &names, TenantStatus::Active).await)
        .collect();
    let mut admin = product.superuser().await;
    let (mut busy_renew, mut busy_challenge) = (0_u64, 0_u64);

    // Baseline: a gateway alone, full renewals one after the other.
    let key = keys.remove("ld-base").expect("a key");
    let mut alone = Vec::new();
    for _ in 0..10 {
        let started = Instant::now();
        let asked = challenge(&service, "ld-base").await;
        let answer = send(&service, &renew_request(&key, &asked)).await;
        assert_eq!(answer.status, 201, "{answer:?}");
        alone.push(started.elapsed());
    }
    println!(
        "measure: {}",
        json!({"level": "alone", "renewals": summary(alone)})
    );

    for level in LEVELS {
        // The challenges first (30 seconds of life each), several gateways at a time.
        let share = level.div_ceil(PER_GATEWAY);
        let mut taking = tokio::task::JoinSet::new();
        for n in 0..share {
            let hardware_id = format!("ld-{level}-{n}");
            let key = keys.remove(&hardware_id).expect("a key");
            let count = PER_GATEWAY.min(level - n * PER_GATEWAY);
            let service = Arc::clone(&service);
            taking.spawn(async move {
                let mut bodies = Vec::new();
                for _ in 0..count {
                    let asked = challenge(&service, &hardware_id).await;
                    bodies.push(renew_request(&key, &asked));
                }
                (hardware_id, bodies)
            });
        }
        let mut flood: Vec<(String, Value)> = Vec::new();
        for (hardware_id, bodies) in taking.join_all().await {
            flood.extend(bodies.into_iter().map(|body| (hardware_id.clone(), body)));
        }
        assert_eq!(flood.len(), level);
        let ledger_before = ledger_of(&product, &format!("ld-{level}-")).await;

        // All at once, and a gateway that follows `Retry-After` starts with them.
        let started = Instant::now();
        let mut sending = tokio::task::JoinSet::new();
        for (_, body) in flood {
            let service = Arc::clone(&service);
            sending.spawn(async move {
                let (answer, took) = timed(&service, &body).await;
                (answer, took, started.elapsed())
            });
        }
        let real_id = format!("ld-{level}-real");
        let real = tokio::spawn(follow_retry_after(
            Arc::clone(&service),
            real_id.clone(),
            keys.remove(&real_id).expect("a key"),
        ));
        let answers = sending.join_all().await;
        let finished = answers
            .iter()
            .map(|(_, _, at)| *at)
            .max()
            .unwrap_or_default();
        let real = real.await.expect("the gateway's task");

        let mut served = Vec::new();
        let mut busy = Vec::new();
        for (answer, took, _) in &answers {
            match answer.status {
                201 => served.push(*took),
                503 => {
                    assert_eq!(answer.retry_after, Some(RETRY_BUSY_SECS), "{answer:?}");
                    busy.push(*took);
                }
                // The challenges were taken just before; a 410 would mean the host was too slow
                // for the measurement, not that the endpoint is wrong.
                status => panic!("level {level}: unexpected {status}: {answer:?}"),
            }
        }
        busy_renew += u64::try_from(busy.len()).expect("a count") + real.busy_at_redemption;
        busy_challenge += real.busy_at_challenge;
        let issued = ledger_of(&product, &format!("ld-{level}-")).await - ledger_before;
        assert_eq!(
            issued,
            i64::try_from(served.len()).expect("a count") + 1,
            "level {level}: a ledger row for each 201 (and the gateway's), none for a busy"
        );
        let counted: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM certs.renewal_refusals WHERE hardware_id LIKE $1)
                + (SELECT count(*) FROM certs.renewal_unverified_refusals
                   WHERE hardware_id LIKE $1)",
        )
        .bind(format!("ld-{level}-%"))
        .fetch_one(&mut admin)
        .await
        .expect("count the refusal rows");
        assert_eq!(counted, 0, "level {level}: `busy` is not counted");
        let seconds = finished.as_secs_f64().max(0.001);
        println!(
            "measure: {}",
            json!({
                "level": level,
                "answered_201": served.len(),
                "answered_503_busy": busy.len(),
                "busy_ratio": format!("{:.3}", busy.len() as f64 / level as f64),
                "all_answered_after_ms": finished.as_millis(),
                "issued_per_second": format!("{:.1}", served.len() as f64 / seconds),
                "served_latency": summary(served),
                "busy_latency": summary(busy),
                "gateway_following_retry_after": {
                    "first": real.first,
                    "tries": real.tries,
                    "busy_answers_at_challenge": real.busy_at_challenge,
                    "busy_answers_at_redemption": real.busy_at_redemption,
                    "until_certificate_ms": real.total.as_millis(),
                },
            })
        );
    }

    // The metric counts what the clients saw. `counters` keeps one number for each metric and
    // reason, the larger of the two steps' points, so that is what the clients' answers add up
    // to: the busy answers of the redemption step, or those of the challenge step.
    let metric = ("certs.renewal.refused".to_owned(), "busy".to_owned());
    let expected = busy_renew.max(busy_challenge);
    wait_for("the busy counter to equal the busy answers", || async {
        (counters(&collector).get(&metric).copied().unwrap_or(0) == expected).then_some(())
    })
    .await;
    println!(
        "measure: {}",
        json!({
            "certs.renewal.refused{reason=busy,step=renew}": busy_renew,
            "certs.renewal.refused{reason=busy,step=challenge}": busy_challenge,
        })
    );
    // A made-up nonce after the load is answered as before.
    let made_up = send(&service, &made_up_renewal("ld-base", 1)).await;
    assert!(made_up.is(409, &CHALLENGE_UNKNOWN), "{made_up:?}");
}

/// The ledger rows of the gateways whose hardware ID starts with `prefix`.
async fn ledger_of(product: &ProductPostgres, prefix: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM certs.issued_certificates
         WHERE profile = 'gateway' AND subject LIKE $1",
    )
    .bind(format!("{prefix}%"))
    .fetch_one(&mut product.superuser().await)
    .await
    .expect("count the ledger")
}

/// The refusal counters of the gateways with `prefix`: hardware ID, window start and count.
const VERIFIED_ROWS: &str = "SELECT hardware_id, window_start::text, refusals
FROM certs.renewal_refusals WHERE hardware_id LIKE $1";
const UNVERIFIED_ROWS: &str = "SELECT hardware_id, window_start::text, refusals
FROM certs.renewal_unverified_refusals WHERE hardware_id LIKE $1";

/// How many gateways, and how often each is refused by each kind of request, per round.
const GATEWAYS: usize = 20;
const MADE_UP_PER_ROUND: u8 = 3;
const WRONG_KEY_PER_ROUND: usize = 1;
const SUSPENDED_PER_ROUND: usize = 3;

/// What the clients of one run saw.
#[derive(Default)]
struct Seen {
    /// Refusals the service counts: 409, 401 and 403 answers.
    counted: u64,
    /// Everything else (503 `busy` and 429), which is not counted for the event.
    other: u64,
    /// Rows seen after each round: (hardware ID, window start) of each table.
    verified: BTreeSet<(String, String)>,
    unverified: BTreeSet<(String, String)>,
    peak_verified: usize,
    peak_unverified: usize,
}

/// One round: every gateway is refused by each kind of request. Gateways run side by side.
async fn round(
    service: &Arc<Service>,
    prefix: &str,
    keys: &Arc<Vec<KeyPair>>,
    n: u8,
) -> (u64, u64) {
    let forger = Arc::new(KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key"));
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..GATEWAYS {
        let (service, keys, forger) = (Arc::clone(service), Arc::clone(keys), Arc::clone(&forger));
        let hardware_id = format!("{prefix}{index}");
        tasks.spawn(async move {
            let (mut counted, mut other) = (0_u64, 0_u64);
            let mut tally = |answer: &Answer, expected: u16| {
                if answer.status == expected {
                    counted += 1;
                } else {
                    other += 1;
                }
            };
            // Without the key: a made-up nonce, and another key's signature.
            for k in 0..MADE_UP_PER_ROUND {
                let nonce = n.wrapping_mul(7).wrapping_add(k).wrapping_add(1);
                tally(
                    &send(&service, &made_up_renewal(&hardware_id, nonce)).await,
                    409,
                );
            }
            for _ in 0..WRONG_KEY_PER_ROUND {
                let asked = challenge(&service, &hardware_id).await;
                tally(&send(&service, &renew_request(&forger, &asked)).await, 401);
            }
            // With the key, after which the tenant turns out suspended.
            for _ in 0..SUSPENDED_PER_ROUND {
                let asked = challenge(&service, &hardware_id).await;
                tally(
                    &send(&service, &renew_request(&keys[index], &asked)).await,
                    403,
                );
            }
            (counted, other)
        });
    }
    tasks
        .join_all()
        .await
        .into_iter()
        .fold((0, 0), |(a, b), (c, d)| (a + c, b + d))
}

/// Looks at the counters now, so a window is seen while its row exists.
async fn sample(admin: &mut PgConnection, prefix: &str, seen: &mut Seen) {
    for (sql, rows, peak) in [
        (VERIFIED_ROWS, &mut seen.verified, &mut seen.peak_verified),
        (
            UNVERIFIED_ROWS,
            &mut seen.unverified,
            &mut seen.peak_unverified,
        ),
    ] {
        let now: Vec<(String, String, i64)> = sqlx::query_as(sql)
            .bind(format!("{prefix}%"))
            .fetch_all(&mut *admin)
            .await
            .expect("read the refusal rows");
        *peak = (*peak).max(now.len());
        rows.extend(
            now.into_iter()
                .map(|(hardware_id, start, _)| (hardware_id, start)),
        );
    }
}

/// The refusal events of the gateways with `prefix`, as payloads.
async fn events(admin: &mut PgConnection, prefix: &str) -> Vec<Value> {
    let envelopes: Vec<String> = sqlx::query_scalar(
        "SELECT envelope::text FROM certs.outbox_event
         WHERE event_type = 'certs.gateway_renewal_refused'
            AND aggregate_type = 'gateway_registration' AND aggregate_id LIKE $1",
    )
    .bind(format!("{prefix}%"))
    .fetch_all(admin)
    .await
    .expect("read the refusal events");
    envelopes
        .iter()
        .map(|envelope| {
            let envelope: Value = serde_json::from_str(envelope).expect("an envelope");
            envelope["payload"].clone()
        })
        .collect()
}

/// Whether an event's reasons are the ones that need no key.
fn unverified(payload: &Value) -> bool {
    payload["reasons"].as_object().is_some_and(|reasons| {
        reasons.keys().all(|reason| {
            [
                "challenge_unknown",
                "challenge_mismatch",
                "challenge_expired",
                "signature_invalid",
            ]
            .contains(&reason.as_str())
        })
    })
}

/// A run of one `renewal.refusal_window`: starts a replica with it, refuses the gateways
/// `rounds` times `gap` apart, and prints what the rows and the events came to.
///
/// `end_verified` moves the windows of the reasons after the signature two days back once the
/// rounds are over (as `tests/renewal.rs` does), so the events come at once; without it the
/// run waits for the windows to end in real time. The windows of the reasons that need no key
/// are a UTC day, so they are always moved.
#[allow(
    clippy::print_stdout,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "a measurement that prints its data; one run in one place"
)]
async fn run(
    product: &ProductPostgres,
    ca: &TestCa,
    label: &str,
    window: &str,
    rounds: u8,
    gap: Duration,
    end_verified: bool,
) {
    let mut env = FAST_CRLS.to_vec();
    env.extend([
        ("TRUSTIOT__RENEWAL__GATEWAY_BURST", "100"),
        ("TRUSTIOT__RENEWAL__GLOBAL_BURST", "10000"),
        ("TRUSTIOT__RENEWAL__GLOBAL_INTERVAL", "1ms"),
        ("TRUSTIOT__RENEWAL__REFUSAL_WINDOW", window),
        ("TRUSTIOT__RENEWAL__REFUSAL_FLUSH_INTERVAL", "1s"),
    ]);
    // Each run's own collector: it tells when the replica knows both lags.
    let collector = FakeCollector::start();
    let endpoint = collector.url();
    env.extend([
        ("TRUSTIOT__TELEMETRY__OTLP_ENDPOINT", endpoint.as_str()),
        ("OTEL_METRIC_EXPORT_INTERVAL", "300"),
    ]);
    let service = Arc::new(Service::start(product, ca, &env).await);
    wait_for("the published CRLs", || async {
        let (status, _) = service.get(certs::http::CRL_PATH).await;
        (status == 200).then_some(())
    })
    .await;
    let prefix = format!("vv-{label}-");
    let names: Vec<String> = (0..GATEWAYS).map(|n| format!("{prefix}{n}")).collect();
    let factory = Factory::new(&service).await;
    let keys = Arc::new(
        registered(
            product,
            &factory,
            &collector,
            &names,
            TenantStatus::Suspended,
        )
        .await,
    );
    let mut admin = product.superuser().await;

    let mut seen = Seen::default();
    for n in 0..rounds {
        if n > 0 {
            tokio::time::sleep(gap).await;
        }
        let (counted, other) = round(&service, &prefix, &keys, n).await;
        seen.counted += counted;
        seen.other += other;
        sample(&mut admin, &prefix, &mut seen).await;
    }
    let verified_windows: BTreeSet<&String> =
        seen.verified.iter().map(|(_, start)| start).collect();
    let unverified_windows: BTreeSet<&String> =
        seen.unverified.iter().map(|(_, start)| start).collect();
    // The most a window can hold: one row for each registered hardware ID.
    assert!(seen.peak_verified <= GATEWAYS * verified_windows.len().max(1));
    assert!(seen.peak_unverified <= GATEWAYS * unverified_windows.len().max(1));
    assert!(seen.verified.len() <= GATEWAYS * verified_windows.len());
    assert!(seen.unverified.len() <= GATEWAYS * unverified_windows.len());
    // Every request must have been refused the way its kind is, or the totals below do not
    // describe the scenario (a 503 `busy` or a 429 is not counted for the event).
    assert_eq!(
        seen.other, 0,
        "{label}: requests that were not refused as expected: the volume is not comparable"
    );
    let refused_requests = seen.counted + seen.other;
    println!(
        "measure: {}",
        json!({
            "window": window, "gateways": GATEWAYS, "rounds": rounds,
            "requests_refused_per_gateway_per_round":
                usize::from(MADE_UP_PER_ROUND) + WRONG_KEY_PER_ROUND + SUSPENDED_PER_ROUND,
            "refusals_seen_by_clients": seen.counted, "other_answers": seen.other,
            "refused_requests": refused_requests,
            "rows_after_the_rounds": {
                "renewal_refusals": {
                    "peak_rows": seen.peak_verified, "windows": verified_windows.len(),
                    "distinct_gateway_windows": seen.verified.len(),
                },
                "renewal_unverified_refusals": {
                    "peak_rows": seen.peak_unverified, "windows": unverified_windows.len(),
                    "distinct_gateway_windows": seen.unverified.len(),
                },
            },
        })
    );

    // The windows end: the day's always, the others when the run asks (or in real time).
    let like = format!("{prefix}%");
    sqlx::query(
        "UPDATE certs.renewal_unverified_refusals
         SET window_start = window_start - interval '2 days',
             window_end = window_end - interval '2 days'
         WHERE hardware_id LIKE $1",
    )
    .bind(&like)
    .execute(&mut admin)
    .await
    .expect("end the windows");
    if end_verified {
        sqlx::query(
            "UPDATE certs.renewal_refusals
             SET window_start = window_start - interval '2 days',
                 window_end = window_end - interval '2 days'
             WHERE hardware_id LIKE $1",
        )
        .bind(&like)
        .execute(&mut admin)
        .await
        .expect("end the windows");
    }
    // Real time: the last window ends within one window, then waits the report margin; allow
    // for both and the flush interval.
    let waited = Instant::now();
    let expected_verified = seen.verified.len();
    let expected_unverified = seen.unverified.len();
    let reported = {
        let deadline = Instant::now() + LIMIT * 4;
        loop {
            let reported = events(&mut admin, &prefix).await;
            let verified_events = reported.iter().filter(|event| !unverified(event)).count();
            let unverified_events = reported.len() - verified_events;
            if verified_events >= expected_verified && unverified_events >= expected_unverified {
                break reported;
            }
            assert!(
                Instant::now() < deadline,
                "{label}: {verified_events} of {expected_verified} events of the reasons after \
                 the signature and {unverified_events} of {expected_unverified} of the others \
                 within {:?}",
                LIMIT * 4
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    // Give a stray second event of a window time to show before counting.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let reported = {
        let later = events(&mut admin, &prefix).await;
        assert!(later.len() >= reported.len());
        later
    };

    let mut per_window: BTreeMap<(String, String, bool), usize> = BTreeMap::new();
    let mut event_refusals = 0_u64;
    let mut lengths: BTreeSet<(bool, i64)> = BTreeSet::new();
    for event in &reported {
        let kind = unverified(event);
        let start = event["window_start"].as_str().expect("a start").to_owned();
        let hardware_id = event["hardware_id"].as_str().expect("an ID").to_owned();
        *per_window.entry((hardware_id, start, kind)).or_default() += 1;
        event_refusals += event["refusals"].as_u64().expect("a count");
        let span = |key: &str| {
            chrono::DateTime::parse_from_rfc3339(event[key].as_str().expect("a time"))
                .expect("RFC 3339")
        };
        lengths.insert((
            kind,
            (span("window_end") - span("window_start")).num_seconds(),
        ));
    }
    assert!(
        per_window.values().all(|events| *events == 1),
        "one event per hardware ID, category and window: {per_window:?}"
    );
    assert_eq!(
        event_refusals, seen.counted,
        "{label}: the events' counts add up to the refusals the clients saw"
    );
    let rows_left: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM certs.renewal_unverified_refusals WHERE hardware_id LIKE $1)
            + (SELECT count(*) FROM certs.renewal_refusals WHERE hardware_id LIKE $1
               AND window_end < now() - interval '2 minutes')",
    )
    .bind(&like)
    .fetch_one(&mut admin)
    .await
    .expect("count the rows left");
    assert_eq!(rows_left, 0, "{label}: a reported window's row is deleted");
    let verified_events = reported.iter().filter(|event| !unverified(event)).count();
    let expected_window = humantime::parse_duration(window)
        .expect("a window")
        .as_secs();
    let expected_window = i64::try_from(expected_window).expect("seconds");
    assert!(lengths.contains(&(false, expected_window)), "{lengths:?}");
    assert!(lengths.contains(&(true, 86_400)), "{lengths:?}");
    println!(
        "measure: {}",
        json!({
            "window": window,
            "events_of_the_reasons_after_the_signature": verified_events,
            "events_of_the_reasons_that_need_no_key": reported.len() - verified_events,
            "events_total": reported.len(),
            "max_events_per_gateway_category_window": per_window.values().max(),
            "refusals_in_events": event_refusals,
            "refusals_seen_by_clients": seen.counted,
            "window_lengths_seconds": lengths.iter().map(|(kind, seconds)| json!({
                "reasons_need_no_key": kind, "seconds": seconds,
            })).collect::<Vec<_>>(),
            "seconds_until_the_events_were_all_there": waited.elapsed().as_secs(),
            "mean_event_bytes": mean_bytes(&mut admin, &prefix).await,
        })
    );
}

/// The mean size of a refusal event's envelope, in bytes.
async fn mean_bytes(admin: &mut PgConnection, prefix: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT coalesce(avg(octet_length(envelope::text))::bigint, 0) FROM certs.outbox_event
         WHERE event_type = 'certs.gateway_renewal_refused' AND aggregate_id LIKE $1",
    )
    .bind(format!("{prefix}%"))
    .fetch_one(admin)
    .await
    .expect("measure the events")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement: run it as the module documentation says"]
async fn refusal_event_volume() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    // Windows that are long (an hour, the day the reasons after the signature can have at most):
    // the windows are moved back to end them, and a gateway has one row and one event for each,
    // however often it is refused.
    run(&product, &ca, "hour", "1h", 3, Duration::from_secs(2), true).await;
    run(&product, &ca, "day", "24h", 3, Duration::from_secs(2), true).await;
    // The shortest window, in real time: rounds across at least two windows, and the events of
    // the windows that ended are written by the replica itself.
    run(
        &product,
        &ca,
        "minute",
        "60s",
        6,
        Duration::from_secs(15),
        false,
    )
    .await;
}
