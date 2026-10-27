//! The one thread that owns the TPM: requests arrive on a bounded queue and are sent to the TPM
//! one at a time, so commands never interleave and a slow signature (about 70 ms on the gateway's
//! TPM) blocks only this thread and the caller that waits for it.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread;
use std::time::Duration;

use super::TpmError;
use super::marshal::MAX_RESPONSE_BYTES;

/// How many requests may wait while the TPM works on another. The agent signs during one TLS
/// handshake at a time; a full queue is refused rather than waited on.
pub(crate) const QUEUE_CAPACITY: usize = 8;

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
    /// Starts the thread that owns `channel`. A response must arrive within `timeout`.
    pub(crate) fn spawn(channel: Box<dyn TpmChannel>, timeout: Duration) -> Result<Self, TpmError> {
        let (requests, queue) = mpsc::sync_channel(QUEUE_CAPACITY);
        thread::Builder::new()
            .name("tpm".to_owned())
            .spawn(move || serve(channel, &queue))
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
fn serve(mut channel: Box<dyn TpmChannel>, queue: &Receiver<Request>) {
    while let Ok(Request { command, reply }) = queue.recv() {
        let result = channel.transact(&command).map_err(TpmError::Io);
        // The caller may have given up waiting; then nobody needs the response.
        let _ = reply.send(result);
    }
}
