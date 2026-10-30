//! The messages received and not yet applied, one queue per gateway, taken in turn: the broker
//! connection is read as fast as messages come, and a gateway that floods its topics delays the
//! others by at most one message per turn (services/gateway-ops/README.md, "在線狀態").
//!
//! A gateway's queue holds at most [`PER_GATEWAY`] messages and all queues together at most
//! [`TOTAL`]; a message beyond either is dropped, so only a gateway that sends far more than its
//! heartbeats and statuses loses its own messages. Order within a gateway is kept: the rules
//! depend on it.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use protocol::GatewayId;
use tokio::sync::Notify;
use tokio::time::Instant;

use super::rules::Signal;

/// Most messages one gateway may have waiting. A well-behaved gateway has one or two at a time
/// (its status, a heartbeat a minute).
pub const PER_GATEWAY: usize = 16;

/// Most messages waiting in all; a status or heartbeat is well under a kilobyte here.
pub const TOTAL: usize = 50_000;

/// A message waiting to be applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    /// The gateway its topic names.
    pub gateway: GatewayId,
    /// What it says.
    pub signal: Signal,
    /// Whether the broker sent it as a retained message because of a new subscription.
    pub replayed: bool,
    /// When it was received.
    pub received: Instant,
}

/// Why a message was not queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Full {
    /// Its gateway already has [`PER_GATEWAY`] messages waiting.
    Gateway,
    /// [`TOTAL`] messages are waiting.
    Total,
}

impl Full {
    /// The metric attribute.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway_backlog",
            Self::Total => "backlog",
        }
    }
}

#[derive(Debug, Default)]
struct Queues {
    /// The gateways with messages waiting, in the order of their turns.
    turns: VecDeque<GatewayId>,
    waiting: HashMap<GatewayId, VecDeque<Pending>>,
    total: usize,
}

/// See the module.
#[derive(Debug)]
pub struct Backlog {
    queues: Mutex<Queues>,
    added: Notify,
    per_gateway: usize,
    total: usize,
}

impl Default for Backlog {
    fn default() -> Self {
        Self::with_limits(PER_GATEWAY, TOTAL)
    }
}

impl Backlog {
    /// A backlog with other limits than [`PER_GATEWAY`] and [`TOTAL`], for tests.
    #[must_use]
    pub fn with_limits(per_gateway: usize, total: usize) -> Self {
        Self {
            queues: Mutex::new(Queues::default()),
            added: Notify::new(),
            per_gateway,
            total,
        }
    }

    fn queues(&self) -> std::sync::MutexGuard<'_, Queues> {
        self.queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Queues `pending` behind its gateway's earlier messages.
    ///
    /// # Errors
    ///
    /// [`Full`] when a limit is reached; the message is dropped.
    pub fn push(&self, pending: Pending) -> Result<(), Full> {
        {
            let mut queues = self.queues();
            if queues.total >= self.total {
                return Err(Full::Total);
            }
            let queue = queues.waiting.entry(pending.gateway.clone()).or_default();
            if queue.len() >= self.per_gateway {
                return Err(Full::Gateway);
            }
            let first = queue.is_empty();
            let gateway = pending.gateway.clone();
            queue.push_back(pending);
            queues.total += 1;
            if first {
                queues.turns.push_back(gateway);
            }
        }
        self.added.notify_one();
        Ok(())
    }

    /// The next gateway's oldest message, if any is waiting; the gateway's turn moves to the
    /// back.
    pub fn pop(&self) -> Option<Pending> {
        let mut queues = self.queues();
        let gateway = queues.turns.pop_front()?;
        let queue = queues.waiting.get_mut(&gateway)?;
        let pending = queue.pop_front();
        if queue.is_empty() {
            queues.waiting.remove(&gateway);
        } else {
            queues.turns.push_back(gateway);
        }
        if pending.is_some() {
            queues.total -= 1;
        }
        pending
    }

    /// Waits for the next message ([`Backlog::pop`]).
    pub async fn next(&self) -> Pending {
        loop {
            // Registered before the check, so a push in between is not missed.
            let added = self.added.notified();
            if let Some(pending) = self.pop() {
                return pending;
            }
            added.await;
        }
    }

    /// When the oldest waiting message was received, `None` when none waits: everything the
    /// broker delivered before it has been applied.
    #[must_use]
    pub fn oldest(&self) -> Option<Instant> {
        self.queues()
            .waiting
            .values()
            .filter_map(|queue| queue.front().map(|pending| pending.received))
            .min()
    }

    /// How many messages wait.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queues().total
    }

    /// Whether none waits.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use protocol::{AgentVersion, SessionId};

    use super::*;

    fn pending(gateway: &str, session: &str, received: Instant) -> Pending {
        Pending {
            gateway: GatewayId::new(gateway).expect("a gateway ID"),
            signal: Signal::Online {
                session: SessionId::new(session).expect("a session ID"),
                agent_version: AgentVersion::new("1.0.0").expect("a version"),
            },
            replayed: false,
            received,
        }
    }

    fn taken(backlog: &Backlog) -> Vec<(String, String)> {
        std::iter::from_fn(|| backlog.pop())
            .map(|pending| {
                (
                    pending.gateway.as_str().to_owned(),
                    pending.signal.session().as_str().to_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn gateways_take_turns_and_each_keeps_its_order() {
        let backlog = Backlog::with_limits(100, 1000);
        let now = Instant::now();
        for n in 0..5 {
            backlog
                .push(pending("flood", &format!("f{n}"), now))
                .expect("queued");
        }
        backlog.push(pending("quiet", "q0", now)).expect("queued");
        backlog.push(pending("quiet", "q1", now)).expect("queued");
        let order: Vec<(&str, &str)> = vec![
            ("flood", "f0"),
            ("quiet", "q0"),
            ("flood", "f1"),
            ("quiet", "q1"),
            ("flood", "f2"),
            ("flood", "f3"),
            ("flood", "f4"),
        ];
        let expected: Vec<(String, String)> = order
            .into_iter()
            .map(|(gateway, session)| (gateway.to_owned(), session.to_owned()))
            .collect();
        assert_eq!(taken(&backlog), expected);
        assert!(backlog.is_empty());
    }

    #[test]
    fn a_flooding_gateway_loses_only_its_own_messages() {
        let backlog = Backlog::with_limits(3, 1000);
        let now = Instant::now();
        let refused: Vec<_> = (0..10)
            .map(|n| backlog.push(pending("flood", &format!("f{n}"), now)))
            .filter_map(Result::err)
            .collect();
        assert_eq!(refused, vec![Full::Gateway; 7]);
        backlog.push(pending("quiet", "q0", now)).expect("queued");
        assert_eq!(backlog.len(), 4);
        // The flood's first three, in order, and the quiet gateway's second.
        assert_eq!(
            taken(&backlog),
            vec![
                ("flood".to_owned(), "f0".to_owned()),
                ("quiet".to_owned(), "q0".to_owned()),
                ("flood".to_owned(), "f1".to_owned()),
                ("flood".to_owned(), "f2".to_owned()),
            ]
        );
    }

    #[test]
    fn the_total_is_bounded() {
        let backlog = Backlog::with_limits(10, 4);
        let now = Instant::now();
        for n in 0..4 {
            backlog
                .push(pending(&format!("gw-{n}"), "s", now))
                .expect("queued");
        }
        assert_eq!(backlog.push(pending("gw-9", "s", now)), Err(Full::Total));
        backlog.pop().expect("one waits");
        backlog.push(pending("gw-9", "s", now)).expect("room again");
    }

    #[test]
    fn the_oldest_waiting_message_bounds_what_was_applied() {
        let backlog = Backlog::with_limits(10, 100);
        assert_eq!(backlog.oldest(), None);
        let start = Instant::now();
        let later = start + Duration::from_secs(5);
        backlog.push(pending("b", "s", later)).expect("queued");
        backlog.push(pending("a", "s", start)).expect("queued");
        assert_eq!(backlog.oldest(), Some(start));
        // "b" has its turn first.
        backlog.pop().expect("b");
        assert_eq!(backlog.oldest(), Some(start));
        backlog.pop().expect("a");
        assert_eq!(backlog.oldest(), None);
    }

    #[tokio::test]
    async fn next_waits_for_a_message() {
        let backlog = std::sync::Arc::new(Backlog::default());
        let waiting = tokio::spawn({
            let backlog = std::sync::Arc::clone(&backlog);
            async move { backlog.next().await }
        });
        tokio::task::yield_now().await;
        backlog
            .push(pending("gw", "s1", Instant::now()))
            .expect("queued");
        let got = waiting.await.expect("the waiting task");
        assert_eq!(got.signal.session().as_str(), "s1");
    }
}
