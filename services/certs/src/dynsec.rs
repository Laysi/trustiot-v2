//! The MQTT broker's gateway permissions (Laysi/trustiot-v2#633; `README.md`, "broker 權限
//! （dynamic security）"): this service keeps the broker's dynamic-security gateway clients in
//! line with Postgres.
//!
//! Mosquitto 2.1 decides everything a client may do through dynamic security, and a gateway
//! certificate whose common name has no dynamic-security client connects but may do nothing
//! (docs/feasibility/2026-10-09-cloud-poc.md, "MQTT broker" 3). A gateway therefore has a client
//! named by its hardware ID, without a password, with the chart's gateway role
//! ([`protocol::acl::GATEWAY_ROLE`]) and nothing else, exactly when Postgres says it should
//! ([`DESIRED`], decided 2026-10-24): it is registered at the factory, not decommissioned, bound
//! to a tenant now, and has at least one gateway certificate that is neither revoked nor expired.
//! Otherwise it has no client.
//!
//! - **Only gateway clients are touched**: a client whose username is a valid
//!   [`protocol::GatewayId`]. The services' accounts (`service:*`, which a gateway ID can never
//!   be) and the chart's roles and groups are never changed, so the broker's init container keeps
//!   owning them.
//! - **The whole comparison** ([`Broker::full`]) lists every client of the broker (`listClients`,
//!   paged), then reads Postgres, and sends only the changes ([`plan`]): nothing when nothing
//!   differs. A missing gateway gets `createClient` with the role in the same command, which
//!   Mosquitto applies to a connection that is already up without dropping it (2.1.2
//!   `plugins/dynamic-security/clients.c` kicks clients on `addClientRole` and `deleteClient`,
//!   not on `createClient`), so a gateway bound while connected may publish at once. A gateway
//!   client that should not exist gets `deleteClient`, which also drops its connection. A gateway
//!   client with other roles, a group, `disabled`, a client id, or without this service's text
//!   name ([`CLIENT_TEXTNAME`]) is deleted and created again, which also clears a password
//!   someone may have set on it (the broker never says whether a client has one; see
//!   [`BrokerClient::is_correct_gateway_client`]).
//! - **One gateway now** ([`BrokerSync::gateway`]) compares that gateway alone (`getClient`):
//!   after a factory registration commits ([`crate::registrations`]), after a binding event is
//!   applied ([`SyncAfterBinding`], which waits for the delivery's transaction to commit), and
//!   after a renewal signs a new certificate (Laysi/trustiot-v2#81).
//! - **When**: the whole comparison runs on every connection to the broker (the first one and each
//!   reconnect, for example after the broker restarted with a reset dynamic-security file) and
//!   every `dynsec.interval`.
//! - **One replica at a time**: every comparison, whole or for one gateway, runs under the job
//!   lock ([`JOB`], `service_kit::job_lock`), so two replicas never interleave their changes; a
//!   replica that does not get it tries again shortly ([`LOCK_RETRY`]). Every replica keeps its own
//!   connection, so each can sync the gateways its own requests and events name.
//!
//! The commands go to [`CONTROL_TOPIC`] and the answers come back on [`RESPONSE_TOPIC`], matched
//! by the `correlationData` each command carries. Mosquitto sends the answers to the client that
//! sent the commands only. Every change logs one INFO line with the hardware ID and the action,
//! every failure a WARN; no line carries the broker password (the client in `crates/mqtt-client`
//! never shows it) or a command's text.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use contract::events::assets::GatewayBound;
use mqtt_client::{Client, Event, Events, PublishError, PublishOptions};
use outbox::{Delivery, Handler, HandlerError};
use protocol::GatewayId;
use protocol::acl::GATEWAY_ROLE;
use serde_json::{Value, json};
use service_kit::Context;
use service_kit::job_lock::{JobLock, JobLockError};
use sqlx::{PgPool, Postgres, Transaction};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::{Instant, sleep, sleep_until, timeout_at};

use crate::bindings::{Projection, ReadError};
use crate::config::DynsecSchedule;

/// The job's name for its advisory lock and its log lines.
pub const JOB: &str = "dynsec";

/// Where dynamic-security commands are published.
pub const CONTROL_TOPIC: &str = "$CONTROL/dynamic-security/v1";

/// Where Mosquitto answers them, to the sender alone.
pub const RESPONSE_TOPIC: &str = "$CONTROL/dynamic-security/v1/response";

/// Clients per `listClients` page. A verbose gateway client is about 100 bytes, so a page stays
/// far below the broker's packet limit (1 MiB + 8 KiB).
pub const LIST_PAGE: usize = 500;

/// Changes per control message: at most twice as many commands (a replacement is two).
pub const CHANGES_PER_MESSAGE: usize = 50;

/// How long one control message may wait for all of its answers.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// How soon a comparison that did not get the job lock tries again.
pub const LOCK_RETRY: Duration = Duration::from_secs(1);

/// How soon a comparison that failed tries again, at most; `dynsec.interval` when that is
/// shorter.
pub const ERROR_RETRY: Duration = Duration::from_secs(30);

/// How long a gateway's comparison waits for the transaction that asked for it to end (a binding
/// event's delivery) before it is put back for the next round.
pub const COMMIT_WAIT: Duration = Duration::from_secs(10);

/// Requests for single gateways waiting for the worker. A request that does not fit is dropped
/// with a WARN; the next whole comparison covers it.
pub const REQUEST_QUEUE: usize = 4096;

/// Most gateways compared one by one per lock.
const GATEWAYS_PER_ROUND: usize = 256;

/// How often [`COMMIT_WAIT`] checks the transaction.
const COMMIT_POLL: Duration = Duration::from_millis(50);

/// The audit log's purpose of every Postgres read of the sync.
pub const PURPOSE: &str = "sync-broker-permissions";

/// The text name of the clients this service creates.
pub const CLIENT_TEXTNAME: &str = "TrustIoT gateway (certs-managed)";

/// The gateways that should have the gateway role, by hardware ID (byte order): `$1` is NULL for
/// every gateway, or one hardware ID. Registered and not decommissioned; at least one gateway
/// certificate neither revoked nor expired (`now()` is the transaction's start); and bound now:
/// the latest binding in the assets service's log has not ended.
pub const DESIRED: &str = "SELECT r.hardware_id
FROM certs.gateway_registrations AS r
WHERE r.decommissioned_at IS NULL
  AND ($1::text IS NULL OR r.hardware_id = $1::text)
  AND EXISTS (
      SELECT 1 FROM certs.issued_certificates AS c
      WHERE c.profile = 'gateway' AND c.subject = r.hardware_id
        AND c.revoked_at IS NULL AND c.not_after > now())
  AND (
      SELECT b.unbound_at IS NULL FROM certs.gateway_bindings AS b
      WHERE b.hardware_id = r.hardware_id
      ORDER BY b.position_tx DESC, b.position_id DESC
      LIMIT 1) IS TRUE
ORDER BY r.hardware_id COLLATE \"C\"";

/// The status of a transaction (`$1`, an `xid8` as text): `in progress`, `committed`, `aborted`,
/// or NULL when it is too old to tell.
const TRANSACTION_STATUS: &str = "SELECT pg_xact_status($1::text::xid8)::text";

/// The current transaction's ID, assigning one if needed.
const CURRENT_TRANSACTION: &str = "SELECT pg_current_xact_id()::text";

/// Why a comparison failed. Never carries a password or a command's text; the broker's errors
/// are Mosquitto's fixed messages.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// Postgres failed.
    #[error("reading postgres: {0}")]
    Database(String),
    /// The broker refused or never acknowledged the control message.
    #[error("publishing the dynamic-security command: {0}")]
    Publish(#[from] PublishError),
    /// The connection was lost before every answer came.
    #[error("the mqtt connection was lost before the broker answered")]
    ConnectionLost,
    /// The broker did not answer in time.
    #[error("the broker did not answer within {}s", RESPONSE_TIMEOUT.as_secs())]
    NoAnswer,
    /// The broker answered something this service does not understand.
    #[error("unexpected answer to {command}: {problem}")]
    Malformed {
        command: &'static str,
        problem: &'static str,
    },
    /// The broker refused a read.
    #[error("the broker refused {command}: {error}")]
    Refused {
        command: &'static str,
        error: String,
    },
    /// Some changes failed; each was logged.
    #[error("{0} change(s) failed")]
    Changes(usize),
    /// The job lock failed.
    #[error(transparent)]
    Lock(#[from] JobLockError),
}

impl From<ReadError> for SyncError {
    fn from(error: ReadError) -> Self {
        Self::Database(error.log_kind())
    }
}

impl From<sqlx::Error> for SyncError {
    fn from(error: sqlx::Error) -> Self {
        Self::from(ReadError::from(error))
    }
}

/// A client as the broker lists it, with what decides whether it is a correct gateway client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrokerClient {
    /// The username: a gateway's hardware ID, a service's `service:<name>`, or anything else.
    pub username: String,
    /// The names of its roles.
    pub roles: Vec<String>,
    /// How many groups list it.
    pub groups: usize,
    /// Whether it is disabled.
    pub disabled: bool,
    /// Its text name: [`CLIENT_TEXTNAME`] on the clients this service creates.
    pub textname: Option<String>,
    /// The client id it is tied to; none on the clients this service creates.
    pub clientid: Option<String>,
}

impl BrokerClient {
    /// A client from a verbose `listClients` entry or a `getClient` answer; `None` when it has no
    /// username.
    #[must_use]
    pub fn from_json(value: &Value) -> Option<Self> {
        let username = value.get("username")?.as_str()?.to_owned();
        let roles = value
            .get("roles")
            .and_then(Value::as_array)
            .map(|roles| {
                roles
                    .iter()
                    .map(|role| {
                        role.get("rolename")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned()
                    })
                    .collect()
            })
            .unwrap_or_default();
        let groups = value
            .get("groups")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let disabled = value
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        Some(Self {
            username,
            roles,
            groups,
            disabled,
            textname: text("textname"),
            clientid: text("clientid"),
        })
    }

    /// Whether it is what this service creates for a gateway: the gateway role alone, in no
    /// group, enabled, with this service's text name and no client id.
    ///
    /// Mosquitto 2.1 never says whether a client has a password (`listClients` and `getClient`
    /// leave it out), so the text name stands in for it: a gateway client this service did not
    /// create, for example one made by hand with a password, is replaced by one without. A
    /// password set later on a client this service created (`setClientPassword`) cannot be seen;
    /// only a holder of the dynamic-security admin role can set one, and that role already
    /// controls the whole broker.
    #[must_use]
    pub fn is_correct_gateway_client(&self) -> bool {
        self.roles.len() == 1
            && self.roles[0] == GATEWAY_ROLE
            && self.groups == 0
            && !self.disabled
            && self.textname.as_deref() == Some(CLIENT_TEXTNAME)
            && self.clientid.is_none()
    }
}

/// One change to the broker.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    /// The gateway has no client: create it with the gateway role.
    Add(GatewayId),
    /// The gateway's client is not what this service creates: delete it and create it again.
    Replace(GatewayId),
    /// The gateway should have no client: delete it.
    Remove(GatewayId),
}

impl Change {
    /// The gateway it changes.
    #[must_use]
    pub fn gateway(&self) -> &GatewayId {
        match self {
            Self::Add(id) | Self::Replace(id) | Self::Remove(id) => id,
        }
    }

    /// The word the log line uses.
    #[must_use]
    pub fn action(&self) -> &'static str {
        match self {
            Self::Add(_) => "added",
            Self::Replace(_) => "replaced",
            Self::Remove(_) => "removed",
        }
    }

    /// The dynamic-security commands, in order, without `correlationData`.
    #[must_use]
    pub fn commands(&self) -> Vec<Value> {
        let create = |id: &GatewayId| {
            json!({
                "command": "createClient",
                "username": id.as_str(),
                "textname": CLIENT_TEXTNAME,
                "roles": [{ "rolename": GATEWAY_ROLE, "priority": 0 }],
            })
        };
        let delete = |id: &GatewayId| json!({ "command": "deleteClient", "username": id.as_str() });
        match self {
            Self::Add(id) => vec![create(id)],
            Self::Replace(id) => vec![delete(id), create(id)],
            Self::Remove(id) => vec![delete(id)],
        }
    }
}

/// The changes that make the broker's gateway clients match `desired`, given the broker's
/// clients `current`. With `only`, that gateway alone is compared. Clients whose username is not
/// a valid gateway ID are never changed. Empty when nothing differs.
#[must_use]
pub fn plan(
    desired: &BTreeSet<GatewayId>,
    current: &[BrokerClient],
    only: Option<&GatewayId>,
) -> Vec<Change> {
    let in_scope = |id: &GatewayId| only.is_none_or(|only| only == id);
    // A paged listing may repeat a client; the first entry counts.
    let mut gateways: BTreeMap<GatewayId, &BrokerClient> = BTreeMap::new();
    for client in current {
        if let Ok(id) = GatewayId::new(client.username.as_str())
            && in_scope(&id)
        {
            gateways.entry(id).or_insert(client);
        }
    }
    let mut changes = Vec::new();
    for id in desired.iter().filter(|id| in_scope(id)) {
        match gateways.get(id) {
            None => changes.push(Change::Add(id.clone())),
            Some(client) if !client.is_correct_gateway_client() => {
                changes.push(Change::Replace(id.clone()));
            }
            Some(_) => {}
        }
    }
    for id in gateways.keys() {
        if !desired.contains(id) {
            changes.push(Change::Remove(id.clone()));
        }
    }
    changes.sort();
    changes
}

/// What one comparison changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Gateways that should have the role.
    pub desired: usize,
    /// Gateway clients the broker had.
    pub gateway_clients: usize,
    pub added: usize,
    pub replaced: usize,
    pub removed: usize,
}

/// A request to compare one gateway now.
#[derive(Debug)]
struct Request {
    hardware_id: GatewayId,
    /// The transaction to wait for (an `xid8` as text): the binding event's delivery, which
    /// commits after the request is made.
    after: Option<String>,
}

/// Asks the sync to compare single gateways now. Cheap to clone. Without broker settings it does
/// nothing.
#[derive(Clone, Debug)]
pub struct BrokerSync {
    requests: Option<mpsc::Sender<Request>>,
}

impl BrokerSync {
    /// A handle that does nothing: no broker is configured.
    #[must_use]
    pub fn disabled() -> Self {
        Self { requests: None }
    }

    /// Whether a broker is configured.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.requests.is_some()
    }

    /// Compares `hardware_id` with Postgres as soon as possible, after the caller's change has
    /// committed: call it after the commit (a registration, a renewal). Never waits; when the
    /// queue is full the request is dropped with a WARN and the next whole comparison covers it.
    pub fn gateway(&self, hardware_id: &GatewayId) {
        self.request(Request {
            hardware_id: hardware_id.clone(),
            after: None,
        });
    }

    fn request(&self, request: Request) {
        let Some(requests) = &self.requests else {
            return;
        };
        let hardware_id = request.hardware_id.clone();
        if let Err(error) = requests.try_send(request) {
            let reason = match error {
                mpsc::error::TrySendError::Full(_) => "the queue is full",
                mpsc::error::TrySendError::Closed(_) => "the sync has stopped",
            };
            tracing::warn!(
                job = JOB,
                hardware_id = hardware_id.as_str(),
                reason,
                "dropped a gateway's broker permission sync; the next whole comparison covers it"
            );
        }
    }
}

/// The handler of the assets service's events with an immediate sync of the gateway each applied
/// `assets.gateway_bound` names, once its delivery has committed. The unbinding of
/// Laysi/trustiot-v2#115 adds its event here.
#[derive(Clone, Debug)]
pub struct SyncAfterBinding<H> {
    inner: H,
    sync: BrokerSync,
}

impl<H> SyncAfterBinding<H> {
    /// `inner`, then a request to `sync`.
    #[must_use]
    pub fn new(inner: H, sync: BrokerSync) -> Self {
        Self { inner, sync }
    }
}

impl<H: Handler> Handler for SyncAfterBinding<H> {
    fn begin(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> impl Future<Output = Result<(), HandlerError>> + Send {
        self.inner.begin(tx, delivery)
    }

    async fn handle(
        &self,
        tx: &mut Transaction<'static, Postgres>,
        delivery: &Delivery,
    ) -> Result<(), HandlerError> {
        self.inner.handle(tx, delivery).await?;
        if !self.sync.is_enabled() || !delivery.envelope.is::<GatewayBound>() {
            return Ok(());
        }
        // The inner handler decoded it already; one that does not decode was refused there.
        let Ok(bound) = delivery.envelope.decode::<GatewayBound>() else {
            return Ok(());
        };
        let transaction: String = sqlx::query_scalar(CURRENT_TRANSACTION)
            .fetch_one(&mut **tx)
            .await?;
        self.sync.request(Request {
            hardware_id: bound.hardware_id,
            after: Some(transaction),
        });
        Ok(())
    }
}

/// The answers this replica waits for, by `correlationData`.
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;

fn lock_pending(
    pending: &Pending,
) -> std::sync::MutexGuard<'_, HashMap<String, oneshot::Sender<Value>>> {
    // Nothing panics while holding it; a poisoned map is still consistent.
    pending.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Removes the entries of one control message when its wait ends, whatever the outcome.
struct Waiting<'a> {
    pending: &'a Pending,
    keys: Vec<String>,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let mut pending = lock_pending(self.pending);
        for key in &self.keys {
            pending.remove(key);
        }
    }
}

/// The connection to the broker and the Postgres reads; one comparison at a time.
struct Broker {
    client: Client,
    pending: Pending,
    /// Makes every `correlationData` of this replica unique.
    prefix: String,
    next: AtomicU64,
    projection: Projection,
    pool: PgPool,
}

impl fmt::Debug for Broker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Broker")
            .field("client_id", &self.client.client_id())
            .finish_non_exhaustive()
    }
}

impl Broker {
    /// Sends `commands` in one control message and returns the broker's answers in the same
    /// order.
    async fn send(&self, mut commands: Vec<Value>) -> Result<Vec<Value>, SyncError> {
        let mut receivers = Vec::with_capacity(commands.len());
        let mut waiting = Waiting {
            pending: &self.pending,
            keys: Vec::with_capacity(commands.len()),
        };
        for command in &mut commands {
            let key = format!(
                "{}-{}",
                self.prefix,
                self.next.fetch_add(1, Ordering::Relaxed)
            );
            let (sender, receiver) = oneshot::channel();
            lock_pending(&self.pending).insert(key.clone(), sender);
            if let Value::Object(object) = command {
                object.insert("correlationData".to_owned(), Value::String(key.clone()));
            }
            waiting.keys.push(key);
            receivers.push(receiver);
        }
        let payload = json!({ "commands": commands }).to_string();
        self.client
            .publish(CONTROL_TOPIC, payload, PublishOptions::default())
            .await?;
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        let mut answers = Vec::with_capacity(receivers.len());
        for receiver in receivers {
            match timeout_at(deadline, receiver).await {
                Ok(Ok(answer)) => answers.push(answer),
                Ok(Err(_)) => return Err(SyncError::ConnectionLost),
                Err(_) => return Err(SyncError::NoAnswer),
            }
        }
        drop(waiting);
        Ok(answers)
    }

    /// One read command's `data`.
    async fn read(&self, command: &'static str, body: Value) -> Result<Value, SyncError> {
        let mut answers = self.send(vec![body]).await?;
        let answer = answers.pop().ok_or(SyncError::Malformed {
            command,
            problem: "no answer",
        })?;
        if let Some(error) = answer.get("error").and_then(Value::as_str) {
            return Err(SyncError::Refused {
                command,
                error: error.to_owned(),
            });
        }
        answer.get("data").cloned().ok_or(SyncError::Malformed {
            command,
            problem: "no data",
        })
    }

    /// Every client of the broker, page by page.
    async fn list_clients(&self) -> Result<Vec<BrokerClient>, SyncError> {
        const COMMAND: &str = "listClients";
        let mut clients = Vec::new();
        let mut offset = 0_usize;
        loop {
            let data = self
                .read(
                    COMMAND,
                    json!({
                        "command": COMMAND,
                        "verbose": true,
                        "count": LIST_PAGE,
                        "offset": offset,
                    }),
                )
                .await?;
            let malformed = |problem| SyncError::Malformed {
                command: COMMAND,
                problem,
            };
            let total = data
                .get("totalCount")
                .and_then(Value::as_u64)
                .and_then(|total| usize::try_from(total).ok())
                .ok_or_else(|| malformed("no totalCount"))?;
            let page = data
                .get("clients")
                .and_then(Value::as_array)
                .ok_or_else(|| malformed("no clients"))?;
            for entry in page {
                clients.push(
                    BrokerClient::from_json(entry)
                        .ok_or_else(|| malformed("a client without a username"))?,
                );
            }
            offset = offset.saturating_add(page.len());
            if page.is_empty() || offset >= total {
                return Ok(clients);
            }
        }
    }

    /// The broker's client named `id`, if any.
    async fn get_client(&self, id: &GatewayId) -> Result<Vec<BrokerClient>, SyncError> {
        const COMMAND: &str = "getClient";
        match self
            .read(
                COMMAND,
                json!({ "command": COMMAND, "username": id.as_str() }),
            )
            .await
        {
            Ok(data) => {
                let client = data.get("client").and_then(BrokerClient::from_json).ok_or(
                    SyncError::Malformed {
                        command: COMMAND,
                        problem: "no client",
                    },
                )?;
                Ok(vec![client])
            }
            Err(SyncError::Refused { error, .. }) if error == "Client not found" => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    /// The gateways that should have the role ([`DESIRED`]); `only` limits it to one.
    async fn desired(&self, only: Option<&GatewayId>) -> Result<BTreeSet<GatewayId>, SyncError> {
        let mut tx = self.projection.begin(PURPOSE).await?;
        let rows: Vec<String> = sqlx::query_scalar(DESIRED)
            .bind(only.map(GatewayId::as_str))
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        rows.into_iter()
            .map(|id| GatewayId::new(id).map_err(|_| SyncError::from(ReadError::Corrupt)))
            .collect()
    }

    /// Sends `changes`, logging each one; fails when any failed.
    async fn apply(&self, changes: &[Change]) -> Result<(), SyncError> {
        let mut failed = 0_usize;
        for batch in changes.chunks(CHANGES_PER_MESSAGE) {
            let commands: Vec<Value> = batch.iter().flat_map(Change::commands).collect();
            let answers = self.send(commands).await?;
            let mut answers = answers.iter();
            for change in batch {
                let mut errors = Vec::new();
                for command in change.commands() {
                    let answer = answers.next();
                    let error = answer
                        .and_then(|answer| answer.get("error"))
                        .and_then(Value::as_str);
                    match (command["command"].as_str(), error) {
                        (_, None) => {}
                        // Gone already: what deleting it was for.
                        (Some("deleteClient"), Some("Client not found")) => {}
                        (_, Some(error)) => errors.push(error.to_owned()),
                    }
                }
                if errors.is_empty() {
                    tracing::info!(
                        job = JOB,
                        hardware_id = change.gateway().as_str(),
                        action = change.action(),
                        role = GATEWAY_ROLE,
                        "changed a gateway's broker permissions"
                    );
                } else {
                    failed += 1;
                    tracing::warn!(
                        job = JOB,
                        hardware_id = change.gateway().as_str(),
                        action = change.action(),
                        errors = %errors.join("; "),
                        "the broker refused a change of a gateway's permissions"
                    );
                }
            }
        }
        if failed == 0 {
            Ok(())
        } else {
            Err(SyncError::Changes(failed))
        }
    }

    /// The whole comparison: the broker's clients first, then Postgres, so a change that commits
    /// in between is either seen by both or left for the immediate sync that follows its commit.
    async fn full(&self) -> Result<Outcome, SyncError> {
        let current = self.list_clients().await?;
        let desired = self.desired(None).await?;
        let changes = plan(&desired, &current, None);
        let outcome = outcome(&desired, &current, &changes);
        self.apply(&changes).await?;
        if changes.is_empty() {
            tracing::debug!(
                job = JOB,
                desired = outcome.desired,
                gateway_clients = outcome.gateway_clients,
                "the broker's gateway permissions match postgres"
            );
        } else {
            tracing::info!(
                job = JOB,
                desired = outcome.desired,
                gateway_clients = outcome.gateway_clients,
                added = outcome.added,
                replaced = outcome.replaced,
                removed = outcome.removed,
                "brought the broker's gateway permissions in line with postgres"
            );
        }
        Ok(outcome)
    }

    /// The comparison of one gateway.
    async fn one(&self, id: &GatewayId) -> Result<(), SyncError> {
        let current = self.get_client(id).await?;
        let desired = self.desired(Some(id)).await?;
        self.apply(&plan(&desired, &current, Some(id))).await
    }

    /// Whether the transaction `xid` has ended (committed or rolled back), waiting up to
    /// [`COMMIT_WAIT`].
    async fn ended(&self, xid: &str) -> Result<bool, SyncError> {
        let deadline = Instant::now() + COMMIT_WAIT;
        loop {
            let status: Option<String> = sqlx::query_scalar(TRANSACTION_STATUS)
                .bind(xid)
                .fetch_one(&self.pool)
                .await?;
            if status.as_deref() != Some("in progress") {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            sleep(COMMIT_POLL).await;
        }
    }

    /// Compares the gateways of `pending` one by one, at most [`GATEWAYS_PER_ROUND`], removing
    /// each one done. A gateway whose transaction is still running, or whose comparison failed,
    /// stays; the first failure ends the round.
    async fn gateways(
        &self,
        pending: &mut BTreeMap<GatewayId, Option<String>>,
    ) -> Result<(), SyncError> {
        let ids: Vec<GatewayId> = pending.keys().take(GATEWAYS_PER_ROUND).cloned().collect();
        for id in ids {
            if let Some(Some(xid)) = pending.get(&id)
                && !self.ended(xid).await?
            {
                continue;
            }
            self.one(&id).await?;
            pending.remove(&id);
        }
        Ok(())
    }
}

fn outcome(desired: &BTreeSet<GatewayId>, current: &[BrokerClient], changes: &[Change]) -> Outcome {
    let mut outcome = Outcome {
        desired: desired.len(),
        gateway_clients: current
            .iter()
            .filter(|client| GatewayId::new(client.username.as_str()).is_ok())
            .count(),
        ..Outcome::default()
    };
    for change in changes {
        match change {
            Change::Add(_) => outcome.added += 1,
            Change::Replace(_) => outcome.replaced += 1,
            Change::Remove(_) => outcome.removed += 1,
        }
    }
    outcome
}

/// Starts the sync when `mqtt` is set and returns the handle that asks it for single gateways;
/// without `mqtt` logs one INFO line and returns [`BrokerSync::disabled`]. `pool` holds the job
/// lock; `projection` reads Postgres as the platform role.
///
/// # Errors
///
/// When the broker settings or the password file cannot be used: the service does not start.
pub fn start<T>(
    context: &Context<T>,
    mqtt: Option<&mqtt_client::Config>,
    schedule: &DynsecSchedule,
    projection: Projection,
    pool: PgPool,
) -> Result<BrokerSync, mqtt_client::StartError> {
    let Some(mqtt) = mqtt else {
        tracing::info!(
            job = JOB,
            "no mqtt broker is configured; the broker's gateway permissions are not synced"
        );
        return Ok(BrokerSync::disabled());
    };
    let (client, events) = Client::start(mqtt, vec![RESPONSE_TOPIC.to_owned()])?;
    let mut random = [0_u8; 8];
    // The client id's randomness came from the same source a moment ago; without it the prefix
    // still differs per process through the client id.
    let _ = getrandom::fill(&mut random);
    let prefix = format!(
        "certs-{}",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let pending: Pending = Arc::default();
    let connected = Arc::new(Notify::new());
    let (requests, receiver) = mpsc::channel(REQUEST_QUEUE);
    context.spawn(read_events(
        events,
        Arc::clone(&pending),
        Arc::clone(&connected),
    ));
    let sync = Broker {
        client,
        pending,
        prefix,
        next: AtomicU64::new(0),
        projection,
        pool: pool.clone(),
    };
    let lock = JobLock::new(pool, "certs", JOB);
    let shutdown = context.shutdown().clone();
    let interval = schedule.interval;
    context.spawn(async move {
        tokio::select! {
            () = work(&sync, &lock, interval, &connected, receiver) => {}
            () = shutdown.stopping() => {}
        }
        sync.client.shutdown().await;
    });
    tracing::info!(
        job = JOB,
        host = %mqtt.host,
        port = mqtt.port,
        interval_s = interval.as_secs(),
        "syncing the broker's gateway permissions"
    );
    Ok(BrokerSync {
        requests: Some(requests),
    })
}

/// Hands every answer on [`RESPONSE_TOPIC`] to whoever waits for it, and signals every
/// connection. Answers in flight when the connection drops never come: their waits fail at once.
async fn read_events(mut events: Events, pending: Pending, connected: Arc<Notify>) {
    while let Some(event) = events.recv().await {
        match event {
            Event::Connected { subscriptions } => {
                if !subscriptions.all_granted() {
                    tracing::warn!(
                        job = JOB,
                        "the broker refused the dynamic-security answers to this account; \
                         check that it holds the dynamic-security admin role"
                    );
                }
                connected.notify_one();
            }
            Event::Disconnected { .. } => lock_pending(&pending).clear(),
            Event::Message(message) if message.topic == RESPONSE_TOPIC => {
                dispatch(&message.payload, &pending);
            }
            Event::Message(message) => {
                tracing::debug!(job = JOB, topic = %message.topic, "ignored a message");
            }
        }
    }
}

/// Delivers each answer of a response message to its waiter.
fn dispatch(payload: &[u8], pending: &Pending) {
    let responses = match serde_json::from_slice::<Value>(payload) {
        Ok(Value::Object(mut root)) => match root.remove("responses") {
            Some(Value::Array(responses)) => responses,
            _ => Vec::new(),
        },
        _ => {
            tracing::warn!(
                job = JOB,
                bytes = payload.len(),
                "a dynamic-security answer is not a JSON object"
            );
            return;
        }
    };
    for response in responses {
        let key = response
            .get("correlationData")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let waiter = key.and_then(|key| lock_pending(pending).remove(&key));
        match waiter {
            Some(waiter) => {
                let _ = waiter.send(response);
            }
            None => tracing::debug!(job = JOB, "a dynamic-security answer nobody waits for"),
        }
    }
}

/// When the next attempt of something that failed may run.
fn retry_after_error(interval: Duration) -> Instant {
    Instant::now() + ERROR_RETRY.min(interval)
}

/// The worker: whole comparisons on every connection and every `interval`, single gateways as
/// they are asked for, each under the job lock.
async fn work(
    sync: &Broker,
    lock: &JobLock,
    interval: Duration,
    connected: &Notify,
    mut requests: mpsc::Receiver<Request>,
) {
    // The first whole comparison follows the first connection.
    let mut full_at: Option<Instant> = None;
    let mut next_tick = Instant::now() + interval;
    let mut pending: BTreeMap<GatewayId, Option<String>> = BTreeMap::new();
    let mut gateways_at = Instant::now();
    loop {
        let wake = [
            Some(next_tick),
            full_at,
            (!pending.is_empty()).then_some(gateways_at),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(next_tick);
        tokio::select! {
            () = connected.notified() => full_at = Some(Instant::now()),
            request = requests.recv() => match request {
                Some(request) => {
                    pending.insert(request.hardware_id, request.after);
                }
                // Every handle is gone: the service is stopping.
                None => return,
            },
            () = sleep_until(wake) => {}
        }
        // Take everything that queued up meanwhile, so one lock covers it.
        while let Ok(request) = requests.try_recv() {
            pending.insert(request.hardware_id, request.after);
        }
        let now = Instant::now();
        if now >= next_tick {
            next_tick = now + interval;
            full_at = Some(now);
        }
        if full_at.is_some_and(|at| at <= now) {
            full_at = match lock.run_exclusive(|| sync.full()).await {
                Ok(Some(Ok(_))) => None,
                Ok(None) => Some(Instant::now() + LOCK_RETRY),
                Ok(Some(Err(error))) => {
                    tracing::warn!(job = JOB, error = %error, "the whole comparison with the broker failed");
                    Some(retry_after_error(interval))
                }
                Err(error) => {
                    tracing::warn!(job = JOB, error = %error, "the whole comparison with the broker failed");
                    Some(retry_after_error(interval))
                }
            };
        }
        if !pending.is_empty() && gateways_at <= Instant::now() {
            gateways_at = match lock.run_exclusive(|| sync.gateways(&mut pending)).await {
                Ok(Some(Ok(()))) if pending.is_empty() => Instant::now(),
                // Gateways whose transaction still runs, or more than one round holds.
                Ok(Some(Ok(()))) | Ok(None) => Instant::now() + LOCK_RETRY,
                Ok(Some(Err(error))) => {
                    tracing::warn!(job = JOB, error = %error, waiting = pending.len(), "a gateway's broker permission sync failed");
                    retry_after_error(interval)
                }
                Err(error) => {
                    tracing::warn!(job = JOB, error = %error, waiting = pending.len(), "a gateway's broker permission sync failed");
                    retry_after_error(interval)
                }
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> GatewayId {
        GatewayId::new(value).expect("a gateway id")
    }

    fn client(username: &str, roles: &[&str]) -> BrokerClient {
        BrokerClient {
            username: username.to_owned(),
            roles: roles.iter().map(|role| (*role).to_owned()).collect(),
            groups: 0,
            disabled: false,
            textname: Some(CLIENT_TEXTNAME.to_owned()),
            clientid: None,
        }
    }

    fn desired(ids: &[&str]) -> BTreeSet<GatewayId> {
        ids.iter().map(|value| id(value)).collect()
    }

    #[test]
    fn nothing_changes_when_the_broker_matches() {
        let current = [
            client("gw-a", &[GATEWAY_ROLE]),
            client("gw-b", &[GATEWAY_ROLE]),
            client(
                "service:certs",
                &["trustiot-service-certs", "trustiot-dynsec-admin"],
            ),
            client("service:ingest", &["trustiot-service-ingest"]),
        ];
        assert!(plan(&desired(&["gw-a", "gw-b"]), &current, None).is_empty());
        assert!(plan(&desired(&["gw-a", "gw-b"]), &current, Some(&id("gw-a"))).is_empty());
    }

    #[test]
    fn missing_gateways_are_added_and_extra_ones_removed_and_services_never_touched() {
        let current = [
            client("gw-a", &[GATEWAY_ROLE]),
            client("gw-ghost", &[GATEWAY_ROLE]),
            client("service:certs", &["trustiot-dynsec-admin"]),
            // Not gateway IDs: never touched, whatever they hold.
            client("service:ingest", &[GATEWAY_ROLE]),
            client("not a gateway", &[]),
            client("", &[GATEWAY_ROLE]),
        ];
        assert_eq!(
            plan(&desired(&["gw-a", "gw-b"]), &current, None),
            [Change::Add(id("gw-b")), Change::Remove(id("gw-ghost"))]
        );
        // One gateway: the others are not looked at.
        assert_eq!(
            plan(&desired(&["gw-a", "gw-b"]), &current, Some(&id("gw-b"))),
            [Change::Add(id("gw-b"))]
        );
        assert_eq!(
            plan(&desired(&["gw-a"]), &current, Some(&id("gw-ghost"))),
            [Change::Remove(id("gw-ghost"))]
        );
        assert!(plan(&desired(&[]), &current, Some(&id("gw-b"))).is_empty());
    }

    #[test]
    fn a_gateway_client_that_is_not_what_the_service_creates_is_replaced() {
        let mut grouped = client("gw-c", &[GATEWAY_ROLE]);
        grouped.groups = 1;
        let mut disabled = client("gw-d", &[GATEWAY_ROLE]);
        disabled.disabled = true;
        // Made by someone else (it may have a password, which the broker never shows).
        let mut foreign = client("gw-f", &[GATEWAY_ROLE]);
        foreign.textname = None;
        let mut renamed = client("gw-g", &[GATEWAY_ROLE]);
        renamed.textname = Some("by hand".to_owned());
        let mut tied = client("gw-h", &[GATEWAY_ROLE]);
        tied.clientid = Some("gw-h".to_owned());
        let current = [
            client("gw-a", &[]),
            client("gw-b", &[GATEWAY_ROLE, "trustiot-dynsec-admin"]),
            grouped,
            disabled,
            // A repeated entry of a paged listing: the first counts.
            client("gw-e", &[GATEWAY_ROLE]),
            client("gw-e", &[]),
            foreign,
            renamed,
            tied,
        ];
        assert_eq!(
            plan(
                &desired(&[
                    "gw-a", "gw-b", "gw-c", "gw-d", "gw-e", "gw-f", "gw-g", "gw-h"
                ]),
                &current,
                None
            ),
            [
                Change::Replace(id("gw-a")),
                Change::Replace(id("gw-b")),
                Change::Replace(id("gw-c")),
                Change::Replace(id("gw-d")),
                Change::Replace(id("gw-f")),
                Change::Replace(id("gw-g")),
                Change::Replace(id("gw-h")),
            ]
        );
    }

    #[test]
    fn commands_carry_the_gateway_role_and_no_password() {
        let add = Change::Add(id("gw-a")).commands();
        assert_eq!(
            add,
            [json!({
                "command": "createClient",
                "username": "gw-a",
                "textname": CLIENT_TEXTNAME,
                "roles": [{ "rolename": GATEWAY_ROLE, "priority": 0 }],
            })]
        );
        let replace = Change::Replace(id("gw-a")).commands();
        assert_eq!(
            replace[0],
            json!({ "command": "deleteClient", "username": "gw-a" })
        );
        assert_eq!(replace[1], add[0]);
        assert_eq!(
            Change::Remove(id("gw-a")).commands(),
            [json!({ "command": "deleteClient", "username": "gw-a" })]
        );
        for command in add.iter().chain(&replace) {
            assert!(command.get("password").is_none(), "{command}");
        }
    }

    #[test]
    fn clients_are_read_from_verbose_entries() {
        let entry = json!({
            "username": "gw-a",
            "clientid": "gw-a-1",
            "textname": "x",
            "roles": [{ "rolename": GATEWAY_ROLE, "priority": 0 }],
            "groups": [{ "groupname": "g", "priority": 1 }],
            "disabled": true,
        });
        assert_eq!(
            BrokerClient::from_json(&entry),
            Some(BrokerClient {
                username: "gw-a".to_owned(),
                roles: vec![GATEWAY_ROLE.to_owned()],
                groups: 1,
                disabled: true,
                textname: Some("x".to_owned()),
                clientid: Some("gw-a-1".to_owned()),
            })
        );
        let mut bare = client("gw-b", &[]);
        bare.textname = None;
        assert_eq!(
            BrokerClient::from_json(&json!({ "username": "gw-b" })),
            Some(bare)
        );
        // What the service creates reads back as correct.
        let created = BrokerClient::from_json(&json!({
            "username": "gw-c",
            "textname": CLIENT_TEXTNAME,
            "roles": [{ "rolename": GATEWAY_ROLE, "priority": 0 }],
            "groups": [],
        }))
        .expect("a client");
        assert!(created.is_correct_gateway_client());
        assert_eq!(BrokerClient::from_json(&json!({ "roles": [] })), None);
    }

    #[test]
    fn answers_reach_their_waiters_only() {
        let pending: Pending = Arc::default();
        let (first, mut first_answer) = oneshot::channel();
        let (second, mut second_answer) = oneshot::channel();
        lock_pending(&pending).insert("p-1".to_owned(), first);
        lock_pending(&pending).insert("p-2".to_owned(), second);
        let payload = json!({ "responses": [
            { "command": "getClient", "error": "Client not found", "correlationData": "p-2" },
            { "command": "getClient", "correlationData": "unknown" },
            { "command": "getClient" },
        ]});
        dispatch(payload.to_string().as_bytes(), &pending);
        dispatch(b"not json", &pending);
        assert_eq!(
            second_answer.try_recv().expect("p-2 answered")["error"],
            "Client not found"
        );
        assert!(first_answer.try_recv().is_err());
        assert_eq!(lock_pending(&pending).len(), 1);
    }

    #[test]
    fn outcomes_count_only_gateway_clients() {
        let current = [
            client("gw-a", &[GATEWAY_ROLE]),
            client("gw-ghost", &[GATEWAY_ROLE]),
            client("service:certs", &[]),
        ];
        let wanted = desired(&["gw-a", "gw-b"]);
        let changes = plan(&wanted, &current, None);
        assert_eq!(
            outcome(&wanted, &current, &changes),
            Outcome {
                desired: 2,
                gateway_clients: 2,
                added: 1,
                replaced: 0,
                removed: 1,
            }
        );
    }
}
