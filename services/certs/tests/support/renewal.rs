//! Helpers of the renewal tests (`tests/renewal.rs`, `tests/renewal_faults.rs`,
//! `tests/renewal_load.rs`): gateways registered through the factory registration API with
//! software P-256 keys standing in for their TPM keys, bindings written as the assets service
//! writes them, and plain calls to `POST /certs/renew`.

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use certs::pem;
use certs::registrations::REGISTRATIONS_PATH;
use certs::workstations::CERTIFICATES_PATH;
use contract::certs::{
    WORKSTATION_CERTIFICATE_HEADER, WORKSTATION_NONCE_HEADER, WORKSTATION_SIGNATURE_HEADER,
    WORKSTATION_TIMESTAMP_HEADER, WorkstationCertificate, workstation_signing_input,
};
use contract::events::assets::{GATEWAY_BINDING_AGGREGATE, GatewayBound};
use contract::{GatewayBindingId, MessageKey, ServiceName, TenantId};
use outbox::Aggregate;
use protocol::GatewayId;
use protocol::renewal::{Challenge, RENEWAL_PATH};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256,
    PKCS_ECDSA_P384_SHA384, PublicKeyData as _, SigningKey as _,
};
use serde_json::{Value, json};
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{BackgroundTenantDb, TenantDb};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use testkit::product::{DATABASE, ProductPostgres};

use super::{FakeCollector, Service, now, wait_for};

/// The roles these tests log in with: the certs service's two, and the assets service's, which
/// writes the bindings.
pub const LOGINS: &[&str] = &[
    "assets_app",
    "assets_platform",
    "certs_app",
    "certs_platform",
];

/// The CRL schedule of `tests/registration.rs`: the first CRL is out within seconds, so an
/// issuing CA can sign.
pub const FAST_CRLS: [(&str, &str); 4] = [
    ("TRUSTIOT__CRL__CHECK_INTERVAL", "1s"),
    ("TRUSTIOT__CRL__VALIDITY", "1h"),
    ("TRUSTIOT__CRL__RENEW_BEFORE", "3590s"),
    ("TRUSTIOT__CRL__ALERT_BEFORE", "3500s"),
];

/// The refusal log line (`certs::renewal`).
pub const REFUSED: &str = "refused a certificate renewal request";

/// A gateway's certificates in the ledger. `$1` hardware ID.
pub const LEDGER: &str = "SELECT count(*) FROM certs.issued_certificates
    WHERE profile = 'gateway' AND subject = $1";

/// Renewal events of an aggregate. `$1` the aggregate ID.
pub const RENEWED_EVENTS: &str = "SELECT envelope::text, tenant_id::text, xmin::text
FROM certs.outbox_event
WHERE event_type = 'certs.gateway_certificate_renewed' AND aggregate_type = 'gateway_certificate'
    AND aggregate_id = $1";

/// Refusal events of a hardware ID.
pub const REFUSED_EVENTS: &str = "SELECT envelope::text FROM certs.outbox_event
WHERE event_type = 'certs.gateway_renewal_refused' AND aggregate_type = 'gateway_registration'
    AND aggregate_id = $1 AND tenant_id IS NULL";

/// A new P-256 key and a CSR for it.
pub fn csr() -> (KeyPair, Vec<u8>) {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "ignored");
    params.distinguished_name = name;
    let request = params
        .serialize_request(&key)
        .expect("a CSR")
        .der()
        .to_vec();
    (key, request)
}

/// A production workstation with its certificate from the platform administrators' API, which
/// registers gateways.
pub struct Factory<'a> {
    service: &'a Service,
    key: KeyPair,
    certificate: Vec<u8>,
}

impl<'a> Factory<'a> {
    pub async fn new(service: &'a Service) -> Self {
        let (key, request) = csr();
        let issued = service
            .post_json(
                CERTIFICATES_PATH,
                Some(&service.admin_token()),
                &json!({
                    "workstation": "renewal-ws",
                    "certificate_request": pem::encode(pem::CERTIFICATE_REQUEST, &request),
                }),
            )
            .await;
        assert_eq!(issued.status, 201, "{}", issued.body);
        let issued: WorkstationCertificate = serde_json::from_str(&issued.body).expect("issued");
        Self {
            service,
            key,
            certificate: pem::decode_all(&issued.certificate, pem::CERTIFICATE)
                .expect("PEM")
                .remove(0),
        }
    }

    /// Registers `hardware_id` with a new TPM key, which it returns.
    pub async fn register(&self, hardware_id: &str) -> KeyPair {
        let (key, request) = csr();
        let endorsement = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).expect("an EK");
        let body = json!({
            "hardware_id": hardware_id,
            "certificate_request": pem::encode(pem::CERTIFICATE_REQUEST, &request),
            "endorsement_key": pem::encode(pem::PUBLIC_KEY, &endorsement.subject_public_key_info()),
        })
        .to_string()
        .into_bytes();
        let timestamp = now();
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).expect("random");
        let nonce = URL_SAFE_NO_PAD.encode(nonce);
        let digest: [u8; 32] = Sha256::digest(&body).into();
        let input =
            workstation_signing_input("POST", REGISTRATIONS_PATH, timestamp, &nonce, &digest);
        let signature = self.key.sign(&input).expect("sign the request");
        let response = reqwest::Client::new()
            .post(format!("{}{REGISTRATIONS_PATH}", self.service.base_url))
            .timeout(Duration::from_secs(30))
            .header("content-type", "application/json")
            .header(
                contract::INTERNAL_IDENTITY_HEADER,
                self.service.admin_token(),
            )
            .header(
                WORKSTATION_CERTIFICATE_HEADER,
                STANDARD.encode(&self.certificate),
            )
            .header(WORKSTATION_TIMESTAMP_HEADER, timestamp.to_string())
            .header(WORKSTATION_NONCE_HEADER, nonce)
            .header(WORKSTATION_SIGNATURE_HEADER, STANDARD.encode(signature))
            .body(body)
            .send()
            .await
            .expect("register a gateway");
        let status = response.status().as_u16();
        let text = response.text().await.expect("read the body");
        assert_eq!(status, 201, "register {hardware_id}: {text}");
        key
    }
}

/// The assets service's role as its binding uses it: a tenant transaction per binding.
pub fn assets_writer(product: &ProductPostgres) -> BackgroundTenantDb {
    TenantDb::connect_lazy_with_background(
        product.postgres.options("assets_app", DATABASE),
        PgPoolOptions::new().max_connections(2),
        ServiceIdentity::new(ServiceName::Assets),
    )
    .expect("the assets service role's pool")
    .1
}

/// The certs service's role, for the tenant statuses.
pub fn certs_work(product: &ProductPostgres) -> BackgroundTenantDb {
    TenantDb::connect_lazy_with_background(
        product.postgres.options("certs_app", DATABASE),
        PgPoolOptions::new().max_connections(2),
        ServiceIdentity::new(ServiceName::Certs),
    )
    .expect("the certs service role's pool")
    .1
}

/// Binds `hardware_id` to `tenant` as the assets service does (`assets.gateway_bound` in the
/// tenant's transaction), and waits until the certs service stored the binding.
pub async fn bind(
    product: &ProductPostgres,
    assets: &BackgroundTenantDb,
    tenant: TenantId,
    hardware_id: &str,
) -> GatewayBindingId {
    let binding_id = GatewayBindingId::generate();
    let mut tx = assets
        .begin(tenant, "test-bind")
        .await
        .expect("a tenant transaction of the assets service");
    let aggregate =
        Aggregate::new(GATEWAY_BINDING_AGGREGATE, binding_id.to_string()).expect("an aggregate");
    let event = GatewayBound {
        binding_id,
        hardware_id: GatewayId::new(hardware_id).expect("a hardware ID"),
    };
    outbox::publish(tx.transaction(), &aggregate, &event, Some(tenant))
        .await
        .expect("write the event");
    tx.commit().await.expect("commit the binding");
    wait_for(
        &format!("the binding of {hardware_id} in certs"),
        || async {
            let stored: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM certs.gateway_bindings WHERE binding_id = $1::uuid",
            )
            .bind(binding_id.to_string())
            .fetch_one(&mut product.superuser().await)
            .await
            .expect("read the bindings");
            (stored == 1).then_some(())
        },
    )
    .await;
    binding_id
}

/// What the renewal endpoint answered.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub body: Value,
    pub retry_after: Option<u64>,
    pub cache_control: Option<String>,
}

impl Answer {
    pub fn message_key(&self) -> &str {
        self.body["message_key"].as_str().unwrap_or_default()
    }

    pub fn is(&self, status: u16, key: &MessageKey) -> bool {
        self.status == status && self.message_key() == key.as_str()
    }
}

/// Posts `body` (JSON) to the renewal endpoint of `service`, without any credential.
pub async fn send(service: &Service, body: &Value) -> Answer {
    let response = reqwest::Client::new()
        .post(format!("{}{RENEWAL_PATH}", service.base_url))
        .timeout(Duration::from_secs(30))
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .expect("call the renewal endpoint");
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let retry_after = header("retry-after").map(|value| {
        value
            .parse()
            .unwrap_or_else(|_| panic!("Retry-After is whole seconds: {value:?}"))
    });
    let cache_control = header("cache-control");
    let status = response.status().as_u16();
    let text = response.text().await.expect("read the body");
    Answer {
        status,
        body: serde_json::from_str(&text).unwrap_or_else(|_| panic!("a JSON body: {text}")),
        retry_after,
        cache_control,
    }
}

pub fn challenge_request(hardware_id: &str) -> Value {
    json!({"step": "challenge", "hardware_id": hardware_id})
}

/// A challenge for `hardware_id`.
pub async fn challenge(service: &Service, hardware_id: &str) -> Challenge {
    let answer = send(service, &challenge_request(hardware_id)).await;
    assert_eq!(
        answer.status, 200,
        "a challenge for {hardware_id}: {answer:?}"
    );
    serde_json::from_value(answer.body).expect("a challenge")
}

/// The renew request for `challenge`, signed by `key` over its signing input.
pub fn renew_request(key: &KeyPair, challenge: &Challenge) -> Value {
    let signature = key.sign(&challenge.signing_input()).expect("sign");
    json!({
        "step": "renew",
        "hardware_id": challenge.hardware_id,
        "nonce": challenge.nonce,
        "signature": STANDARD.encode(signature),
    })
}

pub async fn ledger(product: &ProductPostgres, hardware_id: &str) -> i64 {
    sqlx::query_scalar(LEDGER)
        .bind(hardware_id)
        .fetch_one(&mut product.superuser().await)
        .await
        .expect("count the ledger")
}

/// The service's refusal log lines for `hardware_id`, as their reasons.
pub fn refusal_lines(service: &Service, hardware_id: &str) -> Vec<String> {
    service
        .stdout
        .messages(REFUSED)
        .into_iter()
        .filter(|line| line["fields"]["hardware_id"] == hardware_id)
        .map(|line| {
            line["fields"]["reason"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

/// The latest value of every counter point the collector received, by metric and `reason`
/// (cumulative sums: the latest is the total).
pub fn counters(collector: &FakeCollector) -> BTreeMap<(String, String), u64> {
    use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
    use opentelemetry_proto::tonic::common::v1::any_value;
    use opentelemetry_proto::tonic::metrics::v1::{metric, number_data_point};
    use prost::Message as _;

    let mut totals = BTreeMap::new();
    for body in collector.bodies("/v1/metrics") {
        let request = ExportMetricsServiceRequest::decode(body.as_slice()).expect("OTLP metrics");
        for resource in request.resource_metrics {
            for scope in resource.scope_metrics {
                for metric in scope.metrics {
                    let Some(metric::Data::Sum(sum)) = metric.data else {
                        continue;
                    };
                    for point in sum.data_points {
                        let reason = point
                            .attributes
                            .iter()
                            .find(|attribute| attribute.key == "reason")
                            .and_then(|attribute| attribute.value.as_ref())
                            .and_then(|value| match &value.value {
                                Some(any_value::Value::StringValue(text)) => Some(text.clone()),
                                _ => None,
                            })
                            .unwrap_or_default();
                        let Some(number_data_point::Value::AsInt(value)) = point.value else {
                            continue;
                        };
                        let value = u64::try_from(value).expect("a counter");
                        let total = totals.entry((metric.name.clone(), reason)).or_insert(0);
                        *total = value.max(*total);
                    }
                }
            }
        }
    }
    totals
}

/// Whether the collector received the lag of the service's consumer of `subscription`
/// (`outbox.consumer.lag`), which is reported only while the lag is known.
pub fn lag_reported(collector: &FakeCollector, subscription: &str) -> bool {
    use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
    use opentelemetry_proto::tonic::common::v1::any_value;
    use opentelemetry_proto::tonic::metrics::v1::metric;
    use prost::Message as _;

    collector.bodies("/v1/metrics").iter().any(|body| {
        let request = ExportMetricsServiceRequest::decode(body.as_slice()).expect("OTLP metrics");
        request
            .resource_metrics
            .iter()
            .flat_map(|resource| &resource.scope_metrics)
            .flat_map(|scope| &scope.metrics)
            .filter(|metric| metric.name == "outbox.consumer.lag")
            .filter_map(|metric| match &metric.data {
                Some(metric::Data::Gauge(gauge)) => Some(&gauge.data_points),
                _ => None,
            })
            .flatten()
            .any(|point| {
                point.attributes.iter().any(|attribute| {
                    attribute.key == "outbox.subscription"
                        && attribute
                            .value
                            .as_ref()
                            .and_then(|value| value.value.as_ref())
                            == Some(&any_value::Value::StringValue(subscription.to_owned()))
                })
            })
    })
}

/// Waits until the service knows both its consumers' lags (the bindings' and the tenant states'),
/// so the renewal decides instead of answering 503 (`bindings_stale`). It watches the exported lag rather than polling the endpoint: the rate
/// limits come first and count every request, 503s too.
pub async fn wait_for_bindings_lag(collector: &FakeCollector) {
    wait_for(
        "the lags of the consumers of the assets service's and the identity service's events",
        || async {
            (lag_reported(collector, certs::bindings::SUBSCRIPTION)
                && lag_reported(collector, certs::tenant_states::SUBSCRIPTION))
            .then_some(())
        },
    )
    .await;
}

/// A renew request for a challenge that was never handed out, with a well-formed signature.
pub fn made_up_renewal(hardware_id: &str, n: u8) -> Value {
    json!({"step": "renew", "hardware_id": hardware_id,
        "nonce": URL_SAFE_NO_PAD.encode([n; 32]), "signature": "MEUCIQ=="})
}
