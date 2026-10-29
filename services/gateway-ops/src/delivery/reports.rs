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
//! [`Retries`] keeps the reports whose handling failed for a database error, for a bounded time.

use std::collections::VecDeque;
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

/// The wait between the tries of a failed report.
pub const REPORT_RETRY_DELAY: Duration = Duration::from_secs(5);

/// Most reports waiting for a retry per replica; a newer report replaces a waiting one of the
/// same gateway and version, and when full the oldest is given up.
pub const MAX_PENDING_REPORTS: usize = 1024;

/// A report waiting to be handled again.
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    /// The report.
    pub received: Received,
    /// When it arrived.
    pub since: Instant,
    due: Instant,
}

/// What [`Retries::push`] did.
#[derive(Debug, PartialEq)]
pub enum Queued {
    /// Waits for its next try.
    Waiting,
    /// Replaced a waiting report of the same gateway and version.
    Replaced,
    /// Waits, and the oldest waiting report was given up to make room.
    Evicted(Box<Pending>),
    /// Older than [`REPORT_RETRY_LIMIT`]: given up.
    Expired,
}

/// The reports waiting for a retry, oldest first. Pure bookkeeping on instants the caller passes
/// in, so it is tested without a clock.
#[derive(Debug, Default)]
pub struct Retries(VecDeque<Pending>);

impl Retries {
    /// Queues `received`, which arrived at `since`, for a try one [`REPORT_RETRY_DELAY`] after
    /// `now`.
    pub fn push(&mut self, received: Received, since: Instant, now: Instant) -> Queued {
        if now.saturating_duration_since(since) >= REPORT_RETRY_LIMIT {
            return Queued::Expired;
        }
        let pending = Pending {
            received,
            since,
            due: now + REPORT_RETRY_DELAY,
        };
        if let Some(waiting) = self.0.iter_mut().find(|waiting| {
            waiting.received.gateway == pending.received.gateway
                && waiting.received.report.version == pending.received.report.version
        }) {
            // The later report is the gateway's latest word on that version.
            let since = waiting.since.min(pending.since);
            *waiting = Pending { since, ..pending };
            return Queued::Replaced;
        }
        let evicted = if self.0.len() >= MAX_PENDING_REPORTS {
            self.0.pop_front()
        } else {
            None
        };
        self.0.push_back(pending);
        match evicted {
            Some(evicted) => Queued::Evicted(Box::new(evicted)),
            None => Queued::Waiting,
        }
    }

    /// Drops the waiting report of `gateway` about `version`: a newer report about it arrived,
    /// and an older one retried later must not overwrite it. Returns whether one was waiting.
    pub fn supersede(&mut self, gateway: &GatewayId, version: ConfigVersion) -> bool {
        let before = self.0.len();
        self.0.retain(|waiting| {
            waiting.received.gateway != *gateway || waiting.received.report.version != version
        });
        self.0.len() != before
    }

    /// Takes the reports due at `now`, oldest first.
    pub fn take_due(&mut self, now: Instant) -> Vec<Pending> {
        let (due, waiting): (VecDeque<Pending>, VecDeque<Pending>) =
            self.0.drain(..).partition(|pending| pending.due <= now);
        self.0 = waiting;
        due.into()
    }

    /// The earliest due time.
    #[must_use]
    pub fn next_due(&self) -> Option<Instant> {
        self.0.iter().map(|pending| pending.due).min()
    }

    /// How many reports wait.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether none waits.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
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
    fn failed_reports_are_retried_after_a_delay_until_the_limit() {
        let mut retries = Retries::default();
        let start = Instant::now();
        assert_eq!(
            retries.push(received("gw-a", 1), start, start),
            Queued::Waiting
        );
        assert_eq!(retries.next_due(), Some(start + REPORT_RETRY_DELAY));
        assert!(retries.take_due(start).is_empty());
        let due = retries.take_due(start + REPORT_RETRY_DELAY);
        assert_eq!(due.len(), 1);
        assert!(retries.is_empty());
        // Tried again until the limit from its arrival, then given up.
        let late = start + REPORT_RETRY_LIMIT - Duration::from_millis(1);
        assert_eq!(
            retries.push(due[0].received.clone(), due[0].since, late),
            Queued::Waiting
        );
        retries.take_due(late + REPORT_RETRY_DELAY);
        assert_eq!(
            retries.push(
                due[0].received.clone(),
                due[0].since,
                start + REPORT_RETRY_LIMIT
            ),
            Queued::Expired
        );
        assert!(retries.is_empty());
    }

    #[test]
    fn a_later_report_of_the_same_version_replaces_the_waiting_one() {
        let mut retries = Retries::default();
        let start = Instant::now();
        retries.push(received("gw-a", 1), start, start);
        let rejected = Received {
            gateway: gateway(),
            report: ConfigReport::rejected(version(1), None, Vec::new()),
        };
        let later = start + Duration::from_secs(1);
        assert_eq!(
            retries.push(rejected.clone(), later, later),
            Queued::Replaced
        );
        retries.push(received("gw-a", 2), later, later);
        assert_eq!(retries.len(), 2);
        let due = retries.take_due(later + REPORT_RETRY_DELAY);
        assert_eq!(due[0].received, rejected);
        assert_eq!(
            due[0].since, start,
            "the limit counts from the first arrival"
        );
        assert_eq!(due[1].received.report.version, version(2));
    }

    #[test]
    fn a_new_report_supersedes_a_waiting_one_of_the_same_version() {
        let mut retries = Retries::default();
        let start = Instant::now();
        retries.push(received("gw-a", 1), start, start);
        retries.push(received("gw-a", 2), start, start);
        assert!(retries.supersede(&gateway(), version(1)));
        assert!(!retries.supersede(&gateway(), version(1)));
        let due = retries.take_due(start + REPORT_RETRY_DELAY);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].received.report.version, version(2));
    }

    #[test]
    fn a_full_queue_gives_up_the_oldest_report() {
        let mut retries = Retries::default();
        let start = Instant::now();
        for value in 1..=u64::try_from(MAX_PENDING_REPORTS).expect("fits") {
            assert_eq!(
                retries.push(received("gw-a", value), start, start),
                Queued::Waiting
            );
        }
        let Queued::Evicted(evicted) = retries.push(received("gw-b", 1), start, start) else {
            panic!("the queue is full");
        };
        assert_eq!(evicted.received, received("gw-a", 1));
        assert_eq!(retries.len(), MAX_PENDING_REPORTS);
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
