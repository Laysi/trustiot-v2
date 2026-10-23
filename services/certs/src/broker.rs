//! The MQTT broker's server certificate (Laysi/trustiot-v2#557; README.md, "broker 伺服器憑證"):
//! issued on a port of its own (`broker.listen_addr`, the chart's `broker-certs`), which serves
//! nothing else and which the mesh opens to the broker's workload alone (the chart's
//! AuthorizationPolicy `<release>-certs-broker`, deploy/chart/README.md). The API port never
//! serves it, so neither the ingress gateway nor a service that may call the certs service
//! reaches it.
//!
//! `POST /internal/certs/v1/broker-certificate`, the body a PKCS #10 CSR, DER, with
//! `Content-Type: application/pkcs10`. The CA takes only the CSR's public key (ECDSA P-256, with
//! proof of possession) and decides everything else ([`issue::broker_certificate`]); the
//! private key is generated where the broker runs and never reaches this service
//! (Laysi/trustiot-v2#535). Answers, each with `cache-control: no-store`:
//!
//! | Request | Response |
//! | --- | --- |
//! | Signed and recorded | 201 [`BrokerCertificate`], JSON |
//! | `Content-Type` not `application/pkcs10` (parameters allowed) | 415 |
//! | A body over `issue::MAX_REQUEST_BYTES` | 413 |
//! | Not one DER CSR, not ECDSA P-256 with ecdsa-with-SHA256, or its signature does not verify | 422 |
//! | No issuing CA can sign now: none loaded, none valid with a CRL yet, or the newest expires within an hour | 503 |
//! | The database failed | 503 |
//! | Signing failed | 500 |
//!
//! Error bodies are one line of plain text naming the problem, like the other `/internal`
//! endpoints ([`crate::http`]); they never repeat the request. Each issue logs one INFO line with
//! the serial number, the CA key id, the names and the expiry; never the CSR or the certificate.

use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use contract::certs::BrokerCertificate;
use sqlx::PgPool;

use crate::authority::CaState;
use crate::issue::{self, BrokerNames, IssueError, IssuedCertificate, MAX_REQUEST_BYTES, Refusal};
use crate::pem;

/// The path of the issuance, on the broker port only.
pub const BROKER_CERTIFICATE_PATH: &str = "/internal/certs/v1/broker-certificate";

/// The media type of the request body: a PKCS #10 CSR, DER (RFC 5967).
pub const REQUEST_TYPE: &str = "application/pkcs10";

#[derive(Clone)]
struct Routes {
    pool: PgPool,
    ca: CaState,
    names: BrokerNames,
    validity: Duration,
}

/// The broker port's routes: certificates for `names`, valid for `validity`
/// (`broker.validity`), signed by the CA of `ca` and recorded in `pool`. Every other path on the
/// port answers 404.
pub fn routes(pool: PgPool, ca: CaState, names: BrokerNames, validity: Duration) -> Router {
    Router::new()
        .route(BROKER_CERTIFICATE_PATH, post(issue_certificate))
        .route_layer(axum::middleware::map_response(no_store))
        .with_state(Routes {
            pool,
            ca,
            names,
            validity,
        })
}

/// Marks every response of the issuance as not to be stored.
async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn refuse(status: StatusCode, reason: &'static str) -> Response {
    (status, reason).into_response()
}

async fn issue_certificate(
    State(state): State<Routes>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if !is_pkcs10(&headers) {
        tracing::info!("refused the MQTT broker's server certificate: not application/pkcs10");
        return refuse(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "the body must be a DER certificate signing request, Content-Type application/pkcs10",
        );
    }
    let request: Bytes = match axum::body::to_bytes(body, MAX_REQUEST_BYTES).await {
        Ok(request) => request,
        Err(_) => {
            tracing::info!(
                "refused the MQTT broker's server certificate: the request is too large"
            );
            return refuse(
                StatusCode::PAYLOAD_TOO_LARGE,
                "the certificate signing request is larger than 4096 bytes",
            );
        }
    };
    let issued = issue::sign_and_record(
        &state.pool,
        &state.ca,
        "the MQTT broker's server certificate",
        |ca, now| issue::broker_certificate(ca, &request, &state.names, state.validity, now),
    )
    .await;
    match issued {
        Ok(issued) => {
            let view = view(&issued);
            tracing::info!(
                serial = %view.serial,
                ca_key_id = %view.ca_key_id,
                names = %view.names.join(","),
                not_after = issued.not_after,
                "issued the MQTT broker's server certificate"
            );
            (StatusCode::CREATED, axum::Json(view)).into_response()
        }
        Err(Refusal::Request(problem)) => {
            tracing::info!(problem = %problem, "refused the MQTT broker's server certificate");
            refuse(StatusCode::UNPROCESSABLE_ENTITY, request_reason(&problem))
        }
        Err(Refusal::NotReady) => refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "no issuing CA can sign now: none is loaded, none has a CRL yet, or the newest \
             expires within an hour",
        ),
        Err(Refusal::Database) => refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "the ledger cannot be written now",
        ),
        Err(Refusal::Failed) => refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the certificate could not be signed",
        ),
    }
}

/// The answer's reason for a refused CSR, which names the problem and never the request.
fn request_reason(problem: &IssueError) -> &'static str {
    match problem {
        IssueError::UnsupportedSignature | IssueError::UnsupportedKey => {
            "the certificate signing request must be for an ECDSA P-256 key, signed with \
             ecdsa-with-SHA256"
        }
        IssueError::BadSignature => {
            "the certificate signing request's signature does not verify with its own key"
        }
        IssueError::TooLarge => "the certificate signing request is larger than 4096 bytes",
        _ => "the body is not one DER certificate signing request",
    }
}

/// Whether the request has exactly one `Content-Type`, `application/pkcs10` with or without
/// parameters, compared without regard to ASCII case.
fn is_pkcs10(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(CONTENT_TYPE).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    value.to_str().is_ok_and(|value| {
        let essence = value.split(';').next().unwrap_or_default();
        essence.trim().eq_ignore_ascii_case(REQUEST_TYPE)
    })
}

/// The answer for `issued`: its chain is the certificate, then the issuing CA certificate that
/// signed it.
fn view(issued: &IssuedCertificate) -> BrokerCertificate {
    BrokerCertificate {
        serial: issue::hex(&issued.serial),
        ca_key_id: issued.ca_key_id.clone(),
        names: issued.names.clone(),
        not_before: issue::utc(issued.not_before),
        not_after: issue::utc(issued.not_after),
        certificate_chain: format!(
            "{}{}",
            pem::encode(pem::CERTIFICATE, &issued.der),
            pem::encode(pem::CERTIFICATE, &issued.issuer_der)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(
                CONTENT_TYPE,
                HeaderValue::from_str(value).expect("a header"),
            );
        }
        headers
    }

    #[test]
    fn only_one_pkcs10_content_type_is_accepted() {
        for accepted in [
            "application/pkcs10",
            "Application/PKCS10",
            "application/pkcs10; charset=binary",
        ] {
            assert!(is_pkcs10(&headers(&[accepted])), "{accepted}");
        }
        for refused in [
            &[][..],
            &["application/json"],
            &["application/pkcs10x"],
            &["application/x-pem-file"],
            &["application/pkcs10", "application/pkcs10"],
        ] {
            assert!(!is_pkcs10(&headers(refused)), "{refused:?}");
        }
    }
}
