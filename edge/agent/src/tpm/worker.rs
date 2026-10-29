//! The one thread that owns the TPM: requests arrive on a bounded queue and are sent to the TPM
//! one at a time, so commands never interleave and a slow signature (about 70 ms on the gateway's
//! TPM) blocks only this thread and the caller that waits for it.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread;
use std::time::Duration;

use super::TpmError;
use super::marshal::MAX_RESPONSE_BYTES;

/// How many requests may wait while the TPM works on another. The agent signs during one TLS
/// handshake at a time; a full queue is refused rather than waited on.
pub(crate) const QUEUE_CAPACITY: usize = 8;

/// The warnings after which a command is sent again (TPM 2.0 Part 2, section 6.6.3): the TPM did
/// not run it. `TPM_RC_YIELDED` (0x908), `TPM_RC_TESTING` (0x90a) and `TPM_RC_RETRY` (0x922),
/// which swtpm returned for TPM2_Create on 2026-10-29. tpm2-tss resends the same three.
const RETRY_WARNINGS: [u32; 3] = [0x908, 0x90a, 0x922];
/// How often a command is sent again after those warnings, and the pause before the first
/// resend, doubled for each further one up to [`RETRY_PAUSE_MAX`]: about 1.6 s in all, well
/// within the caller's wait.
const RETRIES: u32 = 8;
const RETRY_PAUSE: Duration = Duration::from_millis(10);
const RETRY_PAUSE_MAX: Duration = Duration::from_millis(500);

/// Whether the worker sends a command again when the TPM answers with one of
/// [`RETRY_WARNINGS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Retry {
    /// Return the warning to the caller (the agent's signing path, unchanged since
    /// Laysi/trustiot-v2#86).
    Never,
    /// Resend up to [`RETRIES`] times (provisioning).
    OnWarnings,
}

/// Something that carries TPM 2.0 commands to a TPM and brings back its responses: the kernel's
/// resource manager (`/dev/tpmrm0`) in production, a TCP socket to a software TPM in tests.
///
/// The worker thread calls it with one complete command at a time and expects the complete
/// response; the response is parsed strictly, so an implementation does not need to check it.
pub trait TpmChannel: Send + 'static {
    /// Sends `command` and returns the TPM's response. It should read at most
    /// [`MAX_RESPONSE_BYTES`] bytes; a longer response is refused anyway.
    ///
    /// # Errors
    ///
    /// When the command cannot be sent or the response cannot be read.
    fn transact(&mut self, command: &[u8]) -> io::Result<Vec<u8>>;
}

/// A TPM character device, `/dev/tpmrm0` (the kernel's resource manager) on the gateway.
///
/// The kernel takes a command in one `write` (and runs it there, so a signature blocks in
/// `write`) and hands back the whole response in one `read` of up to `TPM_BUFSIZE` (4096) bytes.
pub(crate) struct DeviceChannel {
    file: File,
    buffer: Box<[u8; MAX_RESPONSE_BYTES]>,
}

impl DeviceChannel {
    /// Opens `path` for reading and writing; it must be a character device.
    pub(crate) fn open(path: &Path) -> Result<Self, TpmError> {
        let open_error = |source| TpmError::Open {
            path: path.to_owned(),
            source,
        };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(open_error)?;
        let metadata = file.metadata().map_err(open_error)?;
        if !metadata.file_type().is_char_device() {
            return Err(open_error(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a character device",
            )));
        }
        Ok(Self {
            file,
            buffer: Box::new([0; MAX_RESPONSE_BYTES]),
        })
    }
}

impl DeviceChannel {
    /// Opens `path` like [`DeviceChannel::open`], provided the kernel lists the device as one of
    /// its TPM resource manager (`/sys/dev/char/<major>:<minor>` links to a `tpmrm/tpmrm<N>`
    /// entry). The device the path names is checked before it is opened, so a link or a node
    /// that leads to the raw device is not opened; the opened file's device number is compared
    /// with the checked one before any command, so a path swapped in between is refused too.
    pub(crate) fn open_resource_manager(path: &Path) -> Result<Self, TpmError> {
        let open_error = |source| TpmError::Open {
            path: path.to_owned(),
            source,
        };
        let device = fs::metadata(path).map_err(open_error)?;
        if !device.file_type().is_char_device() {
            return Err(open_error(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a character device",
            )));
        }
        let number = device.rdev();
        let (major, minor) = device_numbers(number);
        let entry = PathBuf::from(format!("/sys/dev/char/{major}:{minor}"));
        let refused = |found: String| TpmError::NotResourceManagerDevice {
            path: path.to_owned(),
            found,
        };
        let target = fs::read_link(&entry).map_err(|error| {
            refused(format!(
                "the kernel's entry {} for device {major}:{minor} cannot be read ({error})",
                entry.display()
            ))
        })?;
        if !is_resource_manager_entry(&target) {
            return Err(refused(format!(
                "device {major}:{minor} is {}",
                target.display()
            )));
        }
        let channel = Self::open(path)?;
        let opened = channel.file.metadata().map_err(open_error)?.rdev();
        if opened != number {
            let (major, minor) = device_numbers(opened);
            return Err(refused(format!(
                "the path changed to device {major}:{minor} while it was opened"
            )));
        }
        Ok(channel)
    }
}

/// Splits a Linux device number into major and minor, as glibc's `major` and `minor` do.
fn device_numbers(device: u64) -> (u64, u64) {
    let major = ((device >> 32) & 0xffff_f000) | ((device >> 8) & 0x0000_0fff);
    let minor = ((device >> 12) & 0xffff_ff00) | (device & 0x0000_00ff);
    (major, minor)
}

/// Whether a `/sys/dev/char/<major>:<minor>` link target names a device of the kernel's TPM
/// resource manager: its last two components are `tpmrm` and `tpmrm` with decimal digits (the
/// raw device's entry ends in `tpm/tpm<N>`).
fn is_resource_manager_entry(target: &Path) -> bool {
    let mut components = target.components().rev();
    let name = components.next().and_then(|name| name.as_os_str().to_str());
    let class = components
        .next()
        .and_then(|class| class.as_os_str().to_str());
    class == Some("tpmrm")
        && name
            .and_then(|name| name.strip_prefix("tpmrm"))
            .is_some_and(|number| {
                !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
            })
}

impl TpmChannel for DeviceChannel {
    fn transact(&mut self, command: &[u8]) -> io::Result<Vec<u8>> {
        let written = retry_interrupted(|| self.file.write(command))?;
        if written != command.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!(
                    "the TPM device took {written} of {} command bytes",
                    command.len()
                ),
            ));
        }
        let read = retry_interrupted(|| self.file.read(&mut self.buffer[..]))?;
        Ok(self.buffer.get(..read).unwrap_or_default().to_vec())
    }
}

impl fmt::Debug for DeviceChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceChannel").finish_non_exhaustive()
    }
}

fn retry_interrupted(mut operation: impl FnMut() -> io::Result<usize>) -> io::Result<usize> {
    loop {
        match operation() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

/// One command and where its response goes.
struct Request {
    command: Vec<u8>,
    reply: SyncSender<Result<Vec<u8>, TpmError>>,
}

/// The sending side of the worker's queue. When the last one is dropped, the worker finishes
/// the request it is working on, drops the channel (closing the device) and exits.
#[derive(Debug)]
pub(crate) struct Worker {
    requests: SyncSender<Request>,
    timeout: Duration,
}

impl Worker {
    /// Starts the thread that owns `channel`. A response must arrive within `timeout`, resends
    /// included.
    pub(crate) fn spawn(
        channel: Box<dyn TpmChannel>,
        timeout: Duration,
        retry: Retry,
    ) -> Result<Self, TpmError> {
        let (requests, queue) = mpsc::sync_channel(QUEUE_CAPACITY);
        thread::Builder::new()
            .name("tpm".to_owned())
            .spawn(move || serve(channel, &queue, retry))
            .map_err(TpmError::Spawn)?;
        Ok(Self { requests, timeout })
    }

    /// Sends `command` and waits for the response, at most the configured timeout.
    pub(crate) fn transact(&self, command: Vec<u8>) -> Result<Vec<u8>, TpmError> {
        self.submit(command)?.wait(self.timeout)
    }

    /// Queues `command` without waiting for the TPM.
    pub(crate) fn submit(&self, command: Vec<u8>) -> Result<Pending, TpmError> {
        let (reply, response) = mpsc::sync_channel(1);
        self.requests
            .try_send(Request { command, reply })
            .map_err(|error| match error {
                TrySendError::Full(_) => TpmError::Busy,
                TrySendError::Disconnected(_) => TpmError::WorkerStopped,
            })?;
        Ok(Pending(response))
    }
}

/// A response the worker has not delivered yet.
pub(crate) struct Pending(Receiver<Result<Vec<u8>, TpmError>>);

impl Pending {
    pub(crate) fn wait(self, timeout: Duration) -> Result<Vec<u8>, TpmError> {
        match self.0.recv_timeout(timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(TpmError::Timeout(timeout)),
            // The worker dropped the request without answering: it panicked.
            Err(RecvTimeoutError::Disconnected) => Err(TpmError::WorkerStopped),
        }
    }
}

/// The worker thread: one command at a time, until every sender is gone.
fn serve(mut channel: Box<dyn TpmChannel>, queue: &Receiver<Request>, retry: Retry) {
    while let Ok(Request { command, reply }) = queue.recv() {
        let mut result = channel.transact(&command).map_err(TpmError::Io);
        if retry == Retry::OnWarnings {
            let mut pause = RETRY_PAUSE;
            for _ in 0..RETRIES {
                if !result.as_deref().is_ok_and(is_retry_warning) {
                    break;
                }
                thread::sleep(pause);
                pause = (pause * 2).min(RETRY_PAUSE_MAX);
                result = channel.transact(&command).map_err(TpmError::Io);
            }
        }
        // The caller may have given up waiting; then nobody needs the response.
        let _ = reply.send(result);
    }
}

/// Whether `response` is a bare header with one of [`RETRY_WARNINGS`].
fn is_retry_warning(response: &[u8]) -> bool {
    response.len() == 10
        && response
            .get(6..10)
            .and_then(|code| <[u8; 4]>::try_from(code).ok())
            .is_some_and(|code| RETRY_WARNINGS.contains(&u32::from_be_bytes(code)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_numbers_split_like_glibc() {
        // makedev(10, 224), makedev(254, 65536) and makedev(0x1001, 0x10001) as glibc builds them.
        assert_eq!(device_numbers(0x0000_0000_0000_0ae0), (10, 224));
        assert_eq!(device_numbers(0x0000_0000_1000_fe00), (254, 65536));
        assert_eq!(device_numbers(0x0000_1000_1000_0101), (0x1001, 0x0001_0001));
    }

    #[test]
    fn only_tpmrm_entries_count_as_the_resource_manager() {
        for target in [
            "../../devices/platform/soc/fe204000.spi/spi_master/spi0/spi0.1/tpmrm/tpmrm0",
            "../../devices/LNXSYSTM:00/LNXSYBUS:00/MSFT0101:00/tpmrm/tpmrm12",
            "tpmrm/tpmrm1",
        ] {
            assert!(is_resource_manager_entry(Path::new(target)), "{target}");
        }
        for target in [
            "../../devices/platform/soc/fe204000.spi/spi_master/spi0/spi0.1/tpm/tpm0",
            "../../devices/virtual/mem/null",
            "../../devices/virtual/tpmrm/tpmrm",
            "../../devices/virtual/tpmrm/tpmrmx",
            "../../devices/virtual/tpm/tpmrm0",
            "tpmrm0",
            "",
        ] {
            assert!(!is_resource_manager_entry(Path::new(target)), "{target}");
        }
    }

    #[test]
    fn devices_the_kernel_does_not_list_as_the_resource_manager_are_refused() {
        // /dev/null is a character device whose entry is mem/null.
        let error = DeviceChannel::open_resource_manager(Path::new("/dev/null")).unwrap_err();
        assert!(
            matches!(error, TpmError::NotResourceManagerDevice { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("/dev/null"), "{error}");
        let missing =
            DeviceChannel::open_resource_manager(Path::new("/dev/tpmrm99999")).unwrap_err();
        assert!(
            matches!(missing, TpmError::Open { ref source, .. } if source.kind() == io::ErrorKind::NotFound),
            "{missing}"
        );
    }
}
