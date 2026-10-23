//! The MQTT bridge (services/mqtt-bridge, decisions 6 and 27; Laysi/trustiot-v2#85). Gateways
//! connect to `wss://<hosts.gateway>/mqtt`; the ingress sends that path, without the cookie check,
//! to this service, which turns each WebSocket back into a TCP connection to the MQTT broker's
//! gateway port:
//!
//! - the WebSocket's binary messages are the gateway's inner mTLS to the broker; the bridge relays
//!   them byte for byte and never terminates, parses or logs that TLS ([`relay`]);
//! - the gateway's source IP comes from the one header the deployment trusts, never from
//!   `X-Forwarded-For` or `True-Client-IP` ([`client_ip`]); it is logged with the connection;
//! - a WebSocket ping every 50 s keeps idle connections open through Cloudflare; on SIGTERM every
//!   gateway gets a WebSocket close, so it reconnects to another pod at once;
//! - small buffers (8 KiB), a cap on connections and on connections per source IP
//!   ([`source_limit`]), deadlines that free the slot of a connection whose inner TLS makes no
//!   progress or whose gateway stopped answering pings ([`relay`]), and connection and traffic
//!   metrics ([`metrics`]);
//! - the WebSocket library's logs never go above DEBUG ([`LOG_CEILINGS`]): at TRACE it writes
//!   every frame's payload.
//!
//! See `README.md` in this crate.

pub mod client_ip;
pub mod config;
pub mod handshake;
pub mod memory;
pub mod metrics;
pub mod relay;
pub mod source_limit;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{CONNECTION, SEC_WEBSOCKET_PROTOCOL, SEC_WEBSOCKET_VERSION, UPGRADE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use hyper_util::rt::TokioIo;
use service_kit::shutdown::Shutdown;
use service_kit::telemetry::LogCeiling;
use service_kit::{BoxError, Context};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_util::task::TaskTracker;
use tracing::Level;

use crate::client_ip::ClientIpSource;
use crate::config::Config;
use crate::handshake::SUBPROTOCOL;
use crate::metrics::Metrics;
use crate::relay::{Settings, Totals};
use crate::source_limit::{SourceLimit, SourceSlot};

/// The levels the WebSocket library's logs never exceed, whatever `telemetry.log_filter` says
/// (`main` passes them to service-kit): at TRACE tungstenite writes every frame it reads or
/// writes, payload included, and the payload is the gateway's inner TLS (security review of
/// Laysi/trustiot-v2#85, Low; Laysi/trustiot-v2#600). Its DEBUG lines carry close frames and
/// errors only.
pub const LOG_CEILINGS: &[LogCeiling] = &[
    LogCeiling::new("tungstenite", Level::DEBUG),
    LogCeiling::new("tokio_tungstenite", Level::DEBUG),
];

/// The service's startup for `service_kit::run`.
///
/// # Errors
///
/// When `broker.connect_timeout` is not shorter than the kit's `http.request_timeout` (the
/// upgrade request waits for the broker's connection, so the kit would answer 504 first), and when
/// `max_connections` connections may not fit in the container's memory limit ([`memory`]).
pub async fn start(context: Context<Config>) -> Result<Router, BoxError> {
    let config = context.config();
    if config.broker.connect_timeout >= context.request_timeout() {
        return Err("broker.connect_timeout must be shorter than http.request_timeout".into());
    }
    memory::check(config.max_connections)?;
    let (broker_host, broker_port) = config.broker_address();
    let connections = TaskTracker::new();
    let bridge = Bridge(Arc::new(Inner {
        source: config.client_ip_source(),
        broker_host,
        broker_port,
        connect_timeout: config.broker.connect_timeout,
        settings: Settings {
            ping_interval: config.websocket.ping_interval,
            pong_timeout: config.websocket.pong_timeout,
            handshake_timeout: config.broker.handshake_timeout,
            close_timeout: config.websocket.close_timeout,
        },
        permits: Arc::new(Semaphore::new(config.max_connections)),
        sources: SourceLimit::new(config.connections_per_ip()),
        connections: connections.clone(),
        shutdown: context.shutdown().clone(),
        metrics: Metrics::new(),
    }));
    // service-kit does not wait for upgraded connections; this task does, so the drain lasts until
    // every gateway got its close (each relay ends within close_timeout of SIGTERM).
    let shutdown = context.shutdown().clone();
    context.spawn(async move {
        shutdown.stopping().await;
        connections.close();
        connections.wait().await;
    });
    tracing::info!(
        client_ip_source = bridge.0.source.header(),
        broker.address = %config.broker.address,
        broker.handshake_timeout_ms = millis(config.broker.handshake_timeout),
        websocket.ping_interval_ms = millis(config.websocket.ping_interval),
        websocket.pong_timeout_ms = millis(config.websocket.pong_timeout),
        websocket.close_timeout_ms = millis(config.websocket.close_timeout),
        max_connections = config.max_connections,
        max_connections_per_ip = config.connections_per_ip(),
        "relaying gateway WebSockets to the MQTT broker"
    );
    Ok(routes(bridge))
}

/// The routes: every path is the WebSocket endpoint. The ingress decides which path reaches the
/// bridge (`ingress.publicPaths.mqtt`, an exact path); service-kit serves `/healthz` and `/readyz`
/// in front.
fn routes(bridge: Bridge) -> Router {
    Router::new().fallback(upgrade).with_state(bridge)
}

#[derive(Clone)]
struct Bridge(Arc<Inner>);

struct Inner {
    source: ClientIpSource,
    broker_host: String,
    broker_port: u16,
    connect_timeout: Duration,
    settings: Settings,
    permits: Arc<Semaphore>,
    sources: SourceLimit,
    connections: TaskTracker,
    shutdown: Shutdown,
    metrics: Metrics,
}

async fn upgrade(State(bridge): State<Bridge>, mut request: Request) -> Response {
    let inner = &*bridge.0;
    // Held from before the shutdown check until the relay ends, so the drain (which waits on the
    // tracker after `stopping`, and `stopping` follows `requested`) either waits for this
    // connection or has already begun after a shutdown this check sees.
    let tracked = inner.connections.token();
    if inner.shutdown.is_requested() {
        // Readiness already fails; the ingress retries a 503 on a pod that is still ready.
        return refuse(inner, StatusCode::SERVICE_UNAVAILABLE, "draining", None);
    }
    let accept = match handshake::accept(request.method(), request.version(), request.headers()) {
        Ok(accept) => accept,
        Err(error) => {
            let mut response = refuse(inner, error.status(), error.reason(), None);
            if error == handshake::HandshakeError::WebSocketVersion {
                response
                    .headers_mut()
                    .insert(SEC_WEBSOCKET_VERSION, HeaderValue::from_static("13"));
            }
            return response;
        }
    };
    let client = match inner.source.client_ip(request.headers()) {
        Ok(client) => client,
        Err(error) => return refuse(inner, StatusCode::BAD_REQUEST, error.reason(), None),
    };
    // Before the total: a source over its own cap is told so (429, and the ingress does not
    // retry it on another pod), even while the bridge is full as well.
    let Some(source) = inner.sources.try_acquire(client) else {
        return refuse(
            inner,
            StatusCode::TOO_MANY_REQUESTS,
            "over_source_limit",
            Some(client),
        );
    };
    let Ok(permit) = Arc::clone(&inner.permits).try_acquire_owned() else {
        return refuse(
            inner,
            StatusCode::SERVICE_UNAVAILABLE,
            "over_capacity",
            Some(client),
        );
    };
    let broker = match connect(inner).await {
        Ok(broker) => broker,
        Err(error) => {
            tracing::warn!(
                client.address = %client,
                error = %error,
                "cannot connect to the MQTT broker"
            );
            return refuse(
                inner,
                StatusCode::BAD_GATEWAY,
                "broker_unavailable",
                Some(client),
            );
        }
    };
    if inner.shutdown.is_requested() {
        // SIGTERM arrived while the broker connection opened: refuse rather than upgrade a
        // connection only to close it at once. The ingress retries on a ready pod.
        return refuse(
            inner,
            StatusCode::SERVICE_UNAVAILABLE,
            "draining",
            Some(client),
        );
    }
    let on_upgrade = hyper::upgrade::on(&mut request);
    let id = connection_id();
    let task = run(
        bridge.clone(),
        on_upgrade,
        broker,
        client,
        id,
        (permit, source),
    );
    tokio::spawn(async move {
        task.await;
        drop(tracked);
    });

    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let headers = response.headers_mut();
    headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
    headers.insert(axum::http::header::SEC_WEBSOCKET_ACCEPT, accept);
    headers.insert(
        SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static(SUBPROTOCOL),
    );
    response
}

/// Opens the TCP connection to the broker's gateway port, without Nagle's delay (MQTT packets
/// are small; the broker sets `set_tcp_nodelay true` on its side too, decision 6).
async fn connect(inner: &Inner) -> std::io::Result<TcpStream> {
    let address = (inner.broker_host.as_str(), inner.broker_port);
    let stream = tokio::time::timeout(inner.connect_timeout, TcpStream::connect(address))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "connecting timed out"))??;
    stream.set_nodelay(true)?;
    Ok(stream)
}

/// One relayed connection, after the 101 answer: waits for the upgrade, relays, logs both ends.
async fn run(
    bridge: Bridge,
    on_upgrade: hyper::upgrade::OnUpgrade,
    broker: TcpStream,
    client: IpAddr,
    id: String,
    slots: (OwnedSemaphorePermit, SourceSlot),
) {
    // Both slots are held until the relay ends, and given back when this returns.
    let _slots = slots;
    let inner = &*bridge.0;
    let upgraded = match on_upgrade.await {
        Ok(upgraded) => upgraded,
        Err(error) => {
            tracing::warn!(
                connection.id = %id,
                client.address = %client,
                error = %error,
                "the WebSocket upgrade did not complete"
            );
            inner.metrics.refused("upgrade_failed");
            return;
        }
    };
    let websocket = WebSocketStream::from_raw_socket(
        TokioIo::new(upgraded),
        Role::Server,
        Some(relay::websocket_config()),
    )
    .await;
    let broker_local = address(broker.local_addr());
    let broker_peer = address(broker.peer_addr());
    // The record of the gateway's source IP (decision 6); README.md, "來源 IP", says how to match
    // it with the broker's log line for the same connection.
    tracing::info!(
        connection.id = %id,
        client.address = %client,
        broker.local_address = %broker_local,
        broker.peer_address = %broker_peer,
        "relaying a gateway connection"
    );
    inner.metrics.opened();
    let started = Instant::now();
    let totals = Totals::default();
    let end = relay::relay(
        websocket,
        broker,
        inner.settings,
        inner.shutdown.requested(),
        &inner.metrics,
        &totals,
    )
    .await;
    inner.metrics.ended(end.reason());
    tracing::info!(
        connection.id = %id,
        client.address = %client,
        broker.local_address = %broker_local,
        reason = end.reason(),
        duration_ms = millis(started.elapsed()),
        bytes_to_broker = totals.to_broker(),
        bytes_to_gateway = totals.to_gateway(),
        "gateway connection ended"
    );
}

/// Answers a refused upgrade with `status` and records why.
fn refuse(
    inner: &Inner,
    status: StatusCode,
    reason: &'static str,
    client: Option<IpAddr>,
) -> Response {
    inner.metrics.refused(reason);
    match client {
        Some(client) => tracing::warn!(
            reason,
            client.address = %client,
            "refused a gateway WebSocket"
        ),
        None => tracing::warn!(reason, "refused a gateway WebSocket"),
    }
    (status, format!("{reason}\n")).into_response()
}

/// A random id that ties a connection's two log lines together.
fn connection_id() -> String {
    // The OS's CSPRNG failing leaves only a counter-free id; the log lines still carry the client
    // address and the broker-side address.
    format!("{:016x}", getrandom::u64().unwrap_or(0))
}

fn address(result: std::io::Result<SocketAddr>) -> String {
    result.map_or_else(|_| "unknown".to_owned(), |address| address.to_string())
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
