//! A dataset run's answer body (Laysi/trustiot-v2#150; services/assistant/README.md, 「記憶體」):
//! it holds the result's JSON and the memory reserved for it ([`HeldResult`]) until its last
//! piece goes to the connection, the body is dropped, or [`HELD_BODY_TIMEOUT`] has passed since
//! the answer was made, whichever comes first. A body the connection has not taken whole by the
//! bound ends with an error, so the server aborts the response and closes the connection.
//!
//! - The JSON goes to the connection in copied pieces of at most [`HELD_BODY_CHUNK`] bytes, so
//!   what the connection buffers never keeps the result alive, and at most
//!   [`HELD_BODY_PIECES_IN_FLIGHT`] pieces wait in the connection's write buffer at a time: the
//!   body hands over the next piece only once the connection has written one to the socket (a
//!   piece's bytes are dropped then). So the connection takes the whole body only once it has
//!   written all but its last two pieces; what the reader has not read yet waits in the
//!   socket's send buffer.
//! - That keeps the connection asking the body. hyper's HTTP/1 connection stops asking a body
//!   for frames while its write buffer is full (about 400 KiB, or 16 pieces) and then waits only
//!   for the socket, which a reader that stopped reading never frees. With at most two pieces
//!   queued (a `TcpStream` writes vectored, so hyper queues the pieces as they are rather than
//!   copying them) the buffer is never full, and a connection woken with no piece written asks
//!   the body again.
//! - The bound runs on a timer of its own. At the bound the result is dropped, its memory
//!   released, and the waiting connection woken: it asks the body, which ends with an error, and
//!   the server aborts the response and drops the connection. The reader is left with an
//!   incomplete body, never a whole one.
//! - Once the connection has taken the whole body, the body is done: its last pieces (at most
//!   two) are written like any response's.
//! - The body counts on its reader dropping each piece once it has written it, as the server's
//!   connection does: a reader that keeps the pieces until the end gets two, then the error at
//!   the bound.

use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use axum::body::{Bytes, HttpBody};
use contract::TenantId;
use hyper::body::{Frame, SizeHint};
use tokio::task::AbortHandle;

use crate::dashboards::runner::HeldResult;

/// The bound on handing a run's answer body whole to the connection, from when the answer is
/// made: 60 seconds.
///
/// - A slow reader still gets the largest body: a result's JSON is at most 2 MiB
///   (`MAX_RESULT_BYTES`), and writing it in 60 s needs the reader to take about
///   2 MiB / 60 s ≈ 34.1 KiB/s (about 280 kbit/s), slower than a slow mobile link; the socket's
///   send buffer takes part of it at once besides.
/// - What a tenant whose readers stop reading can hold: it makes at most `RUNS_PER_MINUTE` (60)
///   answers in any 60 seconds, and its `CONCURRENT_RUNS` (2) runs already running may answer
///   too, so at most 62 × 2 MiB = 124 MiB of its 288 MiB share. Two such tenants hold at most
///   248 MiB of the 576 MiB budget, and the 328 MiB left still hold another tenant's whole share
///   (`answers_whose_readers_stopped_leave_another_tenant_its_share`).
pub const HELD_BODY_TIMEOUT: Duration = Duration::from_secs(60);

/// The most bytes of a run's answer body handed to the connection at a time.
pub const HELD_BODY_CHUNK: usize = 64 << 10;

/// The most pieces of a run's answer body waiting in the connection's write buffer at a time.
pub const HELD_BODY_PIECES_IN_FLIGHT: usize = 2;

/// hyper's HTTP/1 write buffer (hyper 1.x, `proto/h1/io.rs`): it stops asking a body for frames
/// once the buffer holds this many bytes or this many queued buffers.
const HYPER_WRITE_BUFFER_BYTES: usize = 8192 + 4096 * 100;
const HYPER_WRITE_BUFFER_PIECES: usize = 16;

// The pieces in flight stay well below both limits, with room for the response's head.
const _: () = assert!(
    2 * HELD_BODY_PIECES_IN_FLIGHT * HELD_BODY_CHUNK <= HYPER_WRITE_BUFFER_BYTES
        && HELD_BODY_PIECES_IN_FLIGHT < HYPER_WRITE_BUFFER_PIECES
);

/// The body of a run's answer: see the module.
pub(crate) struct HeldBody {
    shared: Arc<Mutex<Shared>>,
    /// The bytes handed to the connection so far.
    handed: usize,
    length: usize,
    /// Ends the body at the bound.
    timer: AbortHandle,
}

/// What the body, its pieces and its timer share.
struct Shared {
    /// The result until its last piece is copied out, the body is dropped, or the bound passes.
    held: Option<HeldResult>,
    /// The last piece went to the connection: the bound no longer applies.
    copied: bool,
    /// Pieces handed to the connection that it has not written yet.
    in_flight: usize,
    /// The connection, while it waits for the body to hand over a piece.
    waiting: Option<Waker>,
    /// The bound passed before the connection had taken the whole body.
    expired: bool,
    /// The body was dropped: the bound no longer applies.
    over: bool,
}

/// A copied piece of the body. Dropping it, when the connection has written it, lets the body
/// hand over the next one.
struct Piece {
    bytes: Box<[u8]>,
    shared: Arc<Mutex<Shared>>,
}

/// The connection did not take the whole body within its bound: the server aborts the response.
#[derive(Debug, thiserror::Error)]
#[error("the answer was not handed to the connection within its bound")]
pub(crate) struct HeldBodyExpired;

impl HeldBody {
    /// The body of `result`, a run of `tenant`'s, which ends after `bound` unless the connection
    /// has taken it whole by then. Call it in a Tokio runtime: the bound runs on a task of its
    /// own.
    pub(crate) fn new(result: HeldResult, tenant: TenantId, bound: Duration) -> Self {
        let length = result.json().len();
        let shared = Arc::new(Mutex::new(Shared {
            held: Some(result),
            copied: false,
            in_flight: 0,
            waiting: None,
            expired: false,
            over: false,
        }));
        let expiring = Arc::clone(&shared);
        let timer = tokio::spawn(async move {
            tokio::time::sleep(bound).await;
            let (released, waiting) = {
                let mut state = lock(&expiring);
                if state.over || state.copied {
                    return;
                }
                state.expired = true;
                (state.held.take(), state.waiting.take())
            };
            drop(released);
            tracing::info!(
                tenant_id = %tenant,
                bytes = length,
                bound_secs = bound.as_secs(),
                "a dataset run's answer was not handed to the connection within its bound: \
                 its memory is released and the answer is cut off"
            );
            if let Some(waiting) = waiting {
                waiting.wake();
            }
        })
        .abort_handle();
        Self {
            shared,
            handed: 0,
            length,
            timer,
        }
    }
}

impl HttpBody for HeldBody {
    type Data = Bytes;
    type Error = HeldBodyExpired;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, HeldBodyExpired>>> {
        let this = self.get_mut();
        if this.handed >= this.length {
            return Poll::Ready(None);
        }
        let mut state = lock(&this.shared);
        if state.expired {
            return Poll::Ready(Some(Err(HeldBodyExpired)));
        }
        if state.in_flight >= HELD_BODY_PIECES_IN_FLIGHT {
            state.waiting = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let Some(result) = state.held.as_ref() else {
            // Not reached: the result goes before its last piece only at the bound or the drop.
            return Poll::Ready(Some(Err(HeldBodyExpired)));
        };
        let end = this.handed.saturating_add(HELD_BODY_CHUNK).min(this.length);
        let bytes: Box<[u8]> = result.json()[this.handed..end].into();
        this.handed = end;
        state.in_flight += 1;
        let released = if end >= this.length {
            state.copied = true;
            state.held.take()
        } else {
            None
        };
        drop(state);
        drop(released);
        if end >= this.length {
            this.timer.abort();
        }
        let piece = Piece {
            bytes,
            shared: Arc::clone(&this.shared),
        };
        Poll::Ready(Some(Ok(Frame::data(Bytes::from_owner(piece)))))
    }

    fn is_end_stream(&self) -> bool {
        self.handed >= self.length
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(u64::try_from(self.length - self.handed).unwrap_or(u64::MAX))
    }
}

impl Drop for HeldBody {
    fn drop(&mut self) {
        self.timer.abort();
        let released = {
            let mut state = lock(&self.shared);
            state.over = true;
            state.waiting = None;
            state.held.take()
        };
        drop(released);
    }
}

impl AsRef<[u8]> for Piece {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for Piece {
    fn drop(&mut self) {
        let waiting = {
            let mut state = lock(&self.shared);
            state.in_flight = state.in_flight.saturating_sub(1);
            state.waiting.take()
        };
        if let Some(waiting) = waiting {
            waiting.wake();
        }
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::future::poll_fn;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    use contract::api::dashboard::result::MAX_RESULT_BYTES;
    use tokio::sync::Notify;
    use tokio::time::Instant;

    use super::*;
    use crate::dashboards::caps::{CONCURRENT_RUNS, RUNS_PER_MINUTE};
    use crate::dashboards::memory::{RESULT_MEMORY, RESULT_MEMORY_PER_TENANT, ResultMemory};

    /// The bound on waiting for a condition, generous for a loaded machine.
    const WAIT: Duration = Duration::from_secs(60);

    async fn held(memory: &ResultMemory, json: &[u8]) -> HeldResult {
        let reservation = memory
            .reserve(TenantId::generate(), json.len(), Duration::ZERO)
            .await
            .expect("room for the result");
        HeldResult::new(Bytes::copy_from_slice(json), reservation)
    }

    async fn next(body: &mut HeldBody) -> Option<Result<Frame<Bytes>, HeldBodyExpired>> {
        poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx)).await
    }

    fn data(frame: Option<Result<Frame<Bytes>, HeldBodyExpired>>) -> Bytes {
        frame
            .expect("a frame")
            .expect("a piece")
            .into_data()
            .unwrap_or_else(|_| panic!("data"))
    }

    /// A connection's waker: counts its wakes and lets a test wait for one.
    #[derive(Default)]
    struct Connection {
        wakes: AtomicUsize,
        woken: Notify,
    }

    impl Wake for Connection {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.wakes.fetch_add(1, Ordering::SeqCst);
            self.woken.notify_one();
        }
    }

    /// Asks `body` for a frame once, as a connection with `waker` does.
    fn ask(
        body: &mut HeldBody,
        waker: &Waker,
    ) -> Poll<Option<Result<Frame<Bytes>, HeldBodyExpired>>> {
        Pin::new(body).poll_frame(&mut Context::from_waker(waker))
    }

    #[tokio::test]
    async fn a_held_body_hands_the_whole_json_over_in_pieces_then_lets_it_go() {
        let memory = ResultMemory::new(16 << 20, 16 << 20);
        let json: Vec<u8> = (0..3 * HELD_BODY_CHUNK + 5)
            .map(|index| u8::try_from(index % 251).expect("a byte"))
            .collect();
        // The bound is far beyond the test: only handing over ends the hold here.
        let mut body = HeldBody::new(held(&memory, &json).await, TenantId::generate(), WAIT);
        assert_eq!(
            body.size_hint().exact(),
            Some(u64::try_from(json.len()).expect("a length"))
        );

        // A connection that writes each piece (drops it) before it asks for the next one.
        let mut received = Vec::new();
        let mut pieces = 0;
        while let Some(frame) = next(&mut body).await {
            let piece = data(Some(frame));
            assert!(piece.len() <= HELD_BODY_CHUNK, "{}", piece.len());
            received.extend_from_slice(&piece);
            pieces += 1;
            // The result goes as soon as its last piece is copied out.
            let expected = if received.len() < json.len() {
                json.len()
            } else {
                0
            };
            assert_eq!(memory.used(), expected, "after {} bytes", received.len());
        }
        assert_eq!(received, json, "byte for byte");
        assert_eq!(pieces, 4);
        assert!(body.is_end_stream());
    }

    #[tokio::test]
    async fn at_most_two_pieces_wait_to_be_written_and_a_written_piece_brings_the_next() {
        let memory = ResultMemory::new(16 << 20, 16 << 20);
        let json = vec![b'x'; 3 * HELD_BODY_CHUNK];
        let mut body = HeldBody::new(held(&memory, &json).await, TenantId::generate(), WAIT);
        let connection = Arc::new(Connection::default());
        let waker = Waker::from(Arc::clone(&connection));

        // Two pieces wait in the connection's buffer: the body hands over no third one.
        let first = data(ready(ask(&mut body, &waker)));
        let second = data(ready(ask(&mut body, &waker)));
        assert!(ask(&mut body, &waker).is_pending());
        assert_eq!(connection.wakes.load(Ordering::SeqCst), 0);

        // Writing one wakes the connection, and the body hands over the last piece.
        drop(first);
        assert_eq!(connection.wakes.load(Ordering::SeqCst), 1);
        let third = data(ready(ask(&mut body, &waker)));
        assert_eq!(
            memory.used(),
            0,
            "the result goes with its last piece copied out"
        );

        // With its last piece taken, the body is done, though the connection has not written
        // the last pieces yet.
        assert!(body.is_end_stream());
        assert!(matches!(ask(&mut body, &waker), Poll::Ready(None)));
        drop((second, third));
        assert_eq!(connection.wakes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn at_the_bound_the_result_goes_the_waiting_connection_wakes_and_the_body_fails() {
        let memory = ResultMemory::new(16 << 20, 16 << 20);
        let json = vec![b'x'; 4 * HELD_BODY_CHUNK];
        // A short bound, and a connection that writes nothing: the bound alone ends the hold.
        let mut body = HeldBody::new(
            held(&memory, &json).await,
            TenantId::generate(),
            Duration::from_millis(50),
        );
        let connection = Arc::new(Connection::default());
        let waker = Waker::from(Arc::clone(&connection));
        let pieces = [
            data(ready(ask(&mut body, &waker))),
            data(ready(ask(&mut body, &waker))),
        ];
        assert!(ask(&mut body, &waker).is_pending());

        tokio::time::timeout(WAIT, connection.woken.notified())
            .await
            .expect("the bound woke the connection waiting for its pieces to be written");
        assert_eq!(memory.used(), 0, "the result goes at the bound");
        assert!(matches!(
            ask(&mut body, &waker),
            Poll::Ready(Some(Err(HeldBodyExpired)))
        ));
        assert!(
            !body.is_end_stream(),
            "the body ends with an error, not whole"
        );
        // Writing the pieces afterwards changes nothing.
        drop(pieces);
        assert!(matches!(
            ask(&mut body, &waker),
            Poll::Ready(Some(Err(HeldBodyExpired)))
        ));
    }

    #[tokio::test]
    async fn a_body_never_asked_for_lets_the_result_go_at_the_bound() {
        let memory = ResultMemory::new(16 << 20, 16 << 20);
        let json = vec![b'x'; 2 * HELD_BODY_CHUNK];
        let mut body = HeldBody::new(
            held(&memory, &json).await,
            TenantId::generate(),
            Duration::from_millis(50),
        );
        let deadline = Instant::now() + WAIT;
        while memory.used() > 0 {
            assert!(
                Instant::now() < deadline,
                "the memory of a body never asked for was not released at its bound"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(matches!(next(&mut body).await, Some(Err(HeldBodyExpired))));
    }

    #[tokio::test]
    async fn a_body_dropped_unread_lets_the_result_go_at_once() {
        let memory = ResultMemory::new(16 << 20, 16 << 20);
        let json = vec![b'x'; 2 * HELD_BODY_CHUNK];
        let mut body = HeldBody::new(held(&memory, &json).await, TenantId::generate(), WAIT);
        let piece = data(next(&mut body).await);
        assert!(memory.used() > 0);
        drop(body);
        assert_eq!(memory.used(), 0);
        // A piece the connection still holds outlives the body.
        assert_eq!(piece.len(), HELD_BODY_CHUNK);
    }

    #[test]
    fn answers_whose_readers_stopped_leave_another_tenant_its_share() {
        // The answers a tenant makes within one bound: the runs it starts in any 60 seconds, and
        // the runs already running.
        let minutes = usize::try_from(HELD_BODY_TIMEOUT.as_secs().div_ceil(60)).expect("small");
        let answers = RUNS_PER_MINUTE * minutes + CONCURRENT_RUNS;
        let held = answers * MAX_RESULT_BYTES;
        assert_eq!(held, 124 << 20);
        assert!(held <= RESULT_MEMORY_PER_TENANT);
        assert!(
            2 * held + RESULT_MEMORY_PER_TENANT <= RESULT_MEMORY,
            "two tenants whose readers stopped leave a whole share for another"
        );
        // A reader of 35 KiB/s (about 290 kbit/s) gets the largest body within the bound.
        let slow_reader = 35 * 1024 * usize::try_from(HELD_BODY_TIMEOUT.as_secs()).expect("small");
        assert!(MAX_RESULT_BYTES <= slow_reader);
    }

    fn ready<T>(poll: Poll<Option<T>>) -> Option<T> {
        match poll {
            Poll::Ready(item) => item,
            Poll::Pending => panic!("the body was not ready"),
        }
    }
}
