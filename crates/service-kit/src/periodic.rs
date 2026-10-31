//! Periodic work that every replica runs on its own and that needs no lock, such as reading a
//! mounted file again (README.md, 「每副本的週期性工作（`periodic`）」).
//!
//! A run that panics does not stop the schedule: the panic is caught, one ERROR line names the
//! job (never the panic payload, which may carry data), and the job runs again next period, as
//! [`JobLock::run_periodically`](crate::job_lock::JobLock::run_periodically) does for the jobs
//! one replica runs at a time. Without this, a panic would end the task that `Context::spawn`
//! runs and nothing else: the process stays up and ready, and the work silently stops until the
//! pod restarts. A binary built with `panic = "abort"` still exits on a panic.
//!
//! State the job shares with request handlers must stay usable after a panic: readers of a
//! `std::sync` lock the job writes take the value of a poisoned lock
//! (`unwrap_or_else(PoisonError::into_inner)`) rather than panic, and the job updates what it
//! keeps so that the next run repairs a run cut off half-way (the README has the rule).

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use tokio::time::MissedTickBehavior;

use crate::shutdown::Shutdown;

/// Runs `job` every `period` until `shutdown` begins stopping; the first run comes one period
/// after the call (the caller usually runs the job once itself before it starts serving). A run
/// that is still going when shutdown begins finishes unless the service's drain timeout cuts it
/// off. Spawn the returned future with `Context::spawn`.
///
/// The future `job` returns owns what it uses, as for
/// [`JobLock::run_periodically`](crate::job_lock::JobLock::run_periodically): state kept between
/// runs lives behind an `Arc`, and after a panic the next run finds it as the panic left it.
///
/// # Panics
///
/// If `period` is zero.
pub async fn run_every<F, Fut>(job: &'static str, period: Duration, shutdown: Shutdown, mut run: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    every(job, period, shutdown, || {
        // A closure that panics while it builds its future is caught like one whose future
        // panics. The payload is dropped unread, as `catch_panic` does.
        let built = catch_unwind(AssertUnwindSafe(&mut run));
        async move {
            match built {
                Ok(future) => match catch_panic(future).await {
                    Ok(()) => Ran::Finished,
                    Err(Panicked) => Ran::Panicked,
                },
                Err(payload) => {
                    drop(payload);
                    Ran::Panicked
                }
            }
        }
    })
    .await;
}

/// Runs `job` on a blocking thread (`tokio::task::spawn_blocking`) every `period` until
/// `shutdown` begins stopping, like [`run_every`], for jobs that do synchronous file or other
/// blocking I/O and so must not run on an async worker. A panic on the blocking thread is caught
/// and logged like one in [`run_every`]; it is never resumed on the async side.
///
/// # Panics
///
/// If `period` is zero.
pub async fn run_blocking_every<F>(job: &'static str, period: Duration, shutdown: Shutdown, run: F)
where
    F: Fn() + Send + Sync + 'static,
{
    let run = Arc::new(run);
    every(job, period, shutdown, || {
        let run = Arc::clone(&run);
        async move {
            match tokio::task::spawn_blocking(move || run()).await {
                Ok(()) => Ran::Finished,
                // The payload is dropped unread, as `catch_panic` does.
                Err(error) if error.is_panic() => {
                    drop(error.into_panic());
                    Ran::Panicked
                }
                // The runtime is shutting down and dropped the task before it ran.
                Err(_) => Ran::Cancelled,
            }
        }
    })
    .await;
}

/// How one run ended.
enum Ran {
    Finished,
    Panicked,
    /// The run cannot happen because the runtime is shutting down.
    Cancelled,
}

/// The schedule both public functions share.
async fn every<F, Fut>(job: &'static str, period: Duration, shutdown: Shutdown, mut run: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Ran>,
{
    let mut ticks = tokio::time::interval(period);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // The first tick completes at once; the first run comes one period later.
    ticks.tick().await;
    loop {
        tokio::select! {
            biased;
            () = shutdown.stopping() => break,
            _ = ticks.tick() => {}
        }
        match run().await {
            Ran::Finished => {}
            // The panic hook has already logged the panic with its location; this line carries
            // nothing a job could put in a panic message.
            Ran::Panicked => tracing::error!(
                job = %job,
                "periodic job panicked; it runs again next period"
            ),
            Ran::Cancelled => break,
        }
    }
}

/// A job panicked. The panic payload is deliberately not kept.
pub(crate) struct Panicked;

/// Awaits `future`, turning a panic while it is polled into `Err(Panicked)`.
pub(crate) async fn catch_panic<Fut: Future>(future: Fut) -> Result<Fut::Output, Panicked> {
    let mut future = Box::pin(future);
    std::future::poll_fn(|context| {
        match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(context))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Err(payload) => {
                drop(payload);
                Poll::Ready(Err(Panicked))
            }
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use tracing_subscriber::layer::SubscriberExt as _;

    use super::*;

    /// What a panicking job must not leak: the panic message, which can carry data.
    const SECRET: &str = "secret-customer-data-4711";

    /// The period of the jobs here: short, so a test takes moments. No test asserts on it.
    const PERIOD: Duration = Duration::from_millis(20);

    /// Upper bound for waiting on something that must happen but not at a fixed moment. Only a
    /// failure ever waits this long; no test asserts on how long a wait lasted.
    const EVENT_WAIT: Duration = Duration::from_secs(30);

    /// Held by each test here while it captures logs, so that no other thread reaches the
    /// schedule's log statement meanwhile (see `scheduler_alone` in tests/job_lock.rs for why).
    static ALONE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Collects the events logged on the test's thread: level and `name=value` text.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<(tracing::Level, String)>>>);

    impl Captured {
        fn events(&self) -> Vec<(tracing::Level, String)> {
            self.0.lock().expect("captured events").clone()
        }
    }

    struct EventText<'a>(&'a mut String);

    impl tracing::field::Visit for EventText<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;
            let _ = write!(self.0, "{}={value:?} ", field.name());
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Captured {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut text = String::new();
            event.record(&mut EventText(&mut text));
            self.0
                .lock()
                .expect("captured events")
                .push((*event.metadata().level(), text));
        }
    }

    /// Captures this thread's log events until the guard drops. The test runs on a current-thread
    /// runtime, so the schedule logs on this thread.
    fn capture_logs() -> (Captured, tracing::subscriber::DefaultGuard) {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        (captured, tracing::subscriber::set_default(subscriber))
    }

    /// Waits until `counter` reaches `at_least`; `what` names what is awaited, for the failure.
    async fn wait_for(counter: &AtomicUsize, at_least: usize, what: &str) {
        let started = Instant::now();
        while counter.load(Ordering::SeqCst) < at_least {
            assert!(
                started.elapsed() < EVENT_WAIT,
                "waited {EVENT_WAIT:?} for {what}: the counter is at {} of {at_least}",
                counter.load(Ordering::SeqCst)
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Runs `schedule` (a job named `job` that panics on its first call and counts its calls in
    /// `calls`) until the third call, stops it, and checks the outcome: the schedule went on
    /// after the panic, stopped at shutdown, and logged one ERROR naming the job and not the
    /// payload.
    async fn assert_a_panic_keeps_the_schedule<Fut>(
        job: &str,
        calls: &AtomicUsize,
        shutdown: &Shutdown,
        schedule: Fut,
    ) where
        Fut: Future<Output = ()> + Send + 'static,
    {
        let _alone = ALONE.lock().await;
        let (captured, _logs) = capture_logs();
        let runner = tokio::spawn(schedule);
        wait_for(calls, 3, "the job to be called again after it panicked").await;
        assert!(!runner.is_finished(), "a panicking job ended its schedule");
        shutdown.stop();
        tokio::time::timeout(EVENT_WAIT, runner)
            .await
            .expect("the schedule stops when the service stops")
            .expect("the schedule's task did not panic");

        let events = captured.events();
        let panics = events
            .iter()
            .filter(|(level, text)| {
                *level == tracing::Level::ERROR && text.contains(job) && text.contains("panicked")
            })
            .count();
        assert_eq!(panics, 1, "one ERROR for the one panic: {events:?}");
        // The panic hook, which these tests do not install, logs the message by design (README);
        // the schedule's own lines must not.
        for (level, text) in &events {
            assert!(
                !text.contains(SECRET),
                "{level} event leaks the payload: {text}"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_job_that_panics_runs_again_next_period() {
        let calls = Arc::new(AtomicUsize::new(0));
        let shutdown = Shutdown::new();
        let counted = Arc::clone(&calls);
        let schedule = run_every("test-async-job", PERIOD, shutdown.clone(), move || {
            let counted = Arc::clone(&counted);
            async move {
                // The panic comes after an await, from inside the polled future.
                tokio::task::yield_now().await;
                let call = counted.fetch_add(1, Ordering::SeqCst);
                assert!(call > 0, "{SECRET}");
            }
        });
        assert_a_panic_keeps_the_schedule("test-async-job", &calls, &shutdown, schedule).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_job_that_panics_before_returning_its_future_runs_again_too() {
        let calls = Arc::new(AtomicUsize::new(0));
        let shutdown = Shutdown::new();
        let counted = Arc::clone(&calls);
        let schedule = run_every("test-builder-job", PERIOD, shutdown.clone(), move || {
            let call = counted.fetch_add(1, Ordering::SeqCst);
            assert!(call > 0, "{SECRET}");
            std::future::ready(())
        });
        assert_a_panic_keeps_the_schedule("test-builder-job", &calls, &shutdown, schedule).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_blocking_job_that_panics_runs_again_next_period() {
        let calls = Arc::new(AtomicUsize::new(0));
        let shutdown = Shutdown::new();
        let counted = Arc::clone(&calls);
        let schedule =
            run_blocking_every("test-blocking-job", PERIOD, shutdown.clone(), move || {
                let call = counted.fetch_add(1, Ordering::SeqCst);
                assert!(call > 0, "{SECRET}");
            });
        assert_a_panic_keeps_the_schedule("test-blocking-job", &calls, &shutdown, schedule).await;
    }
}
