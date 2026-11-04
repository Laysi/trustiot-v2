//! Questions answered in the background (Laysi/trustiot-v2#646). A question can run for up to
//! its run delegation's lifetime (15 minutes by default), far longer than a request may wait
//! (Cloudflare answers 524 after 125 seconds without a first byte), so the question API answers
//! 202 at once and the question runs in a task of this replica; the client learns the result by
//! reading the conversation (streaming is Laysi/trustiot-v2#147).
//!
//! [`Questions`] is the handle the router's state owns. For each question it runs one task
//! ([`Questions::ask`]), tracked so shutdown waits for it, that:
//!
//! 1. records the run in the request's tenant transaction: [`store::admit_question`] refuses a
//!    missing conversation, a question already running in it, the user's running limit or a
//!    full history, and measures what the conversation holds; the question then takes a seat of
//!    this replica for that ([`capacity`]: at most `questions.max_running_per_replica`
//!    questions, within `questions.memory`), or is refused (503); only then
//!    [`store::start_question`] records the run (its ID is the one the delegation is then
//!    started with) and loads the conversation. Every refusal comes before anything is written
//!    and before the identity service is called, and a refused request never holds a seat. A
//!    regeneration of the last answer (Laysi/trustiot-v2#649, [`Request::Regenerate`]) differs
//!    in this step only: [`store::admit_regeneration`] takes the same locks and refusals, checks
//!    and discards the last round and measures what remains, and its question is the discarded
//!    round's; the seat, the record and the load follow in the same transaction, so a refusal
//!    (the 503 of a full replica included) rolls the discard back;
//! 2. starts the run delegation ([`QuestionRuns::start`]; without `identity.exchange_url`, an
//!    [`UndelegatedRun`]); when the identity service refuses, the run ends `failed`
//!    (`delegation_<code>`) and the request gets the delegation's M1 error;
//! 3. answers the request (202) and runs [`Agent::ask`], raced against a stop: the run's
//!    cancellation token (a stop or delete handled by this replica cancels at once), a poll of
//!    the run's record every [`CANCEL_POLL_INTERVAL`] (a stop or delete handled by another
//!    replica), and the service's shutdown; it never runs past the run's recorded deadline;
//! 4. records the result: an answered round with the run's end in one transaction
//!    ([`store::finish_answered`], which stores nothing when a stop or delete committed first),
//!    otherwise only the run's end (`failed` with the agent's error kind, `cancelled`, or
//!    `interrupted` at shutdown). A failed, stopped or interrupted round leaves nothing in the
//!    history: dropping the `ask` future is the cancellation, and nothing of an unanswered round
//!    is written.
//!
//! The whole of it runs in the task, not in the request's handler, so a client that disconnects
//! (or a request that times out) never leaves a run recorded as `running` with no task behind
//! it. The task's transactions are in the tenant of the request's verified identity, which it
//! keeps for that (the question is the request's continuation; `TenantDb::begin` reads only the
//! tenant from it). A supervisor awaits each task: a task that panics has its run recorded
//! `failed` (`internal`) rather than left `running` until its deadline, and what it held
//! (its entry in this replica's running questions) is let go as it unwinds.
//!
//! Logs carry IDs, outcomes and error classes only: never the question, the answer, a title or
//! an error's text; a [`StartError`] as its M1 code only, a [`StoreError`] as its class only.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use contract::RunId;
use service_kit::identity::UserIdentity;
use service_kit::shutdown::Shutdown;
use service_kit::tenant::TenantDb;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

pub mod capacity;

use crate::agent::{Agent, AgentError};
use crate::api;
use crate::delegation::{QuestionRuns, StartError, UndelegatedRun};
use crate::history::Principal;
use crate::store::{self, Answered, Loaded, RunOutcome, RunRecord, RunStatus, StoreError};
use crate::tools::{Delegation, millis};
use capacity::{Capacity, Full, Seat};

/// How often a running question reads its run's record to learn of a stop or delete that
/// another replica handled. A stop takes effect within this interval plus one short transaction
/// (bounded by [`crate::ACQUIRE_TIMEOUT`] for a connection and `api::REQUEST_LOCK_TIMEOUT` for
/// a lock, though the read takes no lock); the replica that handles the stop cancels at once.
/// Polling rather than `LISTEN`/`NOTIFY`: a notification needs a dedicated connection outside
/// the pool and outside row-level security, is lost across reconnects (so a poll is needed
/// anyway), and would carry every tenant's run IDs on one channel. Each wait is shortened by a
/// random part of up to [`CANCEL_POLL_JITTER`], so the polls of questions that started together
/// spread out instead of reaching the pool at the same moments.
pub const CANCEL_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The most a poll's wait is shortened by (see [`CANCEL_POLL_INTERVAL`]).
pub const CANCEL_POLL_JITTER: Duration = Duration::from_millis(500);

/// `Retry-After` of a question this replica had no seat or memory for: questions end within
/// seconds to minutes, and a retry may reach another replica.
pub const BUSY_RETRY_AFTER: Duration = Duration::from_secs(5);

/// The question API's limits (`questions.*`) and the run's lifetime (`identity.run_lifetime`).
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    /// The longest question, in characters after trimming.
    pub max_chars: usize,
    /// How many questions one user may have running in one tenant.
    pub max_running_per_user: u32,
    /// How many questions this replica runs at once.
    pub max_running_per_replica: usize,
    /// The bytes this replica's running questions may reserve ([`capacity`]).
    pub memory: usize,
    /// How long a question's run lives: its recorded deadline, and its delegation's lifetime.
    pub run_lifetime: Duration,
}

/// Runs the questions of this replica. Cheap to clone; the router's state owns it.
#[derive(Clone)]
pub struct Questions {
    inner: Arc<Inner>,
}

struct Inner {
    db: TenantDb,
    agent: Arc<Agent>,
    /// `None` without `identity.exchange_url`: questions run with an [`UndelegatedRun`].
    runs: Option<QuestionRuns>,
    settings: Settings,
    /// This replica's seats and memory for questions.
    capacity: Capacity,
    tracker: TaskTracker,
    /// The service's shutdown: running questions end `interrupted`.
    stopping: CancellationToken,
    /// This replica's questions that have not ended, by run ID: a stop or delete handled here
    /// cancels the token at once.
    running: Running,
}

/// This replica's running questions' cancellation tokens, by run ID.
type Running = Arc<Mutex<HashMap<Uuid, CancellationToken>>>;

/// A question to ask: who asks, where, and what.
pub struct Ask {
    /// The request's verified identity: the delegation starts from it, and the task's
    /// transactions are in its tenant.
    pub user: UserIdentity,
    /// The request's user in its tenant, the conversation's owner.
    pub principal: Principal,
    pub conversation: Uuid,
    pub request: Request,
}

/// What a question asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// A new question, trimmed and within [`Settings::max_chars`].
    Question(String),
    /// The last round's question again, after its round is discarded
    /// ([`store::admit_regeneration`], Laysi/trustiot-v2#649).
    Regenerate,
}

impl Request {
    fn regenerates(&self) -> bool {
        matches!(self, Self::Regenerate)
    }
}

/// Why a question did not start.
#[derive(Debug)]
pub enum Refused {
    /// The service is shutting down; nothing was recorded.
    ShuttingDown,
    /// This replica runs as many questions as it may, or has no memory left for this one
    /// (logged); nothing was recorded.
    Busy,
    /// The store refused or failed to record the run (a transaction that did not start
    /// included); nothing was recorded.
    Store(StoreError),
    /// The identity service did not issue the delegation; the run ended `failed` (logged with
    /// the code).
    Delegation(StartError),
    /// The task ended without telling (it panicked).
    Lost,
}

/// The failure code of a run whose task panicked.
const PANICKED: &str = "internal";

/// How a question's task ended, for the run's record and the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ended {
    Answered,
    Failed(AgentError),
    Cancelled,
    Interrupted,
}

impl Questions {
    /// The handle: questions read and write through `db`, run on `agent`, start their
    /// delegations through `runs`, and end `interrupted` when `stopping` is cancelled.
    #[must_use]
    pub fn new(
        db: TenantDb,
        agent: Arc<Agent>,
        runs: Option<QuestionRuns>,
        settings: Settings,
        stopping: CancellationToken,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                db,
                agent,
                runs,
                settings,
                capacity: Capacity::new(settings.max_running_per_replica, settings.memory),
                tracker: TaskTracker::new(),
                stopping,
                running: Running::default(),
            }),
        }
    }

    /// The agent the questions run on (also what starts a conversation).
    #[must_use]
    pub fn agent(&self) -> &Agent {
        &self.inner.agent
    }

    /// The same agent, for routes that keep their own handle on it: a copy starts a
    /// conversation of its own ([`crate::api::sharing`]).
    #[must_use]
    pub fn shared_agent(&self) -> Arc<Agent> {
        Arc::clone(&self.inner.agent)
    }

    #[must_use]
    pub fn settings(&self) -> Settings {
        self.inner.settings
    }

    /// Waits until the service stops (`shutdown`'s stopping), then until every question of this
    /// replica has ended and recorded `interrupted`: for `Context::spawn`, so service-kit's drain
    /// waits for them, within `shutdown.drain_timeout`.
    pub async fn drain(self, shutdown: Shutdown) {
        shutdown.stopping().await;
        self.inner.tracker.close();
        self.inner.tracker.wait().await;
    }

    /// Starts `ask`'s question and returns its run as recorded (`running`) once its delegation
    /// started, while the question goes on in the background. The work runs in a task of its
    /// own (see the module documentation): dropping the returned future does not stop it.
    ///
    /// # Errors
    ///
    /// [`Refused`].
    pub async fn ask(&self, ask: Ask) -> Result<RunRecord, Refused> {
        let (reply, answer) = oneshot::channel();
        let Ask {
            user,
            principal,
            conversation,
            request,
        } = ask;
        let user = Arc::new(user);
        let run_id = RunId::generate();
        let task = {
            let (this, user) = (self.clone(), Arc::clone(&user));
            async move {
                this.launch(&user, principal, conversation, request, run_id, reply)
                    .await;
            }
        };
        let this = self.clone();
        spawn_supervised(&self.inner.tracker, task, move || async move {
            this.panicked(&user, principal, conversation, Uuid::from(run_id))
                .await;
        });
        answer.await.unwrap_or(Err(Refused::Lost))
    }

    /// After the task of the question `run_id` panicked: records the run `failed` with
    /// [`PANICKED`] if it was recorded and is still running, so it neither blocks its
    /// conversation nor counts toward its user's limit until its deadline.
    async fn panicked(
        &self,
        user: &UserIdentity,
        principal: Principal,
        conversation: Uuid,
        run_id: Uuid,
    ) {
        tracing::error!(
            run_id = %run_id,
            conversation_id = %conversation,
            tenant_id = %principal.tenant_id,
            "a question's task panicked"
        );
        let result = match api::begin_tx(&self.inner.db, user).await {
            Ok(mut tx) => {
                let result =
                    store::finish_run(&mut tx, principal, run_id, RunOutcome::Failed(PANICKED))
                        .await;
                api::finish(tx, result).await.map(|_| ())
            }
            Err(failure) => Err(failure),
        };
        match result {
            // Not recorded yet, or ended before the panic: nothing to close.
            Ok(()) | Err(StoreError::NoSuchRun | StoreError::RunEnded) => {}
            Err(failure) => tracing::warn!(
                run_id = %run_id,
                conversation_id = %conversation,
                tenant_id = %principal.tenant_id,
                error = %failure,
                "could not record how a question ended"
            ),
        }
    }

    /// Cancels the question `run_id` if this replica runs it, after a stop or delete recorded
    /// the request: the task drops its `ask` at once and records `cancelled`. Returns whether it
    /// runs here. Call it only for a run the store just accepted the caller's stop for: the run
    /// ID alone authorises nothing.
    pub fn cancel_here(&self, run_id: Uuid) -> bool {
        let running = self
            .inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        running
            .get(&run_id)
            .map(CancellationToken::cancel)
            .is_some()
    }

    /// Enters the question `run_id` in this replica's running questions; it leaves them when
    /// the returned guard is dropped, however its task ends (a panic included).
    fn register(&self, run_id: Uuid) -> (CancellationToken, Registered) {
        let token = CancellationToken::new();
        self.inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(run_id, token.clone());
        let registered = Registered {
            running: Arc::clone(&self.inner.running),
            run_id,
        };
        (token, registered)
    }

    /// The question's task: steps 1 to 4 of the module documentation.
    async fn launch(
        self,
        user: &UserIdentity,
        principal: Principal,
        conversation: Uuid,
        request: Request,
        run_id: RunId,
        reply: oneshot::Sender<Result<RunRecord, Refused>>,
    ) {
        // A question that starts after shutdown began would only be interrupted at once.
        if self.inner.stopping.is_cancelled() {
            let _ = reply.send(Err(Refused::ShuttingDown));
            return;
        }
        let id = Uuid::from(run_id);
        // Registered before the run is recorded, so a stop or delete that commits right after
        // the record finds it here, even before the question's work starts.
        let (cancel, _registered) = self.register(id);
        // Before the run's transaction starts, so `started + run_lifetime` is never later than
        // the deadline the database records (its now() is taken after this).
        let started = Instant::now();
        let lifetime = self.inner.settings.run_lifetime;
        let regenerated = request.regenerates();
        // The seat is held until the task ends, however it ends.
        let (begun, _seat, question) =
            match self.start(user, principal, conversation, id, request).await {
                Ok(begun) => begun,
                Err(refused) => {
                    let _ = reply.send(Err(refused));
                    return;
                }
            };
        let delegation: Arc<dyn Delegation> = match &self.inner.runs {
            Some(runs) => match runs.start(user, principal, run_id).await {
                Ok(run) => Arc::new(run),
                Err(error) => {
                    let code = error.code();
                    let failure = format!("delegation_{}", code.as_str());
                    self.record(
                        user,
                        principal,
                        conversation,
                        id,
                        RunOutcome::Failed(&failure),
                    )
                    .await;
                    // The code only: the error's `Display` and `source()` chain reach the
                    // identity service's response.
                    match code {
                        contract::ErrorCode::Unauthenticated
                        | contract::ErrorCode::PermissionDenied => tracing::info!(
                            run_id = %id,
                            conversation_id = %conversation,
                            tenant_id = %principal.tenant_id,
                            user_id = %principal.user_id,
                            code = code.as_str(),
                            "question not started: the identity service did not issue its run \
                             delegation"
                        ),
                        _ => tracing::warn!(
                            run_id = %id,
                            conversation_id = %conversation,
                            tenant_id = %principal.tenant_id,
                            user_id = %principal.user_id,
                            code = code.as_str(),
                            "question not started: the identity service did not issue its run \
                             delegation"
                        ),
                    }
                    let _ = reply.send(Err(Refused::Delegation(error)));
                    return;
                }
            },
            None => Arc::new(UndelegatedRun::new(run_id, principal, started, lifetime)),
        };
        tracing::info!(
            run_id = %id,
            conversation_id = %conversation,
            tenant_id = %principal.tenant_id,
            user_id = %principal.user_id,
            regenerated,
            discarded_messages = begun.discarded_messages,
            "question started"
        );
        let _ = reply.send(Ok(begun.started.run));
        let ended = self
            .answer(
                user,
                begun.started.loaded,
                &question,
                delegation,
                id,
                started + lifetime,
                &cancel,
            )
            .await;
        tracing::info!(
            run_id = %id,
            conversation_id = %conversation,
            tenant_id = %principal.tenant_id,
            user_id = %principal.user_id,
            outcome = ended.as_str(),
            duration_ms = millis(started.elapsed()),
            "question finished"
        );
    }

    /// Step 1: the run's record, in a transaction of the request's tenant, with the seat that
    /// holds the memory of what the conversation holds, and the question to ask. The seat is
    /// taken only once [`store::admit_question`] (or, regenerating, [`store::admit_regeneration`],
    /// which also discards the last round in this transaction) let the question through: a
    /// request refused there, which may have waited for a connection and for the user's start
    /// lock, never held one.
    async fn start(
        &self,
        user: &UserIdentity,
        principal: Principal,
        conversation: Uuid,
        run_id: Uuid,
        request: Request,
    ) -> Result<(Begun, Seat, String), Refused> {
        let mut tx = api::begin_tx(&self.inner.db, user)
            .await
            .map_err(Refused::Store)?;
        let settings = self.inner.settings;
        let started = async {
            let (admitted, question, discarded_messages) = match request {
                Request::Question(question) => {
                    let admitted = store::admit_question(
                        &mut tx,
                        principal,
                        conversation,
                        settings.max_running_per_user,
                    )
                    .await?;
                    (admitted, question, 0)
                }
                Request::Regenerate => {
                    let regeneration = store::admit_regeneration(
                        &mut tx,
                        principal,
                        conversation,
                        settings.max_running_per_user,
                    )
                    .await?;
                    (
                        regeneration.admitted,
                        regeneration.question,
                        regeneration.discarded_messages,
                    )
                }
            };
            let seat = match self.inner.capacity.take(admitted.stored_bytes()) {
                Ok(seat) => seat,
                Err(full) => {
                    self.busy(principal, conversation, full);
                    return Err(Starting::Busy);
                }
            };
            let started =
                store::start_question(&mut tx, admitted, run_id, settings.run_lifetime).await?;
            let begun = Begun {
                started,
                discarded_messages,
            };
            Ok((begun, seat, question))
        }
        .await;
        api::finish(tx, started)
            .await
            .map_err(|failure| match failure {
                Starting::Store(failure) => Refused::Store(failure),
                Starting::Busy => Refused::Busy,
            })
    }

    /// Logs a question this replica had no seat or memory for.
    fn busy(&self, principal: Principal, conversation: Uuid, full: Full) {
        let settings = self.inner.settings;
        tracing::warn!(
            conversation_id = %conversation,
            tenant_id = %principal.tenant_id,
            user_id = %principal.user_id,
            reason = full.reason,
            running = full.running,
            max_running = settings.max_running_per_replica,
            reserved_bytes = full.reserved,
            wanted_bytes = full.wanted,
            memory_bytes = settings.memory,
            "question refused: the replica runs as many questions as it can"
        );
    }

    /// Steps 3 and 4: the question until it ends, and its record. `guard` is never later than
    /// the run's recorded deadline.
    #[allow(
        clippy::too_many_arguments,
        reason = "the parts of one question, taken apart by its task"
    )]
    async fn answer(
        &self,
        user: &UserIdentity,
        loaded: Loaded,
        question: &str,
        delegation: Arc<dyn Delegation>,
        run_id: Uuid,
        guard: Instant,
        cancel: &CancellationToken,
    ) -> Outcome {
        let Loaded {
            mut conversation,
            stored,
        } = loaded;
        let principal = conversation.principal();
        let ended = {
            // `ask` ends at the delegation's deadline by itself; the guard also holds the task
            // to the recorded deadline when the delegation's would be later.
            let asking = tokio::time::timeout_at(
                guard,
                self.inner
                    .agent
                    .ask(&mut conversation, question, delegation),
            );
            tokio::select! {
                biased;
                () = self.inner.stopping.cancelled() => Ended::Interrupted,
                () = cancel.cancelled() => Ended::Cancelled,
                () = self.stop_requested(user, principal, run_id) => Ended::Cancelled,
                asked = asking => match asked {
                    Ok(Ok(_answer)) => Ended::Answered,
                    Ok(Err(error)) => Ended::Failed(error),
                    Err(_deadline) => Ended::Failed(AgentError::RunExpired),
                },
            }
        };
        // The `ask` future is dropped: nothing of the round reaches the database but through
        // `finish_answered`.
        let outcome = match ended {
            Ended::Answered => {
                return self
                    .store_answer(user, &conversation, stored, run_id, question)
                    .await;
            }
            Ended::Failed(error) => RunOutcome::Failed(error.kind()),
            Ended::Cancelled => RunOutcome::Cancelled,
            Ended::Interrupted => RunOutcome::Interrupted,
        };
        self.record(user, principal, conversation.id(), run_id, outcome)
            .await;
        Outcome::Ended(ended)
    }

    /// The answered round and the run's end, in one transaction. When it cannot be stored, the
    /// run ends `cancelled` (its conversation was deleted) or `failed` with the store's class.
    async fn store_answer(
        &self,
        user: &UserIdentity,
        conversation: &crate::history::Conversation,
        stored: store::Stored,
        run_id: Uuid,
        question: &str,
    ) -> Outcome {
        let principal = conversation.principal();
        let title = store::provisional_title(question);
        let result = match api::begin_tx(&self.inner.db, user).await {
            Ok(mut tx) => {
                let result =
                    store::finish_answered(&mut tx, conversation, stored, run_id, title.as_deref())
                        .await;
                api::finish(tx, result).await
            }
            Err(failure) => Err(failure),
        };
        match result {
            Ok(Answered::Stored { .. }) => Outcome::Ended(Ended::Answered),
            Ok(Answered::Cancelled(_)) => Outcome::Ended(Ended::Cancelled),
            // Deleted between the check and the write (the lock makes that impossible, but the
            // rule holds either way): nothing was stored.
            Err(StoreError::NotStored) => {
                self.record(
                    user,
                    principal,
                    conversation.id(),
                    run_id,
                    RunOutcome::Cancelled,
                )
                .await;
                Outcome::Ended(Ended::Cancelled)
            }
            Err(failure) => {
                tracing::warn!(
                    run_id = %run_id,
                    conversation_id = %conversation.id(),
                    tenant_id = %principal.tenant_id,
                    error = %failure,
                    "could not store an answered question's round"
                );
                if !matches!(failure, StoreError::RunEnded) {
                    let code = format!("store_{}", failure.code());
                    self.record(
                        user,
                        principal,
                        conversation.id(),
                        run_id,
                        RunOutcome::Failed(&code),
                    )
                    .await;
                }
                Outcome::Store(failure.code())
            }
        }
    }

    /// Records how the run ended, in a short transaction of its own. When that fails (the
    /// database is down), the run stays `running` until its deadline and is read as
    /// `interrupted` after it; the failure is logged with its class.
    async fn record(
        &self,
        user: &UserIdentity,
        principal: Principal,
        conversation: Uuid,
        run_id: Uuid,
        outcome: RunOutcome<'_>,
    ) {
        let result = match api::begin_tx(&self.inner.db, user).await {
            Ok(mut tx) => {
                let result = store::finish_run(&mut tx, principal, run_id, outcome).await;
                api::finish(tx, result).await.map(|_| ())
            }
            Err(failure) => Err(failure),
        };
        if let Err(failure) = result {
            tracing::warn!(
                run_id = %run_id,
                conversation_id = %conversation,
                tenant_id = %principal.tenant_id,
                error = %failure,
                "could not record how a question ended"
            );
        }
    }

    /// Resolves when the run's record says to stop: a stop was requested, the run ended
    /// elsewhere, or its conversation is deleted (the run is no longer visible). Reads it every
    /// [`CANCEL_POLL_INTERVAL`]; a read that fails is retried at the next poll, logged once per
    /// streak of failures.
    async fn stop_requested(&self, user: &UserIdentity, principal: Principal, run_id: Uuid) {
        let mut failing = false;
        loop {
            tokio::time::sleep(poll_wait(random_fraction())).await;
            let read = match api::begin_tx(&self.inner.db, user).await {
                Ok(mut tx) => {
                    let read = store::run_status(&mut tx, principal, run_id).await;
                    api::finish(tx, read).await
                }
                Err(failure) => Err(failure),
            };
            match read {
                Ok(Some(run))
                    if run.status == RunStatus::Running && run.cancel_requested_at.is_none() =>
                {
                    failing = false;
                }
                Ok(_) => return,
                Err(failure) => {
                    if !failing {
                        tracing::warn!(
                            run_id = %run_id,
                            tenant_id = %principal.tenant_id,
                            error = %failure,
                            "could not check whether a question was stopped; trying again"
                        );
                    }
                    failing = true;
                }
            }
        }
    }
}

/// A question its run's transaction started: the run and the conversation, and how many
/// messages a regeneration discarded (0 for a new question).
struct Begun {
    started: store::StartedQuestion,
    discarded_messages: usize,
}

/// Why the run's transaction did not start the question.
enum Starting {
    Store(StoreError),
    Busy,
}

impl From<StoreError> for Starting {
    fn from(failure: StoreError) -> Self {
        Self::Store(failure)
    }
}

impl From<sqlx::Error> for Starting {
    fn from(failure: sqlx::Error) -> Self {
        Self::Store(failure.into())
    }
}

/// The wait before a poll: [`CANCEL_POLL_INTERVAL`] shortened by `fraction` (0 to 1) of
/// [`CANCEL_POLL_JITTER`], so a stop still takes effect within the interval.
fn poll_wait(fraction: f64) -> Duration {
    CANCEL_POLL_INTERVAL.saturating_sub(CANCEL_POLL_JITTER.mul_f64(fraction.clamp(0.0, 1.0)))
}

/// A random fraction from 0 to 1, or 0 when the system has no randomness to give (the poll then
/// waits the whole interval).
fn random_fraction() -> f64 {
    getrandom::u32().map_or(0.0, |value| f64::from(value) / f64::from(u32::MAX))
}

/// A question's entry in its replica's running questions ([`Questions::cancel_here`]), removed
/// when this is dropped: when the task returns, and when it unwinds from a panic.
struct Registered {
    running: Running,
    run_id: Uuid,
}

impl Drop for Registered {
    fn drop(&mut self) {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.run_id);
    }
}

/// Runs `task` in `tracker`, and `on_panic` (also tracked) when `task` panics. Shutdown's wait
/// for the tracker covers both.
fn spawn_supervised<T, P, F>(tracker: &TaskTracker, task: T, on_panic: P)
where
    T: Future<Output = ()> + Send + 'static,
    P: FnOnce() -> F + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    let handle = tracker.spawn(task);
    tracker.spawn(async move {
        match handle.await {
            Err(error) if error.is_panic() => on_panic().await,
            _ => {}
        }
    });
}

/// How a question's task ended, as its log line says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Ended(Ended),
    /// The answered round could not be stored: the store's class.
    Store(&'static str),
}

impl Outcome {
    /// `answered`, the agent's error kind, `cancelled`, `interrupted`, or `store_<class>`.
    fn as_str(self) -> String {
        match self {
            Self::Ended(Ended::Answered) => "answered".to_owned(),
            Self::Ended(Ended::Failed(error)) => error.kind().to_owned(),
            Self::Ended(Ended::Cancelled) => "cancelled".to_owned(),
            Self::Ended(Ended::Interrupted) => "interrupted".to_owned(),
            Self::Store(code) => format!("store_{code}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// Waits for the tracker's tasks, within a generous bound.
    async fn settle(tracker: &TaskTracker) {
        tracker.close();
        tokio::time::timeout(Duration::from_secs(60), tracker.wait())
            .await
            .expect("the tasks ended within 60 seconds");
    }

    #[test]
    fn a_poll_waits_the_interval_less_up_to_the_jitter() {
        assert_eq!(poll_wait(0.0), CANCEL_POLL_INTERVAL);
        assert_eq!(poll_wait(1.0), CANCEL_POLL_INTERVAL - CANCEL_POLL_JITTER);
        assert_eq!(poll_wait(2.0), CANCEL_POLL_INTERVAL - CANCEL_POLL_JITTER);
        let waits: Vec<Duration> = (0..64).map(|_| poll_wait(random_fraction())).collect();
        assert!(waits.iter().all(|wait| {
            (CANCEL_POLL_INTERVAL - CANCEL_POLL_JITTER..=CANCEL_POLL_INTERVAL).contains(wait)
        }));
        // 64 draws are all equal only if there is no randomness.
        assert!(waits.iter().any(|wait| *wait != waits[0]), "{waits:?}");
    }

    #[tokio::test]
    async fn a_panicking_task_has_its_supervisor_run_and_a_returning_one_does_not() {
        for panics in [true, false] {
            let tracker = TaskTracker::new();
            let supervised = Arc::new(AtomicBool::new(false));
            let seen = Arc::clone(&supervised);
            spawn_supervised(
                &tracker,
                async move {
                    assert!(!panics, "the task panics");
                },
                move || async move { seen.store(true, Ordering::SeqCst) },
            );
            settle(&tracker).await;
            assert_eq!(
                supervised.load(Ordering::SeqCst),
                panics,
                "panics: {panics}"
            );
        }
    }

    #[tokio::test]
    async fn a_running_question_leaves_the_replicas_list_when_its_task_panics() {
        let running = Running::default();
        let run_id = Uuid::now_v7();
        running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(run_id, CancellationToken::new());
        // The guard as `register` makes it, held by a task that panics.
        let registered = Registered {
            running: Arc::clone(&running),
            run_id,
        };
        let task = tokio::spawn(async move {
            let _registered = registered;
            panic!("the task panics");
        });
        assert!(task.await.expect_err("it panicked").is_panic());
        assert!(
            running
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
        );
    }
}
