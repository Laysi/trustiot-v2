//! The identity service (decisions 13 and 27; docs/architecture/deployment.md, "身分與租戶").
//!
//! Server-side sessions in its own Postgres schema (Laysi/trustiot-v2#61), and the ingress
//! gateway's ext_authz check, which turns a session cookie into a short-lived internal identity
//! token signed for the service the request is routed to. Users log in with a code sent to their
//! email address, the only way to create a session (Laysi/trustiot-v2#103); they log out of one
//! session or of all. Platform administrators create tenants (Laysi/trustiot-v2#65), which other
//! services learn of from the `identity.tenant_created` event, and set users' roles in them
//! (Laysi/trustiot-v2#583), which the `identity.member_role_changed` event records. The service also keeps the
//! platform's event layer and operation log (Laysi/trustiot-v2#130, decision 34): it records
//! every producer's events once and serves them to tenant administrators and the back office.
//! A signed-in user reads their session and tenants, switches the session to another of their
//! tenants, and keeps their preferences (Laysi/trustiot-v2#497).
//!
//! - [`session`]: the cookie, its hash, its lifetime, and the Postgres session store.
//! - [`login`]: `POST /auth/login`, the login codes and the sessions they create.
//! - [`logout`]: revoking one session or all of a user's.
//! - [`current_session`]: the current session and the user's tenants, and switching the session
//!   to one of them.
//! - [`preferences`]: the user's language, AI answer language, time zone and theme.
//! - [`mail`]: the SMTP client that sends the codes.
//! - [`issuer`]: the signing keys the cluster generates, which one signs when, and the token
//!   format `crates/service-kit` verifies.
//! - [`audience`]: which service a request goes to, from the chart's cookie routes.
//! - [`ext_authz`]: the check endpoint.
//! - [`csrf`]: the check's refusal of cross-site requests, before the session is looked up.
//! - [`exchange`]: the token exchange, for services that call another one on behalf of the
//!   request's user (Laysi/trustiot-v2#106).
//! - [`delegation`]: run delegations, for an agent run that acts for its user longer than the
//!   request's token lives (Laysi/trustiot-v2#551).
//! - [`tenants`]: creating tenants, and the event that announces them.
//! - [`tenant_state`]: platform administrators suspending and resuming tenants, and the versioned
//!   event that carries each new state (Laysi/trustiot-v2#776).
//! - [`entitlements`]: each tenant's plan and entitlements, which platform administrators set,
//!   and the versioned event that carries each new snapshot to the services that check them
//!   (Laysi/trustiot-v2#111).
//! - [`state_applied`]: the certs service's confirmations that it applied a tenant's state, which
//!   end a suspension's "processing" in the back office (Laysi/trustiot-v2#779).
//! - [`members`]: platform administrators setting users' roles in tenants, and the event that
//!   records each change.
//! - [`event_log`]: the event layer: recording every producer's events, and reading them.
//! - [`ai_model_roles`]: platform administrators reading and changing the AI gateway's model
//!   roles through its admin API, and the event that records each change.
//! - [`schema`]: the migration files this release expects, which readiness checks
//!   (Laysi/trustiot-v2#402) and the migration Job applies.
//! - [`config`]: the settings.
//!
//! See `README.md` in this crate for the contract with the ingress gateway, the schema, key
//! rotation and the tests.

pub mod ai_model_roles;
pub mod audience;
pub mod config;
pub mod csrf;
pub mod current_session;
pub mod delegation;
pub mod email;
pub mod entitlements;
pub mod event_log;
pub mod exchange;
pub mod ext_authz;
pub mod issuer;
pub mod login;
pub mod logout;
pub mod mail;
pub mod members;
pub mod preferences;
pub mod schema;
pub mod session;
pub mod state_applied;
pub mod tenant_state;
pub mod tenants;

use axum::Router;
use contract::ServiceName;
use outbox::{Consumer, ConsumerConfig};
use service_kit::identity::{ExchangeVerifier, IdentityVerifier, PublishedKeys, ServiceIdentity};
use service_kit::migrations::MigrationCheck;
use service_kit::periodic;
use service_kit::tenant::{BackgroundTenantDb, PlatformActor, PlatformDb, TenantDb};
use service_kit::{BoxError, Context};
use sqlx::postgres::PgPoolOptions;
use utoipa::OpenApi;

use crate::config::Config;
use crate::current_session::AccountStore;
use crate::delegation::{PgDelegationStore, RunDelegations};
use crate::event_log::{EventLog, EventLogRecorder};
use crate::exchange::Exchange;
use crate::ext_authz::ExtAuthz;
use crate::issuer::{Issuer, IssuerSettings};
use crate::login::{Login, LoginStore};
use crate::mail::Mailer;
use crate::preferences::PreferencesStore;
use crate::session::{PgSessionStore, SessionStore};
use crate::state_applied::StateAppliedRecorder;
use crate::tenants::TenantStore;

/// The client API's OpenAPI document (merged into `api/openapi.json` by `tools/openapi`). The
/// ext_authz check and the token exchange under `/internal` are not part of it: clients never
/// reach them (the ingress gateway refuses `/internal*`), and the contracts of the exchange and
/// of run delegations are `contract::api::identity::ExchangeToken`, `CreateRunDelegation` and
/// `ExchangeRunDelegation`.
#[derive(OpenApi)]
#[openapi(
    paths(
        tenants::create_tenant,
        tenant_state::get_tenant_state,
        tenant_state::set_tenant_state,
        entitlements::get_current_entitlements,
        entitlements::get_tenant_entitlements,
        entitlements::set_tenant_entitlements,
        members::set_member_role,
        login::log_in,
        logout::log_out,
        logout::log_out_everywhere,
        current_session::current_session,
        current_session::switch_tenant,
        preferences::get_preferences,
        preferences::update_preferences,
        event_log::tenant_event_log,
        event_log::platform_tenant_event_log,
        event_log::platform_event_log,
        ai_model_roles::list_model_roles,
        ai_model_roles::set_model_role,
        ai_model_roles::reset_model_role
    ),
    tags((name = "identity", description = "Sessions, users and tenants (services/identity)."))
)]
struct ApiDoc;

/// The service's OpenAPI document.
#[must_use]
pub fn openapi() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

/// The service's startup for `service_kit::run`: starts following the public keys file and the
/// signing keys file ([`Issuer::watch`]), starts keeping the Postgres connections open (with the
/// session check of `crates/service-kit`), starts the worker that mails login codes, starts
/// recording the event log ([`event_log`]), serves the token exchange and run delegations on
/// their own port ([`serve_exchange`]), and returns the ext_authz check, login, logout, the
/// current session and preferences APIs, the tenant API, the member API, the AI model role API
/// ([`ai_model_roles`]) and the event log API for the API port. Readiness passes once the connections are open (`PgSessionStore::keep_warm`),
/// the public keys have arrived, a key may sign, and the database has every migration file this
/// release expects ([`schema::SCHEMA`]).
///
/// # Errors
///
/// When the database settings would not verify the server, or an event log subscription name
/// is invalid. Nothing is served then: the service exits with code 1. Missing or unusable key
/// files do not stop it: it starts unready and becomes ready when they arrive, as on a fresh
/// install. Without `TRUSTIOT_SMTP_HOST`, or with SMTP settings it cannot use (logged at ERROR),
/// it runs, but cannot send login codes: the ext_authz check that every request with a cookie
/// needs must not depend on the relay. Likewise without `TRUSTIOT_AI_GATEWAY_ADMIN_URL`, or with
/// one it cannot use: only the AI model role API answers 503.
pub async fn start(context: Context<Config>) -> Result<Router, BoxError> {
    let config = context.config();
    let platform = &config.platform;
    let mailer = match Mailer::from_platform(platform) {
        Ok(Some(mailer)) => Some(mailer),
        Ok(None) => {
            tracing::warn!(
                "TRUSTIOT_SMTP_HOST is not set (the chart's smtp.host): nobody can get a login \
                 code"
            );
            None
        }
        Err(error) => {
            tracing::error!(
                %error,
                "the SMTP settings cannot be used: nobody can get a login code until they are \
                 fixed and the service restarts"
            );
            None
        }
    };
    let published =
        PublishedKeys::watch(&context, platform.internal_identity_keys_file.clone()).await;
    let settings = IssuerSettings {
        signing_keys_file: platform.identity_signing_keys_file.clone(),
        revoke_before: platform.identity_revoke_before_generation,
        propagation_seconds: platform.identity_key_propagation_seconds,
        max_key_age_seconds: platform.identity_max_key_age_seconds,
    };
    let issuer = Issuer::watch(&context, &settings, published.clone()).await;
    tracing::info!(
        routes = platform.ingress_routes.len(),
        revoke_before = settings.revoke_before,
        "serving the ext_authz check"
    );
    let store = PgSessionStore::connect_lazy(
        config.postgres().connect_options(),
        config.database.max_connections,
    )?;
    context.spawn(store.clone().keep_warm(context.shutdown().clone()));
    let ping = store.clone();
    context.health().add_check("postgres", move || {
        let store = ping.clone();
        async move { store.ping().await }
    });
    MigrationCheck::new(&schema::SCHEMA, store.pool().clone()).register(context.health());
    // The tenant API's pool also serves setting members' roles (`members`), tenants' states
    // (`states`), tenants' entitlements (`entitlements`, the members' own read through
    // `accounts`) and the signed-in user's own account (`accounts`) (README, "連線池").
    let (tenants, accounts) =
        TenantStore::connect_lazy_shared(config.postgres().connect_options())?;
    let members = tenants.members();
    let states = tenants.states();
    // Checked by `Config::validate` already; a refusal here stops the startup all the same.
    let backfill_plan = platform
        .backfill_plan()
        .map_err(|invalid| format!("{}: {}", invalid.key, invalid.message))?;
    tracing::info!(
        plan = backfill_plan.plan().as_str(),
        plan_source = backfill_plan.source().as_str(),
        "tenants that existed before entitlements get this plan as their first snapshot"
    );
    let entitlements = tenants.entitlements(backfill_plan);
    let log = start_event_log(&context)?;
    // Tenants without entitlements yet (created before them, or by an older replica during a
    // rollout) get their first at start and then every `BACKFILL_PERIOD`: the platform role
    // lists them, on the event log's platform pool (README, "方案權益").
    if let Some((_, platform)) = &log {
        let reader = platform.reader(
            PlatformActor::service(ServiceIdentity::new(ServiceName::Identity)),
            entitlements::BACKFILL_PURPOSE,
        );
        let store = entitlements.clone();
        let shutdown = context.shutdown().clone();
        context.spawn(async move {
            entitlements::backfill_once(&reader, &store).await;
            periodic::run_every(
                entitlements::BACKFILL_JOB,
                entitlements::BACKFILL_PERIOD,
                shutdown,
                move || {
                    let (reader, store) = (reader.clone(), store.clone());
                    async move { entitlements::backfill_once(&reader, &store).await }
                },
            )
            .await;
        });
    }
    // Requests to the service's own API come through the ingress gateway like any other, with a
    // token the ext_authz check signed for `identity`.
    let verifier = IdentityVerifier::new(ServiceName::Identity, published.clone());
    // Services that call another one on behalf of a user exchange the request's token here, along
    // the deployment's call list (Laysi/trustiot-v2#106), on a port of its own.
    let exchange = Exchange::new(
        ExchangeVerifier::new(published.clone()),
        issuer.clone(),
        platform.on_behalf_calls.clone(),
    );
    // An agent run that acts for its user longer than the request's token lives holds a
    // delegation, exchanged on the same port along the same call list (Laysi/trustiot-v2#551).
    let delegations = RunDelegations::new(
        ExchangeVerifier::new(published),
        issuer.clone(),
        platform.on_behalf_calls.clone(),
        PgDelegationStore::connect_lazy(config.postgres().connect_options())?,
    );
    serve_exchange(
        &context,
        exchange::routes(exchange).merge(delegation::routes(delegations)),
    )
    .await?;
    // Two pools, so a flood of `send_code` never takes the connections `verify_code` and the
    // logouts need (README, "連線池").
    let logins =
        LoginStore::connect_lazy(config.postgres().connect_options(), login::POOL_CONNECTIONS)?;
    let sends = LoginStore::connect_lazy(
        config.postgres().connect_options(),
        login::SEND_POOL_CONNECTIONS,
    )?;
    context.spawn(sends.clone().clean_up(context.shutdown().clone()));
    let (login, worker) = Login::new(logins.clone(), sends, mailer, issuer.clone());
    if let Some(worker) = worker {
        context.spawn(worker.run(context.shutdown().clone()));
    }
    // The signed-in user's own account (Laysi/trustiot-v2#497) opens no pool of its own: the
    // service role's queries use the tenant API's pool, and the platform role's reads of a
    // user's memberships the event log's platform pool, at most `MEMBERSHIP_READS` at once
    // (README, "連線池").
    let memberships = log.as_ref().map(|(_, platform)| {
        platform.reader(
            PlatformActor::service(ServiceIdentity::new(ServiceName::Identity)),
            current_session::MEMBERSHIPS_PURPOSE,
        )
    });
    let account = AccountStore::new(accounts.clone(), memberships);
    // Platform administrators' model role API (Laysi/trustiot-v2#608): its own HTTP client to
    // the AI gateway's admin port; the events of its changes go through the event log's
    // platform pool (README, "連線池").
    let model_roles = ai_model_roles::routes(
        verifier.clone(),
        ai_model_roles::admin_client(platform.ai_gateway_admin_url.as_deref()),
        log.as_ref().map(|(_, platform)| platform.clone()),
    );
    let ext_authz = ExtAuthz::new(store, issuer, platform.ingress_routes.clone());
    Ok(ext_authz::routes(ext_authz)
        .merge(login::routes(login))
        .merge(logout::routes(verifier.clone(), logins))
        .merge(current_session::routes(verifier.clone(), account))
        .merge(preferences::routes(
            verifier.clone(),
            PreferencesStore::new(accounts.clone()),
        ))
        .merge(tenants::routes(verifier.clone(), tenants))
        .merge(members::routes(verifier.clone(), members))
        .merge(tenant_state::routes(verifier.clone(), states))
        .merge(entitlements::routes(
            verifier.clone(),
            entitlements,
            accounts,
        ))
        .merge(model_roles)
        .merge(event_log::routes(verifier, log.map(|(log, _)| log))))
}

/// Most connections one replica opens to Postgres, per role, when the ext_authz check's pool has
/// `max_connections` (`TRUSTIOT__DATABASE__MAX_CONNECTIONS`): every pool [`start`] opens, at its
/// largest. The roles have no connection limits of their own, so all services' sums, old and new
/// replicas during a rollout included, must stay under Postgres' `max_connections`: the chart
/// adds them up from its connection budget (deploy/chart/templates/_postgres.tpl,
/// Laysi/trustiot-v2#575), which tests/chart_connections.rs holds to this sum, as a unit test
/// holds services/identity/README.md, "連線池".
#[must_use]
pub fn connections_per_replica(max_connections: u32) -> ReplicaConnections {
    ReplicaConnections {
        service: max_connections
            + tenants::POOL_CONNECTIONS
            + login::POOL_CONNECTIONS
            + login::SEND_POOL_CONNECTIONS
            + delegation::POOL_CONNECTIONS
            + event_log::SERVICE_CONNECTIONS,
        platform: event_log::platform_connections(),
    }
}

/// Connections per database role; see [`connections_per_replica`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplicaConnections {
    /// As the service role (`identity_app`).
    pub service: u32,
    /// As the platform role (`identity_platform`).
    pub platform: u32,
}

impl ReplicaConnections {
    /// Both roles together.
    #[must_use]
    pub const fn total(self) -> u32 {
        self.service + self.platform
    }
}

/// Opens the event log's pools lazily and starts one consumer per producer in
/// [`schema::CONSUMES`], each recording that producer's events, and the consumer of certs'
/// confirmations of applied tenant states ([`state_applied`]); returns the event log and its
/// platform pool, which the current session API also reads users' memberships from. `None`,
/// with a WARN, when the platform role has no credentials ([`Config::platform_role`]); the
/// current session API answers 503 then.
///
/// Neither pool is part of readiness: the ext_authz check every request passes through must not
/// fail because the platform role cannot log in, or the event log fell behind. Without the
/// platform role, no confirmation is recorded either, so suspensions stay "processing" (the safe
/// side: the back office never shows a cut-off certs did not confirm). A recorder that
/// cannot work retries and its consumer's lag grows (`outbox.consumer.lag.age`, crates/outbox),
/// which is what alerts; the event log's API answers 503 while its pools cannot connect.
fn start_event_log(context: &Context<Config>) -> Result<Option<(EventLog, PlatformDb)>, BoxError> {
    let config = context.config();
    let Some(platform_role) = config.platform_role() else {
        tracing::warn!(
            "the event log and the current session API are off: the platform role has no \
             credentials (TRUSTIOT__DATABASE__PLATFORM_USERNAME, \
             TRUSTIOT__DATABASE__PLATFORM_PASSWORD)"
        );
        return Ok(None);
    };
    let pool = |connections| {
        PgPoolOptions::new()
            .max_connections(connections)
            .min_connections(0)
            .acquire_timeout(event_log::ACQUIRE_TIMEOUT)
    };
    let (db, work): (TenantDb, BackgroundTenantDb) = TenantDb::connect_lazy_with_background(
        config.postgres().connect_options(),
        pool(event_log::SERVICE_CONNECTIONS),
        ServiceIdentity::new(ServiceName::Identity),
    )?;
    let platform = PlatformDb::connect_lazy(
        platform_role.connect_options(),
        pool(event_log::platform_connections()),
    )?;
    for &source in schema::CONSUMES {
        let consumer = Consumer::new(
            &platform,
            work.clone(),
            ConsumerConfig::new(event_log::subscription(source), source)?,
            EventLogRecorder::new(source),
        );
        let stop = context.shutdown().stopping_token();
        context.spawn(async move {
            // `run` retries everything but a configuration mistake, which it logs; the service
            // keeps serving then and the consumer's lag shows the problem.
            let _ = consumer.run(stop).await;
        });
    }
    // Certs' confirmations of applied tenant states (Laysi/trustiot-v2#779), on a subscription of
    // their own, so neither waits for the other; `event_log::consumers` counts it in both pools.
    let consumer = Consumer::new(
        &platform,
        work,
        ConsumerConfig::new(state_applied::SUBSCRIPTION, state_applied::SOURCE)?,
        StateAppliedRecorder,
    );
    let stop = context.shutdown().stopping_token();
    context.spawn(async move {
        let _ = consumer.run(stop).await;
    });
    Ok(Some((EventLog::new(db, platform.clone()), platform)))
}

/// Serves the token exchange and run delegations (`routes`) on their own port
/// (`exchange.listen_addr`), apart from the API port, so the mesh can grant the services that
/// call on behalf of users this port and nothing else (deploy/chart/README.md, "呼叫矩陣"). `Context::serve_port` gives it the API port's layers
/// (handler timeout, panic recovery, request logs and trace context) and drain.
///
/// # Errors
///
/// When the port is taken: the service does not start.
async fn serve_exchange(context: &Context<Config>, routes: Router) -> Result<(), BoxError> {
    let address = context.config().exchange.listen_addr;
    let bound = context
        .serve_port(address, routes)
        .await
        .map_err(|error| format!("token exchange, exchange.listen_addr: {error}"))?;
    tracing::info!(
        address = %bound,
        calls = context.config().platform.on_behalf_calls.len(),
        "serving the token exchange"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_MAX_CONNECTIONS;

    const README: &str = include_str!("../README.md");

    /// The README's table of pools, one row per pool: from its header to the first line that is
    /// not a table row.
    fn pool_rows() -> Vec<&'static str> {
        let (_, after) = README
            .split_once("| 連線池 | 誰用 | 連線數 |")
            .expect("the README's pool table");
        after
            .lines()
            .skip(2)
            .take_while(|line| line.starts_with('|'))
            .collect()
    }

    /// The README states what one replica opens; the numbers come from the constants the pools
    /// are opened with, so a pool that grows or a new one changes the README with it.
    #[test]
    fn the_readme_states_the_connections_per_replica() {
        let connections = connections_per_replica(DEFAULT_MAX_CONNECTIONS);
        let stated = format!(
            "預設設定下每個副本最多 {} 條：{} 條以 `identity_app`、{} 條以 `identity_platform` 連線",
            connections.total(),
            connections.service,
            connections.platform
        );
        assert!(README.contains(&stated), "the README should say: {stated}");
        // No fixed ceiling here: tests/chart_connections.rs holds the chart's connection budget
        // to these numbers, and the chart refuses values whose whole-release sum exceeds
        // Postgres' `max_connections` (deploy/chart/README.md, "連線預算").

        let rows = pool_rows();
        let pools = [
            (
                "`TRUSTIOT__DATABASE__MAX_CONNECTIONS`".to_owned(),
                format!("`TRUSTIOT__DATABASE__MAX_CONNECTIONS`，預設 {DEFAULT_MAX_CONNECTIONS} |"),
            ),
            (
                "`tenants::POOL_CONNECTIONS`".to_owned(),
                format!("| {} |", tenants::POOL_CONNECTIONS),
            ),
            (
                "`login::POOL_CONNECTIONS`".to_owned(),
                format!("| {} |", login::POOL_CONNECTIONS),
            ),
            (
                "`login::SEND_POOL_CONNECTIONS`".to_owned(),
                format!("| {} |", login::SEND_POOL_CONNECTIONS),
            ),
            (
                "`delegation::POOL_CONNECTIONS`".to_owned(),
                format!("| {} |", delegation::POOL_CONNECTIONS),
            ),
            (
                "`event_log::SERVICE_CONNECTIONS`".to_owned(),
                format!("| {} |", event_log::SERVICE_CONNECTIONS),
            ),
            (
                "`event_log::platform_connections()`".to_owned(),
                format!("目前 {} |", event_log::platform_connections()),
            ),
        ];
        assert_eq!(rows.len(), pools.len(), "one row per pool: {rows:#?}");
        for (name, ending) in &pools {
            let row = rows
                .iter()
                .find(|row| row.contains(name.as_str()))
                .unwrap_or_else(|| panic!("no row for {name}: {rows:#?}"));
            assert!(row.ends_with(ending.as_str()), "{name}: {row}");
        }
    }
}
