//! systemd's notification protocol (`sd_notify(3)`), without libsystemd: one datagram per
//! notification to the Unix socket in `NOTIFY_SOCKET`. Without that variable (not started by
//! systemd, or a unit without `Type=notify`) every notification does nothing.
//!
//! The agent sends `READY=1` once its configuration and key are checked, `WATCHDOG=1` at half
//! the unit's `WatchdogSec` while the connection task makes progress ([`crate::liveness`]),
//! `STATUS=...` for `systemctl status`, and `STOPPING=1` when it begins to shut down.

use std::io;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::Path;
use std::time::Duration;

/// How often the connection thread proves it is alive. The watchdog notification goes out only
/// if a tick happened since the last one, so anything that blocks that thread (a TPM signature,
/// at most [`crate::tpm::RESPONSE_TIMEOUT`]) plus one tick must fit in half of the unit's
/// `WatchdogSec` (`tests/systemd_unit.rs` checks it).
pub const PROGRESS_TICK: Duration = Duration::from_secs(1);

/// One notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notification<'a> {
    /// Startup is complete.
    Ready,
    /// Shutdown has begun.
    Stopping,
    /// The service is alive (resets the watchdog timer).
    Watchdog,
    /// A one-line status for `systemctl status`.
    Status(&'a str),
}

/// The datagram for `notifications`: one `KEY=VALUE` line each. A status loses its line breaks,
/// which would otherwise start a new assignment.
pub fn message(notifications: &[Notification<'_>]) -> String {
    let mut text = String::new();
    for notification in notifications {
        match notification {
            Notification::Ready => text.push_str("READY=1\n"),
            Notification::Stopping => text.push_str("STOPPING=1\n"),
            Notification::Watchdog => text.push_str("WATCHDOG=1\n"),
            Notification::Status(status) => {
                text.push_str("STATUS=");
                text.extend(status.chars().map(|c| if c.is_control() { ' ' } else { c }));
                text.push('\n');
            }
        }
    }
    text
}

/// Sends notifications to systemd.
#[derive(Debug)]
pub struct Notifier {
    target: Option<(UnixDatagram, SocketAddr)>,
}

impl Notifier {
    /// The notifier for `NOTIFY_SOCKET`: a path, or `@name` for an abstract socket. Does
    /// nothing when the variable is not set.
    ///
    /// # Errors
    ///
    /// The variable names no usable socket.
    pub fn from_env() -> io::Result<Self> {
        match std::env::var_os("NOTIFY_SOCKET") {
            None => Ok(Self { target: None }),
            Some(value) => {
                let bytes = value.as_encoded_bytes();
                let address = match bytes.strip_prefix(b"@") {
                    Some(name) => abstract_address(name)?,
                    None => SocketAddr::from_pathname(Path::new(&value))?,
                };
                Ok(Self {
                    target: Some((UnixDatagram::unbound()?, address)),
                })
            }
        }
    }

    /// A notifier that sends to the socket at `path` (tests).
    ///
    /// # Errors
    ///
    /// The socket cannot be created.
    pub fn to_path(path: &Path) -> io::Result<Self> {
        Ok(Self {
            target: Some((UnixDatagram::unbound()?, SocketAddr::from_pathname(path)?)),
        })
    }

    /// A notifier that does nothing.
    pub const fn disabled() -> Self {
        Self { target: None }
    }

    /// Whether notifications go anywhere.
    pub const fn is_enabled(&self) -> bool {
        self.target.is_some()
    }

    /// Sends `notifications` in one datagram.
    ///
    /// # Errors
    ///
    /// The datagram cannot be sent.
    pub fn notify(&self, notifications: &[Notification<'_>]) -> io::Result<()> {
        let Some((socket, address)) = &self.target else {
            return Ok(());
        };
        socket.send_to_addr(message(notifications).as_bytes(), address)?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn abstract_address(name: &[u8]) -> io::Result<SocketAddr> {
    use std::os::linux::net::SocketAddrExt;
    SocketAddr::from_abstract_name(name)
}

#[cfg(not(target_os = "linux"))]
fn abstract_address(_name: &[u8]) -> io::Result<SocketAddr> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "abstract sockets need Linux",
    ))
}

/// How often to send `WATCHDOG=1`: half of `WATCHDOG_USEC`, if the watchdog is meant for this
/// process (`WATCHDOG_PID` unset or equal to `pid`).
pub fn watchdog_interval(
    usec: Option<&str>,
    watchdog_pid: Option<&str>,
    pid: u32,
) -> Option<Duration> {
    if let Some(target) = watchdog_pid
        && target.trim().parse::<u32>().ok()? != pid
    {
        return None;
    }
    let usec = usec?.trim().parse::<u64>().ok().filter(|&usec| usec > 0)?;
    Some(Duration::from_micros(usec) / 2)
}

/// [`watchdog_interval`] from this process's environment.
pub fn watchdog_interval_from_env() -> Option<Duration> {
    let usec = std::env::var("WATCHDOG_USEC").ok();
    let pid = std::env::var("WATCHDOG_PID").ok();
    watchdog_interval(usec.as_deref(), pid.as_deref(), std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_newline_separated_assignments() {
        assert_eq!(message(&[Notification::Ready]), "READY=1\n");
        assert_eq!(
            message(&[
                Notification::Ready,
                Notification::Status("connected as gw-1")
            ]),
            "READY=1\nSTATUS=connected as gw-1\n"
        );
        assert_eq!(
            message(&[Notification::Status("a\nREADY=1")]),
            "STATUS=a READY=1\n"
        );
        assert_eq!(
            message(&[Notification::Watchdog, Notification::Stopping]),
            "WATCHDOG=1\nSTOPPING=1\n"
        );
    }

    #[test]
    fn the_watchdog_interval_is_half_the_timeout_for_this_process_only() {
        assert_eq!(
            watchdog_interval(Some("30000000"), None, 7),
            Some(Duration::from_secs(15))
        );
        assert_eq!(
            watchdog_interval(Some("30000000"), Some("7"), 7),
            Some(Duration::from_secs(15))
        );
        assert_eq!(watchdog_interval(Some("30000000"), Some("8"), 7), None);
        assert_eq!(watchdog_interval(None, None, 7), None);
        assert_eq!(watchdog_interval(Some("0"), None, 7), None);
        assert_eq!(watchdog_interval(Some("x"), None, 7), None);
    }

    #[test]
    fn notifications_reach_the_socket_and_do_nothing_without_one() {
        let dir = std::env::temp_dir().join(format!("agent-sdnotify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notify.sock");
        let _ = std::fs::remove_file(&path);
        let receiver = UnixDatagram::bind(&path).unwrap();
        let notifier = Notifier::to_path(&path).unwrap();
        assert!(notifier.is_enabled());
        notifier
            .notify(&[Notification::Ready, Notification::Status("up")])
            .unwrap();
        let mut buffer = [0_u8; 128];
        let len = receiver.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..len], b"READY=1\nSTATUS=up\n");
        assert!(Notifier::disabled().notify(&[Notification::Ready]).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
