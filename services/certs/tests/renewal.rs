//! Certificate renewal (Laysi/trustiot-v2#81; services/certs/README.md, "換發"), with the real
//! binary on the product's schema (`testkit::product`). Gateways are registered through the
//! factory registration API, with software P-256 keys standing in for their TPM keys, and bound
//! through `assets.gateway_bound` written as the assets service writes it.
//!
//! - `renewal_issues_for_the_registered_key_and_refuses_the_rest`: a registered, bound, active
//!   gateway whose only certificate is revoked gets a new certificate for its registered key that
//!   chains to the root, with one ledger row and one tenant event in the same transaction
//!   (acceptance 1, 5); a decommissioned, a never bound, an unbound and a suspended tenant's
//!   gateway get no certificate and no ledger row (acceptance 2), nor does the gateway of a
//!   deleted tenant or of a tenant the service has no state for (fail closed,
//!   Laysi/trustiot-v2#778); a replayed challenge is 409, a
//!   wrong signature 401 and voids the challenge, another device's or another purpose's challenge
//!   403, an expired one 410 (acceptance 3); every refusal has its log line and its metric, the
//!   refusals of a registered hardware ID become one platform-wide event per window with the
//!   counts, and an unregistered one leaves nothing (acceptance 5); the reasons that need no key
//!   are counted per day apart from the others, which keep `renewal.refusal_window`, and many
//!   at once still add up in one row per window (Laysi/trustiot-v2#847); expired challenges are
//!   removed by the next one (acceptance 4); a replica whose consumer lag is unknown answers 503
//!   without touching the challenge (acceptance 7; a too high lag is `tests/renewal_lag.rs`).
//! - `rate_limits_answer_429_and_bound_the_challenges`: requests that name a gateway's hardware ID
//!   without its key (challenges, redemptions signed by another key, made-up nonces), more than
//!   its own limit, leave that limit alone: the gateway still renews (Laysi/trustiot-v2#801).
//!   Past its own limit, which only redemptions whose signature held count against, the answer
//!   is 429 with `Retry-After` and the reason `gateway_rate_limited` (an INFO line, counted for
//!   the refusal event, in the window of the reasons after the signature, while the requests
//!   without the key are counted in the day's); past the global limit it is 429 with
//!   `rate_limited`, and a flood of
//!   challenge requests stores no more rows than the limit lets through (acceptance 4); each step
//!   has a global limit of its own, so a gateway that holds a challenge still renews after the
//!   flood used up the challenges' limit.
//! - `over_the_limits_a_stale_replica_logs_no_refusal_at_info`: the limits come before the lag
//!   check, so on a replica whose lag is unknown a flood of made-up hardware IDs gets 503 (one
//!   INFO line each) only as often as the limits let through, and 429 without an INFO line past
//!   them.
//!
//! A tenant's state stands in for the identity service's events here: the test stores it as
//! their consumer does (`store_tenant_status` in the tenant's transaction); the events
//! themselves are `tests/tenant_states.rs`.
//!
//! Skipped without podman or docker unless `CI` is set. Run with `--nocapture` to see the
//! evidence lines the test report quotes.

mod support;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use certs::bindings::{TenantStatus, store_tenant_status};
use certs::pem;
use certs::renewal::{
    BINDINGS_STALE, CHALLENGE_EXPIRED, CHALLENGE_MISMATCH, CHALLENGE_UNKNOWN, DECOMMISSIONED,
    NONCE_INVALID, NOT_BOUND, SIGNATURE_REFUSED,
};
use contract::events::certs::GatewayRenewalRefused;
use contract::{GatewayBindingId, TenantId};
use protocol::renewal::{Challenge, RENEWAL_PATH, RenewedCertificate};
use rcgen::{KeyPair, PKCS_ECDSA_P256_SHA256, PublicKeyData as _};
use serde_json::{Value, json};
use support::renewal::{
    Answer, FAST_CRLS, Factory, LOGINS, REFUSED, REFUSED_EVENTS, RENEWED_EVENTS, assets_writer,
    bind, certs_work, challenge, challenge_request, counters, ledger, made_up_renewal,
    refusal_lines, renew_request, send, wait_for_bindings_lag,
};
use support::{FakeCollector, Service, TestCa, now, store_status, wait_for};
use testkit::product::{ProductPostgres, product_postgres};
use x509_parser::certificate::X509Certificate;
use x509_parser::prelude::FromDer;

/// The refusal counts of a hardware ID: its windows' totals and reasons, of both categories.
const REFUSAL_ROWS: &str = "SELECT refusals, reasons::text FROM (
    SELECT window_start, refusals, reasons FROM certs.renewal_refusals WHERE hardware_id = $1
    UNION ALL
    SELECT window_start, refusals, reasons FROM certs.renewal_unverified_refusals
    WHERE hardware_id = $1
) AS refusal ORDER BY window_start";

/// The refusal windows of a hardware ID by category (Laysi/trustiot-v2#847): whether the table
/// is the one of the reasons that need no key, the window's length in seconds, the count and
/// the reasons.
const REFUSAL_WINDOWS: &str = "SELECT false, extract(epoch FROM window_end - window_start)::bigint,
    refusals, reasons::text
FROM certs.renewal_refusals WHERE hardware_id = $1
UNION ALL
SELECT true, extract(epoch FROM window_end - window_start)::bigint, refusals, reasons::text
FROM certs.renewal_unverified_refusals WHERE hardware_id = $1
ORDER BY 1, 2";

/// What the platform role and the service role may do with the table of the refusals that need
/// no key: the platform role exactly what it may with `renewal_refusals`, the service role
/// nothing. A list of privileges is true when the role has any of them, so the ones the
/// platform role must have are checked one by one.
const UNVERIFIED_GRANTS: &str = "SELECT
    has_table_privilege('certs_platform', 'certs.renewal_unverified_refusals', 'SELECT')
        AND has_table_privilege('certs_platform', 'certs.renewal_unverified_refusals', 'INSERT')
        AND has_table_privilege('certs_platform', 'certs.renewal_unverified_refusals', 'UPDATE')
        AND has_table_privilege('certs_platform', 'certs.renewal_unverified_refusals', 'DELETE'),
    has_table_privilege('certs_platform', 'certs.renewal_unverified_refusals',
        'TRUNCATE, REFERENCES, TRIGGER'),
    has_table_privilege('certs_app', 'certs.renewal_unverified_refusals',
        'SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER')";

/// A row of a reason after the signature, which the table of the reasons that need no key must
/// refuse.
const STORE_VERIFIED_AS_UNVERIFIED: &str = "INSERT INTO certs.renewal_unverified_refusals
    (hardware_id, window_start, window_end, refusals, reasons)
VALUES ('rn-dec', now(), now() + interval '1 day', 1, jsonb_build_object('not_bound', 1))";

/// The start of the day window (`certs::renewal`'s `UNVERIFIED_WINDOW`) the database is in now.
const DAY_WINDOW_NOW: &str =
    "SELECT date_bin(interval '1 day', now(), TIMESTAMPTZ '2000-01-01 00:00:00+00')::text";

/// One row of [`REFUSAL_WINDOWS`].
type RefusalWindow = (bool, i64, i64, String);

/// The length of a refusal event's window in seconds, from its payload.
fn window_seconds(payload: &Value) -> i64 {
    let event: GatewayRenewalRefused =
        serde_json::from_value(payload.clone()).expect("a refusal event's payload");
    (event.window_end - event.window_start).num_seconds()
}

/// The renewal endpoint's answer to a redemption of a made-up nonce, sent on a task of its own.
async fn made_up_status(url: String, body: Value) -> u16 {
    reqwest::Client::new()
        .post(url)
        .timeout(Duration::from_secs(30))
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .expect("call the renewal endpoint")
        .status()
        .as_u16()
}

/// A stored binding, as the consumer would store it. `$1` tenant, `$2` binding, `$3` hardware
/// ID; its position is far after any event of the test, and it has ended.
const STORE_ENDED_BINDING: &str = "INSERT INTO certs.gateway_bindings
    (tenant_id, binding_id, hardware_id, bound_at, position_tx, position_id, unbound_at)
VALUES ($1::uuid, $2::uuid, $3, now() - interval '1 hour', 9000000000, 0, now())";

/// The most requests a global limit of `burst` and one more every `interval` lets through within
/// `elapsed`.
fn admitted_at_most(burst: u64, interval: Duration, elapsed: Duration) -> u64 {
    burst + elapsed.as_secs() / interval.as_secs().max(1) + 1
}

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario on one database: the refusal counts and metrics add up every step"
)]
async fn renewal_issues_for_the_registered_key_and_refuses_the_rest() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let collector = FakeCollector::start();
    let endpoint = collector.url();
    let mut env = FAST_CRLS.to_vec();
    env.extend([
        ("TRUSTIOT__TELEMETRY__OTLP_ENDPOINT", endpoint.as_str()),
        ("OTEL_METRIC_EXPORT_INTERVAL", "300"),
        // Limits no step of this scenario reaches; as many requests at once as the service
        // allows, for the burst of refusals below.
        ("TRUSTIOT__RENEWAL__GATEWAY_BURST", "100"),
        ("TRUSTIOT__RENEWAL__GLOBAL_BURST", "10000"),
        ("TRUSTIOT__RENEWAL__GLOBAL_INTERVAL", "1ms"),
        ("TRUSTIOT__RENEWAL__MAX_CONCURRENT", "3"),
        // An hour for the refusals after the signature, so their windows differ from the day of
        // the ones that need no key. Each of those gateways is refused once, so it has one
        // window; the checks of rn-ok allow for crossing midnight UTC.
        ("TRUSTIOT__RENEWAL__REFUSAL_WINDOW", "1h"),
        ("TRUSTIOT__RENEWAL__REFUSAL_FLUSH_INTERVAL", "1s"),
    ]);
    let service = Service::start(&product, &ca, &env).await;
    wait_for("the published CRLs", || async {
        let (status, _) = service.get(certs::http::CRL_PATH).await;
        (status == 200).then_some(())
    })
    .await;

    let factory = Factory::new(&service).await;
    let ok_key = factory.register("rn-ok").await;
    let other_key = factory.register("rn-other").await;
    let decommissioned_key = factory.register("rn-dec").await;
    let never_key = factory.register("rn-never").await;
    let ended_key = factory.register("rn-ended").await;
    let suspended_key = factory.register("rn-susp").await;
    let deleted_key = factory.register("rn-del").await;
    let unknown_key = factory.register("rn-unknown").await;
    // Only named by requests without its key, many at once (Laysi/trustiot-v2#847).
    factory.register("rn-burst").await;

    let assets = assets_writer(&product);
    let (tenant, other_tenant) = (TenantId::generate(), TenantId::generate());
    let (deleted_tenant, unknown_tenant) = (TenantId::generate(), TenantId::generate());
    // Only an active tenant renews; `unknown_tenant` never gets a state.
    store_status(&product, tenant, TenantStatus::Active, 1).await;
    for (status, version) in [
        (TenantStatus::Active, 1),
        (TenantStatus::Suspended, 2),
        (TenantStatus::Deleted, 3),
    ] {
        store_status(&product, deleted_tenant, status, version).await;
    }
    bind(&product, &assets, deleted_tenant, "rn-del").await;
    bind(&product, &assets, unknown_tenant, "rn-unknown").await;
    let ok_binding = bind(&product, &assets, tenant, "rn-ok").await;
    bind(&product, &assets, tenant, "rn-other").await;
    bind(&product, &assets, tenant, "rn-dec").await;
    bind(&product, &assets, other_tenant, "rn-susp").await;
    let mut admin = product.superuser().await;
    sqlx::query(STORE_ENDED_BINDING)
        .bind(tenant.to_string())
        .bind(GatewayBindingId::generate().to_string())
        .bind("rn-ended")
        .execute(&mut admin)
        .await
        .expect("store an ended binding");
    sqlx::query(
        "UPDATE certs.gateway_registrations SET decommissioned_at = now()
         WHERE hardware_id = 'rn-dec'",
    )
    .execute(&mut admin)
    .await
    .expect("decommission rn-dec");
    let work = certs_work(&product);
    let mut tx = work
        .begin(other_tenant, "test-suspend")
        .await
        .expect("a tenant transaction");
    assert!(
        store_tenant_status(&mut tx, other_tenant, TenantStatus::Suspended, 1)
            .await
            .expect("suspend the tenant")
    );
    tx.commit().await.expect("commit the suspension");

    // The endpoint decides once the consumer's lag is known; until then it answers 503.
    wait_for_bindings_lag(&collector).await;

    // Acceptance 1: the gateway's only certificate is revoked; the renewal does not look at it
    // (any reason but keyCompromise, which ends renewals: Laysi/trustiot-v2#636).
    sqlx::query(
        "UPDATE certs.issued_certificates SET revoked_at = now(), revocation_reason = 4
         WHERE profile = 'gateway' AND subject = 'rn-ok'",
    )
    .execute(&mut admin)
    .await
    .expect("revoke rn-ok's certificate");
    assert_eq!(ledger(&product, "rn-ok").await, 1);
    let first = challenge(&service, "rn-ok").await;
    assert_eq!(first.hardware_id.as_str(), "rn-ok");
    assert_eq!(first.purpose.as_str(), "cert-renewal");
    let ttl = i64::try_from(first.expires).expect("seconds") - now();
    assert!((25..=31).contains(&ttl), "expires in {ttl} s");
    let renewed_request = renew_request(&ok_key, &first);
    let renewed = send(&service, &renewed_request).await;
    assert_eq!(renewed.status, 201, "{renewed:?}");
    assert_eq!(renewed.cache_control.as_deref(), Some("no-store"));
    println!("201: {}", renewed.body);
    let renewed: RenewedCertificate = serde_json::from_value(renewed.body).expect("renewed");
    assert_eq!(renewed.hardware_id.as_str(), "rn-ok");
    let certificate = pem::decode_all(&renewed.certificate, pem::CERTIFICATE)
        .expect("PEM")
        .remove(0);
    let (_, parsed) = X509Certificate::from_der(&certificate).expect("a certificate");
    let common_names: Vec<&str> = parsed
        .subject()
        .iter_common_name()
        .filter_map(|name| name.as_str().ok())
        .collect();
    assert_eq!(common_names, ["rn-ok"]);
    assert_eq!(
        parsed.public_key().raw,
        ok_key.subject_public_key_info().as_slice(),
        "the certificate is for the registered key"
    );
    assert_eq!(certs::issue::hex(parsed.raw_serial()), renewed.serial);
    let chain = pem::decode_all(&renewed.certificate_chain, pem::CERTIFICATE).expect("PEM");
    assert_eq!(chain, [certificate.clone(), ca.issuing_ders()[0].clone()]);
    assert_eq!(
        pem::decode_all(&renewed.root_certificate, pem::CERTIFICATE).expect("PEM"),
        [ca.root_der()]
    );
    support::verify_client(&ca.root_der(), &ca.issuing_ders(), &certificate, &[], now())
        .expect("the renewed certificate is a client certificate under the root");
    assert_eq!(ledger(&product, "rn-ok").await, 2);
    // Acceptance 5: one tenant event, written in the ledger row's transaction.
    let aggregate = format!("{tenant}:rn-ok");
    let events: Vec<(String, String, String)> = sqlx::query_as(RENEWED_EVENTS)
        .bind(&aggregate)
        .fetch_all(&mut admin)
        .await
        .expect("read the renewal events");
    let [(envelope, event_tenant, event_xmin)] = events.as_slice() else {
        panic!("one renewal event, not {events:?}");
    };
    assert_eq!(*event_tenant, tenant.to_string());
    let envelope: Value = serde_json::from_str(envelope).expect("an envelope");
    assert_eq!(
        envelope["actor"],
        json!({"kind": "service", "service": "certs"})
    );
    assert_eq!(envelope["payload"]["certificate_serial"], renewed.serial);
    assert_eq!(envelope["payload"]["binding_id"], ok_binding.to_string());
    assert_eq!(envelope["payload"]["ca_key_id"], renewed.ca_key_id);
    let ledger_xmin: String = sqlx::query_scalar(
        "SELECT xmin::text FROM certs.issued_certificates WHERE serial = decode($1, 'hex')",
    )
    .bind(&renewed.serial)
    .fetch_one(&mut admin)
    .await
    .expect("read the ledger row");
    assert_eq!(
        *event_xmin, ledger_xmin,
        "the event and the ledger row come from one transaction"
    );
    let read = service
        .stdout
        .messages("read a gateway for a certificate renewal")
        .into_iter()
        .find(|line| line["fields"]["hardware_id"] == "rn-ok")
        .expect("the read's log line");
    assert_eq!(read["fields"]["tenant_id"], tenant.to_string());
    println!(
        "evidence: renewed rn-ok after its only certificate was revoked: serial {}, CN rn-ok, the \
         registered key, chain to the root; ledger 1 -> 2; one event in transaction {event_xmin} \
         (ledger row {ledger_xmin}); read log: {}",
        renewed.serial, read["fields"]
    );

    // Acceptance 2: each gets a valid challenge and signs it, and is refused with nothing written.
    for (hardware_id, key, status, expected) in [
        ("rn-dec", &decommissioned_key, 403, DECOMMISSIONED.as_str()),
        ("rn-never", &never_key, 403, NOT_BOUND.as_str()),
        ("rn-ended", &ended_key, 403, NOT_BOUND.as_str()),
        ("rn-susp", &suspended_key, 403, "error.tenant_suspended"),
        ("rn-del", &deleted_key, 403, "error.tenant_suspended"),
        ("rn-unknown", &unknown_key, 403, "error.tenant_suspended"),
    ] {
        let answer = send(
            &service,
            &renew_request(key, &challenge(&service, hardware_id).await),
        )
        .await;
        assert_eq!(
            (answer.status, answer.message_key()),
            (status, expected),
            "{hardware_id}: {answer:?}"
        );
        assert_eq!(
            answer.body["code"],
            if ["rn-susp", "rn-del", "rn-unknown"].contains(&hardware_id) {
                "tenant_suspended"
            } else {
                "permission_denied"
            }
        );
        assert_eq!(ledger(&product, hardware_id).await, 1, "{hardware_id}");
        println!("evidence: {hardware_id}: {} {}", answer.status, answer.body);
    }
    for (hardware_id, tenant) in [
        ("rn-dec", tenant),
        ("rn-susp", other_tenant),
        ("rn-del", deleted_tenant),
        ("rn-unknown", unknown_tenant),
    ] {
        let events: Vec<(String, String, String)> = sqlx::query_as(RENEWED_EVENTS)
            .bind(format!("{tenant}:{hardware_id}"))
            .fetch_all(&mut admin)
            .await
            .expect("read the renewal events");
        assert!(events.is_empty(), "{hardware_id}: {events:?}");
    }

    // Acceptance 3: a replay of the request that renewed.
    let replayed = send(&service, &renewed_request).await;
    assert!(replayed.is(409, &CHALLENGE_UNKNOWN), "{replayed:?}");
    // A signature by another key: 401, and the challenge is spent.
    let tampered = challenge(&service, "rn-ok").await;
    let mut forged = renew_request(&other_key, &tampered);
    forged["hardware_id"] = json!("rn-ok");
    let answer = send(&service, &forged).await;
    assert!(answer.is(401, &SIGNATURE_REFUSED), "{answer:?}");
    let answer = send(&service, &renew_request(&ok_key, &tampered)).await;
    assert!(answer.is(409, &CHALLENGE_UNKNOWN), "{answer:?}");
    // Another device's challenge: 403, and spent for that device too.
    let theirs = challenge(&service, "rn-other").await;
    let mut borrowed = renew_request(&ok_key, &theirs);
    borrowed["hardware_id"] = json!("rn-ok");
    let answer = send(&service, &borrowed).await;
    assert!(answer.is(403, &CHALLENGE_MISMATCH), "{answer:?}");
    let answer = send(&service, &renew_request(&other_key, &theirs)).await;
    assert!(answer.is(409, &CHALLENGE_UNKNOWN), "{answer:?}");
    // Another purpose's challenge for the same gateway (none hands them out yet).
    let mut nonce = [0_u8; 32];
    getrandom::fill(&mut nonce).expect("random");
    sqlx::query(
        "INSERT INTO certs.renewal_challenges (nonce, hardware_id, purpose, expires)
         VALUES ($1, 'rn-ok', 'rebind', floor(extract(epoch FROM now()))::bigint + 30)",
    )
    .bind(nonce.as_slice())
    .execute(&mut admin)
    .await
    .expect("store a challenge of another purpose");
    let rebind = Challenge {
        nonce: URL_SAFE_NO_PAD.encode(nonce),
        ..first.clone()
    };
    let answer = send(&service, &renew_request(&ok_key, &rebind)).await;
    assert!(answer.is(403, &CHALLENGE_MISMATCH), "{answer:?}");
    // An expired challenge: 410, then spent.
    let late = challenge(&service, "rn-ok").await;
    sqlx::query(
        "UPDATE certs.renewal_challenges SET expires = floor(extract(epoch FROM now()))::bigint - 5
         WHERE nonce = $1",
    )
    .bind(URL_SAFE_NO_PAD.decode(&late.nonce).expect("a nonce"))
    .execute(&mut admin)
    .await
    .expect("expire the challenge");
    let answer = send(&service, &renew_request(&ok_key, &late)).await;
    assert!(answer.is(410, &CHALLENGE_EXPIRED), "{answer:?}");
    assert_eq!(answer.body["code"], "conflict");
    let answer = send(&service, &renew_request(&ok_key, &late)).await;
    assert!(answer.is(409, &CHALLENGE_UNKNOWN), "{answer:?}");
    // A hardware ID that is not registered: the same challenge, the same 401 as a wrong
    // signature.
    let ghost_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
    let ghost = challenge(&service, "rn-ghost").await;
    assert_eq!(ghost.hardware_id.as_str(), "rn-ghost");
    let answer = send(&service, &renew_request(&ghost_key, &ghost)).await;
    assert!(answer.is(401, &SIGNATURE_REFUSED), "{answer:?}");
    // A malformed request is refused before any challenge is touched.
    let answer = send(
        &service,
        &json!({"step": "renew", "hardware_id": "rn-ok", "nonce": "short",
            "signature": "MEUCIQ=="}),
    )
    .await;
    assert_eq!(answer.status, 422, "{answer:?}");
    assert_eq!(
        answer.body["field_errors"][0]["message_key"],
        NONCE_INVALID.as_str()
    );
    assert_eq!(
        ledger(&product, "rn-ok").await,
        2,
        "nothing more was issued"
    );
    assert_eq!(ledger(&product, "rn-other").await, 1);

    // Acceptance 5: every refusal has its log line ...
    let expected_lines = [
        (
            "rn-ok",
            &[
                "challenge_unknown",
                "signature_invalid",
                "challenge_unknown",
                "challenge_mismatch",
                "challenge_mismatch",
                "challenge_expired",
                "challenge_unknown",
            ][..],
        ),
        ("rn-other", &["challenge_unknown"][..]),
        ("rn-dec", &["decommissioned"][..]),
        ("rn-never", &["not_bound"][..]),
        ("rn-ended", &["not_bound"][..]),
        ("rn-susp", &["tenant_suspended"][..]),
        ("rn-del", &["tenant_suspended"][..]),
        ("rn-unknown", &["tenant_suspended"][..]),
        ("rn-ghost", &["signature_invalid"][..]),
    ];
    for (hardware_id, reasons) in expected_lines {
        let lines = wait_for(&format!("the refusal lines of {hardware_id}"), || async {
            let lines = refusal_lines(&service, hardware_id);
            (lines.len() >= reasons.len()).then_some(lines)
        })
        .await;
        assert_eq!(lines, reasons, "{hardware_id}");
    }
    // ... and its metric, by reason.
    let expected_metrics: BTreeMap<&str, u64> = [
        ("challenge_unknown", 4),
        ("signature_invalid", 2),
        ("challenge_mismatch", 2),
        ("challenge_expired", 1),
        ("decommissioned", 1),
        ("not_bound", 2),
        ("tenant_suspended", 3),
        ("invalid_request", 1),
    ]
    .into_iter()
    .collect();
    let totals = wait_for("the refusal metrics", || async {
        let totals = counters(&collector);
        let matches = expected_metrics.iter().all(|(reason, expected)| {
            totals.get(&("certs.renewal.refused".to_owned(), (*reason).to_owned()))
                == Some(expected)
        }) && totals.get(&("certs.renewal.issued".to_owned(), String::new()))
            == Some(&1);
        matches.then_some(totals)
    })
    .await;
    println!("evidence: metrics {totals:?}");

    // The registered hardware IDs' refusals are counted per window; the unregistered one is not.
    let rows = |hardware_id: &'static str| {
        let product = &product;
        async move {
            let rows: Vec<(i64, String)> = sqlx::query_as(REFUSAL_ROWS)
                .bind(hardware_id)
                .fetch_all(&mut product.superuser().await)
                .await
                .expect("read the refusal counts");
            rows
        }
    };
    let counted = rows("rn-ok").await;
    let mut reasons: BTreeMap<String, u64> = BTreeMap::new();
    for (_, text) in &counted {
        let window: BTreeMap<String, u64> = serde_json::from_str(text).expect("reasons");
        for (reason, count) in window {
            *reasons.entry(reason).or_default() += count;
        }
    }
    assert_eq!(counted.iter().map(|(total, _)| total).sum::<i64>(), 7);
    assert_eq!(
        reasons,
        BTreeMap::from([
            ("challenge_expired".to_owned(), 1),
            ("challenge_mismatch".to_owned(), 2),
            ("challenge_unknown".to_owned(), 3),
            ("signature_invalid".to_owned(), 1),
        ])
    );
    assert!(
        rows("rn-ghost").await.is_empty(),
        "nothing for an unregistered hardware ID"
    );
    // Laysi/trustiot-v2#847: the reasons that need no key are counted per day in their own
    // table, the others per `renewal.refusal_window` (an hour here).
    let windows_of = |hardware_id: &'static str| {
        let product = &product;
        async move {
            let rows: Vec<RefusalWindow> = sqlx::query_as(REFUSAL_WINDOWS)
                .bind(hardware_id)
                .fetch_all(&mut product.superuser().await)
                .await
                .expect("read the refusal windows");
            rows
        }
    };
    let ok_windows = windows_of("rn-ok").await;
    assert!(!ok_windows.is_empty());
    for (unverified, seconds, _, reasons) in &ok_windows {
        assert!(unverified, "rn-ok's reasons all need no key: {reasons}");
        assert_eq!(*seconds, 86_400, "a day: {reasons}");
    }
    for hardware_id in ["rn-dec", "rn-never", "rn-susp"] {
        let found = windows_of(hardware_id).await;
        assert_eq!(found.len(), 1, "{hardware_id}: {found:?}");
        let (unverified, seconds, refusals, _) = &found[0];
        assert!(!unverified, "{hardware_id}: refused after the signature");
        assert_eq!((*seconds, *refusals), (3600, 1), "{hardware_id}");
    }
    println!(
        "evidence: refusal windows (unverified, seconds, refusals, reasons) of rn-ok \
         {ok_windows:?}, of rn-dec {:?}",
        windows_of("rn-dec").await
    );
    let grants: (bool, bool, bool) = sqlx::query_as(UNVERIFIED_GRANTS)
        .fetch_one(&mut admin)
        .await
        .expect("read the grants");
    assert_eq!(
        grants,
        (true, false, false),
        "certs_platform: SELECT, INSERT, UPDATE, DELETE only; certs_app: nothing"
    );
    // The table admits only the reasons that need no key.
    let refused = sqlx::query(STORE_VERIFIED_AS_UNVERIFIED)
        .execute(&mut admin)
        .await
        .expect_err("a reason after the signature is refused");
    assert_eq!(
        refused
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514"),
        "{refused}"
    );
    println!("evidence: the grants (platform, platform extra, app) {grants:?}; {refused}");

    // Many refusals at once without the key add up in one row per window: the primary key and
    // the upsert, never a second row or a lost count. The service lets 3 requests at a time do
    // database work; the rest wait up to 2 seconds or get 503 `busy`, which is not counted.
    let day_before: String = sqlx::query_scalar(DAY_WINDOW_NOW)
        .fetch_one(&mut admin)
        .await
        .expect("read the day");
    let mut burst = tokio::task::JoinSet::new();
    for n in 0..40_u8 {
        burst.spawn(made_up_status(
            format!("{}{RENEWAL_PATH}", service.base_url),
            made_up_renewal("rn-burst", n),
        ));
    }
    let statuses = burst.join_all().await;
    let day_after: String = sqlx::query_scalar(DAY_WINDOW_NOW)
        .fetch_one(&mut admin)
        .await
        .expect("read the day");
    assert!(
        statuses.iter().all(|status| [409, 503].contains(status)),
        "{statuses:?}"
    );
    let conflicts = statuses.iter().filter(|status| **status == 409).count();
    assert!(conflicts >= 3, "at least one turn each: {statuses:?}");
    let burst_windows = windows_of("rn-burst").await;
    let days = if day_before == day_after { 1 } else { 2 };
    assert!(
        (1..=days).contains(&burst_windows.len()),
        "one row per day window: {burst_windows:?}"
    );
    assert!(
        burst_windows
            .iter()
            .all(|(unverified, seconds, _, _)| *unverified && *seconds == 86_400),
        "{burst_windows:?}"
    );
    let burst_total: i64 = burst_windows
        .iter()
        .map(|(_, _, refusals, _)| refusals)
        .sum();
    assert_eq!(
        burst_total,
        i64::try_from(conflicts).expect("a count"),
        "every 409 counted once: {burst_windows:?}"
    );
    println!(
        "evidence: 40 made-up redemptions of rn-burst at once: {conflicts} times 409, the rest \
         503 busy; refusal windows {burst_windows:?}"
    );

    // The windows end (moved two days back), and each becomes one platform-wide event.
    sqlx::query(
        "UPDATE certs.renewal_refusals
         SET window_start = window_start - interval '2 days',
             window_end = window_end - interval '2 days'
         WHERE hardware_id LIKE 'rn-%'",
    )
    .execute(&mut admin)
    .await
    .expect("end the windows");
    sqlx::query(
        "UPDATE certs.renewal_unverified_refusals
         SET window_start = window_start - interval '2 days',
             window_end = window_end - interval '2 days'
         WHERE hardware_id LIKE 'rn-%'",
    )
    .execute(&mut admin)
    .await
    .expect("end the windows");
    let windows = counted.len();
    let reported = wait_for("the refusal events of rn-ok", || async {
        let events: Vec<String> = sqlx::query_scalar(REFUSED_EVENTS)
            .bind("rn-ok")
            .fetch_all(&mut product.superuser().await)
            .await
            .expect("read the refusal events");
        (events.len() >= windows).then_some(events)
    })
    .await;
    assert_eq!(reported.len(), windows, "one event per window");
    let mut total = 0;
    for envelope in &reported {
        let envelope: Value = serde_json::from_str(envelope).expect("an envelope");
        assert_eq!(envelope["tenant_id"], Value::Null);
        assert_eq!(
            envelope["actor"],
            json!({"kind": "service", "service": "certs"})
        );
        total += envelope["payload"]["refusals"].as_u64().expect("a count");
        println!("evidence: refusal event {}", envelope["payload"]);
    }
    assert_eq!(total, 7);
    // Each event's window is its category's: a day for rn-ok, an hour for rn-dec; rn-burst's
    // refusals are one event per day window, with every counted refusal.
    for envelope in &reported {
        let envelope: Value = serde_json::from_str(envelope).expect("an envelope");
        assert_eq!(window_seconds(&envelope["payload"]), 86_400, "{envelope}");
    }
    for (hardware_id, events, seconds, refusals) in [
        ("rn-dec", 1, 3600, 1),
        ("rn-burst", burst_windows.len(), 86_400, burst_total),
    ] {
        let found = wait_for(&format!("the refusal events of {hardware_id}"), || async {
            let found: Vec<String> = sqlx::query_scalar(REFUSED_EVENTS)
                .bind(hardware_id)
                .fetch_all(&mut product.superuser().await)
                .await
                .expect("read the refusal events");
            (found.len() >= events).then_some(found)
        })
        .await;
        assert_eq!(found.len(), events, "{hardware_id}: {found:?}");
        let mut sum = 0;
        for envelope in &found {
            let envelope: Value = serde_json::from_str(envelope).expect("an envelope");
            assert_eq!(window_seconds(&envelope["payload"]), seconds, "{envelope}");
            sum += envelope["payload"]["refusals"].as_i64().expect("a count");
            println!("evidence: refusal event {}", envelope["payload"]);
        }
        assert_eq!(sum, refusals, "{hardware_id}: {found:?}");
    }
    // Reported once: the rows are gone, and a later round adds nothing.
    assert!(rows("rn-ok").await.is_empty());
    assert!(windows_of("rn-burst").await.is_empty());
    tokio::time::sleep(Duration::from_secs(3)).await;
    let again: Vec<String> = sqlx::query_scalar(REFUSED_EVENTS)
        .bind("rn-ok")
        .fetch_all(&mut admin)
        .await
        .expect("read the refusal events");
    assert_eq!(again.len(), windows);
    let ghost_events: Vec<String> = sqlx::query_scalar(REFUSED_EVENTS)
        .bind("rn-ghost")
        .fetch_all(&mut admin)
        .await
        .expect("read the refusal events");
    assert!(ghost_events.is_empty());

    // Acceptance 4: a challenge removes the ones that expired long ago, so they do not pile up.
    for n in 0..20 {
        challenge(&service, &format!("rn-flood-{n}")).await;
    }
    let flood = "SELECT count(*) FROM certs.renewal_challenges WHERE hardware_id LIKE 'rn-flood-%'";
    let stored: i64 = sqlx::query_scalar(flood)
        .fetch_one(&mut admin)
        .await
        .expect("count");
    assert_eq!(stored, 20);
    sqlx::query(
        "UPDATE certs.renewal_challenges SET expires = expires - 1000
         WHERE hardware_id LIKE 'rn-flood-%'",
    )
    .execute(&mut admin)
    .await
    .expect("age the challenges");
    challenge(&service, "rn-flood-last").await;
    let stored: i64 = sqlx::query_scalar(flood)
        .fetch_one(&mut admin)
        .await
        .expect("count");
    assert_eq!(stored, 1, "the next challenge removed the expired ones");

    // Acceptance 7: a replica whose consumer lag is unknown (its platform role cannot log in)
    // answers 503 before touching the challenge, and issues nothing.
    let mut blind_env = env.clone();
    blind_env.push(("TRUSTIOT__DATABASE__PLATFORM_PASSWORD", "not-the-password"));
    let blind = Service::start(&product, &ca, &blind_env).await;
    let pending = challenge(&service, "rn-ok").await;
    let request = renew_request(&ok_key, &pending);
    for body in [&request, &challenge_request("rn-ok")] {
        let answer = send(&blind, body).await;
        assert!(answer.is(503, &BINDINGS_STALE), "{answer:?}");
        assert_eq!(answer.retry_after, Some(30));
    }
    assert_eq!(ledger(&product, "rn-ok").await, 2);
    // The challenge was left alone: the working replica redeems it.
    let answer = send(&service, &request).await;
    assert_eq!(answer.status, 201, "{answer:?}");
    assert_eq!(ledger(&product, "rn-ok").await, 3);
    println!(
        "evidence: unknown lag: 503 bindings_stale, Retry-After 30; the challenge was still valid"
    );
}

/// Per hardware ID 2 redemptions whose signature held, then one an hour; per step 12 requests at
/// once, then one a minute.
const TIGHT_LIMITS: [(&str, &str); 4] = [
    ("TRUSTIOT__RENEWAL__GATEWAY_BURST", "2"),
    ("TRUSTIOT__RENEWAL__GATEWAY_INTERVAL", "1h"),
    ("TRUSTIOT__RENEWAL__GLOBAL_BURST", "12"),
    ("TRUSTIOT__RENEWAL__GLOBAL_INTERVAL", "60s"),
];

/// The limits of [`TIGHT_LIMITS`].
const GATEWAY_BURST: u64 = 2;
const GLOBAL_BURST: u64 = 12;
const GLOBAL_INTERVAL: Duration = Duration::from_secs(60);

/// The turns a test took from each step's global limit, so it can tell how many are left.
#[derive(Default)]
struct Turns {
    challenges: u64,
    renewals: u64,
}

impl Turns {
    async fn challenge(&mut self, service: &Service, hardware_id: &str) -> Challenge {
        self.challenges += 1;
        challenge(service, hardware_id).await
    }

    async fn send(&mut self, service: &Service, body: &Value) -> Answer {
        if body["step"] == "renew" {
            self.renewals += 1;
        } else {
            self.challenges += 1;
        }
        send(service, body).await
    }
}

/// The refusal counts of `hardware_id` by reason, over all its windows.
async fn refusal_reasons(product: &ProductPostgres, hardware_id: &str) -> BTreeMap<String, u64> {
    let rows: Vec<(i64, String)> = sqlx::query_as(REFUSAL_ROWS)
        .bind(hardware_id)
        .fetch_all(&mut product.superuser().await)
        .await
        .expect("read the refusal counts");
    let mut reasons = BTreeMap::new();
    for (_, text) in rows {
        let window: BTreeMap<String, u64> = serde_json::from_str(&text).expect("reasons");
        for (reason, count) in window {
            *reasons.entry(reason).or_default() += count;
        }
    }
    reasons
}

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario on one replica: each step uses up the turns the next one counts on"
)]
async fn rate_limits_answer_429_and_bound_the_challenges() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    let collector = FakeCollector::start();
    let endpoint = collector.url();
    let mut env = FAST_CRLS.to_vec();
    env.extend(TIGHT_LIMITS);
    env.extend([
        ("TRUSTIOT__TELEMETRY__OTLP_ENDPOINT", endpoint.as_str()),
        ("OTEL_METRIC_EXPORT_INTERVAL", "300"),
    ]);
    let service = Service::start(&product, &ca, &env).await;
    wait_for("the published CRLs", || async {
        let (status, _) = service.get(certs::http::CRL_PATH).await;
        (status == 200).then_some(())
    })
    .await;
    // Two registered, bound gateways; rl-two holds a challenge through the flood below.
    let factory = Factory::new(&service).await;
    let key = factory.register("rl-ok").await;
    let two_key = factory.register("rl-two").await;
    let tenant = TenantId::generate();
    store_status(&product, tenant, TenantStatus::Active, 1).await;
    let assets = assets_writer(&product);
    bind(&product, &assets, tenant, "rl-ok").await;
    bind(&product, &assets, tenant, "rl-two").await;
    wait_for_bindings_lag(&collector).await;
    let started = Instant::now();
    let mut turns = Turns::default();

    // Laysi/trustiot-v2#801: requests that name rl-ok without its key, more of them than its own
    // limit: challenges, redemptions signed by another key, and made-up nonces.
    let forger = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
    let mut forged = 0_u64;
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(turns.challenge(&service, "rl-ok").await);
        forged += 1;
    }
    for pending in &held[..2] {
        let answer = turns.send(&service, &renew_request(&forger, pending)).await;
        assert!(answer.is(401, &SIGNATURE_REFUSED), "{answer:?}");
        forged += 1;
    }
    for n in 0..2 {
        let answer = turns.send(&service, &made_up_renewal("rl-ok", n)).await;
        assert!(answer.is(409, &CHALLENGE_UNKNOWN), "{answer:?}");
        forged += 1;
    }
    assert!(forged > 2 * GATEWAY_BURST);
    // The gateway still renews, as often as its own limit lets it.
    let before = ledger(&product, "rl-ok").await;
    let burst = i64::try_from(GATEWAY_BURST).expect("a count");
    let mut first_verified = None;
    for _ in 0..GATEWAY_BURST {
        let pending = turns.challenge(&service, "rl-ok").await;
        first_verified.get_or_insert_with(Instant::now);
        let renewed = turns.send(&service, &renew_request(&key, &pending)).await;
        assert_eq!(renewed.status, 201, "{renewed:?}");
    }
    assert_eq!(ledger(&product, "rl-ok").await, before + burst);
    println!(
        "evidence: {forged} requests named rl-ok without its key (4 challenges, 2 redemptions \
         signed by another key: 401, 2 made-up nonces: 409); rl-ok then renewed \
         {GATEWAY_BURST} times: 201 each"
    );
    // Past its own limit: 429 with its own reason; the challenge is spent and nothing is issued.
    let pending = turns.challenge(&service, "rl-ok").await;
    let request = renew_request(&key, &pending);
    let answer = turns.send(&service, &request).await;
    assert_eq!(answer.status, 429, "{answer:?}");
    assert_eq!(answer.body["code"], "rate_limited");
    // The limit opens again an hour after the first redemption whose signature held; the
    // service took it no earlier than this test sent it.
    let since = first_verified.expect("a renewal").elapsed();
    let earliest = 3600_u64.saturating_sub(since.as_secs() + 1);
    let wait = answer.retry_after.expect("Retry-After");
    assert!(
        (earliest..=3600).contains(&wait),
        "Retry-After {wait}, {since:?} after the first renewal"
    );
    let again = turns.send(&service, &request).await;
    assert!(again.is(409, &CHALLENGE_UNKNOWN), "{again:?}");
    assert_eq!(ledger(&product, "rl-ok").await, before + burst);
    println!("evidence: rl-ok's own limit: 429, Retry-After {wait}; the challenge was spent: 409");
    // Only the holder of the key reaches that limit, so its refusal is an INFO line and counted.
    let lines = wait_for("the refusal lines of rl-ok", || async {
        let lines = refusal_lines(&service, "rl-ok");
        (lines.len() >= 6).then_some(lines)
    })
    .await;
    assert_eq!(
        lines,
        [
            "signature_invalid",
            "signature_invalid",
            "challenge_unknown",
            "challenge_unknown",
            "gateway_rate_limited",
            "challenge_unknown"
        ]
    );
    let limited_line = service
        .stdout
        .messages(REFUSED)
        .into_iter()
        .find(|line| line["fields"]["reason"] == "gateway_rate_limited")
        .expect("the refusal line");
    assert_eq!(limited_line["level"], "INFO", "{limited_line}");
    assert_eq!(limited_line["fields"]["step"], "renew", "{limited_line}");
    assert_eq!(
        refusal_reasons(&product, "rl-ok").await,
        BTreeMap::from([
            ("challenge_unknown".to_owned(), 3),
            ("gateway_rate_limited".to_owned(), 1),
            ("signature_invalid".to_owned(), 2),
        ])
    );
    // Laysi/trustiot-v2#847: the requests without the key are counted per day, apart from the
    // one only the key reached, which keeps the default hour.
    let rows: Vec<RefusalWindow> = sqlx::query_as(REFUSAL_WINDOWS)
        .bind("rl-ok")
        .fetch_all(&mut product.superuser().await)
        .await
        .expect("read the refusal windows");
    let verified: Vec<&RefusalWindow> = rows.iter().filter(|row| !row.0).collect();
    assert_eq!(verified.len(), 1, "{rows:?}");
    assert_eq!(
        (verified[0].1, verified[0].3.as_str()),
        (3600, r#"{"gateway_rate_limited": 1}"#),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .filter(|row| row.0)
            .all(|row| row.1 == 86_400 && !row.3.contains("gateway_rate_limited")),
        "{rows:?}"
    );
    println!("evidence: refusal windows of rl-ok {rows:?}");
    let metric = |reason: &str| ("certs.renewal.refused".to_owned(), reason.to_owned());
    wait_for("the gateway_rate_limited metric", || async {
        (counters(&collector).get(&metric("gateway_rate_limited")) == Some(&1)).then_some(())
    })
    .await;
    println!("evidence: refusal line {}", limited_line["fields"]);

    // Challenges together: 12 in all, then 429 whatever the hardware ID; only the requests the
    // limit let through stored a challenge.
    let pending = turns.challenge(&service, "rl-two").await;
    let before_flood = turns.challenges;
    let (mut admitted, mut refused) = (0_u64, 0_u64);
    for n in 0..50 {
        let answer = turns
            .send(&service, &challenge_request(&format!("rl-flood-{n}")))
            .await;
        match answer.status {
            200 => {
                assert_eq!(refused, 0, "a 200 after a 429: {answer:?}");
                admitted += 1;
            }
            429 => {
                assert_eq!(answer.body["code"], "rate_limited");
                let wait = answer.retry_after.expect("Retry-After");
                assert!((1..=60).contains(&wait), "Retry-After {wait}");
                refused += 1;
            }
            _ => panic!("a challenge request: {answer:?}"),
        }
    }
    let most = admitted_at_most(GLOBAL_BURST, GLOBAL_INTERVAL, started.elapsed()) - 1;
    assert!(
        (GLOBAL_BURST - before_flood..=most - before_flood).contains(&admitted),
        "{admitted} challenge requests of the flood let through, {refused} refused"
    );
    assert!(refused > 0, "the challenge step's limit was reached");
    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM certs.renewal_challenges WHERE hardware_id LIKE 'rl-flood-%'",
    )
    .fetch_one(&mut product.superuser().await)
    .await
    .expect("count");
    assert_eq!(
        u64::try_from(stored).expect("a count"),
        admitted,
        "only the requests the limits let through stored a challenge"
    );
    // A request past a global limit keeps its reason, `rate_limited`.
    wait_for("the rate_limited metric", || async {
        (counters(&collector).get(&metric("rate_limited")) == Some(&refused)).then_some(())
    })
    .await;
    println!(
        "evidence: global: {refused} challenge requests of new hardware IDs refused with 429; \
         {admitted} of the flood stored a challenge"
    );

    // The renew step has a global limit of its own: the flood of challenge requests did not take
    // the turn of the gateway that holds a challenge.
    let renewed = turns
        .send(&service, &renew_request(&two_key, &pending))
        .await;
    assert_eq!(renewed.status, 201, "{renewed:?}");
    println!("evidence: challenge limit used up by the flood; rl-two still renewed: 201");
    // Made-up redemptions use up the renew step's limit, and only it.
    let before_made_up = turns.renewals;
    let (mut admitted, mut limited) = (0_u64, 0_u64);
    for n in 0..20 {
        let answer = turns
            .send(&service, &made_up_renewal(&format!("rl-made-up-{n}"), n))
            .await;
        match answer.status {
            409 => {
                assert_eq!(limited, 0, "a 409 after a 429: {answer:?}");
                admitted += 1;
            }
            429 => limited += 1,
            _ => panic!("a made-up redemption: {answer:?}"),
        }
    }
    let most = admitted_at_most(GLOBAL_BURST, GLOBAL_INTERVAL, started.elapsed()) - 1;
    assert!(
        (GLOBAL_BURST - before_made_up..=most - before_made_up).contains(&admitted),
        "{admitted} made-up redemptions let through, {limited} refused"
    );
    assert!(limited > 0, "the renew step's limit was reached");
    let answer = send(&service, &challenge_request("rl-e")).await;
    assert_eq!(
        answer.status, 429,
        "the challenges are still limited: {answer:?}"
    );
    println!(
        "evidence: renew limit: {admitted} made-up redemptions answered 409, then {limited} 429"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn over_the_limits_a_stale_replica_logs_no_refusal_at_info() {
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let ca = TestCa::new(&[1]);
    // The consumer's lag stays unknown: the platform role cannot log in.
    let mut env = TIGHT_LIMITS.to_vec();
    env.push(("TRUSTIOT__DATABASE__PLATFORM_PASSWORD", "not-the-password"));
    let blind = Service::start(&product, &ca, &env).await;
    // Each hardware ID sends one request of each step; the global limits let 12 of each step
    // through to the lag check.
    let started = Instant::now();
    let (mut stale, mut limited) = (0_u64, 0_u64);
    for n in 0..30 {
        let hardware_id = format!("bl-{n}");
        for body in [
            challenge_request(&hardware_id),
            made_up_renewal(&hardware_id, n),
        ] {
            let answer = send(&blind, &body).await;
            if answer.is(503, &BINDINGS_STALE) {
                stale += 1;
            } else {
                assert_eq!(answer.status, 429, "{answer:?}");
                limited += 1;
            }
        }
    }
    let most = 2 * admitted_at_most(GLOBAL_BURST, GLOBAL_INTERVAL, started.elapsed());
    assert!(
        (2 * GLOBAL_BURST..=most).contains(&stale),
        "{stale} requests reached the lag check, {limited} were limited"
    );
    // One INFO line for each request the limits let through, none for the others.
    let lines = wait_for("the refusal lines of the stale replica", || async {
        let lines = blind.stdout.messages(REFUSED);
        (u64::try_from(lines.len()).expect("a count") >= stale).then_some(lines)
    })
    .await;
    assert_eq!(
        u64::try_from(lines.len()).expect("a count"),
        stale,
        "{lines:?}"
    );
    for line in &lines {
        assert_eq!(line["level"], "INFO", "{line}");
        assert_eq!(line["fields"]["reason"], "bindings_stale", "{line}");
    }
    println!(
        "evidence: unknown lag: 60 requests of 30 new hardware IDs: {stale} answered 503 with an \
         INFO line each, {limited} answered 429 without one"
    );
}
