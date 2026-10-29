//! The delivery's coalescing schedule: which gateway's configuration to publish when
//! (services/gateway-ops/README.md, "下發"). Pure bookkeeping on instants the caller passes in,
//! so the throttle is tested without a clock.
//!
//! A gateway has at most one entry. The first request opens it, due one throttle later for a
//! change or a report, at once for a republish; a later request before it is taken merges into
//! it (the highest expected version, `force` if either forces) and never moves it later, so a
//! burst of changes becomes one publish of the latest version, and a gateway whose changes never
//! stop still gets one publish per throttle.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use protocol::{ConfigVersion, GatewayId};
use tokio::time::Instant;

/// Why a gateway's configuration is to be published. Ordered by precedence: a merged entry
/// names the strongest reason, for the logs and metrics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Trigger {
    /// A new version was stored (the consumer's handler).
    Change,
    /// A gateway's report showed it received an older version than the latest
    /// (README.md, "套用回報").
    Report,
    /// The broker may have lost the retained messages: the republish after a connection.
    Republish,
}

impl Trigger {
    /// The name in logs and metrics.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Change => "change",
            Self::Report => "report",
            Self::Republish => "republish",
        }
    }
}

/// What to do for a gateway when its entry is due.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    /// The strongest reason merged into the entry.
    pub trigger: Trigger,
    /// The highest version a change announced: the publish waits until the stored latest version
    /// is at least this one (the handler announces a version before its transaction commits).
    pub expected: Option<ConfigVersion>,
    /// Publish the latest version even when this replica already published it: the broker or
    /// the gateway may have lost it.
    pub force: bool,
    /// When the entry was opened; once a version is not visible for the delivery's limit after
    /// this, it is looked for less often.
    pub since: Instant,
}

impl Request {
    fn merge(&mut self, other: &Self) {
        self.trigger = self.trigger.max(other.trigger);
        self.expected = self.expected.max(other.expected);
        self.force |= other.force;
        self.since = self.since.min(other.since);
    }
}

/// Whether a request opened an entry or joined one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Added {
    /// A new entry.
    Opened,
    /// Merged into the gateway's waiting entry: no publish of its own.
    Coalesced,
}

#[derive(Debug)]
struct Entry {
    due: Instant,
    request: Request,
}

/// The waiting entries, one per gateway.
#[derive(Debug)]
pub struct Schedule {
    throttle: Duration,
    entries: HashMap<GatewayId, Entry>,
}

impl Schedule {
    /// An empty schedule whose changes and reports wait `throttle`.
    #[must_use]
    pub fn new(throttle: Duration) -> Self {
        Self {
            throttle,
            entries: HashMap::new(),
        }
    }

    /// The coalescing window.
    #[must_use]
    pub fn throttle(&self) -> Duration {
        self.throttle
    }

    /// Asks for `gateway`'s configuration to be published for `trigger` at `now`; `expected` is
    /// the version a change announced.
    pub fn request(
        &mut self,
        gateway: &GatewayId,
        trigger: Trigger,
        expected: Option<ConfigVersion>,
        now: Instant,
    ) -> Added {
        let due = match trigger {
            Trigger::Change | Trigger::Report => now + self.throttle,
            Trigger::Republish => now,
        };
        let request = Request {
            trigger,
            expected,
            force: trigger != Trigger::Change,
            since: now,
        };
        self.insert(gateway, request, due)
    }

    /// Puts a taken request back, due at `due` (a retry), merged with any request that arrived
    /// meanwhile.
    pub fn retry(&mut self, gateway: &GatewayId, request: Request, due: Instant) {
        self.insert(gateway, request, due);
    }

    fn insert(&mut self, gateway: &GatewayId, request: Request, due: Instant) -> Added {
        if let Some(entry) = self.entries.get_mut(gateway) {
            entry.request.merge(&request);
            entry.due = entry.due.min(due);
            Added::Coalesced
        } else {
            self.entries.insert(gateway.clone(), Entry { due, request });
            Added::Opened
        }
    }

    /// The earliest due time of an entry whose gateway is not in `busy`.
    #[must_use]
    pub fn next_due(&self, busy: &HashSet<GatewayId>) -> Option<Instant> {
        self.entries
            .iter()
            .filter(|(gateway, _)| !busy.contains(*gateway))
            .map(|(_, entry)| entry.due)
            .min()
    }

    /// Takes at most `limit` entries due at `now` whose gateway is not in `busy`, earliest first.
    pub fn take_due(
        &mut self,
        now: Instant,
        busy: &HashSet<GatewayId>,
        limit: usize,
    ) -> Vec<(GatewayId, Request)> {
        let mut due: Vec<(Instant, GatewayId)> = self
            .entries
            .iter()
            .filter(|(gateway, entry)| entry.due <= now && !busy.contains(*gateway))
            .map(|(gateway, entry)| (entry.due, gateway.clone()))
            .collect();
        due.sort();
        due.truncate(limit);
        due.into_iter()
            .filter_map(|(_, gateway)| {
                let entry = self.entries.remove(&gateway)?;
                Some((gateway, entry.request))
            })
            .collect()
    }

    /// Whether `gateway` has a waiting entry.
    #[must_use]
    pub fn contains(&self, gateway: &GatewayId) -> bool {
        self.entries.contains_key(gateway)
    }

    /// How many gateways wait.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no gateway waits.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THROTTLE: Duration = Duration::from_secs(2);

    fn gateway(name: &str) -> GatewayId {
        GatewayId::new(name).expect("a gateway ID")
    }

    fn version(value: u64) -> Option<ConfigVersion> {
        ConfigVersion::new(value)
    }

    #[test]
    fn ten_quick_changes_become_one_publish_of_the_latest_version() {
        let mut schedule = Schedule::new(THROTTLE);
        let start = Instant::now();
        let gw = gateway("gw-a");
        let mut coalesced = 0;
        for step in 1..=10_u64 {
            let at = start + Duration::from_millis(100 * step);
            if schedule.request(&gw, Trigger::Change, version(step), at) == Added::Coalesced {
                coalesced += 1;
            }
        }
        assert_eq!(coalesced, 9);
        let none = HashSet::new();
        // Nothing before the window that the first change opened ends, whatever came later.
        let first = start + Duration::from_millis(100);
        assert_eq!(schedule.next_due(&none), Some(first + THROTTLE));
        assert!(
            schedule
                .take_due(first + THROTTLE - Duration::from_millis(1), &none, 8)
                .is_empty()
        );
        let taken = schedule.take_due(first + THROTTLE, &none, 8);
        assert_eq!(taken.len(), 1);
        let (taken_gateway, request) = &taken[0];
        assert_eq!(taken_gateway, &gw);
        assert_eq!(request.expected, version(10));
        assert_eq!(request.trigger, Trigger::Change);
        assert!(!request.force);
        assert_eq!(request.since, first);
        assert!(schedule.is_empty());
    }

    #[test]
    fn a_late_older_announcement_never_lowers_the_expected_version() {
        let mut schedule = Schedule::new(THROTTLE);
        let now = Instant::now();
        let gw = gateway("gw-a");
        schedule.request(&gw, Trigger::Change, version(7), now);
        schedule.request(&gw, Trigger::Change, version(3), now);
        let taken = schedule.take_due(now + THROTTLE, &HashSet::new(), 1);
        assert_eq!(taken[0].1.expected, version(7));
    }

    #[test]
    fn a_republish_is_due_at_once_and_forces_a_merged_change() {
        let mut schedule = Schedule::new(THROTTLE);
        let now = Instant::now();
        let gw = gateway("gw-a");
        schedule.request(&gw, Trigger::Change, version(2), now);
        assert_eq!(
            schedule.request(&gw, Trigger::Republish, None, now),
            Added::Coalesced
        );
        let taken = schedule.take_due(now, &HashSet::new(), 1);
        let request = taken[0].1;
        assert_eq!(request.trigger, Trigger::Republish);
        assert!(request.force);
        assert_eq!(request.expected, version(2));
    }

    #[test]
    fn a_report_waits_a_throttle_so_a_flood_of_reports_is_one_publish() {
        let mut schedule = Schedule::new(THROTTLE);
        let now = Instant::now();
        let gw = gateway("gw-a");
        for _ in 0..100 {
            schedule.request(&gw, Trigger::Report, None, now);
        }
        assert!(schedule.take_due(now, &HashSet::new(), 8).is_empty());
        let taken = schedule.take_due(now + THROTTLE, &HashSet::new(), 8);
        assert_eq!(taken.len(), 1);
        assert!(taken[0].1.force);
    }

    #[test]
    fn busy_gateways_wait_and_the_limit_takes_the_earliest() {
        let mut schedule = Schedule::new(THROTTLE);
        let now = Instant::now();
        let (a, b, c) = (gateway("gw-a"), gateway("gw-b"), gateway("gw-c"));
        schedule.request(
            &c,
            Trigger::Change,
            version(1),
            now + Duration::from_millis(2),
        );
        schedule.request(&a, Trigger::Change, version(1), now);
        schedule.request(
            &b,
            Trigger::Change,
            version(1),
            now + Duration::from_millis(1),
        );
        let busy: HashSet<GatewayId> = [a.clone()].into();
        assert_eq!(
            schedule.next_due(&busy),
            Some(now + Duration::from_millis(1) + THROTTLE)
        );
        let later = now + THROTTLE + Duration::from_secs(1);
        let taken = schedule.take_due(later, &busy, 1);
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].0, b);
        assert!(schedule.contains(&a) && schedule.contains(&c));
        assert_eq!(schedule.len(), 2);
    }

    #[test]
    fn a_retry_merges_with_what_arrived_meanwhile() {
        let mut schedule = Schedule::new(THROTTLE);
        let now = Instant::now();
        let gw = gateway("gw-a");
        schedule.request(&gw, Trigger::Change, version(1), now);
        let (_, request) = schedule
            .take_due(now + THROTTLE, &HashSet::new(), 1)
            .pop()
            .expect("due");
        // A newer change arrives while the first one is being published, then the first is put
        // back for a retry.
        schedule.request(&gw, Trigger::Change, version(2), now + THROTTLE);
        schedule.retry(&gw, request, now + THROTTLE + Duration::from_secs(5));
        let taken = schedule.take_due(now + THROTTLE * 2, &HashSet::new(), 1);
        assert_eq!(taken[0].1.expected, version(2));
        assert_eq!(taken[0].1.since, now);
    }
}
