//! A question's answer as a stream of server-sent events (Laysi/trustiot-v2#147, decision 25):
//! `GET .../conversations/{id}/runs/{run_id}/stream` holds the response open while the question
//! runs, with a heartbeat at once and then every `streams.heartbeat_interval`, sends the
//! paragraphs of the answer the model finished in the answer tool while it runs, and the rest of
//! the answer and the run's end once it ended. A client that lost the stream (Cloudflare cuts
//! one that sends nothing for 125 seconds and answers 200 all the same, a replica restarts, a
//! page is reopened) reconnects to any replica with the `Last-Event-ID` it last got and goes on
//! from there: no text twice, none missing. services/assistant/README.md, 「回答的串流」,
//! describes the contract.
//!
//! **What is sent.** While the question runs, the run's partial answer
//! ([`store::write_partial`], the user's decision of 2026-11-05): the paragraphs of the answer the
//! model finished in the answer tool (`crate::answer`), after the answer guard, which the replica
//! that runs the question writes to Postgres at most once a second. Once the run is `answered`,
//! the stored answer: the text the chat shows of the answer's message
//! (`NativeMessage::shown_text`), after the answer guard, read back from Postgres
//! ([`store::stream_run`]); it commits in one transaction with the round
//! ([`store::finish_answered`]) and starts with every partial answer, so the stream goes on into
//! it where it was. The stream never carries anything outside the answer tool or the stored
//! answer: no thinking, tool calls or their results, no text the model wrote before or between
//! tool calls, no prompt, nothing the answer guard removed. An answer the model wrote as text
//! goes out only once the question answered. A question that ended without an answer (`failed`
//! with its code, `cancelled`, `interrupted`; or a round a stop or delete cancelled after the
//! model answered) sends only its `end`: text may have gone out before, and the client drops it.
//!
//! **Resume.** A `text` event's `id` is where its text ends in the answer, in UTF-16 code units
//! (what a browser's string length counts), and its data has where it starts (`offset`). The
//! client sends the last `id` back as `Last-Event-ID` (a browser's `EventSource` does by itself)
//! and gets the text from there. A partial answer only grows, and the stored answer starts with
//! it and never changes, so an offset is valid for every replica and every later read. One past
//! the answer (or the partial answer while the question runs) or inside a character is refused
//! (400 `stream.resume_invalid`); for a question that ended without an answer, any offset gets
//! the `end`.
//!
//! **How long a stream lives.** At most until the identity token it was opened with expires (a
//! minute or so: the ingress exchanges the session cookie for a token on every request), so the
//! session and the user's role are checked again at each reconnect; until the run ends; until
//! the conversation is deleted (the stream closes and its reconnect answers 404); and until the
//! service shuts down. A stream that closes without an `end` is to be reconnected.
//!
//! **How a replica learns of the end and of new paragraphs.** When the question runs on this
//! replica, its task's end and each write of its partial answer wake the stream at once
//! ([`Questions::ended_here`], [`Questions::written_here`]), and the stream reads the database
//! only then (and at the run's deadline). When it runs elsewhere, the stream reads the run's
//! record every [`crate::questions::CANCEL_POLL_INTERVAL`] (less a random part), one row with the
//! lengths of the answer and the partial answer but not their text, like the running question's
//! own poll, and reads the text when it grew. No `LISTEN`/`NOTIFY`, for the reasons
//! [`crate::questions::CANCEL_POLL_INTERVAL`] gives.
//!
//! **Limits.** A replica holds at most `streams.max_per_replica` streams (503 `stream.busy`,
//! `Retry-After`) and one user at most `streams.max_per_user` of them (429 `stream.limited`).
//! The work of a stream runs in a task of its own that the response's body reads from a channel
//! of a few frames: it ends at the bounds above whether or not the connection still takes
//! frames, so a reader that stopped reading holds its slot no longer than the stream may live.
//! Reading the answer loads its whole message, so the stream first reserves
//! [`ANSWER_READ_FACTOR`] times its length in the replica's result memory ([`ResultMemory`])
//! and holds only the text's length once it is read: opening a stream of an answered question
//! waits [`ANSWER_MEMORY_WAIT`] for it (then 503 `stream.busy`), and a stream whose question
//! answers while it is open waits with its heartbeats until it must close. Reading a partial
//! answer that grew reserves the same way and gives the memory back once its text went out
//! (opening a stream with a position inside a partial answer reads it at once, within the same
//! wait).
//!
//! | Request | Response |
//! | --- | --- |
//! | No valid internal identity token | 401 `unauthenticated` |
//! | No tenant, or a role this build does not know | 403 `permission_denied` |
//! | Not the caller's conversation, or deleted; a run that is not of that conversation; a reader of a shared conversation | 404 `not_found` |
//! | `Last-Event-ID` that is not a number, past the answer (or the partial answer while the question runs), or inside a character | 400 `invalid_request`, key `stream.resume_invalid` |
//! | The caller holds `streams.max_per_user` streams on this replica | 429 `rate_limited`, key `stream.limited`, `{"max": <max>}` |
//! | The replica holds `streams.max_per_replica` streams, or its result memory has no room to read the answer within [`ANSWER_MEMORY_WAIT`] | 503 `unavailable`, key `stream.busy`, `Retry-After` |
//! | A lock not granted within [`super::REQUEST_LOCK_TIMEOUT`], or the database failed | 503 `unavailable` |

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::rejection::PathRejection;
use axum::extract::{FromRef, Path, State};
use axum::http::header::{CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::get;
use chrono::Utc;
use contract::{ErrorBody, ErrorCode, MessageKey, UserId};
use serde::{Deserialize, Serialize};
use service_kit::identity::{IdentityVerifier, UserIdentity};
use service_kit::tenant::TenantDb;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use utoipa::ToSchema;
use uuid::Uuid;

use super::questions::ConversationRun;
use crate::dashboards::memory::{Reservation, ResultMemory};
use crate::history::Principal;
use crate::questions::{CANCEL_POLL_INTERVAL, Questions, poll_wait, random_fraction};
use crate::store::{self, RunStatus, StoreError, StreamedRun};

/// A question's answer stream.
pub const STREAM_PATH: &str =
    "/api/assistant/v1/conversations/{conversation_id}/runs/{run_id}/stream";

/// `Last-Event-ID` is not a resume position of this run's answer (400).
pub const RESUME_INVALID: MessageKey = MessageKey::new("stream.resume_invalid");
/// The caller holds as many answer streams on this replica as one user may (429);
/// `details.max`.
pub const STREAMS_LIMITED: MessageKey = MessageKey::new("stream.limited");
/// The replica holds as many answer streams as it may (503, with `Retry-After`).
pub const STREAMS_BUSY: MessageKey = MessageKey::new("stream.busy");

/// `Retry-After` of a stream this replica had no room for: streams close within a minute or so
/// (their identity token's lifetime), and a retry may reach another replica.
pub const BUSY_RETRY_AFTER: Duration = Duration::from_secs(5);

/// The most UTF-16 code units of the answer in one `text` event.
pub const TEXT_CHUNK_UNITS: usize = 4_096;
// `AnswerStreamText::text` names the number in the API's documentation.
const _: () = assert!(TEXT_CHUNK_UNITS == 4_096);

/// The reconnect delay a stream asks of the client's `EventSource` (the `retry` field of its
/// first frame), in milliseconds: soon, since a reconnect is how a cut stream goes on.
pub const RECONNECT_MILLIS: u64 = 1_000;

/// The longest a stream lives, whatever its identity token says: the token's own longest
/// lifetime (`service_kit::identity::MAX_LIFETIME_SECS`, 5 minutes).
pub const MAX_STREAM_LIFETIME: Duration = Duration::from_secs(300);

/// The longest `Last-Event-ID` read: ten digits hold any offset of an answer (a message is at
/// most 32 MiB, `store::MAX_MESSAGE_BYTES`).
const MAX_RESUME_DIGITS: usize = 10;

/// Frames a stream's task may have waiting for the connection: a heartbeat and the text of an
/// answer go a few at a time, and a reader that stopped reading cannot make the task hold more.
const FRAMES_IN_FLIGHT: usize = 4;

/// How long after the run's recorded deadline (the database's clock) a stream whose question
/// runs on this replica reads the run anyway: the clocks of the replica and the database differ
/// by far less.
const DEADLINE_MARGIN: Duration = Duration::from_secs(1);

/// The memory reading an answer takes, per byte of its stored message: the row's bytes and their
/// copy, then the message, its parsed form and the text it shows. Reserved in the replica's
/// result memory ([`ResultMemory`]) before the read; once read, the stream holds only the text's
/// length until it closes.
pub const ANSWER_READ_FACTOR: usize = 3;

/// How long opening a stream of an answered question waits for memory to read its answer before
/// it answers 503 `stream.busy`, like the dataset runs' waits for that memory.
pub const ANSWER_MEMORY_WAIT: Duration = Duration::from_secs(10);

/// `streams.*` (crate::config::Streams).
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    /// How often a stream sends a heartbeat; the first goes out at once.
    pub heartbeat_interval: Duration,
    /// How many streams this replica holds open at once.
    pub max_per_replica: usize,
    /// How many of them one user may hold.
    pub max_per_user: usize,
}

/// This replica's answer streams: the settings and the slots. Cheap to clone.
#[derive(Clone)]
pub struct Streams {
    settings: Settings,
    slots: Arc<Mutex<Slots>>,
}

#[derive(Default)]
struct Slots {
    open: usize,
    /// Keyed by the user alone: one user's streams count together in every tenant.
    per_user: HashMap<UserId, usize>,
}

/// Why a stream did not get a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Full {
    Replica,
    User,
    /// The replica's result memory had no room to read the answer within the wait (503).
    Memory,
}

impl Streams {
    #[must_use]
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            slots: Arc::default(),
        }
    }

    #[must_use]
    pub fn settings(&self) -> Settings {
        self.settings
    }

    /// A slot for a stream of `principal`, held until the returned [`Slot`] is dropped.
    fn take(&self, principal: Principal) -> Result<Slot, Full> {
        let key = principal.user_id;
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let users = slots.per_user.get(&key).copied().unwrap_or(0);
        if users >= self.settings.max_per_user {
            return Err(Full::User);
        }
        if slots.open >= self.settings.max_per_replica {
            return Err(Full::Replica);
        }
        slots.open += 1;
        slots.per_user.insert(key, users + 1);
        Ok(Slot {
            slots: Arc::clone(&self.slots),
            key,
        })
    }
}

/// A stream's place among this replica's streams, given back when dropped.
struct Slot {
    slots: Arc<Mutex<Slots>>,
    key: UserId,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        slots.open = slots.open.saturating_sub(1);
        if let Some(count) = slots.per_user.get_mut(&self.key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                slots.per_user.remove(&self.key);
            }
        }
    }
}

#[derive(Clone)]
struct Routes {
    verifier: IdentityVerifier,
    db: TenantDb,
    questions: Questions,
    streams: Streams,
    memory: ResultMemory,
}

impl FromRef<Routes> for IdentityVerifier {
    fn from_ref(state: &Routes) -> Self {
        state.verifier.clone()
    }
}

/// The answer stream's route.
pub fn routes(
    verifier: IdentityVerifier,
    db: TenantDb,
    questions: Questions,
    streams: Streams,
    memory: ResultMemory,
) -> Router {
    Router::new()
        .route(STREAM_PATH, get(stream_answer))
        .with_state(Routes {
            verifier,
            db,
            questions,
            streams,
            memory,
        })
}

/// The data of a `text` event: the next piece of the answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AnswerStreamText {
    /// Where `text` starts in the answer, in UTF-16 code units: what the client has already
    /// shown. The event's `id` is where it ends.
    pub offset: u64,
    /// The piece of the answer: at most 4,096 UTF-16 code units, never splitting a character.
    pub text: String,
}

/// The data of a `heartbeat` event: none. One goes out as the response starts and then at the
/// configured interval (at most 60 seconds), so a client knows the stream is alive and the
/// proxies on the way do not cut it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AnswerStreamHeartbeat {}

/// Streams the answer of one of the caller's questions as server-sent events. Events, in order:
/// `heartbeat` (`AnswerStreamHeartbeat`) at once and then every `streams.heartbeat_interval`
/// (at most 60 s) while the question runs; `text` (`AnswerStreamText`, `id` the end offset)
/// with the answer from `Last-Event-ID` on: while the question runs, the paragraphs the model
/// finished in its answer tool, and once it answered, the rest; and `end` (`ConversationRun`),
/// after which the stream closes. When `end` is not `answered`, the text that went out is no
/// answer: drop it. A stream that closes without `end` (its
/// identity token expired, the conversation was deleted, the service restarted, a proxy cut it)
/// is to be reopened with the last `id` as `Last-Event-ID`; one that answers 404 is gone.
#[utoipa::path(
    get,
    path = "/api/assistant/v1/conversations/{conversation_id}/runs/{run_id}/stream",
    tag = "assistant",
    operation_id = "streamAnswer",
    params(
        ("conversation_id" = Uuid, Path, description = "The conversation's ID."),
        ("run_id" = Uuid, Path, description = "The question's run ID."),
        ("Last-Event-ID" = Option<String>, Header, description = "The `id` of the last `text` event the client got: the stream goes on from there. None (or `0`) for the answer from its start."),
    ),
    responses(
        (status = 200, description = "Server-sent events: `heartbeat`, `text` and `end` (see the operation's description). The first frame also sets `retry` (milliseconds before a reconnect).", content_type = "text/event-stream", body = String),
        (status = 400, description = "`Last-Event-ID` is not a resume position of this question's answer or, while it runs, of what went out of it (`invalid_request`, `stream.resume_invalid`).", body = ErrorBody),
        (status = 401, description = "No valid session (`unauthenticated`).", body = ErrorBody),
        (status = 403, description = "No tenant or no role in it (`permission_denied`).", body = ErrorBody),
        (status = 404, description = "No such conversation of the caller's, it is deleted, or the question is not of it (`not_found`); readers of a shared conversation get the same.", body = ErrorBody),
        (status = 429, description = "The caller holds as many answer streams on this replica as one user may (`rate_limited`, `stream.limited`, `details.max`).", body = ErrorBody),
        (status = 503, description = "The replica holds as many answer streams as it may, or had no memory to read the answer within 10 seconds (`unavailable`, `stream.busy`, with `Retry-After`); or the database failed (`unavailable`).", body = ErrorBody,
            headers(("Retry-After" = u64, description = "With `stream.busy`: seconds to wait before opening it again."))),
    )
)]
async fn stream_answer(
    State(state): State<Routes>,
    user: UserIdentity,
    ids: Result<Path<(String, String)>, PathRejection>,
    headers: HeaderMap,
) -> Response {
    const ACTION: &str = "stream an answer";
    let principal = match super::principal(&user, ACTION) {
        Ok(principal) => principal,
        Err(refused) => return refused,
    };
    let Some((conversation, run_id)) = parse_ids(ids) else {
        return super::not_found();
    };
    let Some(resume) = resume_offset(&headers) else {
        return resume_invalid();
    };
    let read = |with_text| {
        let (db, user) = (&state.db, &user);
        async move {
            let mut tx = super::begin(db, user, ACTION).await?;
            let read = store::stream_run(&mut tx, principal, conversation, run_id, with_text).await;
            match super::finish(tx, read).await {
                Ok(Some(streamed)) => Ok(streamed),
                Ok(None) => Err(super::not_found()),
                Err(failure) => Err(super::store_failure(
                    user,
                    Some(conversation),
                    ACTION,
                    &failure,
                )),
            }
        }
    };
    let mut streamed = match read(false).await {
        Ok(streamed) => streamed,
        Err(refused) => return refused,
    };
    // Only for the owner's run of a live conversation, so everyone else gets the one 404 whatever
    // this replica holds.
    let slot = match state.streams.take(principal) {
        Ok(slot) => slot,
        Err(full) => return state.refuse(principal, conversation, run_id, full),
    };
    // An answered question's answer is read now, to check the resume position against it, and so
    // is a running question's partial answer when the position is not its start: in memory
    // reserved first, waited for a little, then held only for the text.
    let mut reservation = None;
    let to_check = streamed
        .answer_bytes
        .or(streamed.partial_bytes.filter(|_| resume > 0));
    if let Some(stored) = to_check {
        let bytes = answer_memory(stored, &state.memory);
        let Some(reserved) = state
            .memory
            .reserve(principal.tenant_id, bytes, ANSWER_MEMORY_WAIT)
            .await
        else {
            return state.refuse(principal, conversation, run_id, Full::Memory);
        };
        streamed = match read(true).await {
            Ok(streamed) => streamed,
            Err(refused) => return refused,
        };
        reservation = Some(reserved);
        hold_text(&mut reservation, &streamed);
    }
    let Some(from) = resume_at(&streamed, resume) else {
        return resume_invalid();
    };
    let sent = Sent {
        units: resume,
        bytes: from,
    };
    let (sender, receiver) = mpsc::channel(FRAMES_IN_FLIGHT);
    let stream = Stream {
        db: state.db.clone(),
        principal,
        conversation,
        run_id,
        heartbeat_interval: state.streams.settings.heartbeat_interval,
        until: Instant::now() + token_lifetime(&user),
        user,
        ended_here: state.questions.ended_here(run_id),
        written_here: state.questions.written_here(run_id),
        stopping: state.questions.stopping(),
        memory: state.memory.clone(),
        reservation,
        sender,
        _slot: slot,
    };
    tokio::spawn(stream.run(streamed, sent));
    let body = Body::from_stream(futures_util::stream::unfold(
        receiver,
        |mut receiver| async move {
            receiver
                .recv()
                .await
                .map(|frame| (Ok::<Bytes, Infallible>(frame), receiver))
        },
    ));
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    response
}

impl Routes {
    /// The refusal of a stream that got no slot or no memory for its answer, logged at WARN.
    fn refuse(
        &self,
        principal: Principal,
        conversation: Uuid,
        run_id: Uuid,
        full: Full,
    ) -> Response {
        let settings = self.streams.settings;
        match full {
            Full::User => {
                tracing::warn!(
                    run_id = %run_id,
                    conversation_id = %conversation,
                    tenant_id = %principal.tenant_id,
                    user_id = %principal.user_id,
                    max = settings.max_per_user,
                    "answer stream refused: the user holds as many streams as one user may"
                );
                let max = u64::try_from(settings.max_per_user).unwrap_or(u64::MAX);
                super::error(
                    ErrorBody::new(ErrorCode::RateLimited)
                        .with_message_key(STREAMS_LIMITED)
                        .with_detail("max", max),
                )
            }
            Full::Replica => {
                tracing::warn!(
                    run_id = %run_id,
                    conversation_id = %conversation,
                    tenant_id = %principal.tenant_id,
                    user_id = %principal.user_id,
                    max = settings.max_per_replica,
                    "answer stream refused: the replica holds as many streams as it may"
                );
                busy()
            }
            Full::Memory => {
                tracing::warn!(
                    run_id = %run_id,
                    conversation_id = %conversation,
                    tenant_id = %principal.tenant_id,
                    user_id = %principal.user_id,
                    "answer stream refused: the replica's result memory had no room for the answer"
                );
                busy()
            }
        }
    }
}

/// 503 `stream.busy` with `Retry-After`.
fn busy() -> Response {
    let mut response =
        super::error(ErrorBody::new(ErrorCode::Unavailable).with_message_key(STREAMS_BUSY));
    response.headers_mut().insert(
        RETRY_AFTER,
        HeaderValue::from(BUSY_RETRY_AFTER.as_secs().max(1)),
    );
    response
}

/// The memory to reserve for reading an answer whose message is `stored` bytes: at most the
/// tenant's share, so that one fits at all.
fn answer_memory(stored: u64, memory: &ResultMemory) -> usize {
    usize::try_from(stored)
        .unwrap_or(usize::MAX)
        .saturating_mul(ANSWER_READ_FACTOR)
        .min(memory.per_tenant())
}

/// Keeps only the memory the read text (the answer's, or the partial answer's) holds.
fn hold_text(reservation: &mut Option<Reservation>, streamed: &StreamedRun) {
    if let Some(reservation) = reservation.as_mut() {
        let text = streamed.answer.as_ref().or(streamed.partial.as_ref());
        reservation.shrink_to(text.map_or(0, String::len));
    }
}

fn resume_invalid() -> Response {
    super::error(ErrorBody::new(ErrorCode::InvalidRequest).with_message_key(RESUME_INVALID))
}

/// The conversation and run IDs of the path; `None` (answered 404) unless both are UUIDs.
fn parse_ids(ids: Result<Path<(String, String)>, PathRejection>) -> Option<(Uuid, Uuid)> {
    let Path((conversation, run)) = ids.ok()?;
    Some((conversation.parse().ok()?, run.parse().ok()?))
}

/// The offset `Last-Event-ID` asks for, in UTF-16 code units: 0 without the header, `None` when
/// it is there more than once or is not 1 to [`MAX_RESUME_DIGITS`] ASCII digits.
fn resume_offset(headers: &HeaderMap) -> Option<usize> {
    let mut values = headers.get_all("last-event-id").iter();
    let Some(value) = values.next() else {
        return Some(0);
    };
    if values.next().is_some() {
        return None;
    }
    let digits = value.to_str().ok()?;
    if digits.is_empty()
        || digits.len() > MAX_RESUME_DIGITS
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    digits.parse().ok()
}

/// Where in the text (a byte index) the stream of `streamed` goes on from `offset` (UTF-16 code
/// units). An answered run's answer, or a running run's partial answer (none written yet is an
/// empty one): `None` when `offset` is past it or inside a character. A run that ended without
/// an answer to send (it failed, was stopped or interrupted; its round was discarded since) sends
/// only its `end`, whatever the offset: the client drops what it got.
fn resume_at(streamed: &StreamedRun, offset: usize) -> Option<usize> {
    match streamed.run.status {
        RunStatus::Answered => match &streamed.answer {
            Some(text) => byte_index(text, offset),
            None => Some(0),
        },
        RunStatus::Running if offset == 0 => Some(0),
        RunStatus::Running => byte_index(streamed.partial.as_deref().unwrap_or_default(), offset),
        RunStatus::Failed | RunStatus::Cancelled | RunStatus::Interrupted => Some(0),
    }
}

/// The byte index of `text` that is `units` UTF-16 code units in; `None` past the end or inside
/// a character.
fn byte_index(text: &str, units: usize) -> Option<usize> {
    let mut counted = 0;
    for (index, character) in text.char_indices() {
        if counted == units {
            return Some(index);
        }
        if counted > units {
            return None;
        }
        counted += character.len_utf16();
    }
    (counted == units).then_some(text.len())
}

/// The pieces of `text` from byte `from` (which is `offset` UTF-16 code units in): each at most
/// [`TEXT_CHUNK_UNITS`] code units, split between characters, with where it starts and ends in
/// code units.
fn pieces(text: &str, from: usize, offset: usize) -> Vec<(usize, usize, &str)> {
    let rest = text.get(from..).unwrap_or_default();
    let mut pieces = Vec::new();
    let (mut start_byte, mut start_units, mut units) = (0, offset, offset);
    for (index, character) in rest.char_indices() {
        let width = character.len_utf16();
        if units + width - start_units > TEXT_CHUNK_UNITS {
            pieces.push((
                start_units,
                units,
                rest.get(start_byte..index).unwrap_or_default(),
            ));
            start_byte = index;
            start_units = units;
        }
        units += width;
    }
    if start_byte < rest.len() {
        pieces.push((
            start_units,
            units,
            rest.get(start_byte..).unwrap_or_default(),
        ));
    }
    pieces
}

/// One server-sent event: `event`, `id` when given, and `data` as one line of JSON (JSON text
/// escapes every line break, so it is one `data` line).
fn event<T: Serialize>(name: &str, id: Option<usize>, data: &T) -> Bytes {
    let json = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_owned());
    let mut frame = format!("event: {name}\n");
    if let Some(id) = id {
        frame.push_str(&format!("id: {id}\n"));
    }
    frame.push_str("data: ");
    frame.push_str(&json);
    frame.push_str("\n\n");
    Bytes::from(frame)
}

/// The first frame: the reconnect delay and a heartbeat.
fn first_frame() -> Bytes {
    let mut frame = format!("retry: {RECONNECT_MILLIS}\n").into_bytes();
    frame.extend_from_slice(&heartbeat_frame());
    Bytes::from(frame)
}

/// A `heartbeat` event.
fn heartbeat_frame() -> Bytes {
    event("heartbeat", None, &AnswerStreamHeartbeat {})
}

/// How long the stream may live for the token it was opened with: until the token's `exp`, at
/// most [`MAX_STREAM_LIFETIME`].
fn token_lifetime(user: &UserIdentity) -> Duration {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let exp = u64::try_from(user.claims().exp).unwrap_or(0);
    Duration::from_secs(exp.saturating_sub(now)).min(MAX_STREAM_LIFETIME)
}

/// Why a stream ended, for its log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Closed {
    /// The run ended and its `end` went out.
    Ended,
    /// The run is no longer the caller's to see: its conversation was deleted.
    Gone,
    /// The identity token the stream was opened with expired.
    Expired,
    /// The service is shutting down.
    ShuttingDown,
    /// The client went away.
    Left,
    /// The replica's result memory had no room to read the answer before the stream's end.
    Busy,
    /// Sending the answer, it could not read whether the conversation is still there.
    Unchecked,
    /// The stored answer does not go on from what the stream sent of its partial answer: a fault
    /// (the agent stores no answer that does not start with what went out), logged. The stream
    /// closes without the rest.
    Mismatch,
}

impl Closed {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ended => "ended",
            Self::Gone => "gone",
            Self::Expired => "expired",
            Self::ShuttingDown => "shutting_down",
            Self::Left => "left",
            Self::Busy => "busy",
            Self::Unchecked => "unchecked",
            Self::Mismatch => "mismatch",
        }
    }
}

/// How much of the text the stream has sent (or the client had, from its `Last-Event-ID`): the
/// same position in the partial answer and in the answer, which starts with every partial one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sent {
    /// In UTF-16 code units: the last `id`.
    units: usize,
    /// In bytes of the text.
    bytes: usize,
}

/// One open stream's task.
struct Stream {
    db: TenantDb,
    user: UserIdentity,
    principal: Principal,
    conversation: Uuid,
    run_id: Uuid,
    heartbeat_interval: Duration,
    /// When the stream closes at the latest ([`token_lifetime`]).
    until: Instant,
    /// The question's end, when it runs on this replica.
    ended_here: Option<CancellationToken>,
    /// The writes of its partial answer, when it runs on this replica.
    written_here: Option<watch::Receiver<u64>>,
    stopping: CancellationToken,
    /// The replica's result memory, which reading the answer reserves in.
    memory: ResultMemory,
    /// The memory the answer's text holds, once it is read.
    reservation: Option<Reservation>,
    sender: mpsc::Sender<Bytes>,
    _slot: Slot,
}

impl Stream {
    /// Sends the first heartbeat, then, while the run runs, its partial answer from `sent` on as
    /// it grows, with a heartbeat every interval; once it ended, the rest of the answer and the
    /// end.
    async fn run(mut self, mut streamed: StreamedRun, sent: Sent) {
        let started = Instant::now();
        let closed = self.serve(&mut streamed, sent).await;
        tracing::debug!(
            run_id = %self.run_id,
            conversation_id = %self.conversation,
            tenant_id = %self.principal.tenant_id,
            user_id = %self.principal.user_id,
            closed = closed.as_str(),
            duration_ms = crate::tools::millis(started.elapsed()),
            "answer stream closed"
        );
    }

    async fn serve(&mut self, streamed: &mut StreamedRun, mut sent: Sent) -> Closed {
        if let Err(closed) = self.send(first_frame()).await {
            return closed;
        }
        let mut checked = Instant::now();
        let mut heartbeat = tokio::time::interval_at(
            Instant::now() + self.heartbeat_interval,
            self.heartbeat_interval,
        );
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut failing = false;
        // Set after each read, not at each heartbeat: a heartbeat shorter than a poll's wait
        // must not put the next read off.
        let mut next_read = self.next_read(streamed, failing);
        loop {
            let ended = streamed.run.status != RunStatus::Running;
            if !ended && let Some(partial) = streamed.partial.take() {
                // What the partial answer added since the last time, then its memory back.
                let sending = self.send_text(&partial, sent, &mut checked).await;
                self.reservation = None;
                match sending {
                    Ok(now) => sent = now,
                    Err(closed) => return closed,
                }
            }
            // Its answer is stored but not read yet, or its partial answer grew: read it in
            // memory reserved first.
            let unread_answer =
                ended && streamed.answer.is_none() && streamed.answer_bytes.is_some();
            let unread_partial = !ended
                && streamed
                    .partial_bytes
                    .is_some_and(|bytes| bytes > u64::try_from(sent.bytes).unwrap_or(u64::MAX));
            if ended && !unread_answer {
                return self.finish(streamed, sent, &mut checked).await;
            }
            let unread = unread_answer || unread_partial;
            if !unread || failing {
                let ended_here = self.ended_here.clone();
                tokio::select! {
                    biased;
                    () = self.stopping.cancelled() => return Closed::ShuttingDown,
                    () = self.sender.closed() => return Closed::Left,
                    () = tokio::time::sleep_until(self.until) => return Closed::Expired,
                    () = wait(ended_here.as_ref()) => self.ended_here = None,
                    written = changed(self.written_here.as_mut()) => {
                        // The question's task let its entry go: its end wakes the stream.
                        if !written {
                            self.written_here = None;
                        }
                    }
                    () = tokio::time::sleep_until(next_read) => {}
                    _ = heartbeat.tick() => {
                        if let Err(closed) = self.send(heartbeat_frame()).await {
                            return closed;
                        }
                        continue;
                    }
                }
            }
            let bytes = if unread_answer {
                streamed.answer_bytes
            } else {
                streamed.partial_bytes
            };
            if let Some(stored) = bytes.filter(|_| unread)
                && self.reservation.is_none()
            {
                match self.reserve(stored, &mut heartbeat).await {
                    Ok(reserved) => self.reservation = Some(reserved),
                    Err(closed) => return closed,
                }
            }
            match self.read(unread).await {
                Ok(Some(read)) => {
                    failing = false;
                    *streamed = read;
                    hold_text(&mut self.reservation, streamed);
                    checked = Instant::now();
                }
                Ok(None) => return Closed::Gone,
                Err(failure) => {
                    if !failing {
                        tracing::warn!(
                            run_id = %self.run_id,
                            tenant_id = %self.principal.tenant_id,
                            error = %failure,
                            "an answer stream could not read its question; trying again"
                        );
                    }
                    failing = true;
                }
            }
            next_read = self.next_read(streamed, failing);
        }
    }

    /// Memory to read an answer whose message is `stored` bytes, waited for with a heartbeat every
    /// interval, until the stream must close.
    async fn reserve(
        &self,
        stored: u64,
        heartbeat: &mut tokio::time::Interval,
    ) -> Result<Reservation, Closed> {
        let bytes = answer_memory(stored, &self.memory);
        let wait = self.until.saturating_duration_since(Instant::now());
        let reserve = self.memory.reserve(self.principal.tenant_id, bytes, wait);
        let mut reserve = std::pin::pin!(reserve);
        loop {
            tokio::select! {
                biased;
                () = self.stopping.cancelled() => return Err(Closed::ShuttingDown),
                () = self.sender.closed() => return Err(Closed::Left),
                () = tokio::time::sleep_until(self.until) => return Err(Closed::Expired),
                reserved = &mut reserve => return reserved.ok_or(Closed::Busy),
                _ = heartbeat.tick() => self.send(heartbeat_frame()).await?,
            }
        }
    }

    /// When the stream reads the run next: while its question runs on this replica, only at its
    /// deadline (its task's end wakes the stream before); otherwise, or after a failed read, after
    /// a poll's wait.
    fn next_read(&self, streamed: &StreamedRun, failing: bool) -> Instant {
        if self.ended_here.is_some() && !failing {
            let left = (streamed.run.deadline_at - Utc::now())
                .to_std()
                .unwrap_or_default();
            Instant::now() + left + DEADLINE_MARGIN
        } else {
            Instant::now() + poll_wait(random_fraction())
        }
    }

    /// The rest of the answer from `sent` on, then the end. A run without an answer to send
    /// sends only the end.
    async fn finish(&self, streamed: &StreamedRun, sent: Sent, checked: &mut Instant) -> Closed {
        if let Some(text) = streamed
            .answer
            .as_deref()
            .filter(|_| streamed.run.status == RunStatus::Answered)
            && let Err(closed) = self.send_text(text, sent, checked).await
        {
            return closed;
        }
        if let Err(closed) = self.still_there(checked).await {
            return closed;
        }
        let end = event("end", None, &ConversationRun::from(&streamed.run));
        match self.send(end).await {
            Ok(()) => Closed::Ended,
            Err(closed) => closed,
        }
    }

    /// The `text` events of `text` (the partial answer or the answer) after `sent`; returns
    /// what has been sent then.
    ///
    /// The conversation may be deleted while the text goes out to a slow reader: every
    /// [`CANCEL_POLL_INTERVAL`] of sending, the stream reads the run again (one row, without the
    /// text) and closes without the rest once it is gone, or when it cannot tell.
    async fn send_text(
        &self,
        text: &str,
        sent: Sent,
        checked: &mut Instant,
    ) -> Result<Sent, Closed> {
        // The answer and every partial answer start with what went out: anything else is a fault.
        if !text.is_char_boundary(sent.bytes)
            || text
                .get(..sent.bytes)
                .map(|done| done.encode_utf16().count())
                != Some(sent.units)
        {
            tracing::error!(
                run_id = %self.run_id,
                conversation_id = %self.conversation,
                tenant_id = %self.principal.tenant_id,
                sent_units = sent.units,
                text_bytes = text.len(),
                "an answer stream's text does not go on from what it sent"
            );
            return Err(Closed::Mismatch);
        }
        let mut now = sent;
        for (start, end, piece) in pieces(text, sent.bytes, sent.units) {
            self.still_there(checked).await?;
            let data = AnswerStreamText {
                offset: u64::try_from(start).unwrap_or(u64::MAX),
                text: piece.to_owned(),
            };
            self.send(event("text", Some(end), &data)).await?;
            now = Sent {
                units: end,
                bytes: now.bytes + piece.len(),
            };
        }
        Ok(now)
    }

    /// Whether the run is still the caller's to see, read again once [`CANCEL_POLL_INTERVAL`]
    /// passed since `checked`.
    async fn still_there(&self, checked: &mut Instant) -> Result<(), Closed> {
        if checked.elapsed() < CANCEL_POLL_INTERVAL {
            return Ok(());
        }
        let read = self.read(false).await;
        *checked = Instant::now();
        match read {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(Closed::Gone),
            Err(failure) => {
                tracing::warn!(
                    run_id = %self.run_id,
                    tenant_id = %self.principal.tenant_id,
                    error = %failure,
                    "an answer stream could not read its question; closing it"
                );
                Err(Closed::Unchecked)
            }
        }
    }

    /// Hands `frame` to the response's body, unless the stream must close first.
    async fn send(&self, frame: Bytes) -> Result<(), Closed> {
        tokio::select! {
            biased;
            () = self.stopping.cancelled() => Err(Closed::ShuttingDown),
            () = tokio::time::sleep_until(self.until) => Err(Closed::Expired),
            sent = self.sender.send(frame) => sent.map_err(|_| Closed::Left),
        }
    }

    /// The run as it stands, with its answer's or partial answer's text when `with_text`, in a
    /// short transaction of the caller's tenant.
    async fn read(&self, with_text: bool) -> Result<Option<StreamedRun>, StoreError> {
        let mut tx = super::begin_tx(&self.db, &self.user).await?;
        let read = store::stream_run(
            &mut tx,
            self.principal,
            self.conversation,
            self.run_id,
            with_text,
        )
        .await;
        super::finish(tx, read).await
    }
}

/// Resolves when `receiver` changes (`true`) or its sender is gone (`false`); never without
/// one.
async fn changed(receiver: Option<&mut watch::Receiver<u64>>) -> bool {
    match receiver {
        Some(receiver) => receiver.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

/// Resolves when `token` is cancelled; never without one.
async fn wait(token: Option<&CancellationToken>) {
    match token {
        Some(token) => token.cancelled().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;

    use contract::TenantId;

    use super::*;
    use crate::store::RunRecord;

    fn streamed(status: RunStatus, answer: Option<&str>) -> StreamedRun {
        let at = DateTime::<Utc>::UNIX_EPOCH;
        StreamedRun {
            run: RunRecord {
                run_id: Uuid::nil(),
                conversation_id: Uuid::nil(),
                status,
                result_code: None,
                started_at: at,
                deadline_at: at,
                ended_at: None,
                cancel_requested_at: None,
            },
            answer_bytes: answer.map(|text| u64::try_from(text.len()).expect("a length")),
            answer: answer.map(str::to_owned),
            partial_bytes: None,
            partial: None,
        }
    }

    fn running(partial: Option<&str>) -> StreamedRun {
        StreamedRun {
            partial_bytes: partial.map(|text| u64::try_from(text.len()).expect("a length")),
            partial: partial.map(str::to_owned),
            ..streamed(RunStatus::Running, None)
        }
    }

    fn headers(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(
                "last-event-id",
                HeaderValue::from_str(value).expect("a header value"),
            );
        }
        headers
    }

    #[test]
    fn a_resume_position_is_a_number_of_up_to_ten_digits() {
        assert_eq!(resume_offset(&HeaderMap::new()), Some(0));
        assert_eq!(resume_offset(&headers(&["0"])), Some(0));
        assert_eq!(resume_offset(&headers(&["4096"])), Some(4096));
        assert_eq!(
            resume_offset(&headers(&["9999999999"])),
            Some(9_999_999_999)
        );
        for value in [
            "",
            " 1",
            "1 ",
            "-1",
            "+1",
            "1.0",
            "0x10",
            "10000000000",
            "abc",
        ] {
            assert_eq!(resume_offset(&headers(&[value])), None, "{value:?}");
        }
        assert_eq!(resume_offset(&headers(&["1", "1"])), None, "twice");
    }

    /// Offsets count UTF-16 code units, as a browser's string length does: a character outside
    /// the Basic Multilingual Plane is two, and no offset falls inside one.
    #[test]
    fn an_offset_resumes_only_between_characters_of_the_answer() {
        let text = "a用😀b";
        assert_eq!(byte_index(text, 0), Some(0));
        assert_eq!(byte_index(text, 1), Some(1));
        assert_eq!(byte_index(text, 2), Some(4));
        assert_eq!(
            byte_index(text, 3),
            None,
            "inside the emoji's surrogate pair"
        );
        assert_eq!(byte_index(text, 4), Some(8));
        assert_eq!(byte_index(text, 5), Some(9), "the end");
        assert_eq!(byte_index(text, 6), None, "past the end");
        assert_eq!(byte_index("", 0), Some(0));
        assert_eq!(byte_index("", 1), None);

        let answered = streamed(RunStatus::Answered, Some(text));
        assert_eq!(resume_at(&answered, 4), Some(8));
        assert_eq!(resume_at(&answered, 6), None);
        // While the question runs, positions are in its partial answer, which may not exist yet.
        assert_eq!(resume_at(&running(None), 0), Some(0));
        assert_eq!(resume_at(&running(None), 1), None, "no text went out yet");
        assert_eq!(resume_at(&running(Some(text)), 4), Some(8));
        assert_eq!(
            resume_at(&running(Some(text)), 3),
            None,
            "inside a character"
        );
        assert_eq!(
            resume_at(&running(Some(text)), 6),
            None,
            "past the partial answer"
        );
        // A question that ended without an answer sends only its end, wherever the client was:
        // text may have gone out before it failed, and the client drops it.
        for status in [
            RunStatus::Failed,
            RunStatus::Cancelled,
            RunStatus::Interrupted,
        ] {
            let run = streamed(status, None);
            assert_eq!(resume_at(&run, 0), Some(0), "{status:?}");
            assert_eq!(resume_at(&run, 7), Some(0), "{status:?}: only the end");
        }
        // Answered before its answer's position was recorded, or a discarded round: no text.
        assert_eq!(resume_at(&streamed(RunStatus::Answered, None), 0), Some(0));
        assert_eq!(resume_at(&streamed(RunStatus::Answered, None), 3), Some(0));
    }

    /// The pieces cover the rest of the answer exactly once, in order, each within the chunk
    /// size, never splitting a character, with offsets that chain.
    #[test]
    fn the_pieces_of_an_answer_chain_without_gaps_or_overlaps() {
        let text = format!(
            "{}{}{}",
            "x".repeat(TEXT_CHUNK_UNITS - 1),
            "😀",
            "用".repeat(TEXT_CHUNK_UNITS + 3)
        );
        let total = text.encode_utf16().count();
        for offset in [0, 1, TEXT_CHUNK_UNITS - 1, TEXT_CHUNK_UNITS + 1, total] {
            let from = byte_index(&text, offset).expect("a position between characters");
            let pieces = pieces(&text, from, offset);
            let mut at = offset;
            let mut joined = String::new();
            for (start, end, piece) in &pieces {
                assert_eq!(*start, at, "each piece starts where the last ended");
                assert_eq!(end - start, piece.encode_utf16().count());
                assert!(end - start <= TEXT_CHUNK_UNITS, "within the chunk size");
                assert!(!piece.is_empty());
                joined.push_str(piece);
                at = *end;
            }
            assert_eq!(at, total, "from {offset}");
            assert_eq!(Some(joined.as_str()), text.get(from..), "from {offset}");
        }
        assert!(pieces("", 0, 0).is_empty());
    }

    #[test]
    fn an_event_is_one_data_line_of_json() {
        let frame = event(
            "text",
            Some(12),
            &AnswerStreamText {
                offset: 0,
                text: "two\nlines\r\nand\u{2028}more".to_owned(),
            },
        );
        let frame = std::str::from_utf8(&frame).expect("UTF-8");
        assert_eq!(
            frame,
            "event: text\nid: 12\ndata: {\"offset\":0,\"text\":\"two\\nlines\\r\\nand\u{2028}more\"}\n\n"
        );
        let first = first_frame();
        assert_eq!(
            std::str::from_utf8(&first).expect("UTF-8"),
            "retry: 1000\nevent: heartbeat\ndata: {}\n\n"
        );
    }

    #[tokio::test]
    async fn reading_an_answer_reserves_a_multiple_of_it_then_holds_its_text() {
        let memory = ResultMemory::new(1_000, 600);
        assert_eq!(answer_memory(100, &memory), 100 * ANSWER_READ_FACTOR);
        // An answer larger than the tenant's share takes all of it, so it is read at all.
        assert_eq!(answer_memory(10_000, &memory), 600);
        assert_eq!(answer_memory(u64::MAX, &memory), 600);

        let tenant = TenantId::generate();
        let mut reservation = memory
            .reserve(tenant, answer_memory(100, &memory), Duration::ZERO)
            .await;
        assert_eq!(memory.used(), 300);
        let text = "Floor 3 used 412 kWh, 8 % more than last week.";
        hold_text(&mut reservation, &streamed(RunStatus::Answered, Some(text)));
        assert_eq!(memory.used(), text.len());
        // A partial answer holds its own length.
        let partial = "Floor 3 used 412 kWh.";
        hold_text(&mut reservation, &running(Some(partial)));
        assert_eq!(memory.used(), partial.len());
        // A round discarded between the reads: nothing to hold.
        hold_text(&mut reservation, &streamed(RunStatus::Answered, None));
        assert_eq!(memory.used(), 0);
        drop(reservation);
        assert_eq!(memory.used(), 0);
    }

    #[test]
    fn a_user_and_a_replica_hold_at_most_their_streams() {
        let streams = Streams::new(Settings {
            heartbeat_interval: Duration::from_secs(20),
            max_per_replica: 3,
            max_per_user: 2,
        });
        let tenant = TenantId::generate();
        let (alice, bob, carol) = (
            Principal {
                tenant_id: tenant,
                user_id: UserId::generate(),
            },
            Principal {
                tenant_id: tenant,
                user_id: UserId::generate(),
            },
            Principal {
                tenant_id: TenantId::generate(),
                user_id: UserId::generate(),
            },
        );
        let first = streams.take(alice).expect("a slot");
        let second = streams.take(alice).expect("a second slot");
        assert_eq!(streams.take(alice).err(), Some(Full::User));
        let alice_elsewhere = Principal {
            tenant_id: TenantId::generate(),
            ..alice
        };
        assert_eq!(
            streams.take(alice_elsewhere).err(),
            Some(Full::User),
            "a user's streams count together in every tenant"
        );
        let bobs = streams.take(bob).expect("another user's slot");
        assert_eq!(streams.take(carol).err(), Some(Full::Replica));
        drop(first);
        let again = streams.take(alice).expect("the slot given back");
        assert_eq!(streams.take(carol).err(), Some(Full::Replica), "full again");
        let slots = streams.slots.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(slots.open, 3);
        drop(slots);
        drop((again, second, bobs));
        let slots = streams.slots.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(slots.open, 0);
        assert!(slots.per_user.is_empty(), "no user is left counted");
    }
}
