//! Graceful shutdown on SIGTERM and SIGINT.
//!
//! A service in a container is PID 1, and the kernel applies no default action to a signal that
//! PID 1 does not handle: without a handler, SIGTERM is ignored and the pod lingers until the
//! kubelet sends SIGKILL at the end of the grace period (docs/feasibility/2026-10-10-edge-poc.md).
//! The kit therefore handles both signals itself:
//!
//! 1. On the first signal readiness fails at once ([`Shutdown::requested`]), so the pod leaves
//!    the Service endpoints, while the service keeps serving for `shutdown.drain_delay` until
//!    the ingress, ztunnel, and kube-proxy stop sending it new requests.
//! 2. Then the server stops accepting connections and [`Shutdown::stopping`] resolves: requests
//!    in flight finish, background tasks and long-lived connections (WebSocket, SSE) must wind
//!    down, all within `shutdown.drain_timeout`. Handlers and background tasks still running
//!    when it runs out are dropped (their requests answer 504) and the service exits with 1.
//! 3. A second signal exits immediately without draining.

use std::io;
use std::time::Duration;

use serde::Deserialize;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::config::Invalid;
use crate::health::Health;
use crate::telemetry::duration_ms;

/// The longest `shutdown.drain_delay` or `shutdown.drain_timeout` accepted, and the longest their
/// sum may be: one hour. Far beyond any pod's grace period, yet far below the point where
/// `tokio::time::Instant + Duration` overflows (a panic) or a sleep never ends in practice.
pub const MAX_DRAIN: Duration = Duration::from_secs(60 * 60);

/// `shutdown.*`: draining on SIGTERM. The time the pod gets between SIGTERM and SIGKILL (the
/// chart's `drainSeconds`) must cover `drain_delay + drain_timeout` plus 3 s to cut off work that
/// overran the drain and to flush telemetry (README.md).
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShutdownConfig {
    /// How long to keep serving after the signal while traffic moves to other pods. Default 5s.
    #[serde(deserialize_with = "crate::config::duration")]
    pub drain_delay: Duration,
    /// How long requests in flight and background tasks may take to finish once the server stops
    /// accepting connections. Default 20s.
    #[serde(deserialize_with = "crate::config::duration")]
    pub drain_timeout: Duration,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            drain_delay: Duration::from_secs(5),
            drain_timeout: Duration::from_secs(20),
        }
    }
}

impl ShutdownConfig {
    pub(crate) fn validate(&self) -> Result<(), Invalid> {
        if self.drain_timeout.is_zero() {
            return Err(Invalid::new(
                "shutdown.drain_timeout",
                "must be longer than zero, or requests in flight are cut off",
            ));
        }
        // Messages name the setting and the limit, never the configured value.
        for (key, value) in [
            ("shutdown.drain_delay", self.drain_delay),
            ("shutdown.drain_timeout", self.drain_timeout),
        ] {
            if value > MAX_DRAIN {
                return Err(Invalid::new(key, "must not be longer than 1 hour"));
            }
        }
        // Both are at most an hour here, so the sum cannot overflow.
        if self.drain_delay + self.drain_timeout > MAX_DRAIN {
            return Err(Invalid::new(
                "shutdown.drain_timeout",
                "must be short enough that shutdown.drain_delay plus shutdown.drain_timeout \
                 is at most 1 hour",
            ));
        }
        Ok(())
    }
}

/// The shutdown state that services watch. Cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct Shutdown {
    requested: CancellationToken,
    stopping: CancellationToken,
}

impl Shutdown {
    /// A handle that no signal triggers, for tests of code that watches shutdown.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a shutdown signal has arrived (readiness already fails).
    pub fn is_requested(&self) -> bool {
        self.requested.is_cancelled()
    }

    /// Resolves once a shutdown signal arrives.
    pub async fn requested(&self) {
        self.requested.cancelled().await;
    }

    /// Whether the drain delay has passed and work must wind down.
    pub fn is_stopping(&self) -> bool {
        self.stopping.is_cancelled()
    }

    /// Resolves once the server stops accepting connections. Background tasks stop here, and
    /// handlers of long-lived connections close them so clients reconnect to another pod.
    pub async fn stopping(&self) {
        self.stopping.cancelled().await;
    }

    /// A token cancelled when [`Shutdown::stopping`] resolves, for APIs that take one.
    pub fn stopping_token(&self) -> CancellationToken {
        self.stopping.child_token()
    }

    /// Marks shutdown as requested. Does not stop anything by itself.
    pub fn request(&self) {
        self.requested.cancel();
    }

    /// Begins stopping (and marks shutdown as requested).
    pub fn stop(&self) {
        self.requested.cancel();
        self.stopping.cancel();
    }
}

/// Starts watching SIGTERM and SIGINT. Must be called inside the Tokio runtime.
pub(crate) fn listen(
    config: &ShutdownConfig,
    health: Health,
    shutdown: Shutdown,
) -> io::Result<()> {
    let mut signals = Signals {
        terminate: signal(SignalKind::terminate())?,
        interrupt: signal(SignalKind::interrupt())?,
    };
    let delay = config.drain_delay;
    tokio::spawn(async move {
        let first = signals.next().await;
        health.set_draining();
        shutdown.request();
        tracing::info!(
            signal = first,
            drain_delay_ms = duration_ms(delay),
            "shutdown requested; readiness now fails"
        );
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            second = signals.next() => force_exit(second),
        }
        shutdown.stop();
        let second = signals.next().await;
        force_exit(second);
    });
    Ok(())
}

struct Signals {
    terminate: Signal,
    interrupt: Signal,
}

impl Signals {
    async fn next(&mut self) -> &'static str {
        tokio::select! {
            _ = self.terminate.recv() => "SIGTERM",
            _ = self.interrupt.recv() => "SIGINT",
        }
    }
}

fn force_exit(signal: &'static str) -> ! {
    tracing::warn!(signal, "second signal received; exiting without draining");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stop_resolves_both_phases() {
        let shutdown = Shutdown::new();
        let watcher = shutdown.clone();
        assert!(!watcher.is_requested() && !watcher.is_stopping());
        shutdown.request();
        watcher.requested().await;
        assert!(watcher.is_requested() && !watcher.is_stopping());
        let token = watcher.stopping_token();
        shutdown.stop();
        watcher.stopping().await;
        assert!(token.is_cancelled());
    }

    #[test]
    fn drain_timeout_must_be_positive() {
        let config = ShutdownConfig {
            drain_delay: Duration::ZERO,
            drain_timeout: Duration::ZERO,
        };
        assert_eq!(
            config.validate().expect_err("zero timeout").key,
            "shutdown.drain_timeout"
        );
        assert!(ShutdownConfig::default().validate().is_ok());
    }

    fn config(delay: Duration, timeout: Duration) -> ShutdownConfig {
        ShutdownConfig {
            drain_delay: delay,
            drain_timeout: timeout,
        }
    }

    #[test]
    fn each_drain_time_is_capped_at_one_hour() {
        let second = Duration::from_secs(1);
        let delay = config(MAX_DRAIN + second, second)
            .validate()
            .expect_err("delay over the cap");
        assert_eq!(delay.key, "shutdown.drain_delay");
        let timeout = config(Duration::ZERO, MAX_DRAIN + second)
            .validate()
            .expect_err("timeout over the cap");
        assert_eq!(timeout.key, "shutdown.drain_timeout");
        // Values that overflow `Instant + Duration` are rejected without arithmetic on them.
        assert!(config(Duration::MAX, second).validate().is_err());
        assert!(config(second, Duration::MAX).validate().is_err());
    }

    #[test]
    fn the_sum_of_both_is_capped_at_one_hour() {
        let half_and_a_bit = MAX_DRAIN / 2 + Duration::from_secs(1);
        let error = config(half_and_a_bit, half_and_a_bit)
            .validate()
            .expect_err("sum over the cap");
        assert_eq!(error.key, "shutdown.drain_timeout");
        // Exactly the cap is fine, whichever way it is split.
        assert!(config(Duration::ZERO, MAX_DRAIN).validate().is_ok());
        assert!(config(MAX_DRAIN / 2, MAX_DRAIN / 2).validate().is_ok());
        assert!(
            config(MAX_DRAIN - Duration::from_secs(1), Duration::from_secs(1))
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn messages_name_the_limit_not_the_value() {
        for config in [
            config(Duration::from_secs(7_654_321), Duration::from_secs(1)),
            config(Duration::ZERO, Duration::from_secs(7_654_321)),
            config(Duration::from_secs(3_000), Duration::from_secs(3_000)),
        ] {
            let error = config.validate().expect_err("over the cap");
            assert!(!error.message.contains("7654321"), "{}", error.message);
            assert!(error.message.contains("1 hour"), "{}", error.message);
        }
    }
}
