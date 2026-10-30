//! The platform administrators' gateway revocation API (Laysi/trustiot-v2#636; services/certs/
//! README.md, "閘道器憑證的撤銷"), against a ledger seeded as the factory registration and the
//! renewal leave it: a registered gateway with a valid certificate of each of two issuing CAs,
//! one revoked before and one expired, and a registered gateway whose only certificate expired.
//!
//! - Refusals change nothing: no token or a token that does not verify (401), a tenant
//!   administrator, a user without a tenant, a platform administrator's token another service
//!   exchanged (403), another content type, a body too large or not an object (400), an unknown
//!   field or reason (422), a hardware ID nobody registered or that is not a gateway ID (404), a
//!   gateway without a valid certificate (409). The ledger and the stored CRLs are compared
//!   before and after, and every response carries `cache-control: no-store`.
//! - A platform administrator revokes: the answer lists the two valid certificates with the time
//!   and the reason; the earlier revocation and the expired certificate stay as they were; each
//!   of the two CAs got one new CRL (cause `revocation`) before the answer, and the published
//!   `crl.pem` lists each certificate on its CA's CRL with the reason. One INFO line names the
//!   administrator, the hardware ID and the serial numbers, and no certificate.
//! - Again: 409, nothing changes (reentrant).
//!
//! The broker side (the kick, the reload, the renewal) is in `tests/dynsec.rs`. Starts a
//! throwaway Postgres with TLS (podman or docker); skipped without either unless `CI` is set.

mod support;

use std::collections::BTreeMap;

use certs::gateways::{
    BODY_INVALID, BODY_TOO_LARGE, MAX_BODY_BYTES, NO_VALID_CERTIFICATE, REASON_INVALID,
    UNKNOWN_FIELD,
};
use certs::http::CRL_PATH;
use certs::issue;
use certs::pem;
use contract::certs::{RevocationReason, RevokedGateway};
use contract::{MessageKey, ServiceName, TenantGrant, TenantId, TenantRole};
use serde_json::{Value, json};
use sqlx::Row as _;
use support::{Reply, Service, TestCa, wait_for};
use testkit::identity::TestIssuer;
use testkit::product::{ProductPostgres, product_postgres};
use x509_parser::prelude::FromDer as _;
use x509_parser::revocation_list::CertificateRevocationList;

/// The registered gateway whose certificates are revoked.
const GATEWAY: &str = "gw-636-api";
/// A registered gateway whose only certificate has expired.
const EXPIRED_ONLY: &str = "gw-636-expired";
/// A hardware ID nobody registered.
const UNREGISTERED: &str = "gw-636-none";
const WORKSTATION: &str = "line-636";

/// No CRL job round during the test after the first: refusals must leave the stored CRLs as they
/// are, and only the revocation may change them.
const QUIET_CRLS: [(&str, &str); 1] = [("TRUSTIOT__CRL__CHECK_INTERVAL", "1h")];

fn path(hardware_id: &str) -> String {
    format!("/api/certs/v1/gateways/{hardware_id}/revoke")
}

fn random_serial() -> Vec<u8> {
    let mut serial = [0_u8; 16];
    getrandom::fill(&mut serial).expect("random");
    // Positive, as the service's serial numbers are.
    serial[0] = (serial[0] & 0x7f) | 0x10;
    serial.to_vec()
}

/// A ledger row as the service writes it: `revoked` is a revocation days ago with `superseded`,
/// `expired` a certificate whose validity ended days ago. The certificate's bytes are not read
/// by anything this test calls.
#[derive(Clone)]
struct Row {
    serial: Vec<u8>,
    ca_key_id: String,
    profile: &'static str,
    subject: String,
    revoked: bool,
    expired: bool,
}

async fn insert(product: &ProductPostgres, row: &Row) {
    sqlx::query(
        "INSERT INTO certs.issued_certificates
             (serial, ca_key_id, profile, subject, not_before, not_after, certificate, revoked_at,
              revocation_reason)
         VALUES ($1, $2, $3, $4,
                 now() - interval '100 days',
                 CASE WHEN $5 THEN now() - interval '10 days' ELSE now() + interval '80 days' END,
                 '\\x3000'::bytea,
                 CASE WHEN $6 THEN now() - interval '2 days' END,
                 CASE WHEN $6 THEN 4::smallint END)",
    )
    .bind(&row.serial)
    .bind(&row.ca_key_id)
    .bind(row.profile)
    .bind(&row.subject)
    .bind(row.expired)
    .bind(row.revoked)
    .execute(&mut product.superuser().await)
    .await
    .expect("seed a ledger row");
}

/// Registers `hardware_id` as the factory registration does, with `initial` as its first
/// certificate, by `workstation`'s certificate.
async fn register(
    product: &ProductPostgres,
    hardware_id: &str,
    initial: &[u8],
    workstation: &[u8],
) {
    let mut key = [0_u8; 64];
    getrandom::fill(&mut key).expect("random");
    let mut endorsement = [0_u8; 32];
    getrandom::fill(&mut endorsement).expect("random");
    sqlx::query(
        "INSERT INTO certs.gateway_registrations
             (hardware_id, public_key, endorsement_key, endorsement_key_sha256, certificate_serial,
              workstation, workstation_certificate_serial, registered_by)
         VALUES ($1, $2, $3, $3, $4, $5, $6, gen_random_uuid())",
    )
    .bind(hardware_id)
    .bind(key.to_vec())
    .bind(endorsement.to_vec())
    .bind(initial)
    .bind(WORKSTATION)
    .bind(workstation)
    .execute(&mut product.superuser().await)
    .await
    .expect("seed a registration");
}

/// Every ledger row by serial (hex): its revocation time (Unix microseconds) and reason, and
/// every stored CRL's number by CA.
async fn state(
    product: &ProductPostgres,
) -> (
    BTreeMap<String, (Option<i64>, Option<i16>)>,
    BTreeMap<String, i64>,
) {
    let mut db = product.superuser().await;
    let ledger = sqlx::query(
        "SELECT encode(serial, 'hex') AS serial,
                (extract(epoch FROM revoked_at) * 1000000)::bigint AS revoked_at,
                revocation_reason
         FROM certs.issued_certificates",
    )
    .fetch_all(&mut db)
    .await
    .expect("read the ledger")
    .iter()
    .map(|row| {
        (
            row.get("serial"),
            (row.get("revoked_at"), row.get("revocation_reason")),
        )
    })
    .collect();
    let crls = sqlx::query("SELECT ca_key_id, crl_number FROM certs.crls")
        .fetch_all(&mut db)
        .await
        .expect("read the CRLs")
        .iter()
        .map(|row| (row.get("ca_key_id"), row.get("crl_number")))
        .collect();
    (ledger, crls)
}

/// A request the API must refuse without changing anything.
struct Refused {
    who: &'static str,
    path: String,
    token: Option<String>,
    content_type: Option<&'static str>,
    body: String,
    status: u16,
    code: &'static str,
    /// The message key; for a 422, its field error's.
    key: Option<MessageKey>,
}

/// The M1 code and message key of an error answer.
fn error_of(reply: &Reply) -> (String, String) {
    let body = reply.json();
    (
        body["code"].as_str().unwrap_or_default().to_owned(),
        body["message_key"].as_str().unwrap_or_default().to_owned(),
    )
}

/// The reason code (or `None` without one) of `serial` (hex) on the CRL of `issuer` in a PEM
/// bundle; `None` when that CRL does not list it.
fn listed(bundle: &str, issuer: &str, serial: &str) -> Option<Option<u8>> {
    for der in pem::decode_all(bundle, pem::CRL).expect("PEM CRLs") {
        let (_, crl) = CertificateRevocationList::from_der(&der).expect("a CRL");
        if !crl.issuer().to_string().contains(issuer) {
            continue;
        }
        for revoked in crl.iter_revoked_certificates() {
            if issue::hex(revoked.raw_serial()) == serial {
                return Some(revoked.reason_code().map(|(_, code)| code.0));
            }
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread")]
async fn a_platform_administrator_revokes_every_valid_certificate_of_a_gateway() {
    let Some(product) = product_postgres(&["certs_app", "certs_platform"]).await else {
        return;
    };
    let ca = TestCa::new(&[1, 2]);
    let authority = ca.authority();
    let (first, second) = (&authority.issuing()[0], &authority.issuing()[1]);
    // The CRL issuer names: `TrustIoT test issuing CA <generation> <tag>`.
    let (first_issuer, second_issuer) = (
        format!("TrustIoT test issuing CA {} ", first.generation()),
        format!("TrustIoT test issuing CA {} ", second.generation()),
    );
    let (first, second) = (first.key_id().to_owned(), second.key_id().to_owned());
    let gateway_row = |ca_key_id: &str, revoked: bool, expired: bool| Row {
        serial: random_serial(),
        ca_key_id: ca_key_id.to_owned(),
        profile: "gateway",
        subject: GATEWAY.to_owned(),
        revoked,
        expired,
    };
    let workstation = Row {
        serial: random_serial(),
        ca_key_id: first.clone(),
        profile: "workstation",
        subject: format!("workstation:{WORKSTATION}"),
        revoked: false,
        expired: false,
    };
    // The first certificate, revoked when the gateway was renewed; one valid of each CA; one
    // expired.
    let initial = gateway_row(&first, true, false);
    let valid_first = gateway_row(&first, false, false);
    let valid_second = gateway_row(&second, false, false);
    let expired = gateway_row(&first, false, true);
    let expired_only = Row {
        subject: EXPIRED_ONLY.to_owned(),
        ..gateway_row(&second, false, true)
    };
    for row in [
        &workstation,
        &initial,
        &valid_first,
        &valid_second,
        &expired,
        &expired_only,
    ] {
        insert(&product, row).await;
    }
    register(&product, GATEWAY, &initial.serial, &workstation.serial).await;
    register(
        &product,
        EXPIRED_ONLY,
        &expired_only.serial,
        &workstation.serial,
    )
    .await;

    let service = Service::start(&product, &ca, &QUIET_CRLS).await;
    // The first CRL of each CA, from the job's first round.
    wait_for("the first CRL of both CAs", || async {
        (state(&product).await.1.len() == 2).then_some(())
    })
    .await;
    let before = state(&product).await;

    // Requests that must change nothing.
    let stranger = TestIssuer::new();
    let mut admin_claims = TestIssuer::claims(ServiceName::Certs, None);
    admin_claims.platform_admin = true;
    let tenant_admin = TenantGrant {
        id: TenantId::generate(),
        role: TenantRole::Admin,
    };
    let admin = service.admin_token();
    let reason = json!({"reason": "key_compromise"});
    let json_type = Some("application/json");
    let refuse = |who: &'static str,
                  target: String,
                  token: Option<String>,
                  content_type: Option<&'static str>,
                  body: String,
                  status: u16,
                  code: &'static str,
                  key: Option<MessageKey>| Refused {
        who,
        path: target,
        token,
        content_type,
        body,
        status,
        code,
        key,
    };
    let cases = vec![
        refuse(
            "no internal identity",
            path(GATEWAY),
            None,
            json_type,
            reason.to_string(),
            401,
            "unauthenticated",
            None,
        ),
        refuse(
            "a token of another key",
            path(GATEWAY),
            Some(stranger.token(&admin_claims)),
            json_type,
            reason.to_string(),
            401,
            "unauthenticated",
            None,
        ),
        refuse(
            "a tenant administrator",
            path(GATEWAY),
            Some(service.token(false, Some(tenant_admin), None)),
            json_type,
            reason.to_string(),
            403,
            "permission_denied",
            None,
        ),
        refuse(
            "a user without a tenant",
            path(GATEWAY),
            Some(service.token(false, None, None)),
            json_type,
            reason.to_string(),
            403,
            "permission_denied",
            None,
        ),
        refuse(
            "a platform administrator's token the assets service exchanged (act)",
            path(GATEWAY),
            Some(service.token(true, None, Some(ServiceName::Assets))),
            json_type,
            reason.to_string(),
            403,
            "permission_denied",
            None,
        ),
        refuse(
            "text/plain",
            path(GATEWAY),
            Some(admin.clone()),
            Some("text/plain"),
            reason.to_string(),
            400,
            "invalid_request",
            None,
        ),
        refuse(
            "no content type",
            path(GATEWAY),
            Some(admin.clone()),
            None,
            reason.to_string(),
            400,
            "invalid_request",
            None,
        ),
        refuse(
            "a body over the limit",
            path(GATEWAY),
            Some(admin.clone()),
            json_type,
            json!({"reason": "x".repeat(MAX_BODY_BYTES)}).to_string(),
            400,
            "invalid_request",
            Some(BODY_TOO_LARGE),
        ),
        refuse(
            "a body that is not an object",
            path(GATEWAY),
            Some(admin.clone()),
            json_type,
            "[]".to_owned(),
            400,
            "invalid_request",
            Some(BODY_INVALID),
        ),
        refuse(
            "an unknown field",
            path(GATEWAY),
            Some(admin.clone()),
            json_type,
            json!({"serial": "4f"}).to_string(),
            422,
            "validation_failed",
            Some(UNKNOWN_FIELD),
        ),
        refuse(
            "certificate_hold",
            path(GATEWAY),
            Some(admin.clone()),
            json_type,
            json!({"reason": "certificate_hold"}).to_string(),
            422,
            "validation_failed",
            Some(REASON_INVALID),
        ),
        refuse(
            "a hardware ID nobody registered",
            path(UNREGISTERED),
            Some(admin.clone()),
            json_type,
            reason.to_string(),
            404,
            "not_found",
            None,
        ),
        refuse(
            "a path that is not a gateway ID",
            path("service:certs"),
            Some(admin.clone()),
            json_type,
            reason.to_string(),
            404,
            "not_found",
            None,
        ),
        refuse(
            "a gateway whose only certificate expired",
            path(EXPIRED_ONLY),
            Some(admin.clone()),
            json_type,
            "{}".to_owned(),
            409,
            "conflict",
            Some(NO_VALID_CERTIFICATE),
        ),
    ];
    for case in &cases {
        let who = case.who;
        let reply = service
            .post(
                &case.path,
                case.token.as_deref(),
                case.content_type,
                case.body.clone(),
            )
            .await;
        println!("evidence: {who}: {} {}", reply.status, reply.body);
        assert_eq!(reply.status, case.status, "{who}: {}", reply.body);
        let (code, key) = error_of(&reply);
        assert_eq!(code, case.code, "{who}");
        if let Some(expected) = &case.key {
            // A 422 names the problem in its field error.
            let key = if case.status == 422 {
                reply.json()["field_errors"][0]["message_key"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            } else {
                key
            };
            assert_eq!(key, expected.as_str(), "{who}");
        }
        assert_eq!(reply.cache_control.as_deref(), Some("no-store"), "{who}");
    }
    assert_eq!(
        state(&product).await,
        before,
        "a refused request changed the ledger or a CRL"
    );
    assert!(
        service
            .stdout
            .messages("revoked a gateway's certificates")
            .is_empty()
    );
    println!(
        "evidence: {} refused requests: ledger ({} rows) and CRL numbers {:?} unchanged",
        cases.len(),
        before.0.len(),
        before.1
    );

    // The platform administrator revokes.
    let reply = service
        .post_json(&path(GATEWAY), Some(&admin), &reason)
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.cache_control.as_deref(), Some("no-store"));
    let revoked: RevokedGateway = serde_json::from_str(&reply.body).expect("RevokedGateway");
    println!("evidence: revoke {GATEWAY}: 200 {}", reply.body);
    let mut expected = vec![
        (issue::hex(&valid_first.serial), first.clone()),
        (issue::hex(&valid_second.serial), second.clone()),
    ];
    expected.sort();
    assert_eq!(revoked.hardware_id, GATEWAY);
    assert_eq!(
        revoked
            .revoked
            .iter()
            .map(|certificate| (certificate.serial.clone(), certificate.ca_key_id.clone()))
            .collect::<Vec<_>>(),
        expected
    );
    assert!(revoked.revoked.iter().all(|certificate| {
        certificate.revocation_reason == Some(RevocationReason::KeyCompromise)
    }));
    let after = state(&product).await;
    for (row, revoked_now) in [
        (&initial, false),
        (&valid_first, true),
        (&valid_second, true),
        (&expired, false),
        (&expired_only, false),
        (&workstation, false),
    ] {
        let serial = issue::hex(&row.serial);
        if revoked_now {
            assert_eq!(
                after.0[&serial].1,
                Some(1),
                "{serial} revoked, key_compromise"
            );
            assert!(after.0[&serial].0.is_some());
        } else {
            assert_eq!(after.0[&serial], before.0[&serial], "{serial} unchanged");
        }
    }
    // One new CRL per CA, before the answer: the published bundle lists both.
    for ca_key_id in [&first, &second] {
        assert_eq!(
            after.1[ca_key_id.as_str()],
            before.1[ca_key_id.as_str()] + 1,
            "one new CRL of {ca_key_id}"
        );
    }
    let new_crls: Vec<Value> = service
        .stdout
        .messages("issued a new CRL")
        .into_iter()
        .filter(|line| line["fields"]["cause"] == "revocation")
        .collect();
    assert_eq!(new_crls.len(), 2, "{new_crls:?}");
    let (status, bundle) = service.get(CRL_PATH).await;
    assert_eq!(status, 200);
    assert_eq!(
        listed(&bundle, &first_issuer, &issue::hex(&valid_first.serial)),
        Some(Some(1))
    );
    assert_eq!(
        listed(&bundle, &second_issuer, &issue::hex(&valid_second.serial)),
        Some(Some(1))
    );
    assert_eq!(
        listed(&bundle, &first_issuer, &issue::hex(&initial.serial)),
        Some(Some(4)),
        "the earlier revocation keeps its reason"
    );
    assert_eq!(
        listed(&bundle, &first_issuer, &issue::hex(&expired.serial)),
        None
    );
    println!(
        "evidence: ledger: both valid certificates revoked (reason 1), the earlier revocation and \
         the expired certificate unchanged; CRL numbers {:?} -> {:?}; crl.pem lists {} on the \
         CRL of CA {} and {} on the CRL of CA {}, reason keyCompromise",
        before.1,
        after.1,
        issue::hex(&valid_first.serial),
        first,
        issue::hex(&valid_second.serial),
        second
    );

    // The audit line: the administrator, the hardware ID, the serial numbers; no certificate.
    let lines = service.stdout.messages("revoked a gateway's certificates");
    assert_eq!(lines.len(), 1, "{lines:?}");
    let fields = &lines[0]["fields"];
    assert_eq!(lines[0]["level"], "INFO");
    assert_eq!(fields["requested_by"], "platform_admin");
    assert!(fields["user_id"].is_string(), "{fields}");
    assert_eq!(fields["hardware_id"], GATEWAY);
    assert_eq!(
        fields["serials"],
        expected
            .iter()
            .map(|(serial, _)| serial.as_str())
            .collect::<Vec<_>>()
            .join(",")
    );
    assert_eq!(fields["reason"], "key_compromise");
    assert!(!service.output().contains("BEGIN CERTIFICATE"));
    println!("evidence: audit line: {}", lines[0]);

    // Again: nothing left to revoke.
    let again = service
        .post_json(&path(GATEWAY), Some(&admin), &reason)
        .await;
    assert_eq!(again.status, 409, "{}", again.body);
    assert_eq!(
        error_of(&again),
        (
            "conflict".to_owned(),
            NO_VALID_CERTIFICATE.as_str().to_owned()
        )
    );
    assert_eq!(
        state(&product).await,
        after,
        "the second request changed something"
    );
    println!("evidence: revoke {GATEWAY} again: 409 {}", again.body);
}
