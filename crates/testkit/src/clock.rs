//! Time control for tests that run real sockets (the fake Modbus endpoint, [`crate::modbus`]).
//!
//! - [`manual`] keeps a paused tokio clock still until the test calls
//!   `tokio::time::advance`, so timeouts fire exactly when the test says and never while a
//!   socket is waiting for bytes.
//! - [`bounded`] waits for a condition with a wall-clock limit ([`WAIT_LIMIT`]), which works
//!   whether or not the tokio clock is paused.
//!
//! Why: with `#[tokio::test(start_paused = true)]` alone, tokio auto-advances the paused clock
//! to the next timer whenever the runtime has nothing to run. A timer of the code under test (a
//! read timeout) can then fire while the bytes it waits for are still on their way through a
//! socket. The crate README, section 「時間控制」, has the measurements and the recommended
//! pattern.

use std::fmt::Display;
use std::future::Future;
use std::sync::mpsc;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// How long [`bounded`] waits by default. Generous on purpose: CI runners have been seen at a
/// load average of 25–35 on 8 cores, and a wait that ends early only turns a slow run into a
/// false failure. A condition that is met returns at once.
pub const WAIT_LIMIT: Duration = Duration::from_secs(60);

/// Keeps a paused tokio clock from auto-advancing while it lives; see [`manual`].
#[derive(Debug)]
#[must_use = "the clock auto-advances again as soon as the guard is dropped"]
pub struct ManualClock {
    stop: Option<mpsc::Sender<()>>,
    // Dropping a `JoinHandle` detaches the task; the task ends once `stop` is dropped.
    _blocking: JoinHandle<()>,
}

/// Stops a paused clock from auto-advancing until the guard is dropped: time then moves only
/// when the test calls `tokio::time::advance`.
///
/// Use it in a `#[tokio::test(start_paused = true)]` test (tokio's `test-util` feature, a
/// current-thread runtime). It relies on documented tokio behavior: while a
/// [`tokio::task::spawn_blocking`] task runs, the paused clock does not auto-advance (see
/// `tokio::time::pause`, "Preventing auto-advance"); the guard keeps one such task waiting
/// until it is dropped. On a clock that is not paused it does nothing but hold one blocking
/// thread.
///
/// # Panics
///
/// Outside a tokio runtime.
pub fn manual() -> ManualClock {
    let (stop, stopped) = mpsc::channel::<()>();
    let blocking = tokio::task::spawn_blocking(move || {
        // Returns when the guard drops the sender.
        let _ = stopped.recv();
    });
    ManualClock {
        stop: Some(stop),
        _blocking: blocking,
    }
}

impl Drop for ManualClock {
    fn drop(&mut self) {
        self.stop.take();
    }
}

/// Awaits `future`, failing the test when it has not finished after [`WAIT_LIMIT`] of wall-clock
/// time; see [`bounded_by`].
///
/// # Panics
///
/// When `future` does not finish within [`WAIT_LIMIT`]; the message names `what`.
pub async fn bounded<F: Future>(what: impl Display, future: F) -> F::Output {
    bounded_by(WAIT_LIMIT, what, future).await
}

/// Awaits `future`, failing the test when it has not finished after `limit` of wall-clock time.
///
/// The limit is measured by a watchdog thread, not by a tokio timer: under a paused clock a
/// tokio timer either never fires ([`manual`]) or fires as soon as the runtime is idle
/// (auto-advance), and neither is a wait.
///
/// # Panics
///
/// When `future` does not finish within `limit`; the message names `what`.
pub async fn bounded_by<F: Future>(limit: Duration, what: impl Display, future: F) -> F::Output {
    let (cancel, cancelled) = mpsc::channel::<()>();
    let (expire, expired) = oneshot::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if cancelled.recv_timeout(limit) == Err(mpsc::RecvTimeoutError::Timeout) {
            let _ = expire.send(());
        }
    });
    let outcome = tokio::select! {
        biased;
        output = future => Some(output),
        _ = expired => None,
    };
    drop(cancel);
    let _ = watchdog.join();
    match outcome {
        Some(output) => output,
        None => panic!("timed out after {limit:?} waiting for {what}"),
    }
}
