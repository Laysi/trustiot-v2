//! What a gateway's status or heartbeat does to its stored presence (services/gateway-ops/
//! README.md, "在線狀態"): a pure function of the stored row and the message, so the order rules
//! of crates/protocol ("在線狀態與心跳") are tested without a broker or a database.
//!
//! Every connection of a gateway is a new session. The rules:
//!
//! - **The latest `online` names the session.** An `online` of another session than the stored
//!   one replaces it: the gateway reconnected, and the last will of the old connection may still
//!   come. The same session's `online` again (the broker replays the retained status to every
//!   new subscription) says nothing new and is ignored, so it neither refreshes `last_seen_at`
//!   nor undoes a heartbeat timeout.
//! - **An `offline` counts only for the stored session.** A last will (or a `shutdown`) of
//!   another session is an older connection's, which crates/protocol says to ignore.
//! - **An ended session never comes back.** After the session's own `shutdown` or last will, its
//!   late `online` or heartbeats are ignored: a session ID is never reused, so they are stale.
//! - **A heartbeat proves the session is connected.** It refreshes the stored session; it brings
//!   back a gateway that is offline for another session, or whose heartbeats were only overdue
//!   (`heartbeat_timeout`, which only a late heartbeat of the session undoes). A
//!   heartbeat of another session than an online row's is ignored: the session's `online`
//!   decides, or the timeout does when it never comes.
//! - **A heartbeat counts once.** A heartbeat of the stored session that is not newer (by its
//!   `sent_at`, the gateway's clock) than the session's latest stored one is ignored: every
//!   replica receives every heartbeat, and a replica that lags behind must not roll the
//!   configuration version or the interval back.
//! - **First contact.** Without a row, any status or heartbeat writes one.
//!
//! Every replica receives every status and heartbeat in the order the broker delivers them, and
//! applies each one in its own transaction on the locked row. A replica that lags behind can
//! apply an older session's `online` after another replica applied a newer one, and so put the
//! older session back for a moment; its own stream holds the newer session's messages after it,
//! so the row is right again once that replica catches up, whatever the interleaving
//! (`replicas_in_any_interleaving_end_in_the_state_of_one`). If the lagging replica loses those
//! messages instead (a database pause, a broker queue limit), the heartbeat timeout ends the
//! older session and the newer session's next heartbeat brings the gateway back.

use protocol::health::OfflineReason;
use protocol::{AgentVersion, ConfigVersion, SessionId, UnixMillis};

/// Why a gateway is offline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offline {
    /// The gateway said so before a clean disconnect (`shutdown`).
    Shutdown,
    /// The broker published the gateway's last will (`connection_lost`).
    ConnectionLost,
    /// No heartbeat arrived within the time the gateway's interval allows (the service's own
    /// judgement, which a heartbeat or `online` of the same session undoes).
    HeartbeatTimeout,
}

impl Offline {
    /// The name stored and returned by the API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Shutdown => "shutdown",
            Self::ConnectionLost => "connection_lost",
            Self::HeartbeatTimeout => "heartbeat_timeout",
        }
    }

    /// The stored name read back.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "shutdown" => Some(Self::Shutdown),
            "connection_lost" => Some(Self::ConnectionLost),
            "heartbeat_timeout" => Some(Self::HeartbeatTimeout),
            _ => None,
        }
    }

    /// Whether the session is over for certain: the gateway or the broker said so.
    const fn ended(self) -> bool {
        matches!(self, Self::Shutdown | Self::ConnectionLost)
    }
}

/// A gateway's presence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Connected.
    Online,
    /// Not connected, and why.
    Offline(Offline),
}

impl State {
    /// The stored `state` column.
    #[must_use]
    pub const fn column(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Offline(_) => "offline",
        }
    }

    /// The stored `offline_reason` column.
    #[must_use]
    pub const fn reason(self) -> Option<Offline> {
        match self {
            Self::Online => None,
            Self::Offline(reason) => Some(reason),
        }
    }
}

/// What the stored row says, as far as the rules care.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    /// The state.
    pub state: State,
    /// The session it is about.
    pub session: SessionId,
    /// The `sent_at` of the session's latest stored heartbeat; `None` before its first.
    pub heartbeat_sent_at: Option<UnixMillis>,
}

/// A gateway's message, as far as the rules care.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    /// `up/status` `online`.
    Online {
        /// The new connection.
        session: SessionId,
        /// The agent's release.
        agent_version: AgentVersion,
    },
    /// `up/status` `offline`: the gateway's `shutdown` or the broker's last will.
    Offline {
        /// The connection that ended.
        session: SessionId,
        /// Why.
        reason: Offline,
    },
    /// `up/heartbeat`.
    Heartbeat {
        /// The connection that sent it.
        session: SessionId,
        /// When the gateway sent it, by its clock.
        sent_at: UnixMillis,
        /// The agent's release.
        agent_version: AgentVersion,
        /// Seconds until the next heartbeat (at least 1, crates/protocol).
        interval_s: u32,
        /// The configuration version the gateway polls with.
        config_version: Option<ConfigVersion>,
    },
}

impl Signal {
    /// The session the message belongs to.
    #[must_use]
    pub fn session(&self) -> &SessionId {
        match self {
            Self::Online { session, .. }
            | Self::Offline { session, .. }
            | Self::Heartbeat { session, .. } => session,
        }
    }

    /// The `offline` of a protocol status, or `None` for a reason this build does not know,
    /// which the rules cannot place.
    #[must_use]
    pub fn offline(session: SessionId, reason: &OfflineReason) -> Option<Self> {
        let reason = match reason {
            OfflineReason::Shutdown => Offline::Shutdown,
            OfflineReason::ConnectionLost => Offline::ConnectionLost,
            OfflineReason::Other(_) => return None,
        };
        Some(Self::Offline { session, reason })
    }
}

/// Why a message leaves the row as it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ignored {
    /// It belongs to another session than the stored one, which it cannot override.
    OtherSession,
    /// Its session already ended (`shutdown` or last will).
    EndedSession,
    /// The row says the same already.
    Repeated,
    /// A heartbeat older than the session's latest stored one.
    Older,
    /// A retained status the broker replayed on subscribing, for a binding without a row: it
    /// may be from an earlier binding (decided by the store, which knows the retain flag).
    Replayed,
}

impl Ignored {
    /// The metric attribute.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OtherSession => "other_session",
            Self::EndedSession => "ended_session",
            Self::Repeated => "repeated",
            Self::Older => "older",
            Self::Replayed => "replayed",
        }
    }
}

/// What to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Write {
    /// The state afterwards.
    pub state: State,
    /// The session afterwards: the message's.
    pub session: SessionId,
    /// Whether the state differs from the stored one (or there was none): `changed_at` moves
    /// and the change is logged.
    pub changed: bool,
    /// The session's latest heartbeat afterwards: a heartbeat's own `sent_at`, the stored one
    /// for another message of the same session, `None` for a new session.
    pub heartbeat_sent_at: Option<UnixMillis>,
}

/// What a message does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Write the row: the state and session below, the message's time as `last_seen_at`, and
    /// the message's agent version and heartbeat fields where it has them.
    Write(Write),
    /// Leave the row as it is.
    Ignore(Ignored),
}

/// What `signal` does to the stored row `stored` (`None`: no row yet); see the module.
#[must_use]
pub fn decide(stored: Option<&Stored>, signal: &Signal) -> Decision {
    let session = signal.session();
    let same = stored.is_some_and(|stored| stored.session == *session);
    let heartbeat_sent_at = match signal {
        Signal::Heartbeat { sent_at, .. } => Some(*sent_at),
        Signal::Online { .. } | Signal::Offline { .. } if same => {
            stored.and_then(|stored| stored.heartbeat_sent_at)
        }
        Signal::Online { .. } | Signal::Offline { .. } => None,
    };
    let write = |state: State| {
        Decision::Write(Write {
            state,
            session: session.clone(),
            changed: stored.is_none_or(|stored| stored.state.column() != state.column()),
            heartbeat_sent_at,
        })
    };
    let Some(stored) = stored else {
        return match signal {
            Signal::Online { .. } | Signal::Heartbeat { .. } => write(State::Online),
            Signal::Offline { reason, .. } => write(State::Offline(*reason)),
        };
    };
    // A heartbeat of the stored session that another replica, or an earlier delivery, already
    // brought, or an older one.
    let not_newer = |sent_at: UnixMillis| match stored.heartbeat_sent_at {
        Some(latest) if sent_at == latest => Some(Ignored::Repeated),
        Some(latest) if sent_at < latest => Some(Ignored::Older),
        _ => None,
    };
    match (stored.state, signal) {
        (State::Online, Signal::Online { .. }) if same => Decision::Ignore(Ignored::Repeated),
        // A reconnect.
        (State::Online, Signal::Online { .. }) => write(State::Online),
        (State::Online, Signal::Heartbeat { sent_at, .. }) if same => {
            not_newer(*sent_at).map_or_else(|| write(State::Online), Decision::Ignore)
        }
        (State::Online, Signal::Offline { reason, .. }) if same => write(State::Offline(*reason)),
        (State::Online, Signal::Offline { .. } | Signal::Heartbeat { .. }) => {
            Decision::Ignore(Ignored::OtherSession)
        }
        (State::Offline(was), Signal::Online { .. } | Signal::Heartbeat { .. }) if same => {
            if was.ended() {
                Decision::Ignore(Ignored::EndedSession)
            } else if let Signal::Heartbeat { sent_at, .. } = signal {
                not_newer(*sent_at).map_or_else(|| write(State::Online), Decision::Ignore)
            } else {
                Decision::Ignore(Ignored::Repeated)
            }
        }
        (State::Offline(_), Signal::Online { .. } | Signal::Heartbeat { .. }) => {
            write(State::Online)
        }
        // The overdue session's own last will or shutdown says why it is offline.
        (State::Offline(Offline::HeartbeatTimeout), Signal::Offline { reason, .. }) if same => {
            write(State::Offline(*reason))
        }
        (State::Offline(_), Signal::Offline { .. }) => Decision::Ignore(if same {
            Ignored::Repeated
        } else {
            Ignored::OtherSession
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str) -> SessionId {
        SessionId::new(name).expect("a session ID")
    }

    fn online(name: &str) -> Signal {
        Signal::Online {
            session: session(name),
            agent_version: AgentVersion::new("1.0.0").expect("a version"),
        }
    }

    fn offline(name: &str, reason: Offline) -> Signal {
        Signal::Offline {
            session: session(name),
            reason,
        }
    }

    fn heartbeat(name: &str) -> Signal {
        heartbeat_at(name, 1_000)
    }

    fn heartbeat_at(name: &str, sent_at: u64) -> Signal {
        Signal::Heartbeat {
            session: session(name),
            sent_at: UnixMillis::new(sent_at),
            agent_version: AgentVersion::new("1.0.0").expect("a version"),
            interval_s: 60,
            config_version: ConfigVersion::new(3),
        }
    }

    fn stored(state: State, name: &str) -> Stored {
        Stored {
            state,
            session: session(name),
            heartbeat_sent_at: None,
        }
    }

    fn write(state: State, name: &str, changed: bool) -> Decision {
        write_sent(state, name, changed, None)
    }

    fn write_sent(state: State, name: &str, changed: bool, sent_at: Option<u64>) -> Decision {
        Decision::Write(Write {
            state,
            session: session(name),
            changed,
            heartbeat_sent_at: sent_at.map(UnixMillis::new),
        })
    }

    /// The row after `signal`, as the store writes it.
    fn apply(row: Option<Stored>, signal: &Signal) -> Option<Stored> {
        match decide(row.as_ref(), signal) {
            Decision::Write(write) => Some(Stored {
                state: write.state,
                session: write.session,
                heartbeat_sent_at: write.heartbeat_sent_at,
            }),
            Decision::Ignore(_) => row,
        }
    }

    /// Applies the messages in order, as one replica does, from no row.
    fn replay(signals: &[Signal]) -> Option<Stored> {
        signals.iter().fold(None, apply)
    }

    /// The state and session of a row, without the heartbeat fence.
    fn presence(row: Option<&Stored>) -> Option<(State, SessionId)> {
        row.map(|row| (row.state, row.session.clone()))
    }

    #[test]
    fn the_first_message_writes_a_row() {
        assert_eq!(
            decide(None, &online("s1")),
            write(State::Online, "s1", true)
        );
        assert_eq!(
            decide(None, &heartbeat("s1")),
            write_sent(State::Online, "s1", true, Some(1_000))
        );
        assert_eq!(
            decide(None, &offline("s1", Offline::ConnectionLost)),
            write(State::Offline(Offline::ConnectionLost), "s1", true)
        );
    }

    #[test]
    fn the_session_goes_offline_by_its_last_will_or_shutdown() {
        let row = stored(State::Online, "s1");
        for reason in [Offline::ConnectionLost, Offline::Shutdown] {
            assert_eq!(
                decide(Some(&row), &offline("s1", reason)),
                write(State::Offline(reason), "s1", true)
            );
        }
    }

    #[test]
    fn an_older_sessions_last_will_after_the_new_online_is_ignored() {
        let row = replay(&[online("s1"), online("s2")]).expect("a row");
        assert_eq!(row, stored(State::Online, "s2"));
        assert_eq!(
            decide(Some(&row), &offline("s1", Offline::ConnectionLost)),
            Decision::Ignore(Ignored::OtherSession)
        );
        assert_eq!(
            replay(&[
                online("s1"),
                online("s2"),
                offline("s1", Offline::ConnectionLost)
            ]),
            Some(stored(State::Online, "s2"))
        );
    }

    #[test]
    fn a_reconnect_is_online_without_a_change() {
        let row = stored(State::Online, "s1");
        assert_eq!(
            decide(Some(&row), &online("s2")),
            write(State::Online, "s2", false)
        );
        // A retained replay of the same status says nothing new.
        assert_eq!(
            decide(Some(&row), &online("s1")),
            Decision::Ignore(Ignored::Repeated)
        );
    }

    #[test]
    fn heartbeats_refresh_their_own_session_only() {
        let row = stored(State::Online, "s1");
        assert_eq!(
            decide(Some(&row), &heartbeat("s1")),
            write_sent(State::Online, "s1", false, Some(1_000))
        );
        assert_eq!(
            decide(Some(&row), &heartbeat("s0")),
            Decision::Ignore(Ignored::OtherSession)
        );
    }

    #[test]
    fn an_ended_session_never_comes_back() {
        for reason in [Offline::ConnectionLost, Offline::Shutdown] {
            let row = stored(State::Offline(reason), "s1");
            assert_eq!(
                decide(Some(&row), &online("s1")),
                Decision::Ignore(Ignored::EndedSession)
            );
            assert_eq!(
                decide(Some(&row), &heartbeat("s1")),
                Decision::Ignore(Ignored::EndedSession)
            );
            assert_eq!(
                decide(Some(&row), &offline("s1", Offline::ConnectionLost)),
                Decision::Ignore(Ignored::Repeated)
            );
        }
    }

    #[test]
    fn a_new_session_brings_an_offline_gateway_back() {
        let row = stored(State::Offline(Offline::ConnectionLost), "s1");
        assert_eq!(
            decide(Some(&row), &online("s2")),
            write(State::Online, "s2", true)
        );
        assert_eq!(
            decide(Some(&row), &heartbeat("s2")),
            write_sent(State::Online, "s2", true, Some(1_000))
        );
        // Another session's last will keeps the row as it is.
        assert_eq!(
            decide(Some(&row), &offline("s2", Offline::ConnectionLost)),
            Decision::Ignore(Ignored::OtherSession)
        );
    }

    #[test]
    fn an_overdue_session_comes_back_with_a_late_heartbeat_only() {
        let row = stored(State::Offline(Offline::HeartbeatTimeout), "s1");
        assert_eq!(
            decide(Some(&row), &heartbeat("s1")),
            write_sent(State::Online, "s1", true, Some(1_000))
        );
        // The retained `online` replayed to a new subscription proves nothing.
        assert_eq!(
            decide(Some(&row), &online("s1")),
            Decision::Ignore(Ignored::Repeated)
        );
        assert_eq!(
            decide(Some(&row), &online("s2")),
            write(State::Online, "s2", true)
        );
        // Its own last will says why it is gone, without a change of state.
        assert_eq!(
            decide(Some(&row), &offline("s1", Offline::ConnectionLost)),
            write(State::Offline(Offline::ConnectionLost), "s1", false)
        );
        assert_eq!(
            decide(Some(&row), &offline("s0", Offline::ConnectionLost)),
            Decision::Ignore(Ignored::OtherSession)
        );
    }

    #[test]
    fn the_protocols_orders_end_in_the_right_state() {
        // A reconnect whose old last will arrives after the new online.
        assert_eq!(
            replay(&[
                online("s1"),
                heartbeat("s1"),
                online("s2"),
                offline("s1", Offline::ConnectionLost),
                heartbeat("s2"),
            ]),
            Some(Stored {
                heartbeat_sent_at: Some(UnixMillis::new(1_000)),
                ..stored(State::Online, "s2")
            })
        );
        // A reconnect whose old last will arrives first.
        assert_eq!(
            replay(&[
                online("s1"),
                offline("s1", Offline::ConnectionLost),
                online("s2"),
            ]),
            Some(stored(State::Online, "s2"))
        );
        // A clean stop, then a straggling heartbeat of the stopped session.
        assert_eq!(
            replay(&[
                online("s1"),
                offline("s1", Offline::Shutdown),
                heartbeat("s1"),
            ]),
            Some(stored(State::Offline(Offline::Shutdown), "s1"))
        );
    }

    #[test]
    fn a_heartbeat_counts_once_and_never_rolls_back() {
        let row = replay(&[online("s1"), heartbeat_at("s1", 2_000)]).expect("a row");
        assert_eq!(row.heartbeat_sent_at, Some(UnixMillis::new(2_000)));
        // The same heartbeat from another replica, or redelivered.
        assert_eq!(
            decide(Some(&row), &heartbeat_at("s1", 2_000)),
            Decision::Ignore(Ignored::Repeated)
        );
        // An older one that a lagging replica brings late.
        assert_eq!(
            decide(Some(&row), &heartbeat_at("s1", 1_000)),
            Decision::Ignore(Ignored::Older)
        );
        assert_eq!(
            decide(Some(&row), &heartbeat_at("s1", 3_000)),
            write_sent(State::Online, "s1", false, Some(3_000))
        );
        // Nor does an older heartbeat undo a timeout.
        let overdue = Stored {
            state: State::Offline(Offline::HeartbeatTimeout),
            ..row.clone()
        };
        assert_eq!(
            decide(Some(&overdue), &heartbeat_at("s1", 2_000)),
            Decision::Ignore(Ignored::Repeated)
        );
        assert_eq!(
            decide(Some(&overdue), &heartbeat_at("s1", 3_000)),
            write_sent(State::Online, "s1", true, Some(3_000))
        );
        // The same session's last will keeps the fence; a new session starts without one, even
        // when its gateway's clock went back.
        assert_eq!(
            decide(Some(&row), &offline("s1", Offline::ConnectionLost)),
            write_sent(
                State::Offline(Offline::ConnectionLost),
                "s1",
                true,
                Some(2_000)
            )
        );
        let reconnected = apply(Some(row), &online("s2")).expect("a row");
        assert_eq!(reconnected.heartbeat_sent_at, None);
        assert_eq!(
            decide(Some(&reconnected), &heartbeat_at("s2", 500)),
            write_sent(State::Online, "s2", false, Some(500))
        );
    }

    /// Every way of interleaving the work of two replicas, each applying the whole `stream` in
    /// order, one message per transaction.
    fn interleavings(stream: &[Signal], each: &mut dyn FnMut(Option<&Stored>)) {
        fn walk(
            stream: &[Signal],
            row: Option<Stored>,
            a: usize,
            b: usize,
            each: &mut dyn FnMut(Option<&Stored>),
        ) {
            if a == stream.len() && b == stream.len() {
                each(row.as_ref());
                return;
            }
            if a < stream.len() {
                walk(stream, apply(row.clone(), &stream[a]), a + 1, b, each);
            }
            if b < stream.len() {
                walk(stream, apply(row, &stream[b]), a, b + 1, each);
            }
        }
        walk(stream, None, 0, 0, each);
    }

    #[test]
    fn replicas_in_any_interleaving_end_in_the_state_of_one() {
        // Reconnects with the old session's last will before and after the new `online`, a
        // clean stop, and heartbeats in between: the orders crates/protocol allows.
        let streams = [
            vec![
                online("s1"),
                heartbeat_at("s1", 1),
                heartbeat_at("s1", 2),
                online("s2"),
                offline("s1", Offline::ConnectionLost),
                heartbeat_at("s2", 3),
                heartbeat_at("s2", 4),
            ],
            vec![
                online("s1"),
                heartbeat_at("s1", 1),
                offline("s1", Offline::ConnectionLost),
                online("s2"),
                heartbeat_at("s2", 2),
                offline("s2", Offline::Shutdown),
                online("s3"),
            ],
            vec![
                online("s1"),
                heartbeat_at("s1", 5),
                online("s2"),
                heartbeat_at("s2", 1),
                online("s3"),
                offline("s2", Offline::ConnectionLost),
                offline("s1", Offline::ConnectionLost),
                heartbeat_at("s3", 2),
            ],
        ];
        for stream in streams {
            let alone = replay(&stream);
            let mut runs = 0_u32;
            interleavings(&stream, &mut |row| {
                runs += 1;
                assert_eq!(
                    presence(row),
                    presence(alone.as_ref()),
                    "two replicas ended elsewhere than one: {stream:?}"
                );
            });
            assert!(runs > 1000, "{runs} interleavings");
        }
    }

    #[test]
    fn unknown_offline_reasons_are_not_placed() {
        assert!(Signal::offline(session("s1"), &OfflineReason::Other("later".into())).is_none());
        assert_eq!(
            Signal::offline(session("s1"), &OfflineReason::Shutdown),
            Some(offline("s1", Offline::Shutdown))
        );
    }

    #[test]
    fn reasons_round_trip_through_their_names() {
        for reason in [
            Offline::Shutdown,
            Offline::ConnectionLost,
            Offline::HeartbeatTimeout,
        ] {
            assert_eq!(Offline::parse(reason.as_str()), Some(reason));
        }
        assert_eq!(Offline::parse("online"), None);
    }
}
