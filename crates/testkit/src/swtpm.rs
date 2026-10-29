//! A software TPM 2.0 for tests (Laysi/trustiot-v2#86, Laysi/trustiot-v2#611): swtpm (libtpms)
//! in a throwaway container, reached over its raw command port, with tpm2-tools inside the
//! container to set up and inspect the TPM.
//!
//! - [`Swtpm::start`] starts a blank TPM (TPM2_Startup done, as the gateway's kernel does) and
//!   waits until it answers. Without podman or docker it returns `None` after
//!   [`crate::container::unavailable`] (a skip locally, a failure with `CI` set).
//! - [`Swtpm::session`] is what opening `/dev/tpmrm0` is on the gateway: it carries commands to
//!   the TPM one at a time and, when dropped, flushes the transient objects and sessions it
//!   loaded, as the kernel's resource manager does when a process closes the device (or dies).
//!   swtpm serves one connection at a time, so every session shares one TCP connection and a lock
//!   keeps commands whole; two sessions can be used from two threads, like two processes.
//! - [`Swtpm::tools`] runs a shell script with tpm2-tools in the container. It closes the shared
//!   connection first (tpm2-tools needs its own) while holding the lock, so no session command is
//!   in flight; the next session command connects again. The TPM keeps its state either way.
//! - [`Swtpm::create_standard_srk`] puts the storage root key the gateway has at `0x81000001`;
//!   [`Swtpm::snapshot`], [`Swtpm::persistent_handles`] and [`Swtpm::transient_handles`] show
//!   what the TPM holds.
//!
//! The image is built from `src/swtpm/Containerfile` (Debian's swtpm and tpm2-tools, the base
//! pinned by digest) with an empty build context, once per process, and kept as
//! `localhost/testkit-swtpm:<hash of the Containerfile>` so later runs reuse it. swtpm's command
//! port is published on the host's loopback interface only.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::container::{Container, container_runtime, runtime_output, unavailable, unique_name};

/// The image's build file.
pub const CONTAINERFILE: &str = include_str!("swtpm/Containerfile");

/// The largest response accepted from swtpm (the kernel's `TPM_BUFSIZE`).
pub const MAX_RESPONSE_BYTES: usize = 4096;

/// The persistent handle of the storage root key (as on the gateway).
pub const SRK_HANDLE: u32 = 0x8100_0001;

/// swtpm's raw TPM command port inside the container.
const TPM_PORT: &str = "2321/tcp";

/// How long a socket read or write may wait for swtpm before the test fails instead of hanging.
/// Nothing here comes close: a command takes milliseconds.
const IO_BOUND: Duration = Duration::from_secs(60);

/// The longest a [`Swtpm::tools`] script may run (seconds, for coreutils' `timeout`). Waiting
/// for swtpm at start is the longest step, at most 60 s.
const TOOLS_BOUND_SECONDS: u32 = 120;

/// Runs swtpm in the foreground (PID 1). `not-need-init,startup-clear`: it runs TPM2_Startup
/// itself. The control port stays inside the container.
const SWTPM_SCRIPT: &str = "mkdir -p /run/swtpm && exec swtpm socket --tpm2 \
    --tpmstate dir=/run/swtpm \
    --server type=tcp,port=2321,bindaddr=0.0.0.0 \
    --ctrl type=tcp,port=2322,bindaddr=127.0.0.1 \
    --flags not-need-init,startup-clear";

/// Waits up to 60 s until swtpm answers tpm2-tools.
const WAIT_SCRIPT: &str = r#"tries=0
until tpm2_getcap properties-fixed > /dev/null 2>&1; do
    tries=$((tries + 1))
    if [ "$tries" -ge 600 ]; then echo "swtpm did not answer on port 2321 within 60 s" >&2; exit 1; fi
    sleep 0.1
done"#;

/// TPM command codes whose response carries a new transient handle in its handle area:
/// TPM2_CreatePrimary, TPM2_Load, TPM2_LoadExternal, TPM2_ContextLoad, TPM2_StartAuthSession and
/// TPM2_CreateLoaded.
const LOADING_COMMANDS: [u32; 6] = [0x131, 0x157, 0x167, 0x161, 0x176, 0x191];
/// TPM2_FlushContext.
const FLUSH_CONTEXT: u32 = 0x165;

/// A software TPM in a container, removed (with its state) when dropped.
#[derive(Debug)]
pub struct Swtpm {
    link: Arc<Mutex<Link>>,
    // Dropped last: sessions may still flush through the link while the container runs.
    container: Container,
}

impl Swtpm {
    /// Builds the image if needed, starts a blank TPM, and waits until it answers.
    ///
    /// # Panics
    ///
    /// When the image cannot be built or the container does not start, and when no container
    /// runtime is available while `CI` is set.
    #[must_use]
    pub fn start() -> Option<Self> {
        let Some(runtime) = container_runtime() else {
            unavailable("podman or docker");
            return None;
        };
        let image = image(runtime);
        let container = Container {
            runtime,
            name: unique_name("swtpm"),
        };
        let _id = runtime_output(
            runtime,
            &[
                "run",
                "--detach",
                "--name",
                &container.name,
                "--publish",
                "127.0.0.1::2321",
                "--memory",
                "256m",
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges",
                "--entrypoint",
                "/bin/sh",
                &image,
                "-c",
                SWTPM_SCRIPT,
            ],
        );
        let address = container.host_address(TPM_PORT);
        let swtpm = Self {
            link: Arc::new(Mutex::new(Link {
                address,
                stream: None,
            })),
            container,
        };
        let _ = swtpm.tools(WAIT_SCRIPT);
        Some(swtpm)
    }

    /// A new session, as if a process opened `/dev/tpmrm0`.
    #[must_use]
    pub fn session(&self) -> Session {
        Session {
            link: Arc::clone(&self.link),
            transients: Vec::new(),
        }
    }

    /// Closes the shared connection; the next session command opens a new one.
    pub fn disconnect(&self) {
        lock(&self.link).stream = None;
    }

    /// Runs `script` with `sh -eu` and tpm2-tools (`TPM2TOOLS_TCTI` set to this TPM) in a new
    /// temporary directory in the container, and returns its standard output. Session commands
    /// wait until it ends.
    ///
    /// # Panics
    ///
    /// When the script fails or runs longer than 120 s.
    #[must_use]
    pub fn tools(&self, script: &str) -> String {
        let mut link = lock(&self.link);
        link.stream = None;
        let script = format!("set -eu\ncd \"$(mktemp -d)\"\n{script}\n");
        let output = Command::new(self.container.runtime)
            .args([
                "exec",
                "--env",
                "TPM2TOOLS_TCTI=swtpm:host=127.0.0.1,port=2321",
                &self.container.name,
                "timeout",
                &TOOLS_BOUND_SECONDS.to_string(),
                "/bin/sh",
                "-c",
                &script,
            ])
            .output()
            .expect("run the container runtime");
        drop(link);
        assert!(
            output.status.success(),
            "tpm2-tools script failed ({}):\n{script}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Creates the standard ECC storage root key (`tpm2_createprimary -C o -g sha256 -G
    /// ecc256:aes128cfb`, the tpm2-tools default the gateway's SRK matches) and persists it at
    /// [`SRK_HANDLE`].
    pub fn create_standard_srk(&self) {
        let _ = self.tools(&format!(
            "tpm2_createprimary -Q -C o -g sha256 -G ecc256:aes128cfb -c srk.ctx\n\
             tpm2_evictcontrol -Q -C o -c srk.ctx {SRK_HANDLE:#010x}\n\
             tpm2_flushcontext -t"
        ));
    }

    /// The persistent object handles, in the TPM's order.
    #[must_use]
    pub fn persistent_handles(&self) -> Vec<u32> {
        handles(&self.tools("tpm2_getcap handles-persistent"))
    }

    /// The loaded transient object handles.
    #[must_use]
    pub fn transient_handles(&self) -> Vec<u32> {
        handles(&self.tools("tpm2_getcap handles-transient"))
    }

    /// What the TPM holds, as text to compare before and after a test step: the persistent,
    /// transient and NV index handles, the public area and names of every persistent object,
    /// and the variable properties (among them the dictionary-attack counter,
    /// `TPM2_PT_LOCKOUT_COUNTER`).
    #[must_use]
    pub fn snapshot(&self) -> String {
        self.tools(
            r#"echo "persistent:"; tpm2_getcap handles-persistent
echo "transient:"; tpm2_getcap handles-transient
echo "nv-index:"; tpm2_getcap handles-nv-index
for handle in $(tpm2_getcap handles-persistent | sed 's/^- //'); do
    echo "object $handle:"; tpm2_readpublic -c "$handle"
done
echo "properties:"; tpm2_getcap properties-variable"#,
        )
    }
}

/// `- 0x81000001` lines of `tpm2_getcap handles-*`.
fn handles(listing: &str) -> Vec<u32> {
    listing
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- 0x"))
        .filter_map(|hex| u32::from_str_radix(hex, 16).ok())
        .collect()
}

/// One process's use of the TPM; see the module documentation.
#[derive(Debug)]
pub struct Session {
    link: Arc<Mutex<Link>>,
    transients: Vec<u32>,
}

impl Session {
    /// Sends one complete command and returns the complete response (at most
    /// [`MAX_RESPONSE_BYTES`]). Remembers the transient handles the command loaded, so dropping
    /// the session flushes them.
    ///
    /// # Errors
    ///
    /// When swtpm cannot be reached or answers with a broken frame; the connection is closed
    /// then, and the next command opens a new one.
    pub fn transact(&mut self, command: &[u8]) -> io::Result<Vec<u8>> {
        let response = lock(&self.link).transact(command)?;
        let code = |bytes: &[u8], at: usize| {
            bytes
                .get(at..at + 4)
                .and_then(|field| <[u8; 4]>::try_from(field).ok())
                .map(u32::from_be_bytes)
        };
        if code(&response, 6) == Some(0) {
            match code(command, 6) {
                Some(command_code) if LOADING_COMMANDS.contains(&command_code) => {
                    if let Some(handle) = code(&response, 10) {
                        self.transients.push(handle);
                    }
                }
                Some(FLUSH_CONTEXT) => {
                    if let Some(handle) = code(command, 10) {
                        self.transients.retain(|loaded| *loaded != handle);
                    }
                }
                _ => {}
            }
        }
        Ok(response)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let mut link = lock(&self.link);
        for handle in self.transients.drain(..) {
            let mut flush = vec![0x80, 0x01, 0, 0, 0, 14];
            flush.extend_from_slice(&FLUSH_CONTEXT.to_be_bytes());
            flush.extend_from_slice(&handle.to_be_bytes());
            // As the resource manager does; a failure leaves the object, which a test notices.
            let _ = link.transact(&flush);
        }
    }
}

/// The shared connection to swtpm's command port, opened on demand.
#[derive(Debug)]
struct Link {
    address: SocketAddr,
    stream: Option<TcpStream>,
}

impl Link {
    fn transact(&mut self, command: &[u8]) -> io::Result<Vec<u8>> {
        let result = self.exchange(command);
        if result.is_err() {
            self.stream = None;
        }
        result
    }

    fn exchange(&mut self, command: &[u8]) -> io::Result<Vec<u8>> {
        let stream = match &mut self.stream {
            Some(stream) => stream,
            empty => {
                let stream = TcpStream::connect_timeout(&self.address, IO_BOUND)?;
                stream.set_read_timeout(Some(IO_BOUND))?;
                stream.set_write_timeout(Some(IO_BOUND))?;
                stream.set_nodelay(true)?;
                empty.insert(stream)
            }
        };
        // swtpm's command port speaks the TPM 2.0 command stream itself: a command, then a
        // response whose header carries its size.
        stream.write_all(command)?;
        let mut response = vec![0_u8; 10];
        stream.read_exact(&mut response)?;
        let size = response
            .get(2..6)
            .and_then(|field| <[u8; 4]>::try_from(field).ok())
            .map(u32::from_be_bytes)
            .and_then(|size| usize::try_from(size).ok())
            .unwrap_or(0);
        if !(10..=MAX_RESPONSE_BYTES).contains(&size) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("swtpm sent a response of {size} bytes"),
            ));
        }
        response.resize(size, 0);
        if let Some(rest) = response.get_mut(10..) {
            stream.read_exact(rest)?;
        }
        Ok(response)
    }
}

fn lock(link: &Mutex<Link>) -> MutexGuard<'_, Link> {
    // A test that panicked while holding the lock leaves a usable link.
    link.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The image, built once per process and Containerfile content.
fn image(runtime: &'static str) -> String {
    static BUILD: Mutex<()> = Mutex::new(());
    let _building = BUILD.lock().unwrap_or_else(PoisonError::into_inner);
    let digest = ring::digest::digest(&ring::digest::SHA256, CONTAINERFILE.as_bytes());
    let hash: String = digest
        .as_ref()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let image = format!("localhost/testkit-swtpm:{hash}");
    let exists = Command::new(runtime)
        .args(["image", "inspect", &image])
        .output()
        .is_ok_and(|output| output.status.success());
    if exists {
        return image;
    }
    // No file of the repository reaches the builder: the context is an empty directory.
    let context = std::env::temp_dir().join(unique_name("swtpm-context"));
    std::fs::create_dir_all(&context).expect("create the empty build context");
    let containerfile = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/swtpm/Containerfile");
    let built = Command::new(runtime)
        .args(["build", "--quiet", "--file"])
        .arg(&containerfile)
        .args(["--tag", &image])
        .arg(&context)
        .output();
    let _ = std::fs::remove_dir(&context);
    let built = built.expect("run the container runtime");
    assert!(
        built.status.success(),
        "building the swtpm image failed: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    image
}

#[cfg(test)]
mod tests {
    use super::handles;

    #[test]
    fn handle_listings_are_parsed() {
        assert_eq!(
            handles("- 0x81000001\n- 0x81000100\n"),
            [0x8100_0001, 0x8100_0100]
        );
        assert!(handles("").is_empty());
    }
}
