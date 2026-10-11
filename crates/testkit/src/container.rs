//! Throwaway containers for tests, with podman or docker.
//!
//! A test that needs a container calls [`container_runtime`]; without one it calls
//! [`unavailable`], which skips the test on a workstation and fails it when `CI` is set, so a
//! green CI run means the test really ran.

use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

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

/// A container that is removed (with `rm --force`) when dropped.
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
            .args(["rm", "--force", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}
