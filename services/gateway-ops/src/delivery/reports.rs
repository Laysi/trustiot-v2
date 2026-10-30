//! The gateways' apply reports (`up/config-report`; services/gateway-ops/README.md, "套用回報").
//!
//! [`check_message`] is everything decided from the message alone: the gateway comes from the
//! topic, whose level the broker's ACL lets a gateway publish under only as itself (the report
//! names no gateway; a payload field that does is an unknown key the decoder skips, never a
//! source of identity); the retain flag, which a report never carries (the broker replays
//! retained messages at every new subscription, so a retained report would be counted again);
//! QoS 1; and the payload, decoded and validated by crates/protocol. [`judge`] then compares the
//! report with what is stored ([`ReportTarget`]): versions the service never generated are
//! refused, and only reports about versions of the gateway's current binding are recorded.
//! [`Inbox`] holds the checked reports until the delivery's report task handles them (off the
//! connection's event loop), at most one per gateway, and takes back those whose handling failed
//! for a database error, for a bounded time.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use mqtt_client::Message;
use protocol::config::{ConfigOutcome, ConfigReport};
use protocol::topic::{Channel, parse_topic};
use protocol::{ConfigVersion, GatewayId};
use tokio::time::Instant;

use crate::desired::ReportTarget;

/// Why a report was refused, by the `reason` in logs and metrics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The topic is not a gateway's `up/config-report`.
    Topic,
    /// The message carries the retain flag.
    Retained,
    /// The broker delivered it with QoS 0: the gateway did not publish it with QoS 1.
    Qos0,
    /// The payload does not decode or breaks the protocol's rules.
    Decode,
    /// The outcome is one this build does not know.
    UnknownOutcome,
    /// The gateway has no configuration at all.
    UnknownGateway,
    /// The report names a version, or an active version, above the gateway's latest one: no
    /// such version was ever generated.
    VersionAhead,
    /// The gateway has no such version, though it is not above the latest (cannot happen:
    /// versions are consecutive).
    UnknownVersion,
    /// The gateway's latest version was not made for its latest binding (a broken rule, see
    /// `crate::desired`): nothing is recorded for it.
    NotForLatestBinding,
    /// The version belongs to a binding before the gateway's current one, possibly another
    /// tenant's: not recorded in that tenant's data.
    PreviousBinding,
}

impl Refusal {
    /// The name in logs and metrics.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::Topic => "topic",
            Self::Retained => "retained",
            Self::Qos0 => "qos0",
            Self::Decode => "decode",
            Self::UnknownOutcome => "unknown_outcome",
            Self::UnknownGateway => "unknown_gateway",
            Self::VersionAhead => "version_ahead",
            Self::UnknownVersion => "unknown_version",
            Self::NotForLatestBinding => "not_for_latest_binding",
            Self::PreviousBinding => "previous_binding",
        }
    }
}

/// A report that passed [`check_message`].
#[derive(Clone, Debug, PartialEq)]
pub struct Received {
    /// The gateway, from the topic.
    pub gateway: GatewayId,
    /// The decoded report.
    pub report: ConfigReport,
}

/// A refusal from the message alone, with the gateway when the topic named one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    /// Why.
    pub refusal: Refusal,
    /// The topic's gateway, when the topic is a gateway's report topic.
    pub gateway: Option<GatewayId>,
}

/// Checks a message from the report subscription; see the module.
///
/// # Errors
///
/// [`Refused`] naming why the message is not a report to handle.
pub fn check_message(message: &Message) -> Result<Received, Refused> {
    let gateway = match parse_topic(&message.topic) {
        Ok(parsed) if parsed.channel == Channel::ConfigReport => parsed.gateway,
        _ => {
            return Err(Refused {
                refusal: Refusal::Topic,
                gateway: None,
            });
        }
    };
    let refuse = |refusal| {
        Err(Refused {
            refusal,
            gateway: Some(gateway.clone()),
        })
    };
    if message.retain {
        return refuse(Refusal::Retained);
    }
    if message.qos == 0 {
        return refuse(Refusal::Qos0);
    }
    let Ok(report) = ConfigReport::decode(&message.payload) else {
        return refuse(Refusal::Decode);
    };
    if matches!(report.outcome, ConfigOutcome::Other(_)) {
        return refuse(Refusal::UnknownOutcome);
    }
    Ok(Received { gateway, report })
}

/// What to do with a report after comparing it with what is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// Record it in the reported version's tenant (`Ok`), or refuse it.
    pub record: Result<(), Refusal>,
    /// The gateway received a version older than its latest: publish the latest again
    /// (throttled), whether or not the report is recorded.
    pub republish: bool,
}

/// Compares `report` with `target` (`None`: the gateway has no configuration); see the module.
#[must_use]
pub fn judge(report: &ConfigReport, target: Option<&ReportTarget>) -> Verdict {
    let refuse = |refusal| Verdict {
        record: Err(refusal),
        republish: false,
    };
    let Some(target) = target else {
        return refuse(Refusal::UnknownGateway);
    };
    let ahead = |version: ConfigVersion| version > target.latest;
    if ahead(report.version) || report.active_version.is_some_and(ahead) {
        return refuse(Refusal::VersionAhead);
    }
    if !target.for_latest_binding {
        return refuse(Refusal::NotForLatestBinding);
    }
    let republish = report.version < target.latest;
    let record = match target.reported {
        None => Err(Refusal::UnknownVersion),
        Some(reported) if reported != (target.latest_tenant, target.latest_binding) => {
            Err(Refusal::PreviousBinding)
        }
        Some(_) => Ok(()),
    };
    Verdict { record, republish }
}

/// Most WARN lines about refused reports per [`LOG_WINDOW`]; the rest are counted and the next
/// line logged says how many were left out, so a gateway that floods its report topic cannot
/// flood the log.
pub const LOG_LINES_PER_WINDOW: u32 = 60;

/// The window of [`LOG_LINES_PER_WINDOW`].
pub const LOG_WINDOW: Duration = Duration::from_secs(60);

/// A budget of log lines per window, shared by every gateway.
#[derive(Debug)]
pub struct LogBudget(Mutex<Window>);

#[derive(Debug)]
struct Window {
    started: Instant,
    used: u32,
    suppressed: u64,
}

impl LogBudget {
    /// A full budget.
    #[must_use]
    pub fn new() -> Self {
        Self(Mutex::new(Window {
            started: Instant::now(),
            used: 0,
            suppressed: 0,
        }))
    }

    /// `Some(lines suppressed since the last line logged)` when a line may be logged at `now`.
    pub fn allow(&self, now: Instant) -> Option<u64> {
        let mut window = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if now.saturating_duration_since(window.started) >= LOG_WINDOW {
            window.started = now;
            window.used = 0;
        }
        if window.used < LOG_LINES_PER_WINDOW {
            window.used += 1;
            Some(std::mem::take(&mut window.suppressed))
        } else {
            window.suppressed += 1;
            None
        }
    }
}

impl Default for LogBudget {
    fn default() -> Self {
        Self::new()
    }
}

/// How long a report whose handling failed for a database error is tried again, from its
/// arrival. The client acknowledged it when it was handed over (crates/mqtt-client), so the
/// gateway does not send it again; after this it is given up (WARN, counted).
pub const REPORT_RETRY_LIMIT: Duration = Duration::from_secs(600);

/// The wait between the tries of a failed report, and the pause of the report handling after a
/// database error (the database is likely down: the waiting reports are not tried in a burst).
pub const REPORT_RETRY_DELAY: Duration = Duration::from_secs(5);

/// Most gateways with a report waiting per replica. A gateway has at most one waiting report,
/// so this bounds the inbox whatever the gateways send; when it is full, a report of another
/// gateway gives up the report that has waited longest in line.
pub const MAX_WAITING_GATEWAYS: usize = 4096;

/// A report waiting to be handled.
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    /// The report.
    pub received: Received,
    /// When it arrived.
    pub since: Instant,
    due: Instant,
}

/// What [`Inbox::offer`] did with a new report.
#[derive(Debug, PartialEq)]
pub enum Offered {
    /// Waits for its turn.
    Waiting,
    /// Took the place of the gateway's waiting report, which is never handled: the gateway's
    /// later word replaces it.
    Replaced(Box<Pending>),
    /// Waits, and the report that had waited longest was given up to make room.
    Evicted(Box<Pending>),
}

/// What [`Inbox::retry`] did with a report whose handling failed.
#[derive(Debug, PartialEq)]
pub enum Retried {
    /// Waits for its next try.
    Waiting,
    /// A newer report of the same gateway arrived while this one was handled: the newer one
    /// waits instead.
    Superseded,
    /// Older than [`REPORT_RETRY_LIMIT`]: given up.
    Expired,
    /// Waits, and the report that had waited longest was given up to make room.
    Evicted(Box<Pending>),
}

/// The reports waiting to be handled: at most one per gateway (its latest), in the order their
/// gateways got in line, so a gateway that floods its report topic holds one place and cannot
/// push another gateway's report out or ahead. Pure bookkeeping on instants the caller passes in,
/// so it is tested without a clock.
#[derive(Debug, Default)]
pub struct Inbox {
    waiting: HashMap<GatewayId, Pending>,
    /// The gateways with a waiting report, in line order.
    line: VecDeque<GatewayId>,
}

impl Inbox {
    /// Puts a report that arrived at `now` in line, due at once; see [`Offered`].
    pub fn offer(&mut self, received: Received, now: Instant) -> Offered {
        let pending = Pending {
            received,
            since: now,
            due: now,
        };
        if let Some(waiting) = self.waiting.get_mut(&pending.received.gateway) {
            // The gateway keeps its place in line, so replacing its report never moves it ahead.
            let replaced = std::mem::replace(waiting, pending);
            return Offered::Replaced(Box::new(replaced));
        }
        match self.enter(pending) {
            Some(evicted) => Offered::Evicted(evicted),
            None => Offered::Waiting,
        }
    }

    /// Puts back a report whose handling failed at `now`, for a try one [`REPORT_RETRY_DELAY`]
    /// later at the end of the line; see [`Retried`].
    pub fn retry(&mut self, pending: Pending, now: Instant) -> Retried {
        if now.saturating_duration_since(pending.since) >= REPORT_RETRY_LIMIT {
            return Retried::Expired;
        }
        if self.waiting.contains_key(&pending.received.gateway) {
            return Retried::Superseded;
        }
        let pending = Pending {
            due: now + REPORT_RETRY_DELAY,
            ..pending
        };
        match self.enter(pending) {
            Some(evicted) => Retried::Evicted(evicted),
            None => Retried::Waiting,
        }
    }

    /// Adds a report of a gateway that has none waiting at the end of the line; returns the
    /// report given up when the inbox was full.
    fn enter(&mut self, pending: Pending) -> Option<Box<Pending>> {
        let evicted = if self.waiting.len() >= MAX_WAITING_GATEWAYS {
            self.line
                .pop_front()
                .and_then(|gateway| self.waiting.remove(&gateway))
                .map(Box::new)
        } else {
            None
        };
        self.line.push_back(pending.received.gateway.clone());
        self.waiting
            .insert(pending.received.gateway.clone(), pending);
        evicted
    }

    /// Takes the first report in line that is due at `now`.
    pub fn take_due(&mut self, now: Instant) -> Option<Pending> {
        let position = self.line.iter().position(|gateway| {
            self.waiting
                .get(gateway)
                .is_some_and(|pending| pending.due <= now)
        })?;
        let gateway = self.line.remove(position)?;
        self.waiting.remove(&gateway)
    }

    /// The earliest due time.
    #[must_use]
    pub fn next_due(&self) -> Option<Instant> {
        self.waiting.values().map(|pending| pending.due).min()
    }

    /// How many reports wait.
    #[must_use]
    pub fn len(&self) -> usize {
        self.waiting.len()
    }

    /// Whether none waits.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use contract::{GatewayBindingId, TenantId};
    use protocol::{Issue, IssueCode};

    use super::*;

    fn gateway() -> GatewayId {
        GatewayId::new("gw-a").expect("a gateway ID")
    }

    fn version(value: u64) -> ConfigVersion {
        ConfigVersion::new(value).expect("a version")
    }

    fn message(topic: String, payload: Vec<u8>, qos: u8, retain: bool) -> Message {
        Message {
            topic,
            payload: payload.into(),
            qos,
            retain,
        }
    }

    fn applied(value: u64) -> Vec<u8> {
        ConfigReport::applied(version(value))
            .encode()
            .expect("encode")
    }

    #[test]
    fn the_gateway_comes_from_the_topic() {
        let topic = Channel::ConfigReport.topic(&gateway());
        let received = check_message(&message(topic, applied(3), 1, false)).expect("a report");
        assert_eq!(received.gateway, gateway());
        assert_eq!(received.report, ConfigReport::applied(version(3)));
    }

    #[test]
    fn messages_that_break_the_protocol_are_refused() {
        let topic = Channel::ConfigReport.topic(&gateway());
        let refusal = |message: Message| check_message(&message).expect_err("refused").refusal;
        assert_eq!(
            refusal(message(topic.clone(), applied(1), 1, true)),
            Refusal::Retained
        );
        assert_eq!(
            refusal(message(topic.clone(), applied(1), 0, false)),
            Refusal::Qos0
        );
        assert_eq!(
            refusal(message(topic.clone(), b"not cbor".to_vec(), 1, false)),
            Refusal::Decode
        );
        assert_eq!(
            refusal(message(topic.clone(), Vec::new(), 1, false)),
            Refusal::Decode
        );
        // Another channel of the same gateway, and topics that are no gateway's.
        for other in [
            Channel::Status.topic(&gateway()),
            "trustiot/v1/gw/gw-a/up/config-report/extra".to_owned(),
            "trustiot/v2/gw/gw-a/up/config-report".to_owned(),
        ] {
            let refused = check_message(&message(other, applied(1), 1, false)).expect_err("topic");
            assert_eq!(refused.refusal, Refusal::Topic);
            assert_eq!(refused.gateway, None);
        }
        let mut unknown = ConfigReport::applied(version(1));
        unknown.outcome = ConfigOutcome::Other("postponed".to_owned());
        assert_eq!(
            refusal(message(topic, unknown.encode().expect("encode"), 1, false)),
            Refusal::UnknownOutcome
        );
    }

    /// The current binding's tenant and binding in these tests.
    fn current() -> Option<(TenantId, GatewayBindingId)> {
        Some((
            "6f9c1d2e-3b4a-4c5d-8e7f-0a1b2c3d4e5f"
                .parse()
                .expect("a tenant"),
            "0b1c2d3e-4f5a-4b6c-9d7e-8f9a0b1c2d3e"
                .parse()
                .expect("a binding"),
        ))
    }

    fn target(latest: u64, reported: Option<(TenantId, GatewayBindingId)>) -> ReportTarget {
        let (latest_tenant, latest_binding) = current().expect("the current binding");
        ReportTarget {
            latest: version(latest),
            latest_tenant,
            latest_binding,
            for_latest_binding: true,
            reported,
        }
    }

    #[test]
    fn reports_are_recorded_only_for_generated_versions_of_the_current_binding() {
        let latest = target(5, current());
        assert_eq!(
            judge(&ConfigReport::applied(version(5)), Some(&latest)),
            Verdict {
                record: Ok(()),
                republish: false
            }
        );
        // An older version of the same binding: recorded, and the latest is published again.
        assert_eq!(
            judge(
                &ConfigReport::applied(version(4)),
                Some(&target(5, current()))
            ),
            Verdict {
                record: Ok(()),
                republish: true
            }
        );
        let rejected = ConfigReport::rejected(
            version(5),
            Some(version(4)),
            vec![Issue::new(
                IssueCode::InvalidValue,
                "devices[0].port",
                "bad",
            )],
        );
        assert_eq!(judge(&rejected, Some(&latest)).record, Ok(()));
        assert!(!judge(&rejected, Some(&latest)).republish);
        // Versions never generated.
        assert_eq!(
            judge(&ConfigReport::applied(version(6)), Some(&latest)).record,
            Err(Refusal::VersionAhead)
        );
        let stale_ahead = ConfigReport::stale(version(2), version(9));
        assert_eq!(
            judge(&stale_ahead, Some(&latest)),
            Verdict {
                record: Err(Refusal::VersionAhead),
                republish: false
            }
        );
        assert_eq!(
            judge(&ConfigReport::applied(version(1)), None).record,
            Err(Refusal::UnknownGateway)
        );
        assert_eq!(
            judge(&ConfigReport::applied(version(3)), Some(&target(5, None))).record,
            Err(Refusal::UnknownVersion)
        );
        // A version of an earlier binding (another tenant's): not recorded there, but the
        // gateway still gets its latest version again.
        let earlier = target(
            5,
            Some((TenantId::generate(), GatewayBindingId::generate())),
        );
        assert_eq!(
            judge(&ConfigReport::stale(version(2), version(5)), Some(&earlier)),
            Verdict {
                record: Err(Refusal::PreviousBinding),
                republish: true
            }
        );
        let mut broken = target(5, current());
        broken.for_latest_binding = false;
        assert_eq!(
            judge(&ConfigReport::applied(version(5)), Some(&broken)).record,
            Err(Refusal::NotForLatestBinding)
        );
    }

    #[test]
    fn a_stale_report_republishes_the_latest_version() {
        // The gateway runs version 5 and received an outdated retained version 3.
        let verdict = judge(
            &ConfigReport::stale(version(3), version(5)),
            Some(&target(5, current())),
        );
        assert!(verdict.republish);
        assert_eq!(verdict.record, Ok(()));
    }

    fn received(gateway: &str, value: u64) -> Received {
        Received {
            gateway: GatewayId::new(gateway).expect("a gateway ID"),
            report: ConfigReport::applied(version(value)),
        }
    }

    #[test]
    fn a_flooding_gateway_holds_one_place_and_cannot_crowd_out_another() {
        let mut inbox = Inbox::default();
        let start = Instant::now();
        assert_eq!(inbox.offer(received("gw-a", 1), start), Offered::Waiting);
        assert_eq!(inbox.offer(received("gw-b", 1), start), Offered::Waiting);
        // Far more reports than the inbox holds, each about another version.
        let flood = u64::try_from(MAX_WAITING_GATEWAYS * 4).expect("fits");
        for value in 2..=flood {
            assert!(matches!(
                inbox.offer(received("gw-a", value), start),
                Offered::Replaced(_)
            ));
        }
        assert_eq!(inbox.len(), 2);
        // gw-a keeps its place in line with its latest report; gw-b's report is still there.
        let first = inbox.take_due(start).expect("gw-a's report");
        assert_eq!(first.received, received("gw-a", flood));
        let second = inbox.take_due(start).expect("gw-b's report");
        assert_eq!(second.received, received("gw-b", 1));
        assert!(inbox.take_due(start).is_none());
    }

    #[test]
    fn a_full_inbox_gives_up_the_report_that_waited_longest() {
        let mut inbox = Inbox::default();
        let start = Instant::now();
        for index in 0..MAX_WAITING_GATEWAYS {
            assert_eq!(
                inbox.offer(received(&format!("gw-{index}"), 1), start),
                Offered::Waiting
            );
        }
        let Offered::Evicted(evicted) = inbox.offer(received("gw-new", 1), start) else {
            panic!("the inbox is full");
        };
        assert_eq!(evicted.received, received("gw-0", 1));
        assert_eq!(inbox.len(), MAX_WAITING_GATEWAYS);
    }

    #[test]
    fn failed_reports_are_retried_after_a_delay_until_the_limit() {
        let mut inbox = Inbox::default();
        let start = Instant::now();
        inbox.offer(received("gw-a", 1), start);
        inbox.offer(received("gw-b", 1), start);
        let failed = inbox.take_due(start).expect("gw-a's report");
        // Back at the end of the line, due one delay later: gw-b goes first.
        assert_eq!(inbox.retry(failed, start), Retried::Waiting);
        assert_eq!(
            inbox.take_due(start).expect("gw-b's report").received,
            received("gw-b", 1)
        );
        assert!(inbox.take_due(start).is_none());
        assert_eq!(inbox.next_due(), Some(start + REPORT_RETRY_DELAY));
        let again = inbox
            .take_due(start + REPORT_RETRY_DELAY)
            .expect("gw-a's report again");
        assert_eq!(again.since, start, "the limit counts from the arrival");
        // Tried again until the limit from its arrival, then given up.
        let late = start + REPORT_RETRY_LIMIT - Duration::from_millis(1);
        assert_eq!(inbox.retry(again, late), Retried::Waiting);
        let last = inbox
            .take_due(late + REPORT_RETRY_DELAY)
            .expect("the last try");
        assert_eq!(
            inbox.retry(last, start + REPORT_RETRY_LIMIT),
            Retried::Expired
        );
        assert!(inbox.is_empty());
    }

    #[test]
    fn a_report_that_arrives_during_a_failed_try_supersedes_it() {
        let mut inbox = Inbox::default();
        let start = Instant::now();
        inbox.offer(received("gw-a", 1), start);
        let in_hand = inbox.take_due(start).expect("the report");
        // The gateway reports again while the first one is being handled; then that one fails.
        let rejected = Received {
            gateway: gateway(),
            report: ConfigReport::rejected(version(1), None, Vec::new()),
        };
        assert_eq!(inbox.offer(rejected.clone(), start), Offered::Waiting);
        assert_eq!(inbox.retry(in_hand, start), Retried::Superseded);
        assert_eq!(inbox.len(), 1);
        assert_eq!(
            inbox.take_due(start).expect("the later report").received,
            rejected
        );
    }

    #[test]
    fn a_waiting_retry_is_replaced_by_a_new_report_of_its_gateway() {
        let mut inbox = Inbox::default();
        let start = Instant::now();
        inbox.offer(received("gw-a", 1), start);
        let failed = inbox.take_due(start).expect("the report");
        assert_eq!(inbox.retry(failed, start), Retried::Waiting);
        let later = start + Duration::from_secs(1);
        let Offered::Replaced(replaced) = inbox.offer(received("gw-a", 2), later) else {
            panic!("gw-a has a waiting report");
        };
        assert_eq!(replaced.received, received("gw-a", 1));
        // The new report is due at once, and its retry limit counts from its own arrival.
        let taken = inbox.take_due(later).expect("the new report");
        assert_eq!(taken.received, received("gw-a", 2));
        assert_eq!(taken.since, later);
    }

    #[test]
    fn the_log_budget_limits_lines_per_window_and_counts_the_rest() {
        let budget = LogBudget::new();
        let start = Instant::now();
        for _ in 0..LOG_LINES_PER_WINDOW {
            assert_eq!(budget.allow(start), Some(0));
        }
        assert_eq!(budget.allow(start), None);
        assert_eq!(budget.allow(start), None);
        assert_eq!(budget.allow(start + LOG_WINDOW), Some(2));
    }
}
