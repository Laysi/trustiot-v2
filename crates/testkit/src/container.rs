//! Throwaway containers for tests, with podman or docker.
//!
//! A test that needs a container calls [`container_runtime`]; without one it calls
//! [`unavailable`], which skips the test on a workstation and fails it when `CI` is set, so a
//! green CI run means the test really ran.

use std::io::ErrorKind;
use std::net::{SocketAddr, TcpListener};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::mqtt::stop_listening;

/// Fails the test when `CI` is set, where every prerequisite must be present; otherwise prints
/// why the test does not run, and the caller returns early.
///
/// # Panics
///
/// When `CI` is set.
#[allow(
    clippy::print_stderr,
    reason = "tells the developer why a test did not run"
)]
pub fn unavailable(what: &str) {
    assert!(
        std::env::var_os("CI").is_none(),
        "{what} is required when CI is set"
    );
    eprintln!("skipped: {what} is not available (set CI to make this a failure)");
}

/// `podman` or `docker`, whichever can run containers: `info` reaches the engine (the docker
/// daemon, the podman service), so an installed CLI without a working engine counts as
/// unavailable.
#[must_use]
pub fn container_runtime() -> Option<&'static str> {
    ["podman", "docker"].into_iter().find(|runtime| {
        Command::new(runtime)
            .arg("info")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// Runs a container runtime command and returns its trimmed stdout.
///
/// # Panics
///
/// When the command cannot start or fails.
#[must_use]
pub fn runtime_output(runtime: &str, args: &[&str]) -> String {
    let output = Command::new(runtime)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("{runtime} {args:?}: {error}"));
    assert!(
        output.status.success(),
        "{runtime} {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A name for a container that is unique to this test process.
#[must_use]
pub fn unique_name(kind: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "testkit-{kind}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// A container that is removed when dropped, together with its anonymous volumes: the images of
/// Postgres, ClickHouse and Mosquitto declare `VOLUME`s, and `rm --force` alone leaves every one
/// of them behind (a workstation ran out of podman locks that way, Laysi/trustiot-v2#334).
#[derive(Debug)]
pub struct Container {
    /// `podman` or `docker`.
    pub runtime: &'static str,
    /// The container's name.
    pub name: String,
}

impl Container {
    /// The host address published for `port` inside the container, for example `5432/tcp`.
    ///
    /// # Panics
    ///
    /// When the port is not published.
    #[must_use]
    pub fn host_address(&self, port: &str) -> SocketAddr {
        let mapping = runtime_output(self.runtime, &["port", &self.name, port]);
        mapping
            .lines()
            .find_map(|line| line.trim().parse().ok())
            .unwrap_or_else(|| panic!("no host address for {port}: {mapping}"))
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new(self.runtime)
            .args(["rm", "--force", "--volumes", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// How many sets of host ports [`PortHolder::start`] tries when one it picked was taken before
/// the runtime bound it.
pub const PORT_ATTEMPTS: u32 = 8;

/// What the port holder runs: nothing but a wait, ended at once by `stop` or `rm --force`.
const HOLD_SCRIPT: &str = r"trap 'exit 0' TERM INT
while :; do sleep 600 & wait $!; done
";

/// A container that only owns host ports published on 127.0.0.1, for a test that restarts the
/// container behind them and keeps using the same ports (Laysi/trustiot-v2#857).
///
/// A port that the test picks and publishes itself (`--publish 127.0.0.1:<port>:<port>`) stays
/// the same across a restart on podman and on Docker (Docker reassigns a port it picked itself,
/// Laysi/trustiot-v2#348). But a restarted container that publishes its own ports gives them up
/// while it is down and binds them again when it starts, and in between any process can take
/// one, an outgoing connection's local port for instance; the start then fails (`pasta failed
/// ... Listen failed for HOST TCP port ...: Address already in use`). This container publishes
/// the ports instead, and the container under test joins its network namespace with
/// `--network` [`PortHolder::network`]: it listens on the published ports inside the namespace,
/// and restarting it never touches what holds them on the host (pasta or rootlessport on
/// podman, the userland proxy on Docker), which lives as long as this container. Both runtimes
/// support `--network container:<name>`; on Docker the joining container must not publish
/// ports itself.
///
/// While nothing listens inside the namespace, a connection to a published port is accepted on
/// the host and then closed, rather than refused.
///
/// Remove the joined containers first: podman refuses to remove a container whose network
/// namespace another one uses. A struct that owns both declares the joined container before
/// the holder, so it is dropped first.
#[derive(Debug)]
pub struct PortHolder {
    /// The holder; removed when dropped.
    pub container: Container,
    /// Each published port: the port inside the namespace and the host port on 127.0.0.1.
    ports: Vec<(u16, u16)>,
}

impl PortHolder {
    /// Starts a holder from `image` (it needs `/bin/sh` and `sleep`; the image the test starts
    /// next is a good choice, as nothing more is pulled) that publishes each of
    /// `container_ports` on a free port of 127.0.0.1.
    ///
    /// # Panics
    ///
    /// When the runtime fails, for any other reason than a picked port being taken or for that
    /// reason [`PORT_ATTEMPTS`] times.
    #[must_use]
    pub fn start(runtime: &'static str, kind: &str, image: &str, container_ports: &[u16]) -> Self {
        Self::start_with(runtime, kind, image, container_ports, free_loopback_ports)
    }

    /// [`Self::start`] with the host ports of each attempt picked by `pick` (called with the
    /// number of ports), for a test of the retry.
    ///
    /// Picking the ports and publishing them are two steps, so another process can take one in
    /// between: the run then fails, and it is safe to try again with new ports, since nothing
    /// uses them yet. Any other failure is not retried.
    ///
    /// # Panics
    ///
    /// Like [`Self::start`], and when `pick` returns the wrong number of ports or one twice.
    #[must_use]
    pub fn start_with(
        runtime: &'static str,
        kind: &str,
        image: &str,
        container_ports: &[u16],
        mut pick: impl FnMut(usize) -> Vec<u16>,
    ) -> Self {
        let mut attempt = 1;
        loop {
            let hosts = pick(container_ports.len());
            assert_eq!(
                hosts.len(),
                container_ports.len(),
                "picked ports: {hosts:?}"
            );
            assert!(
                hosts
                    .iter()
                    .enumerate()
                    .all(|(index, port)| !hosts[..index].contains(port)),
                "picked a port twice: {hosts:?}"
            );
            // Removes what a failed run leaves behind (a created container) when dropped.
            let container = Container {
                runtime,
                name: unique_name(kind),
            };
            let ports: Vec<(u16, u16)> = container_ports.iter().copied().zip(hosts).collect();
            let mut command = Command::new(runtime);
            command.args(["run", "--detach", "--name", &container.name]);
            for (inside, host) in &ports {
                command.args(["--publish", &format!("127.0.0.1:{host}:{inside}")]);
            }
            let output = command
                .args(["--entrypoint", "/bin/sh", image, "-c", HOLD_SCRIPT])
                .output()
                .unwrap_or_else(|error| panic!("{runtime} run: {error}"));
            if output.status.success() {
                return Self { container, ports };
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                port_is_taken(&stderr) && attempt < PORT_ATTEMPTS,
                "{runtime} run of the port holder with {ports:?} failed (attempt {attempt} of \
                 {PORT_ATTEMPTS}): {stderr}"
            );
            attempt += 1;
        }
    }

    /// The `--network` value that joins a container to the holder's network namespace.
    #[must_use]
    pub fn network(&self) -> String {
        format!("container:{}", self.container.name)
    }

    /// The host port on 127.0.0.1 published for `container_port`.
    ///
    /// # Panics
    ///
    /// When the holder does not publish `container_port`.
    #[must_use]
    pub fn host_port(&self, container_port: u16) -> u16 {
        self.ports
            .iter()
            .find_map(|&(inside, host)| (inside == container_port).then_some(host))
            .unwrap_or_else(|| panic!("port {container_port} is not published: {:?}", self.ports))
    }

    /// Checks that every published host port is still held: binding it on 127.0.0.1 fails
    /// with `AddrInUse`. A test calls this while the joined container is stopped, which is when
    /// a container publishing its own ports would have given them up.
    ///
    /// # Panics
    ///
    /// When a port can be bound, or binding fails for another reason.
    pub fn assert_held(&self) {
        for &(inside, host) in &self.ports {
            match TcpListener::bind(("127.0.0.1", host)) {
                Ok(listener) => {
                    stop_listening(&listener);
                    panic!(
                        "127.0.0.1:{host} (published for {inside}) could be bound: nothing \
                         holds it on the host"
                    );
                }
                Err(error) => assert_eq!(
                    error.kind(),
                    ErrorKind::AddrInUse,
                    "bind 127.0.0.1:{host} (published for {inside}): {error}"
                ),
            }
        }
    }
}

/// `count` different ports of 127.0.0.1 that were free a moment ago: all are bound before any
/// is released, so they differ.
fn free_loopback_ports(count: usize) -> Vec<u16> {
    let listeners: Vec<TcpListener> = (0..count)
        .map(|_| TcpListener::bind(("127.0.0.1", 0)).expect("bind a free port"))
        .collect();
    listeners
        .iter()
        .map(|listener| {
            let port = listener.local_addr().expect("local address").port();
            // Free the port even if a process spawned meanwhile holds a copy of the descriptor.
            stop_listening(listener);
            port
        })
        .collect()
}

/// Whether a runtime's error says a host port could not be bound (podman's pasta or
/// rootlessport, rootful podman, Docker).
fn port_is_taken(stderr: &str) -> bool {
    let stderr = stderr.to_lowercase();
    ["address already in use", "port is already allocated"]
        .iter()
        .any(|text| stderr.contains(text))
}
