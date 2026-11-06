//! The agent's side of the fieldbus allowlist (Laysi/trustiot-v2#865): which TCP destinations
//! the agent hands to the component that keeps them in the default deny's sets
//! (`trustiot-agent-allowlist`, [`crate::allowlist`], Laysi/trustiot-v2#864), when, and the gate
//! that keeps a Modbus connection from opening before the list that holds its destination is
//! applied.
//!
//! - **The list** is every endpoint (host and port) of the applied configuration with its
//!   addresses: an IP address as it is, a DNS name with the addresses the address guard accepted
//!   when it last resolved the name for a connection. At most [`MAX_ADDRESSES_PER_HOST`] of
//!   them, the first in the resolver's order, which is the order the guard tries them in;
//!   addresses past that are neither listed nor connected to. With at most
//!   [`protocol::config::MAX_DEVICES`] endpoints, no configuration can push the list past
//!   [`crate::allowlist::MAX_ENTRIES`], so one device's name can never make every device's update
//!   fail. An address the guard refuses is left out on its own; the others stay.
//! - **When it is sent** ([`Allowance::run`]): once the first configuration (the saved one, or
//!   none) is applied, then whenever the list changes, and again on [`Allowance::resend`] (the
//!   agent's `SIGHUP`, which the unit's `ExecReload=` sends after a load of the rules that empties
//!   the sets). Each request is the complete list, so a lost or repeated one leaves nothing
//!   behind. Without `ok` it is sent again after 1 s, doubling up to 60 s.
//! - **The gate** ([`Allowance::permit`]): a connection to an endpoint first records the
//!   addresses it resolved, then waits, at most [`PERMIT_WAIT`], until a list at least as new as
//!   that is applied, and connects only to addresses in it. A device added to the configuration
//!   is thus polled only after the list with its destination came back `ok`; a device removed
//!   leaves the list only after its polling stopped ([`crate::desired::fleet`] updates the
//!   endpoints between the two); and a name that resolves to an address the applied list lacks
//!   waits for a new list before it is connected to.
//!
//! The kernel enforces the list (the default deny refuses every other TCP destination of the
//! agent's account); the gate only keeps the agent from trying a destination the list does not
//! hold yet. Neither is a limit on a compromised agent, which can send any list
//! (`README.md`, "fieldbus 放行集合", threat model).
//!
//! Until the image installs the component (Laysi/trustiot-v2#866) there is no socket: the agent
//! then sends nothing and polls as before ([`installed`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::FileTypeExt as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::allowlist::{Allowlist, Destination, MAX_ENTRIES, Reply};
use crate::modbus::address::{Interfaces, refusal};

/// The most addresses one host contributes to the list: the first ones the resolver returns
/// that the address guard accepts. A configuration has at most
/// [`protocol::config::MAX_DEVICES`] endpoints, so the list holds at most 4000 destinations,
/// within [`MAX_ENTRIES`].
pub const MAX_ADDRESSES_PER_HOST: usize = 4;

const _: () = assert!(protocol::config::MAX_DEVICES * MAX_ADDRESSES_PER_HOST <= MAX_ENTRIES);

/// How long a connection waits for the list that holds its destination to be applied. The
/// exchange takes milliseconds; a connection that waits longer fails and is tried again after
/// the polling's backoff.
pub const PERMIT_WAIT: Duration = Duration::from_secs(5);

/// The first delay before a list is sent again after an attempt without `ok`, and the longest.
/// At least a second, so a failing component never sees a tight loop (its socket accepts at
/// most 10 connections in 2 s).
pub const RETRY_FIRST: Duration = Duration::from_secs(1);
/// See [`RETRY_FIRST`].
pub const RETRY_CAP: Duration = Duration::from_secs(60);

/// How long one exchange with the component may take: its unit ends it after 30 s
/// (`RuntimeMaxSec=`), and its answer comes within milliseconds otherwise.
const EXCHANGE_LIMIT: Duration = Duration::from_secs(40);

/// The longest answer read from the component: one line.
const MAX_ANSWER: u64 = 4096;

/// Whether the component's socket is at `socket`: the image installs the component, its socket
/// unit and the default deny together (Laysi/trustiot-v2#866), and the agent's unit starts the
/// socket before the agent. Without it the agent sends no list and polls as before: no default
/// deny is loaded either, and if one were, its empty sets would refuse the fieldbus (fail
/// closed), not open anything.
pub fn installed(socket: &Path) -> bool {
    std::fs::symlink_metadata(socket).is_ok_and(|metadata| metadata.file_type().is_socket())
}

/// The delay before attempt `failures + 1` after `failures` attempts in a row without `ok`:
/// [`RETRY_FIRST`] doubling up to [`RETRY_CAP`].
pub fn retry_delay(failures: u32) -> Duration {
    let factor = 1_u32.checked_shl(failures.min(31)).unwrap_or(u32::MAX);
    RETRY_FIRST
        .checked_mul(factor)
        .unwrap_or(RETRY_CAP)
        .min(RETRY_CAP)
}

/// Why a connection may not open (yet).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// The endpoint is not in the applied configuration.
    #[error("the endpoint is not in the configuration's fieldbus allowlist")]
    NotConfigured,
    /// None of the host's addresses can be listed.
    #[error("no address of the host can be put in the fieldbus allowlist")]
    NothingToList,
    /// No list with the destination came back `ok` within [`PERMIT_WAIT`].
    #[error("the fieldbus allowlist with this destination has not been applied yet{}", last_failure(.0))]
    NotApplied(Option<String>),
}

fn last_failure(failure: &Option<String>) -> String {
    failure
        .as_ref()
        .map_or_else(String::new, |failure| format!(" (last attempt: {failure})"))
}

/// The list as last applied: its generation (0 before the first) and its content.
#[derive(Clone, Debug, Default)]
struct Applied {
    generation: u64,
    list: Arc<Allowlist>,
}

#[derive(Debug, Default)]
struct State {
    /// Every endpoint of the applied configuration, host (canonical) and port, with the
    /// destinations it contributes.
    endpoints: BTreeMap<(String, u16), BTreeSet<Destination>>,
    /// Bumped by every change of the list and every resend: the list of a generation is the one
    /// the endpoints held then.
    generation: u64,
    /// Set by the first [`Allowance::configure`]: nothing is sent before it.
    configured: bool,
    /// Why the last attempt failed, for the gate's error; cleared by an `ok`.
    failure: Option<String>,
}

impl State {
    fn list(&self) -> Allowlist {
        // Within MAX_ENTRIES by MAX_ADDRESSES_PER_HOST (the assertion above).
        Allowlist::new(self.endpoints.values().flatten().copied()).unwrap_or_default()
    }
}

struct Shared {
    socket: PathBuf,
    state: Mutex<State>,
    /// The latest generation, for the sender.
    wanted: watch::Sender<u64>,
    /// Counts [`Allowance::resend`]s: one cuts the sender's backoff short.
    resent: watch::Sender<u64>,
    applied: watch::Sender<Applied>,
}

/// The agent's fieldbus allowlist: what [`crate::desired::fleet::Fleet`] configures, what the
/// address guard asks before it connects, and what [`Allowance::run`] sends. A clone is another
/// handle to the same list.
#[derive(Clone)]
pub struct Allowance(Arc<Shared>);

impl fmt::Debug for Allowance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state();
        f.debug_struct("Allowance")
            .field("socket", &self.0.socket)
            .field("endpoints", &state.endpoints.len())
            .field("generation", &state.generation)
            .field("applied", &self.0.applied.borrow().generation)
            .finish()
    }
}

impl Allowance {
    /// An empty list, sent to the component at `socket` ([`crate::allowlist::SOCKET`] in
    /// production) once [`Allowance::run`] runs and the first configuration is applied.
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self(Arc::new(Shared {
            socket: socket.into(),
            state: Mutex::new(State::default()),
            wanted: watch::channel(0).0,
            resent: watch::channel(0).0,
            applied: watch::channel(Applied::default()).0,
        }))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Bumps the generation and wakes the sender.
    fn bump(&self, state: &mut State) -> u64 {
        state.generation += 1;
        self.0.wanted.send_replace(state.generation);
        state.generation
    }

    /// The endpoints of the configuration being applied, as host (canonical, as
    /// [`protocol::config::Host::canonical`] writes it) and port. An endpoint that stays keeps
    /// the addresses it has; a new one starts with its address when the host is an IP address
    /// the address guard accepts, and with none when it is a name (its addresses come with the
    /// first connection, [`Allowance::permit`]); one that is gone leaves the list. The list is
    /// sent when it changed, and always the first time.
    pub fn configure(&self, endpoints: impl IntoIterator<Item = (String, u16)>) {
        // A read failure leaves IP addresses for the connection to add, which fails too then.
        let interfaces = Interfaces::read().ok();
        let mut state = self.state();
        let before = state.list();
        let mut next = BTreeMap::new();
        for (host, port) in endpoints {
            let destinations = state
                .endpoints
                .remove(&(host.clone(), port))
                .unwrap_or_else(|| literal(&host, port, interfaces.as_ref()));
            next.insert((host, port), destinations);
        }
        state.endpoints = next;
        if !state.configured || state.list() != before {
            state.configured = true;
            self.bump(&mut state);
        }
    }

    /// The addresses a connection to `host` and `port` may dial, out of `allowed` (what the host
    /// resolved to and the address guard accepted, in the resolver's order), once the list that
    /// holds them is applied: the first [`MAX_ADDRESSES_PER_HOST`] that can be listed, as the
    /// endpoint's addresses from now on. Waits at most [`PERMIT_WAIT`].
    ///
    /// # Errors
    ///
    /// [`Refused`]: the endpoint is not configured, no address can be listed, or no list with
    /// them came back `ok` in time.
    pub async fn permit(
        &self,
        host: &str,
        port: u16,
        allowed: &[SocketAddr],
    ) -> Result<Vec<SocketAddr>, Refused> {
        let wanted: Vec<Destination> = allowed
            .iter()
            .filter_map(|address| Destination::new(address.ip(), address.port()).ok())
            .take(MAX_ADDRESSES_PER_HOST)
            .collect();
        let needed = {
            let mut state = self.state();
            let Some(current) = state.endpoints.get_mut(&(host.to_owned(), port)) else {
                return Err(Refused::NotConfigured);
            };
            let set: BTreeSet<Destination> = wanted.iter().copied().collect();
            if *current != set {
                *current = set;
                self.bump(&mut state);
            }
            state.generation
        };
        if wanted.is_empty() {
            return Err(Refused::NothingToList);
        }
        let mut applied = self.0.applied.subscribe();
        let applied = tokio::time::timeout(
            PERMIT_WAIT,
            applied.wait_for(|applied| applied.generation >= needed),
        )
        .await;
        let list = match applied {
            Ok(Ok(applied)) => Arc::clone(&applied.list),
            // The sender is gone only when the agent stops.
            Ok(Err(_)) | Err(_) => return Err(Refused::NotApplied(self.state().failure.clone())),
        };
        let permitted: Vec<SocketAddr> = wanted
            .iter()
            .filter(|destination| list.contains(destination))
            .map(|destination| SocketAddr::new(destination.address(), destination.port()))
            .collect();
        if permitted.is_empty() {
            return Err(Refused::NotApplied(None));
        }
        Ok(permitted)
    }

    /// Forgets the addresses of `host` and `port` when it no longer resolves to an address the
    /// address guard accepts, so the next list leaves them out.
    pub fn forget(&self, host: &str, port: u16) {
        let mut state = self.state();
        if let Some(current) = state.endpoints.get_mut(&(host.to_owned(), port))
            && !current.is_empty()
        {
            current.clear();
            self.bump(&mut state);
        }
    }

    /// Sends the list again, changed or not, and makes connections wait for its `ok`: a load of
    /// the rules that empties the sets (`nftables.service`'s reload) leaves them empty until the
    /// agent sends again, and the component cannot tell.
    pub fn resend(&self) {
        let mut state = self.state();
        let generation = self.bump(&mut state);
        self.0
            .resent
            .send_modify(|count| *count = count.wrapping_add(1));
        tracing::info!(
            generation,
            "sending the fieldbus allowlist again (the agent's unit was reloaded)"
        );
    }

    /// The list as last applied, and its generation (0 before the first `ok`).
    pub fn applied(&self) -> (u64, Arc<Allowlist>) {
        let applied = self.0.applied.borrow();
        (applied.generation, Arc::clone(&applied.list))
    }

    /// The list as it is now, applied or not.
    pub fn wanted(&self) -> Allowlist {
        self.state().list()
    }

    /// Sends the list whenever a newer one is wanted, until `shutdown`; after an attempt without
    /// `ok`, again after [`retry_delay`], or sooner after a [`Allowance::resend`] (but at least
    /// [`RETRY_FIRST`] after the failed attempt).
    pub async fn run(self, shutdown: CancellationToken) {
        let mut wanted = self.0.wanted.subscribe();
        let mut resent = self.0.resent.subscribe();
        let mut failures = 0_u32;
        loop {
            let pending = {
                let state = self.state();
                (state.configured && state.generation > self.0.applied.borrow().generation)
                    .then(|| (state.generation, state.list()))
            };
            let Some((generation, list)) = pending else {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    changed = wanted.changed() => if changed.is_err() { return },
                }
                continue;
            };
            // A resend from here on ends the wait after a failure of this attempt early.
            resent.borrow_and_update();
            let outcome = tokio::select! {
                () = shutdown.cancelled() => return,
                outcome = exchange(&self.0.socket, &list) => outcome,
            };
            match outcome {
                Ok(()) => {
                    let (ipv4, ipv6) = list.families();
                    tracing::info!(
                        generation,
                        entries = list.len(),
                        ipv4,
                        ipv6,
                        digest = %list.digest(),
                        "applied the fieldbus allowlist"
                    );
                    failures = 0;
                    self.state().failure = None;
                    self.0.applied.send_replace(Applied {
                        generation,
                        list: Arc::new(list),
                    });
                }
                Err(error) => {
                    let delay = retry_delay(failures);
                    failures = failures.saturating_add(1);
                    tracing::warn!(
                        %error,
                        generation,
                        entries = list.len(),
                        retry_in_s = delay.as_secs(),
                        "the fieldbus allowlist was not applied; the devices it adds are not \
                         polled until it is"
                    );
                    self.state().failure = Some(error);
                    // A resend (the agent's unit was reloaded after a load of the rules, the
                    // usual end of a failure) ends the wait early, never sooner than RETRY_FIRST
                    // after this attempt, and starts the delays over. Only root reloads the
                    // unit, so it is no loop the agent can drive.
                    // (`self` holds the sender, so `changed` never fails.)
                    let reloaded = async {
                        tokio::time::sleep(RETRY_FIRST).await;
                        if resent.changed().await.is_err() {
                            std::future::pending::<()>().await;
                        }
                    };
                    tokio::select! {
                        () = shutdown.cancelled() => return,
                        () = tokio::time::sleep(delay) => {}
                        () = reloaded => failures = 0,
                    }
                }
            }
        }
    }
}

/// The destination of an endpoint whose host is an IP address the address guard accepts; none
/// for a name, or for an address the guard refuses (`interfaces` unknown: only the fixed
/// refusals).
fn literal(host: &str, port: u16, interfaces: Option<&Interfaces>) -> BTreeSet<Destination> {
    let Ok(address) = IpAddr::from_str(host) else {
        return BTreeSet::new();
    };
    if interfaces.is_some_and(|interfaces| refusal(address, interfaces).is_some()) {
        return BTreeSet::new();
    }
    Destination::new(address, port).into_iter().collect()
}

/// Hands `list` to the component and reads its answer: `Ok` only for `ok <n>` with the list's
/// length.
async fn exchange(socket: &Path, list: &Allowlist) -> Result<(), String> {
    let answer = tokio::time::timeout(EXCHANGE_LIMIT, async {
        let mut stream = UnixStream::connect(socket)
            .await
            .map_err(|error| format!("connecting to {}: {error}", socket.display()))?;
        stream
            .write_all(list.encode().as_bytes())
            .await
            .map_err(|error| format!("sending the list: {error}"))?;
        // Closing the writing side ends the request (README.md, "fieldbus 放行集合").
        stream
            .shutdown()
            .await
            .map_err(|error| format!("ending the request: {error}"))?;
        let mut answer = Vec::new();
        (&mut stream)
            .take(MAX_ANSWER)
            .read_to_end(&mut answer)
            .await
            .map_err(|error| format!("reading the answer: {error}"))?;
        Ok::<_, String>(answer)
    })
    .await
    .map_err(|_| format!("no answer within {EXCHANGE_LIMIT:?}"))??;
    let text = String::from_utf8_lossy(&answer);
    let Some(line) = text.lines().next().filter(|line| !line.is_empty()) else {
        return Err(
            "closed without an answer (another update was in progress, or the \
                    component stopped)"
                .to_owned(),
        );
    };
    match Reply::parse(line) {
        Some(Reply::Applied(count)) if count == list.len() => Ok(()),
        Some(Reply::Applied(count)) => Err(format!(
            "the component applied {count} destinations, not the list's {}",
            list.len()
        )),
        Some(reply @ (Reply::Refused(_) | Reply::Failed(_))) => Err(reply.to_string()),
        None => Err(format!("an answer that is not a reply: {line:?}")),
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use testkit::clock::bounded;
    use tokio::net::UnixListener;

    use super::*;

    fn address(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    fn destination(address: &str, port: u16) -> Destination {
        Destination::new(address.parse().unwrap(), port).unwrap()
    }

    fn endpoint(host: &str, port: u16) -> (String, u16) {
        (host.to_owned(), port)
    }

    /// The component's side of the socket: answers each request with the answer the test set
    /// last (`{n}` stands for the request's length, an empty answer closes without one), and
    /// records each request with the time it arrived.
    struct Component {
        answer: Arc<Mutex<&'static str>>,
        requests: Arc<Mutex<Vec<(std::time::Instant, Allowlist)>>>,
        task: tokio::task::JoinHandle<()>,
        _dir: tempdir::Dir,
    }

    mod tempdir {
        /// A directory for the socket, removed when dropped.
        pub(super) struct Dir(pub(super) std::path::PathBuf);

        impl Dir {
            pub(super) fn new() -> Self {
                use std::sync::atomic::{AtomicU32, Ordering};
                static NEXT: AtomicU32 = AtomicU32::new(0);
                let path = std::env::temp_dir().join(format!(
                    "agent-fieldbus-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    impl Component {
        fn start(answer: &'static str) -> (Self, PathBuf) {
            let dir = tempdir::Dir::new();
            let socket = dir.0.join("socket");
            let listener = UnixListener::bind(&socket).unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let answer = Arc::new(Mutex::new(answer));
            let (recorded, current) = (Arc::clone(&requests), Arc::clone(&answer));
            let task = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    stream.read_to_end(&mut request).await.unwrap();
                    let list = Allowlist::parse(&request).unwrap();
                    let reply = current
                        .lock()
                        .unwrap()
                        .replace("{n}", &list.len().to_string());
                    recorded
                        .lock()
                        .unwrap()
                        .push((std::time::Instant::now(), list));
                    if !reply.is_empty() {
                        stream.write_all(reply.as_bytes()).await.unwrap();
                    }
                }
            });
            (
                Self {
                    answer,
                    requests,
                    task,
                    _dir: dir,
                },
                socket,
            )
        }

        fn answer(&self, answer: &'static str) {
            *self.answer.lock().unwrap() = answer;
        }

        fn requests(&self) -> Vec<Allowlist> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|(_, list)| list.clone())
                .collect()
        }

        fn times(&self) -> Vec<std::time::Instant> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|(at, _)| *at)
                .collect()
        }
    }

    impl Drop for Component {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[test]
    fn the_retry_delay_starts_at_a_second_and_doubles_to_a_minute() {
        let delays: Vec<u64> = (0..9).map(|n| retry_delay(n).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(retry_delay(u32::MAX), RETRY_CAP);
    }

    #[test]
    fn no_configuration_can_overflow_the_list() {
        // Every endpoint a distinct name that resolved to more addresses than the cap: still
        // within MAX_ENTRIES.
        let allowance = Allowance::new("/nonexistent");
        let endpoints: Vec<(String, u16)> = (0..protocol::config::MAX_DEVICES)
            .map(|index| endpoint(&format!("meter-{index}.example"), 502))
            .collect();
        allowance.configure(endpoints.clone());
        {
            let mut state = allowance.state();
            for (index, key) in endpoints.iter().enumerate() {
                let index = u32::try_from(index).unwrap();
                state.endpoints.insert(
                    key.clone(),
                    (0..MAX_ADDRESSES_PER_HOST)
                        .map(|n| {
                            Destination::new(
                                IpAddr::V4(Ipv4Addr::from(
                                    0x0a00_0000 + index * 16 + u32::try_from(n).unwrap(),
                                )),
                                502,
                            )
                            .unwrap()
                        })
                        .collect(),
                );
            }
        }
        assert_eq!(
            allowance.wanted().len(),
            protocol::config::MAX_DEVICES * MAX_ADDRESSES_PER_HOST
        );
    }

    #[test]
    fn ip_hosts_are_listed_at_once_names_once_resolved_and_refused_ones_never() {
        let allowance = Allowance::new("/nonexistent");
        allowance.configure([
            endpoint("192.0.2.10", 502),
            endpoint("2001:db8::7", 8886),
            endpoint("plc.example", 502),
            // The address guard refuses these wherever the gateway is: skipped one by one.
            endpoint("127.0.0.1", 502),
            endpoint("169.254.169.254", 80),
        ]);
        assert_eq!(
            allowance.wanted(),
            Allowlist::new([
                destination("192.0.2.10", 502),
                destination("2001:db8::7", 8886)
            ])
            .unwrap()
        );
        assert_eq!(allowance.state().generation, 1, "sent once, the first time");

        // The same configuration again changes nothing, so nothing is sent.
        allowance.configure([
            endpoint("192.0.2.10", 502),
            endpoint("2001:db8::7", 8886),
            endpoint("plc.example", 502),
            endpoint("127.0.0.1", 502),
            endpoint("169.254.169.254", 80),
        ]);
        assert_eq!(allowance.state().generation, 1);

        // A removed endpoint leaves the list.
        allowance.configure([endpoint("2001:db8::7", 8886), endpoint("plc.example", 502)]);
        assert_eq!(
            allowance.wanted(),
            Allowlist::new([destination("2001:db8::7", 8886)]).unwrap()
        );
        assert_eq!(allowance.state().generation, 2);
    }

    #[test]
    fn even_an_empty_first_configuration_is_sent() {
        let allowance = Allowance::new("/nonexistent");
        assert_eq!(allowance.state().generation, 0);
        allowance.configure([]);
        assert_eq!(allowance.state().generation, 1);
        assert!(allowance.wanted().is_empty());
    }

    #[tokio::test]
    async fn a_name_takes_its_first_four_accepted_addresses_and_other_hosts_keep_theirs() {
        let (component, socket) = Component::start("ok {n}\n");
        let allowance = Allowance::new(socket);
        let shutdown = CancellationToken::new();
        let sender = tokio::spawn(allowance.clone().run(shutdown.clone()));
        allowance.configure([endpoint("192.0.2.10", 502), endpoint("plc.example", 502)]);

        // Six addresses that the guard accepted, in the resolver's order: the first four are
        // listed and dialed, the last two neither.
        let resolved: Vec<SocketAddr> = [
            "198.51.100.6:502",
            "198.51.100.1:502",
            "[2001:db8::5]:502",
            "198.51.100.4:502",
            "198.51.100.2:502",
            "198.51.100.3:502",
        ]
        .into_iter()
        .map(address)
        .collect();
        let permitted = bounded(
            "the permit",
            allowance.permit("plc.example", 502, &resolved),
        )
        .await
        .unwrap();
        assert_eq!(permitted, resolved[..4]);
        let (_, applied) = allowance.applied();
        assert_eq!(
            *applied,
            Allowlist::new(
                [
                    "198.51.100.6",
                    "198.51.100.1",
                    "2001:db8::5",
                    "198.51.100.4"
                ]
                .into_iter()
                .map(|a| destination(a, 502))
                .chain([destination("192.0.2.10", 502)])
            )
            .unwrap()
        );

        // The same answer in another order changes nothing; one that drops an address sends a
        // list without it, and the IP host stays.
        let reordered: Vec<SocketAddr> = [3, 2, 1, 0].map(|i| resolved[i]).to_vec();
        let before = component.requests().len();
        bounded(
            "the permit",
            allowance.permit("plc.example", 502, &reordered),
        )
        .await
        .unwrap();
        assert_eq!(
            component.requests().len(),
            before,
            "an unchanged list was sent"
        );
        bounded(
            "the permit",
            allowance.permit("plc.example", 502, &resolved[1..2]),
        )
        .await
        .unwrap();
        assert_eq!(
            component.requests().last().unwrap(),
            &Allowlist::new([
                destination("198.51.100.1", 502),
                destination("192.0.2.10", 502)
            ])
            .unwrap()
        );

        // A name that now resolves to nothing the guard accepts leaves the list.
        allowance.forget("plc.example", 502);
        bounded("the list without the name", async {
            while allowance.applied().1.len() != 1 {
                tokio::task::yield_now().await;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;

        // An endpoint outside the configuration is not listed or permitted.
        assert_eq!(
            allowance
                .permit("other.example", 502, &[address("198.51.100.9:502")])
                .await,
            Err(Refused::NotConfigured)
        );
        // Neither is a host whose addresses the list format refuses (a loopback address the
        // tests exempt from the guard, for example).
        assert_eq!(
            allowance
                .permit("192.0.2.10", 502, &[address("127.0.0.1:502")])
                .await,
            Err(Refused::NothingToList)
        );
        shutdown.cancel();
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn nothing_is_permitted_until_the_component_says_ok_and_failures_retry() {
        // No default deny loaded: the component fails every list.
        let (component, socket) = Component::start("failed: nft: No such file or directory\n");
        let allowance = Allowance::new(socket);
        let shutdown = CancellationToken::new();
        let sender = tokio::spawn(allowance.clone().run(shutdown.clone()));
        allowance.configure([endpoint("192.0.2.10", 502)]);
        let dial = [address("192.0.2.10:502")];
        let refused = allowance.permit("192.0.2.10", 502, &dial).await;
        assert!(
            matches!(&refused, Err(Refused::NotApplied(Some(reason))) if reason.contains("No such file")),
            "{refused:?}"
        );
        // Closed without an answer (another instance was busy): still not ok.
        component.answer("");
        let refused = allowance.permit("192.0.2.10", 502, &dial).await;
        assert!(
            matches!(&refused, Err(Refused::NotApplied(Some(reason))) if reason.contains("without an answer")),
            "{refused:?}"
        );
        component.answer("ok {n}\n");
        // The next attempt comes after the grown backoff, which may be longer than one
        // permit's PERMIT_WAIT: ask again until it is applied.
        let permitted = bounded("the permit once the component says ok", async {
            loop {
                if let Ok(permitted) = allowance.permit("192.0.2.10", 502, &dial).await {
                    break permitted;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(permitted, dial);
        // Every attempt after a failure waited at least RETRY_FIRST, the delays growing.
        let gaps: Vec<Duration> = component
            .times()
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .collect();
        assert!(gaps.len() >= 2, "{gaps:?}");
        for (failures, gap) in gaps.iter().enumerate() {
            assert!(
                *gap >= retry_delay(u32::try_from(failures).unwrap()),
                "attempt {} came {gap:?} after the one before: {gaps:?}",
                failures + 2
            );
        }
        shutdown.cancel();
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn a_resend_sends_the_same_list_and_holds_new_connections_until_its_ok() {
        let (component, socket) = Component::start("ok {n}\n");
        let allowance = Allowance::new(socket);
        let shutdown = CancellationToken::new();
        let sender = tokio::spawn(allowance.clone().run(shutdown.clone()));
        allowance.configure([endpoint("192.0.2.10", 502)]);
        let dial = [address("192.0.2.10:502")];
        bounded(
            "the first permit",
            allowance.permit("192.0.2.10", 502, &dial),
        )
        .await
        .unwrap();
        let (first, _) = allowance.applied();

        // The reload emptied the sets and the resend's list does not get through yet: new
        // connections wait for it although the old list was applied.
        component.answer("failed: table gone\n");
        allowance.resend();
        let held = allowance.permit("192.0.2.10", 502, &dial).await;
        assert!(
            matches!(&held, Err(Refused::NotApplied(Some(reason))) if reason.contains("table gone")),
            "{held:?}"
        );
        component.answer("ok {n}\n");
        // The next attempt comes after the backoff, which may be longer than one permit's
        // PERMIT_WAIT: ask again until it is applied.
        bounded("the permit after the resend", async {
            while allowance.permit("192.0.2.10", 502, &dial).await.is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(allowance.applied().0 > first);
        let requests = component.requests();
        assert!(requests.len() >= 3, "{requests:?}");
        assert!(requests.iter().all(|list| *list == requests[0]));
        shutdown.cancel();
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn a_resend_ends_a_long_backoff_but_not_before_the_first_delay() {
        // The rules were flushed: every attempt fails until the check loads them again and
        // reloads the agent's unit.
        let (component, socket) = Component::start("failed: table gone\n");
        let allowance = Allowance::new(socket);
        let shutdown = CancellationToken::new();
        let sender = tokio::spawn(allowance.clone().run(shutdown.clone()));
        allowance.configure([endpoint("192.0.2.10", 502)]);
        // Five failures: the wait after the fifth is retry_delay(4), 16 s.
        let failed = 5;
        bounded("five failed attempts", async {
            while component.times().len() < failed {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        component.answer("ok {n}\n");
        allowance.resend();
        bounded("the list applied after the resend", async {
            while allowance.applied().0 == 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        let times = component.times();
        let gap = times[failed] - times[failed - 1];
        assert!(
            gap >= RETRY_FIRST && gap < retry_delay(4),
            "the attempt after the resend came {gap:?} after the failed one; the backoff was {:?}",
            retry_delay(4)
        );
        shutdown.cancel();
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn a_count_other_than_the_lists_is_not_ok() {
        let (_component, socket) = Component::start("ok 7\n");
        let list = Allowlist::new([destination("192.0.2.10", 502)]).unwrap();
        let error = exchange(&socket, &list).await.unwrap_err();
        assert!(error.contains("applied 7"), "{error}");
        let (_component, socket) = Component::start("refused: line 2: listed twice\n");
        assert_eq!(
            exchange(&socket, &list).await,
            Err("refused: line 2: listed twice".to_owned())
        );
        let missing = std::env::temp_dir().join("agent-fieldbus-no-such-socket");
        assert!(
            exchange(&missing, &list)
                .await
                .unwrap_err()
                .starts_with("connecting to")
        );
    }

    #[test]
    fn only_a_socket_counts_as_installed() {
        let dir = tempdir::Dir::new();
        assert!(!installed(&dir.0.join("socket")), "nothing there");
        std::fs::write(dir.0.join("file"), "").unwrap();
        assert!(!installed(&dir.0.join("file")));
        let _listener = std::os::unix::net::UnixListener::bind(dir.0.join("socket")).unwrap();
        assert!(installed(&dir.0.join("socket")));
    }
}
