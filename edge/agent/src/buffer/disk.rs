//! The disk side of the offline buffer: append-only segment files in one directory.
//!
//! ```text
//! <directory>/seg-<20-digit id>.log    a segment: a 16-byte header, then records
//! <directory>/seg-<20-digit id>.tmp    a segment being created; removed at start (left, with a
//!                                      warning, when it cannot be)
//! <directory>/clock                    the age clock's time (DiskStore::save_clock)
//! <directory>/clock.tmp                a new clock file; replaced at the next save
//! ```
//!
//! The clock file is 32 bytes: `TIOTCLK\0`, its format version, four zero bytes, the age
//! clock's time in milliseconds, and the checksum of those 24 bytes. The buffer saves it while
//! segments hold batches, so that a restart does not start the age clock below where it was
//! ([`crate::clock::AgeClock`]); one that does not verify is ignored with a warning.
//!
//! A segment starts with [`SEGMENT_MAGIC`], the format version and four reserved zero bytes (all
//! checked: a file with anything else there is not ours, and is left alone). Each record
//! is a header and the batch's payload, exactly the bytes published on `up/telemetry`:
//!
//! | Offset | Size | Field |
//! | --- | --- | --- |
//! | 0 | 4 | [`RECORD_MAGIC`] |
//! | 4 | 4 | payload length, 1 to the telemetry channel's limit |
//! | 8 | 8 | sequence number |
//! | 16 | 8 | formation time on the age clock ([`crate::clock::AgeClock`]), ms |
//! | 24 | 8 | the batch's oldest sample time, ms |
//! | 32 | 4 | readings in the batch |
//! | 36 | 1 | stream ID length `n`, 1–64 |
//! | 37 | 3 | zero |
//! | 40 | 8 | payload checksum |
//! | 48 | `n` | stream ID |
//! | 48 + `n` | 8 | header checksum, over bytes 0 to 48 + `n` |
//!
//! Integers are little-endian; a checksum is the first 8 bytes of the SHA-256 (ring, the agent's
//! only crypto library) of what it covers. Start-up reads only the headers, so it never decodes
//! a payload: the header checksum makes a header trustworthy, and the payload checksum is checked
//! when the payload is read for sending, so a torn or corrupt record is never sent. After a
//! corrupt header, start-up reads on at the next position where an intact header starts (its
//! magic and its checksum), so one bad header loses only the bytes up to the next record.
//! Start-up keeps at most a given number of records (the newest by formation time; the others
//! are released and counted) and only the stream IDs those use, so its memory does not grow
//! with what the directory holds; it ends at the next record when the agent is stopping. A
//! stop whose directory had not opened opens it for appending only
//! ([`DiskStore::open_for_appends`]), reading nothing there but the last segment's headers.
//!
//! A store holds an exclusive `flock(2)` on the directory itself for as long as it exists, so
//! a second store (in this process or another agent) cannot open the directory meanwhile.
//!
//! A segment is created as a new `.tmp` with its header and fsynced, published as `.log` with a
//! hard link (`link(2)` never replaces an existing name), its `.tmp` name removed, and the
//! directory fsynced; records are appended and each fsynced unless a whole flush is synced at
//! once (every segment the flush wrote, not only the last; a failed sync leaves them not
//! durable). A process never appends to a segment of an earlier process. A segment is closed
//! for appends once it reaches its target size, and deleted once every record in it is
//! released: nothing is ever rewritten to release one batch. Files are created with mode 0600
//! in a directory of mode 0700 (an existing directory that is wider is set to 0700, or a
//! warning logged when the agent does not own it); a file is opened only when `lstat` and
//! `fstat` agree that it is the same regular file, so a symbolic link is never followed. Files
//! with other names are left alone, and the clock file is the only one ever replaced.
//! A new segment never takes a number that any name in the directory has, whatever that entry
//! is, and never replaces an existing entry: a number whose name is taken after start is skipped
//! and the entry left alone. The highest number is never used, so a directory that holds it
//! gets no new segment. Only a `.tmp` that this process created, or a regular file left from an
//! earlier one, is ever removed; a `.tmp` left by a crash right after publishing is a second
//! name of its segment, and removing it leaves the segment alone. The buffer needs a file
//! system with hard links (the data partition is ext4); without them no segment is created and
//! the disk counts as failed.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use protocol::topic::MAX_BULK_BYTES;
use protocol::{Seq, StreamId, UnixMillis};

/// The first bytes of every segment.
pub(crate) const SEGMENT_MAGIC: [u8; 8] = *b"TIOTSEG\0";
/// The segment format version.
pub(crate) const FORMAT_VERSION: u32 = 1;
/// The size of a segment's header.
pub(crate) const SEGMENT_HEADER_LEN: u64 = 16;
/// The first bytes of every record.
pub(crate) const RECORD_MAGIC: u32 = 0x3143_4552; // "REC1" little-endian
/// The fixed part of a record header, before the stream ID.
const FIXED_LEN: usize = 48;
/// The size of a checksum.
const SUM_LEN: usize = 8;
/// The longest stream ID ([`protocol::StreamId`]: 1–64 characters).
const MAX_STREAM_LEN: usize = 64;
/// The longest record header.
const MAX_HEADER_LEN: usize = FIXED_LEN + MAX_STREAM_LEN + SUM_LEN;
/// How much of a segment start-up reads at a time.
const READ_WINDOW: usize = 64 * 1024;

/// How many taken segment numbers one new segment skips before it gives up.
const MAX_TAKEN_NUMBERS: usize = 16;

/// The file that holds the age clock's time ([`DiskStore::save_clock`]).
pub(crate) const CLOCK_FILE: &str = "clock";
/// A new clock file, before it replaces the old one.
const CLOCK_TEMPORARY: &str = "clock.tmp";
/// The first bytes of the clock file.
const CLOCK_MAGIC: [u8; 8] = *b"TIOTCLK\0";
/// The clock file's format version.
const CLOCK_VERSION: u32 = 1;
/// The clock file's size: the magic, the version, four zero bytes, the time, and the checksum
/// of the 24 bytes before it.
const CLOCK_LEN: usize = 32;

/// What a record says about its batch, so that start-up never decodes a payload. The stream ID
/// is shared by the batches of a stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Meta {
    pub(crate) stream: Arc<StreamId>,
    pub(crate) seq: Seq,
    pub(crate) records: u32,
    pub(crate) oldest: UnixMillis,
    pub(crate) formed_at: u64,
}

/// Where a payload is on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DiskRef {
    pub(crate) segment: u64,
    /// The payload's offset in the segment.
    pub(crate) offset: u64,
    pub(crate) len: u32,
    pub(crate) sum: [u8; SUM_LEN],
}

/// A record found at start-up.
#[derive(Debug)]
pub(crate) struct Found {
    pub(crate) meta: Meta,
    pub(crate) at: DiskRef,
}

/// What start-up found besides the records.
#[derive(Debug, Default)]
pub(crate) struct Recovery {
    /// At most the limit [`DiskStore::open`] was given.
    pub(crate) found: Vec<Found>,
    /// Readings in records whose header is intact but whose payload was cut short.
    pub(crate) torn_records: u64,
    /// Bytes that are not part of a readable record: a header cut short or corrupt, up to the
    /// next intact header or the end of the segment. Their readings cannot be counted.
    pub(crate) unreadable_bytes: u64,
    /// Records beyond the limit (the oldest), released: batches and readings.
    pub(crate) over_limit: (u64, u64),
    /// The newest formation time in any record header that verified, whatever became of the
    /// record: kept, released over the limit, found twice, or with its payload cut short. A
    /// header that verified was written by this format, so the age clock reached that time;
    /// the age clock's floor must not start below it.
    pub(crate) newest: Option<u64>,
    /// The age clock's time in the clock file, when one is there and verifies
    /// ([`DiskStore::save_clock`]): the floor must not start below it either.
    pub(crate) clock: Option<u64>,
    /// Leftover `.tmp` files removed.
    pub(crate) removed_temporary: usize,
    /// Files that are not segments, left alone.
    pub(crate) ignored: Vec<String>,
    /// Tests only: the most records held at once while reading.
    #[cfg(test)]
    pub(crate) peak: usize,
    /// Tests only: the most stream IDs held at once while reading.
    #[cfg(test)]
    pub(crate) streams_peak: usize,
}

/// The stream IDs read at start-up, so that the records of a stream share one. Only IDs of
/// records still held are kept (see [`DiskStore::prune`]), so the map is as bounded as they are.
type Streams = HashMap<Box<str>, Arc<StreamId>>;

/// One segment's bookkeeping.
#[derive(Debug)]
struct Segment {
    /// The file's length as written.
    len: u64,
    /// Records not released yet.
    live: u32,
    /// No more appends: full, from an earlier process, or after a failed write.
    sealed: bool,
}

/// The segment being appended to.
#[derive(Debug)]
struct Writer {
    id: u64,
    file: File,
    /// Records were appended without an fsync since the last one.
    unsynced: bool,
}

/// The segment files of one directory.
#[derive(Debug)]
pub(crate) struct DiskStore {
    dir: PathBuf,
    /// The buffer directory, opened and exclusively locked (`flock`) for as long as this store
    /// exists: one store per directory, whatever the process.
    _lock: File,
    segments: BTreeMap<u64, Segment>,
    writer: Option<Writer>,
    /// The number of the next segment; `u64::MAX` when none is left.
    next_segment: u64,
    /// The sum of the segments' lengths.
    bytes: u64,
    /// A segment is closed once appending would take it past this size.
    target: u64,
    /// Segments closed for appends while they held records appended without an fsync, with
    /// their files kept open until [`DiskStore::sync`] syncs them.
    unsynced: Vec<(u64, File)>,
    /// Tests only: [`DiskStore::sync`] fails, as on EIO.
    #[cfg(test)]
    pub(super) fail_sync: bool,
    /// Tests only: the segments [`DiskStore::sync`] synced, in order.
    #[cfg(test)]
    pub(super) synced: Vec<u64>,
    /// Tests only: another process creates the final name of the segment with this number
    /// right before it is published.
    #[cfg(test)]
    pub(super) taken_before_publishing: Option<u64>,
}

/// The 8-byte checksum of `parts`.
pub(crate) fn checksum(parts: &[&[u8]]) -> [u8; SUM_LEN] {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    for part in parts {
        context.update(part);
    }
    let mut sum = [0_u8; SUM_LEN];
    sum.copy_from_slice(&context.finish().as_ref()[..SUM_LEN]);
    sum
}

/// A record's size on disk.
pub(crate) fn record_len(stream: &StreamId, payload_len: usize) -> u64 {
    (FIXED_LEN + stream.as_str().len() + SUM_LEN + payload_len) as u64
}

fn segment_name(id: u64, extension: &str) -> String {
    format!("seg-{id:020}.{extension}")
}

/// The id and extension of a file name `seg-<20 digits>.<log|tmp>`.
fn parse_name(name: &str) -> Option<(u64, &str)> {
    let rest = name.strip_prefix("seg-")?;
    let (digits, extension) = rest.split_once('.')?;
    if digits.len() != 20 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if extension != "log" && extension != "tmp" {
        return None;
    }
    Some((digits.parse().ok()?, extension))
}

/// Opens `path` for reading only if it is a regular file that `lstat` and `fstat` agree on, so
/// a symbolic link (or one swapped in meanwhile) is never followed.
fn open_regular(path: &Path) -> io::Result<File> {
    let before = fs::symlink_metadata(path)?;
    if !before.file_type().is_file() {
        return Err(io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let file = File::open(path)?;
    let after = file.metadata()?;
    if !after.file_type().is_file() || after.dev() != before.dev() || after.ino() != before.ino() {
        return Err(io::Error::other(format!(
            "{} changed while it was opened",
            path.display()
        )));
    }
    Ok(file)
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Sets an existing buffer directory whose mode lets others in to 0700, through a handle on the
/// directory `lstat` saw (never a symbolic link); only its owner can. Logs either way.
fn make_private(dir: &Path, before: &fs::Metadata) {
    let mode = before.mode() & 0o7777;
    if mode & 0o077 == 0 {
        return;
    }
    let changed = File::open(dir).and_then(|handle| {
        let after = handle.metadata()?;
        if !after.is_dir() || after.dev() != before.dev() || after.ino() != before.ino() {
            return Err(io::Error::other("it changed while it was opened"));
        }
        handle.set_permissions(Permissions::from_mode(0o700))
    });
    match changed {
        Ok(()) => tracing::warn!(
            directory = %dir.display(),
            mode = format!("{mode:o}"),
            "the buffer directory was open to other users; its mode is now 0700"
        ),
        Err(error) => tracing::warn!(
            directory = %dir.display(),
            mode = format!("{mode:o}"),
            %error,
            "the buffer directory is open to other users and the agent cannot change that (it does not own the directory); make it owned by the agent's user with mode 0700"
        ),
    }
}

/// Opens `dir` as the buffer directory: creates it with mode 0700 when it is missing (its parent
/// must exist), makes an existing one private ([`make_private`]), and takes an exclusive lock
/// on it, returned as the handle that holds it. The lock is `flock(2)` on the directory itself
/// (no lock file to create, follow or clean up); the kernel releases it when the handle is
/// closed, also when the process dies.
fn prepare_dir(dir: &Path) -> io::Result<File> {
    match fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.file_type().is_dir() => make_private(dir, &metadata),
        Ok(_) => {
            return Err(io::Error::other(format!(
                "{} is not a directory",
                dir.display()
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(dir)?;
        }
        Err(error) => return Err(error),
    }
    let before = fs::symlink_metadata(dir)?;
    let handle = File::open(dir)?;
    let after = handle.metadata()?;
    if !before.is_dir()
        || !after.is_dir()
        || after.dev() != before.dev()
        || after.ino() != before.ino()
    {
        return Err(io::Error::other(format!(
            "{} changed while it was opened",
            dir.display()
        )));
    }
    match handle.try_lock() {
        Ok(()) => Ok(handle),
        Err(fs::TryLockError::WouldBlock) => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "{} is in use by another agent (it holds the directory's lock)",
                dir.display()
            ),
        )),
        Err(fs::TryLockError::Error(error)) => Err(io::Error::new(
            error.kind(),
            format!("{}: cannot lock: {error}", dir.display()),
        )),
    }
}

/// Whether `file` starts with this format's segment header: the magic, the version, and four
/// reserved bytes that are zero. Anything else is not ours.
fn valid_segment_header(file: &File) -> bool {
    let mut header = [0_u8; 16];
    file.read_exact_at(&mut header, 0).is_ok()
        && header[..8] == SEGMENT_MAGIC
        && le_u32(&header, 8) == FORMAT_VERSION
        && le_u32(&header, 12) == 0
}

/// The clock file's bytes for the age clock's time `at`.
fn encode_clock(at: u64) -> [u8; CLOCK_LEN] {
    let mut bytes = [0_u8; CLOCK_LEN];
    bytes[..8].copy_from_slice(&CLOCK_MAGIC);
    bytes[8..12].copy_from_slice(&CLOCK_VERSION.to_le_bytes());
    bytes[16..24].copy_from_slice(&at.to_le_bytes());
    let sum = checksum(&[&bytes[..24]]);
    bytes[24..].copy_from_slice(&sum);
    bytes
}

/// The age clock's time in the clock file of `dir` ([`DiskStore::save_clock`]): `None`
/// without one, and, with a warning, when it is not a regular file of this format or its
/// checksum does not match (the next save replaces it).
pub(super) fn read_clock(dir: &Path) -> Option<u64> {
    let path = dir.join(CLOCK_FILE);
    let read = (|| {
        let file = open_regular(&path)?;
        if file.metadata()?.len() != CLOCK_LEN as u64 {
            return Err(io::Error::other("it is not the size of a clock file"));
        }
        let mut bytes = [0_u8; CLOCK_LEN];
        file.read_exact_at(&mut bytes, 0)?;
        if bytes[..8] != CLOCK_MAGIC
            || le_u32(&bytes, 8) != CLOCK_VERSION
            || le_u32(&bytes, 12) != 0
            || bytes[24..] != checksum(&[&bytes[..24]])
        {
            return Err(io::Error::other(
                "it is not a clock file of this format, or its checksum does not match",
            ));
        }
        Ok(le_u64(&bytes, 16))
    })();
    match read {
        Ok(at) => Some(at),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::warn!(file = %path.display(), %error, "cannot use the buffer's clock file; the age clock starts from the wall clock and the batches on disk");
            None
        }
    }
}

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0_u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

fn le_u64(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0_u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

/// The header of a record for `meta` and a payload of `len` bytes with checksum `sum`.
fn encode_header(meta: &Meta, len: u32, sum: &[u8; SUM_LEN]) -> Vec<u8> {
    let stream = meta.stream.as_str().as_bytes();
    let mut header = Vec::with_capacity(FIXED_LEN + stream.len() + SUM_LEN);
    header.extend_from_slice(&RECORD_MAGIC.to_le_bytes());
    header.extend_from_slice(&len.to_le_bytes());
    header.extend_from_slice(&meta.seq.get().to_le_bytes());
    header.extend_from_slice(&meta.formed_at.to_le_bytes());
    header.extend_from_slice(&meta.oldest.get().to_le_bytes());
    header.extend_from_slice(&meta.records.to_le_bytes());
    // A stream ID is at most 64 bytes.
    header.push(u8::try_from(stream.len()).unwrap_or(u8::MAX));
    header.extend_from_slice(&[0, 0, 0]);
    header.extend_from_slice(sum);
    header.extend_from_slice(stream);
    let header_sum = checksum(&[&header]);
    header.extend_from_slice(&header_sum);
    header
}

/// Why a header could not be read.
enum HeaderError {
    /// The bytes end inside it.
    Short,
    /// Its bytes are not a valid header.
    Corrupt(&'static str),
    /// Reading failed.
    Io(io::Error),
}

/// A header that was read: the record's metadata, its payload length and checksum, and the
/// header's own length.
type Header = (Meta, u32, [u8; SUM_LEN], u64);

/// Reads the header at the start of `bytes` (at most [`MAX_HEADER_LEN`] are looked at); its
/// stream ID is shared through `streams`.
fn parse_header(bytes: &[u8], streams: &mut Streams) -> Result<Header, HeaderError> {
    let fixed = bytes.get(..FIXED_LEN).ok_or(HeaderError::Short)?;
    if le_u32(fixed, 0) != RECORD_MAGIC {
        return Err(HeaderError::Corrupt("record magic"));
    }
    let len = le_u32(fixed, 4);
    let stream_len = usize::from(fixed[36]);
    if !(1..=MAX_STREAM_LEN).contains(&stream_len) || fixed[37..40] != [0, 0, 0] {
        return Err(HeaderError::Corrupt("stream ID length"));
    }
    let header_len = FIXED_LEN + stream_len + SUM_LEN;
    let rest = bytes.get(FIXED_LEN..header_len).ok_or(HeaderError::Short)?;
    let (stream, stored_sum) = rest.split_at(stream_len);
    if checksum(&[fixed, stream]) != stored_sum {
        return Err(HeaderError::Corrupt("header checksum"));
    }
    // The checksum matched, so these are the values that were written; still refuse values
    // the writer never produces.
    if len == 0 || usize::try_from(len).is_ok_and(|len| len > MAX_BULK_BYTES) {
        return Err(HeaderError::Corrupt("payload length"));
    }
    let seq = Seq::new(le_u64(fixed, 8)).ok_or(HeaderError::Corrupt("sequence number"))?;
    let text = std::str::from_utf8(stream).map_err(|_| HeaderError::Corrupt("stream ID"))?;
    let stream = match streams.get(text) {
        Some(shared) => Arc::clone(shared),
        None => {
            let id = StreamId::new(text).map_err(|_| HeaderError::Corrupt("stream ID"))?;
            let shared = Arc::new(id);
            streams.insert(text.into(), Arc::clone(&shared));
            shared
        }
    };
    let mut sum = [0_u8; SUM_LEN];
    sum.copy_from_slice(&fixed[40..48]);
    let meta = Meta {
        stream,
        seq,
        formed_at: le_u64(fixed, 16),
        oldest: UnixMillis::new(le_u64(fixed, 24)),
        records: le_u32(fixed, 32),
    };
    Ok((meta, len, sum, header_len as u64))
}

/// Reads a segment through a window of [`READ_WINDOW`] bytes: few system calls per record, and
/// never the whole segment in memory.
struct Reader<'a> {
    file: &'a File,
    len: u64,
    start: u64,
    data: Vec<u8>,
}

impl<'a> Reader<'a> {
    fn new(file: &'a File, len: u64) -> Self {
        Self {
            file,
            len,
            start: 0,
            data: Vec::new(),
        }
    }

    /// Up to `want` (at most [`READ_WINDOW`]) bytes at `offset`; fewer at the end of the file.
    fn at(&mut self, offset: u64, want: usize) -> io::Result<&[u8]> {
        let end = offset.saturating_add(want as u64).min(self.len);
        let offset = offset.min(end);
        if offset < self.start || end > self.start + self.data.len() as u64 {
            let size = usize::try_from(self.len.saturating_sub(offset).min(READ_WINDOW as u64))
                .unwrap_or(READ_WINDOW);
            self.data.resize(size, 0);
            self.file.read_exact_at(&mut self.data, offset)?;
            self.start = offset;
        }
        let from = usize::try_from(offset - self.start).unwrap_or(0);
        let to = usize::try_from(end - self.start).unwrap_or(0);
        Ok(&self.data[from..to])
    }

    /// The offset of the first intact record header at or after `from`: [`RECORD_MAGIC`] and a
    /// header whose checksum verifies.
    fn next_header(&mut self, from: u64, streams: &mut Streams) -> io::Result<Option<u64>> {
        let magic = RECORD_MAGIC.to_le_bytes();
        let mut at = from;
        while at < self.len {
            let chunk = self.at(at, READ_WINDOW)?;
            if chunk.len() < magic.len() {
                break;
            }
            // The next chunk overlaps this one by three bytes, so no magic is cut in two.
            let step = (chunk.len() - (magic.len() - 1)) as u64;
            let candidates: Vec<u64> = chunk
                .windows(magic.len())
                .enumerate()
                .filter(|(_, window)| *window == magic)
                .map(|(position, _)| at + position as u64)
                .collect();
            for candidate in candidates {
                let bytes = self.at(candidate, MAX_HEADER_LEN)?;
                if parse_header(bytes, streams).is_ok() {
                    return Ok(Some(candidate));
                }
            }
            at += step;
        }
        Ok(None)
    }
}

/// Reads and checks the payload at `at` in the segments of `dir`.
///
/// # Errors
///
/// The segment cannot be opened or read, or the payload's checksum does not match.
pub(crate) fn read_payload(dir: &Path, at: &DiskRef) -> Result<Vec<u8>, String> {
    let path = dir.join(segment_name(at.segment, "log"));
    let file = open_regular(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut payload = vec![0_u8; usize::try_from(at.len).unwrap_or(usize::MAX)];
    file.read_exact_at(&mut payload, at.offset)
        .map_err(|error| format!("{} at {}: {error}", path.display(), at.offset))?;
    if checksum(&[&payload]) != at.sum {
        return Err(format!(
            "{} at {}: the payload's checksum does not match",
            path.display(),
            at.offset
        ));
    }
    Ok(payload)
}

/// When start-up thins out what it found to `limit` records: half as many again, so the work of
/// thinning stays linear in what is read.
fn prune_at(limit: usize) -> usize {
    limit.saturating_add(limit / 2).max(1)
}

impl DiskStore {
    /// [`DiskStore::open_until`] with all the time it takes.
    #[cfg(test)]
    pub(crate) fn open(dir: &Path, target: u64, limit: usize) -> io::Result<(Self, Recovery)> {
        Self::open_until(dir, target, limit, &mut || true)
    }

    /// An empty store on the locked directory `dir`, whose new segments start at
    /// `next_segment`.
    fn new(dir: &Path, lock: File, next_segment: u64, target: u64) -> Self {
        Self {
            dir: dir.to_owned(),
            _lock: lock,
            segments: BTreeMap::new(),
            writer: None,
            next_segment,
            bytes: 0,
            target,
            unsynced: Vec::new(),
            #[cfg(test)]
            fail_sync: false,
            #[cfg(test)]
            synced: Vec::new(),
            #[cfg(test)]
            taken_before_publishing: None,
        }
    }

    /// Opens the buffer directory `dir` (creating it with mode 0700 if missing; its parent must
    /// exist) and locks it, removes leftover temporary files and reads every segment's record
    /// headers, keeping at most `limit` records: the newest by formation time; the others are
    /// released and counted in [`Recovery::over_limit`]. Segments with no record left are
    /// deleted. Reading stops, and the open fails, as soon as `more` says there is no time
    /// left (the agent is stopping); nothing but leftover temporary files is deleted by then.
    ///
    /// # Errors
    ///
    /// `dir` cannot be created or listed, is not a directory (a symbolic link included), or
    /// another store (in this process or another) has it locked; or `more` ran out.
    pub(crate) fn open_until(
        dir: &Path,
        target: u64,
        limit: usize,
        more: &mut dyn FnMut() -> bool,
    ) -> io::Result<(Self, Recovery)> {
        let lock = prepare_dir(dir)?;
        let mut recovery = Recovery::default();
        let mut candidates = Vec::new();
        let mut next_segment = 0;
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let file_type = entry.file_type()?;
            match name.to_str().and_then(parse_name) {
                Some((id, extension)) => {
                    // No new segment takes a number in use, whatever the entry is; the highest
                    // number in use leaves none.
                    next_segment = next_segment.max(id.saturating_add(1));
                    if !file_type.is_file() {
                        recovery
                            .ignored
                            .push(crate::link::printable(&name.to_string_lossy(), 100));
                    } else if extension == "tmp" {
                        // Its number stays taken whether or not it can be removed.
                        match fs::remove_file(entry.path()) {
                            Ok(()) => recovery.removed_temporary += 1,
                            Err(error) => {
                                tracing::warn!(file = %entry.path().display(), %error, "cannot remove a leftover temporary buffer file; leaving it");
                            }
                        }
                    } else {
                        candidates.push(id);
                    }
                }
                // The clock file is read below; a leftover new one is replaced at the next save.
                None if name == CLOCK_FILE || name == CLOCK_TEMPORARY => {}
                None => recovery
                    .ignored
                    .push(crate::link::printable(&name.to_string_lossy(), 100)),
            }
        }
        recovery.clock = read_clock(dir);
        candidates.sort_unstable();
        let mut store = Self::new(dir, lock, next_segment, target);
        let mut streams = Streams::new();
        for id in candidates {
            if !store.recover_segment(id, &mut recovery, &mut streams, limit, more) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!(
                        "{}: the agent is stopping; the buffer directory was not read to its end",
                        dir.display()
                    ),
                ));
            }
        }
        store.prune(&mut recovery, &mut streams, limit);
        // Segments with no record left: nothing readable in them, or only batches over the
        // limit.
        store.sweep_all();
        Ok((store, recovery))
    }

    /// Opens and locks the buffer directory `dir` for appending only, as a stop does when the
    /// directory could not be opened before: it lists the names (no new segment takes a number
    /// in use) and the sizes of the segments there (they count against the disk limit), but
    /// reads, removes and releases nothing of theirs; the next start recovers them. Returns the
    /// floor for the age clock: the later of the clock file's time ([`DiskStore::save_clock`])
    /// and the newest formation time in the highest-numbered segment, whose records an earlier
    /// process wrote last, read while `more` says there is time (`None` without either).
    ///
    /// # Errors
    ///
    /// As [`DiskStore::open_until`], except that running out of time only leaves the newest
    /// formation time unknown.
    pub(crate) fn open_for_appends(
        dir: &Path,
        target: u64,
        more: &mut dyn FnMut() -> bool,
    ) -> io::Result<(Self, Option<u64>)> {
        let lock = prepare_dir(dir)?;
        let mut next_segment = 0;
        let mut bytes = 0;
        let mut segments = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some((id, extension)) = name.to_str().and_then(parse_name) else {
                continue;
            };
            next_segment = next_segment.max(id.saturating_add(1));
            // On Unix `DirEntry::metadata` does not follow symbolic links, so a linked name is
            // not a regular file here and its target's size is never counted.
            let metadata = entry.metadata()?;
            if metadata.file_type().is_file() && extension == "log" {
                bytes += metadata.len();
                segments.push(id);
            }
        }
        let mut store = Self::new(dir, lock, next_segment, target);
        store.bytes = bytes;
        segments.sort_unstable();
        let mut newest = None;
        for id in segments.into_iter().rev() {
            if !more() {
                break;
            }
            newest = store.newest_in(id, more);
            if newest.is_some() {
                break;
            }
        }
        Ok((store, newest.max(read_clock(dir))))
    }

    /// The newest formation time among the records of segment `id`, read while `more` says
    /// there is time; `None` when the segment has no valid header or no readable record.
    fn newest_in(&self, id: u64, more: &mut dyn FnMut() -> bool) -> Option<u64> {
        let file = open_regular(&self.dir.join(segment_name(id, "log"))).ok()?;
        let file_len = file.metadata().ok()?.len();
        if !valid_segment_header(&file) {
            return None;
        }
        let mut reader = Reader::new(&file, file_len);
        let mut streams = Streams::new();
        let mut newest: Option<u64> = None;
        let mut offset = SEGMENT_HEADER_LEN;
        while offset < file_len && more() {
            // Only the formation times matter here: no stream ID is kept.
            streams.clear();
            match reader
                .at(offset, MAX_HEADER_LEN)
                .ok()
                .map(|bytes| parse_header(bytes, &mut streams))
            {
                Some(Ok((meta, len, _, header_len))) => {
                    newest = newest.max(Some(meta.formed_at));
                    offset += header_len + u64::from(len);
                }
                Some(Err(_)) => match reader.next_header(offset + 1, &mut streams) {
                    Ok(Some(next)) => offset = next,
                    _ => break,
                },
                None => break,
            }
        }
        newest
    }

    /// Reads the headers of segment `id` into `recovery`. A segment whose own header is wrong is
    /// not ours to judge: it is left alone and listed as ignored. `false` when `more` ran out
    /// before the end.
    fn recover_segment(
        &mut self,
        id: u64,
        recovery: &mut Recovery,
        streams: &mut Streams,
        limit: usize,
        more: &mut dyn FnMut() -> bool,
    ) -> bool {
        let name = segment_name(id, "log");
        let path = self.dir.join(&name);
        let file = match open_regular(&path) {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(segment = %path.display(), %error, "cannot open a buffer segment; leaving it alone");
                recovery.ignored.push(name);
                return true;
            }
        };
        let file_len = match file.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                tracing::warn!(segment = %path.display(), %error, "cannot read a buffer segment; leaving it alone");
                recovery.ignored.push(name);
                return true;
            }
        };
        if !valid_segment_header(&file) {
            tracing::warn!(segment = %path.display(), "a buffer segment has no valid segment header; leaving it alone");
            recovery.ignored.push(name);
            return true;
        }
        self.segments.insert(
            id,
            Segment {
                len: file_len,
                live: 0,
                sealed: true,
            },
        );
        self.bytes += file_len;
        let mut reader = Reader::new(&file, file_len);
        let mut offset = SEGMENT_HEADER_LEN;
        while offset < file_len {
            if !more() {
                return false;
            }
            let header = match reader.at(offset, MAX_HEADER_LEN) {
                Ok(bytes) => parse_header(bytes, streams),
                Err(error) => Err(HeaderError::Io(error)),
            };
            let what = match header {
                Ok((meta, len, sum, header_len)) => {
                    recovery.newest = recovery.newest.max(Some(meta.formed_at));
                    let payload = offset + header_len;
                    if payload + u64::from(len) > file_len {
                        tracing::warn!(segment = %path.display(), offset, records = meta.records, "the last record of a buffer segment was cut short; dropping it");
                        recovery.torn_records += u64::from(meta.records);
                        break;
                    }
                    recovery.found.push(Found {
                        meta,
                        at: DiskRef {
                            segment: id,
                            offset: payload,
                            len,
                            sum,
                        },
                    });
                    #[cfg(test)]
                    {
                        recovery.peak = recovery.peak.max(recovery.found.len());
                        recovery.streams_peak = recovery.streams_peak.max(streams.len());
                    }
                    if let Some(segment) = self.segments.get_mut(&id) {
                        segment.live += 1;
                    }
                    offset = payload + u64::from(len);
                    if recovery.found.len() >= prune_at(limit) {
                        self.prune(recovery, streams, limit);
                    }
                    continue;
                }
                Err(HeaderError::Io(error)) => {
                    tracing::warn!(segment = %path.display(), offset, %error, "cannot read a buffer segment; dropping the rest");
                    recovery.unreadable_bytes += file_len - offset;
                    break;
                }
                Err(HeaderError::Short) => "cut short",
                Err(HeaderError::Corrupt(what)) => what,
            };
            match reader.next_header(offset + 1, streams) {
                Ok(Some(next)) => {
                    tracing::warn!(segment = %path.display(), offset, bytes = next - offset, field = what, "a corrupt record header in a buffer segment; reading on at the next intact record");
                    recovery.unreadable_bytes += next - offset;
                    offset = next;
                }
                Ok(None) => {
                    tracing::warn!(segment = %path.display(), offset, bytes = file_len - offset, field = what, "a corrupt or partial record header in a buffer segment, and no intact record after it; dropping the rest");
                    recovery.unreadable_bytes += file_len - offset;
                    break;
                }
                Err(error) => {
                    tracing::warn!(segment = %path.display(), offset, %error, "cannot read a buffer segment; dropping the rest");
                    recovery.unreadable_bytes += file_len - offset;
                    break;
                }
            }
        }
        true
    }

    /// Keeps the `limit` newest records found so far (by formation time; the earlier written
    /// first among equals) and releases the others, counting them, and forgets the stream IDs
    /// no kept record uses (a directory of records on distinct streams would otherwise fill
    /// `streams` with every ID ever read). Segments are not deleted here:
    /// [`DiskStore::open_until`] does that once every segment is read.
    fn prune(&mut self, recovery: &mut Recovery, streams: &mut Streams, limit: usize) {
        let Some(excess) = recovery
            .found
            .len()
            .checked_sub(limit)
            .filter(|excess| *excess > 0)
        else {
            return;
        };
        if excess < recovery.found.len() {
            recovery.found.select_nth_unstable_by_key(excess, |found| {
                (found.meta.formed_at, found.at.segment, found.at.offset)
            });
        }
        for found in recovery.found.drain(..excess) {
            if let Some(segment) = self.segments.get_mut(&found.at.segment) {
                segment.live = segment.live.saturating_sub(1);
            }
            recovery.over_limit.0 += 1;
            recovery.over_limit.1 += u64::from(found.meta.records);
        }
        // An ID only the map holds belongs to no kept record.
        streams.retain(|_, stream| Arc::strong_count(stream) > 1);
    }

    /// The buffer directory.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Bytes of segment files on disk.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Saves the age clock's time `at` in the clock file, so that the next start's age clock
    /// does not start below it: a new temporary file (`O_EXCL`, after removing a leftover one)
    /// with the time and its checksum, fsynced, renamed over the clock file, and the directory
    /// fsynced. A crash at any step leaves the old clock file or the new one, each whole. The
    /// clock file is the only file this store replaces: it is its own, in a directory it holds
    /// locked.
    ///
    /// # Errors
    ///
    /// Creating, writing, syncing or renaming the file, or syncing the directory, failed.
    pub(crate) fn save_clock(&self, at: u64) -> io::Result<()> {
        let temporary = self.dir.join(CLOCK_TEMPORARY);
        let path = self.dir.join(CLOCK_FILE);
        let in_context = |error: io::Error, path: &Path| {
            io::Error::new(error.kind(), format!("{}: {error}", path.display()))
        };
        match fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(in_context(error, &temporary)),
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| in_context(error, &temporary))?;
        let written = file
            .write_all(&encode_clock(at))
            .and_then(|()| file.sync_all())
            .and_then(|()| fs::rename(&temporary, &path));
        if let Err(error) = written {
            // The temporary file is this call's own: it was created anew above.
            let _ = fs::remove_file(&temporary);
            return Err(in_context(error, &path));
        }
        sync_dir(&self.dir).map_err(|error| in_context(error, &self.dir))
    }

    /// The disk space appending a record of `len` bytes takes, a new segment's header included
    /// when the record does not go into the open segment.
    pub(crate) fn space_for(&self, len: u64) -> u64 {
        match self.open_segment_len() {
            Some(current) if current + len <= self.target || current == SEGMENT_HEADER_LEN => len,
            _ => SEGMENT_HEADER_LEN + len,
        }
    }

    fn open_segment_len(&self) -> Option<u64> {
        let writer = self.writer.as_ref()?;
        self.segments.get(&writer.id).map(|segment| segment.len)
    }

    /// Whether segments closed for appends hold records appended without an fsync: a flush
    /// syncs them as it goes, so that its last sync has at most one segment to write.
    pub(crate) fn has_closed_unsynced(&self) -> bool {
        !self.unsynced.is_empty()
    }

    /// Appends a record, fsynced when `sync` is set (otherwise [`DiskStore::sync`] makes it
    /// durable). A failed write closes the segment for appends and cuts the partial record off
    /// where possible; the record is not stored.
    pub(crate) fn append(
        &mut self,
        meta: &Meta,
        payload: &[u8],
        sync: bool,
    ) -> io::Result<DiskRef> {
        let len = u32::try_from(payload.len())
            .ok()
            .filter(|len| *len > 0 && payload.len() <= MAX_BULK_BYTES)
            .ok_or_else(|| io::Error::other("payload length out of range"))?;
        let record = record_len(&meta.stream, payload.len());
        let needs_segment = match self.open_segment_len() {
            Some(current) => current + record > self.target && current > SEGMENT_HEADER_LEN,
            None => true,
        };
        if needs_segment {
            self.seal_writer();
            self.create_segment()?;
        }
        let Some(writer) = &mut self.writer else {
            return Err(io::Error::other("no open segment"));
        };
        let id = writer.id;
        let start = self
            .segments
            .get(&id)
            .map_or(SEGMENT_HEADER_LEN, |segment| segment.len);
        let sum = checksum(&[payload]);
        let header = encode_header(meta, len, &sum);
        let written = writer
            .file
            .write_all(&header)
            .and_then(|()| writer.file.write_all(payload))
            .and_then(|()| {
                if sync {
                    writer.file.sync_data()
                } else {
                    Ok(())
                }
            });
        if let Err(error) = written {
            // Whatever reached the file is not a record; cut it off so the segment stays
            // readable to its end, and never append to it again.
            let _ = writer.file.set_len(start);
            let after = writer
                .file
                .metadata()
                .map_or(start + record, |metadata| metadata.len());
            if let Some(segment) = self.segments.get_mut(&id) {
                self.bytes = self.bytes - segment.len + after;
                segment.len = after;
            }
            self.seal_writer();
            return Err(error);
        }
        // An fsync covers every earlier write to the file.
        writer.unsynced = !sync;
        if let Some(segment) = self.segments.get_mut(&id) {
            segment.len += record;
            segment.live += 1;
        }
        self.bytes += record;
        Ok(DiskRef {
            segment: id,
            offset: start + record - u64::from(len),
            len,
            sum,
        })
    }
    /// Makes every record appended so far durable: the open segment's and those of every
    /// segment closed since with records not yet synced. Tries them all; the first failure is
    /// returned, and then none of those records counts as durable.
    pub(crate) fn sync(&mut self) -> io::Result<()> {
        #[cfg(test)]
        if self.fail_sync {
            return Err(io::Error::other("injected sync failure"));
        }
        let mut result = Ok(());
        for (id, file) in std::mem::take(&mut self.unsynced) {
            #[cfg(test)]
            self.synced.push(id);
            if let Err(error) = file.sync_data()
                && result.is_ok()
            {
                result = Err(io::Error::new(
                    error.kind(),
                    format!("segment {id}: {error}"),
                ));
            }
        }
        if let Some(writer) = &mut self.writer
            && writer.unsynced
        {
            #[cfg(test)]
            self.synced.push(writer.id);
            match writer.file.sync_data() {
                Ok(()) => writer.unsynced = false,
                Err(error) => {
                    if result.is_ok() {
                        result = Err(error);
                    }
                }
            }
        }
        result
    }

    /// Tests only: the segments on disk.
    #[cfg(test)]
    pub(super) fn segment_ids(&self) -> Vec<u64> {
        self.segments.keys().copied().collect()
    }

    /// Releases one record of `segment`; the segment is deleted once none is left.
    pub(crate) fn release(&mut self, segment: u64) {
        if let Some(entry) = self.segments.get_mut(&segment) {
            entry.live = entry.live.saturating_sub(1);
        }
        self.sweep(segment);
    }

    /// Deletes every segment without live records that is still on disk, for example after a
    /// deletion failed.
    pub(crate) fn sweep_all(&mut self) {
        let empty: Vec<u64> = self
            .segments
            .iter()
            .filter(|(_, segment)| segment.live == 0)
            .map(|(id, _)| *id)
            .collect();
        for id in empty {
            self.sweep(id);
        }
    }

    /// Deletes segment `id` if it has no live record (closing it for appends first).
    fn sweep(&mut self, id: u64) {
        let Some(segment) = self.segments.get(&id) else {
            return;
        };
        if segment.live > 0 {
            return;
        }
        if self.writer.as_ref().is_some_and(|writer| writer.id == id) {
            self.seal_writer();
        }
        let path = self.dir.join(segment_name(id, "log"));
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(segment = %path.display(), %error, "cannot delete a released buffer segment; trying again later");
                return;
            }
        }
        if let Some(segment) = self.segments.remove(&id) {
            self.bytes = self.bytes.saturating_sub(segment.len);
        }
    }

    /// Closes the open segment for appends. Its file stays open for [`DiskStore::sync`] when it
    /// holds records not synced yet: dropping it would leave them never synced.
    fn seal_writer(&mut self) {
        if let Some(writer) = self.writer.take() {
            if let Some(segment) = self.segments.get_mut(&writer.id) {
                segment.sealed = true;
            }
            if writer.unsynced {
                self.unsynced.push((writer.id, writer.file));
            }
        }
    }

    /// Creates the next segment under a number whose names are free, skipping (and leaving
    /// alone) up to [`MAX_TAKEN_NUMBERS`] numbers whose temporary or final name some other
    /// entry has taken since start.
    fn create_segment(&mut self) -> io::Result<()> {
        let mut skipped = 0;
        loop {
            match self.create_segment_once() {
                Err(error)
                    if error.kind() == io::ErrorKind::AlreadyExists
                        && skipped < MAX_TAKEN_NUMBERS =>
                {
                    skipped += 1;
                    tracing::warn!(%error, "a buffer segment name is taken by an entry the agent did not create; leaving it alone and using the next number");
                }
                other => return other,
            }
        }
    }

    /// Creates segment `next_segment`: its header in a new temporary file (`O_EXCL`: an entry
    /// of that name is left alone and the number skipped), fsync, a hard link to the final name,
    /// removal of the temporary name, fsync of the directory. `link(2)` fails atomically when
    /// the final name exists, so an entry that appeared there is never replaced; only the
    /// temporary file this call created is removed. A crash between the link and the removal
    /// leaves the temporary name as a second name of the segment, which [`DiskStore::open`]
    /// removes without touching the segment. A file system without hard links fails here like
    /// any other disk error: falling back to `rename` would replace an entry.
    fn create_segment_once(&mut self) -> io::Result<()> {
        let id = self.next_segment;
        if id == u64::MAX {
            return Err(io::Error::other(format!(
                "no segment number is left: {} is in the directory",
                segment_name(u64::MAX, "log")
            )));
        }
        self.next_segment = id + 1;
        let temporary = self.dir.join(segment_name(id, "tmp"));
        let path = self.dir.join(segment_name(id, "log"));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("{}: {error}", temporary.display()),
                ));
            }
        };
        #[cfg(test)]
        let taken = self.taken_before_publishing == Some(id);
        let published = (|| {
            let mut header = Vec::with_capacity(16);
            header.extend_from_slice(&SEGMENT_MAGIC);
            header.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
            header.extend_from_slice(&0_u32.to_le_bytes());
            file.write_all(&header)?;
            file.sync_all()?;
            #[cfg(test)]
            if taken {
                fs::write(&path, b"not ours")?;
            }
            fs::hard_link(&temporary, &path).map_err(|error| {
                let what = if error.kind() == io::ErrorKind::AlreadyExists {
                    String::new()
                } else {
                    " (the buffer needs a file system with hard links)".to_owned()
                };
                io::Error::new(
                    error.kind(),
                    format!("cannot link {}{what}: {error}", path.display()),
                )
            })
        })();
        if let Err(error) = published {
            // The temporary file is this call's own: it was created anew above.
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        // Published: the temporary name is a second name of the segment now.
        if let Err(error) = fs::remove_file(&temporary)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(file = %temporary.display(), %error, "cannot remove the temporary name of a new buffer segment; the next start removes it");
        }
        if let Err(error) = sync_dir(&self.dir) {
            // Published but the directory could not be synced: use it no more, but keep its size
            // counted until it is swept.
            self.segments.insert(
                id,
                Segment {
                    len: SEGMENT_HEADER_LEN,
                    live: 0,
                    sealed: true,
                },
            );
            self.bytes += SEGMENT_HEADER_LEN;
            self.sweep(id);
            return Err(error);
        }
        self.segments.insert(
            id,
            Segment {
                len: SEGMENT_HEADER_LEN,
                live: 0,
                sealed: false,
            },
        );
        self.bytes += SEGMENT_HEADER_LEN;
        self.writer = Some(Writer {
            id,
            file,
            unsynced: false,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(stream: &str, seq: u64) -> Meta {
        Meta {
            stream: Arc::new(StreamId::new(stream).unwrap()),
            seq: Seq::new(seq).unwrap(),
            records: 3,
            oldest: UnixMillis::new(1_700_000_000_000 + seq),
            formed_at: 1_700_000_001_000 + seq,
        }
    }

    fn scratch() -> testkit::mqtt::ScratchDir {
        testkit::mqtt::ScratchDir::new("agent-disk")
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_checksum_is_a_sha256_prefix() {
        // SHA-256("abc") = ba7816bf8f01cfea...
        assert_eq!(
            checksum(&[b"a", b"bc"]),
            [0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea]
        );
    }

    #[test]
    fn records_survive_reopening_in_order_with_their_metadata() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        let (mut store, recovery) = DiskStore::open(&dir, 1024 * 1024, usize::MAX).unwrap();
        assert!(recovery.found.is_empty());
        let mode = fs::metadata(&dir).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let first = store.append(&meta("s1", 1), b"payload one", true).unwrap();
        let second = store.append(&meta("s1", 2), b"payload two!", true).unwrap();
        assert_eq!(read_payload(&dir, &first).unwrap(), b"payload one");
        assert_eq!(read_payload(&dir, &second).unwrap(), b"payload two!");
        let segment = dir.join(segment_name(first.segment, "log"));
        assert_eq!(fs::metadata(&segment).unwrap().mode() & 0o777, 0o600);
        assert_eq!(store.bytes(), fs::metadata(&segment).unwrap().len());
        drop(store);

        let (store, recovery) = DiskStore::open(&dir, 1024 * 1024, usize::MAX).unwrap();
        let found: Vec<(Meta, DiskRef)> = recovery
            .found
            .into_iter()
            .map(|found| (found.meta, found.at))
            .collect();
        assert_eq!(found, vec![(meta("s1", 1), first), (meta("s1", 2), second)]);
        assert_eq!(recovery.torn_records, 0);
        assert_eq!(recovery.unreadable_bytes, 0);
        assert!(store.bytes() > 0);
    }

    #[test]
    fn segments_close_at_their_target_and_go_once_released() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        let payload = vec![7_u8; 1000];
        let record = record_len(&StreamId::new("s1").unwrap(), payload.len());
        // Two records fit in a segment.
        let (mut store, _) =
            DiskStore::open(&dir, SEGMENT_HEADER_LEN + 2 * record, usize::MAX).unwrap();
        let refs: Vec<DiskRef> = (1..=5)
            .map(|seq| store.append(&meta("s1", seq), &payload, true).unwrap())
            .collect();
        let segments: Vec<u64> = refs.iter().map(|at| at.segment).collect();
        assert_eq!(segments, [0, 0, 1, 1, 2]);
        assert_eq!(files(&dir).len(), 3);
        assert_eq!(store.bytes(), 3 * SEGMENT_HEADER_LEN + 5 * record);
        store.release(0);
        assert_eq!(
            files(&dir).len(),
            3,
            "one record of segment 0 is still live"
        );
        store.release(0);
        assert_eq!(files(&dir).len(), 2, "segment 0 is gone once released");
        store.release(2);
        assert_eq!(
            files(&dir),
            [segment_name(1, "log")],
            "the open segment goes too once nothing in it is live"
        );
        assert_eq!(store.bytes(), SEGMENT_HEADER_LEN + 2 * record);
        // The next append opens a new segment rather than appending to a deleted one.
        let next = store.append(&meta("s1", 6), &payload, true).unwrap();
        assert_eq!(next.segment, 3);
        assert_eq!(read_payload(&dir, &next).unwrap(), payload);
    }

    #[test]
    fn a_torn_record_is_dropped_and_a_corrupt_payload_is_never_read() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        let (mut store, _) = DiskStore::open(&dir, 1024 * 1024, usize::MAX).unwrap();
        let good = store.append(&meta("s1", 1), b"good payload", true).unwrap();
        let corrupt = store
            .append(&meta("s1", 2), b"corrupt payload", true)
            .unwrap();
        let torn = store.append(&meta("s1", 3), b"torn payload", true).unwrap();
        drop(store);
        let path = dir.join(segment_name(0, "log"));
        // One flipped bit in the second payload; the third is cut in the middle, as a crash
        // during its write leaves it.
        let mut bytes = fs::read(&path).unwrap();
        bytes[usize::try_from(corrupt.offset).unwrap() + 3] ^= 0x10;
        bytes.truncate(usize::try_from(torn.offset).unwrap() + 4);
        fs::write(&path, &bytes).unwrap();

        let (_store, recovery) = DiskStore::open(&dir, 1024 * 1024, usize::MAX).unwrap();
        let found: Vec<u64> = recovery
            .found
            .iter()
            .map(|found| found.meta.seq.get())
            .collect();
        assert_eq!(found, [1, 2], "the torn record is dropped at start");
        assert_eq!(recovery.torn_records, 3, "its readings are counted");
        assert_eq!(read_payload(&dir, &good).unwrap(), b"good payload");
        let error = read_payload(&dir, &recovery.found[1].at).unwrap_err();
        assert!(error.contains("checksum"), "{error}");
    }

    #[test]
    fn a_corrupt_header_at_the_end_of_a_segment_loses_only_its_record() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        let record = record_len(&StreamId::new("s1").unwrap(), 10);
        let (mut store, _) =
            DiskStore::open(&dir, SEGMENT_HEADER_LEN + 2 * record, usize::MAX).unwrap();
        let refs: Vec<DiskRef> = (1..=4)
            .map(|seq| store.append(&meta("s1", seq), b"0123456789", true).unwrap())
            .collect();
        drop(store);
        // Corrupt the sequence number in the header of the second record of segment 0.
        let path = dir.join(segment_name(0, "log"));
        let mut bytes = fs::read(&path).unwrap();
        let header = usize::try_from(refs[1].offset - (record - 10)).unwrap();
        bytes[header + 8] ^= 0x01;
        fs::write(&path, &bytes).unwrap();
        let (_store, recovery) = DiskStore::open(&dir, 1024 * 1024, usize::MAX).unwrap();
        let found: Vec<u64> = recovery
            .found
            .iter()
            .map(|found| found.meta.seq.get())
            .collect();
        assert_eq!(found, [1, 3, 4]);
        assert_eq!(recovery.unreadable_bytes, record);
    }

    /// Independent review of Laysi/trustiot-v2#90 (2026-10-28), finding 5: one corrupt record
    /// header must not lose the rest of its segment. Start-up reads on at the next intact
    /// header, past a stray magic in the bytes between that does not verify.
    #[test]
    fn records_after_a_corrupt_header_are_found_again() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        let record = record_len(&StreamId::new("s1").unwrap(), 100);
        let (mut store, _) = DiskStore::open(&dir, 1024 * 1024, usize::MAX).unwrap();
        let refs: Vec<DiskRef> = (1..=50)
            .map(|seq| {
                let payload = [u8::try_from(seq).unwrap(); 100];
                store.append(&meta("s1", seq), &payload, true).unwrap()
            })
            .collect();
        drop(store);
        let path = dir.join(segment_name(0, "log"));
        let mut bytes = fs::read(&path).unwrap();
        // One flipped bit in the sequence number of record 2's header, and a record magic in
        // its payload that starts no valid header.
        let header = usize::try_from(refs[1].offset - (record - 100)).unwrap();
        bytes[header + 8] ^= 0x01;
        let payload = usize::try_from(refs[1].offset).unwrap();
        bytes[payload + 10..payload + 14].copy_from_slice(&RECORD_MAGIC.to_le_bytes());
        fs::write(&path, &bytes).unwrap();

        let (_store, recovery) = DiskStore::open(&dir, 1024 * 1024, usize::MAX).unwrap();
        let found: Vec<u64> = recovery
            .found
            .iter()
            .map(|found| found.meta.seq.get())
            .collect();
        let expected: Vec<u64> = std::iter::once(1).chain(3..=50).collect();
        assert_eq!(found, expected, "only record 2 is lost");
        assert_eq!(
            recovery.unreadable_bytes, record,
            "exactly record 2's bytes"
        );
        assert_eq!(recovery.torn_records, 0);
        for found in &recovery.found {
            assert_eq!(
                read_payload(&dir, &found.at).unwrap(),
                [u8::try_from(found.meta.seq.get()).unwrap(); 100]
            );
        }
        // Records of one stream share its ID.
        assert!(Arc::ptr_eq(
            &recovery.found[0].meta.stream,
            &recovery.found[48].meta.stream
        ));
    }

    /// Independent review, finding 3: start-up keeps at most its limit of records, the newest
    /// by formation time, releases the others (counted) and deletes segments left without a
    /// record; it never holds much more than the limit while reading.
    #[test]
    fn recovery_keeps_at_most_its_limit_of_the_newest_records() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        let record = record_len(&StreamId::new("s1").unwrap(), 10);
        // Ten records per segment, a thousand records.
        let (mut store, _) =
            DiskStore::open(&dir, SEGMENT_HEADER_LEN + 10 * record, usize::MAX).unwrap();
        for seq in 1..=1_000 {
            store
                .append(&meta("s1", seq), b"0123456789", false)
                .unwrap();
        }
        store.sync().unwrap();
        drop(store);
        assert_eq!(files(&dir).len(), 100);

        let (store, recovery) = DiskStore::open(&dir, 1024 * 1024, 100).unwrap();
        let mut found: Vec<u64> = recovery
            .found
            .iter()
            .map(|found| found.meta.seq.get())
            .collect();
        found.sort_unstable();
        assert_eq!(found, (901..=1_000).collect::<Vec<_>>(), "the newest 100");
        assert_eq!(recovery.over_limit, (900, 900 * 3));
        assert!(
            recovery.peak <= prune_at(100),
            "held {} records at once",
            recovery.peak
        );
        assert_eq!(
            files(&dir).len(),
            10,
            "segments of released records are gone"
        );
        assert_eq!(store.bytes(), 10 * (SEGMENT_HEADER_LEN + 10 * record));
    }

    /// Independent review, finding 4: a name in use when the directory is opened, whatever the
    /// entry is (a symbolic link, a directory, a temporary that is not a regular file), is never
    /// taken by a new segment, adopted or removed.
    #[test]
    fn names_in_use_at_start_are_never_taken() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        fs::create_dir(&dir).unwrap();
        let outside = scratch.write("outside", b"not ours");
        std::os::unix::fs::symlink(&outside, dir.join(segment_name(0, "log"))).unwrap();
        fs::create_dir(dir.join(segment_name(1, "log"))).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join(segment_name(2, "tmp"))).unwrap();

        let (mut store, recovery) = DiskStore::open(&dir, 1024 * 1024, usize::MAX).unwrap();
        assert_eq!(
            recovery.removed_temporary, 0,
            "a symbolic link is no leftover"
        );
        let mut ignored = recovery.ignored.clone();
        ignored.sort();
        assert_eq!(
            ignored,
            [
                segment_name(0, "log"),
                segment_name(1, "log"),
                segment_name(2, "tmp")
            ]
        );
        let at = store.append(&meta("s1", 1), b"payload", true).unwrap();
        assert_eq!(at.segment, 3);
        store.release(at.segment);
        for name in [segment_name(0, "log"), segment_name(2, "tmp")] {
            assert!(
                fs::symlink_metadata(dir.join(&name))
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{name}"
            );
        }
        assert!(dir.join(segment_name(1, "log")).is_dir());
        assert_eq!(fs::read(&outside).unwrap(), b"not ours");
        assert_eq!(
            files(&dir),
            [
                segment_name(0, "log"),
                segment_name(1, "log"),
                segment_name(2, "tmp")
            ],
            "only the agent's own segment came and went"
        );
    }

    /// Independent review, finding 4: an entry that appears after start under the name of the
    /// next segment, or of its temporary file, is neither replaced nor removed; that number is
    /// skipped and the record goes into a segment with the next one.
    #[test]
    fn a_name_that_appears_after_start_is_never_replaced_or_removed() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        let outside = scratch.write("outside", b"not ours");
        // One record per segment, so that every append creates a segment.
        let record = record_len(&StreamId::new("s1").unwrap(), b"payload".len());
        let (mut store, _) =
            DiskStore::open(&dir, SEGMENT_HEADER_LEN + record, usize::MAX).unwrap();
        let temporary = dir.join(segment_name(0, "tmp"));
        std::os::unix::fs::symlink(&outside, &temporary).unwrap();
        let first = store.append(&meta("s1", 1), b"payload", true).unwrap();
        assert_eq!(first.segment, 1, "number 0 is skipped");
        assert!(
            fs::symlink_metadata(&temporary)
                .unwrap()
                .file_type()
                .is_symlink(),
            "a temporary file this process did not create is not removed"
        );
        let segment = dir.join(segment_name(2, "log"));
        std::os::unix::fs::symlink(&outside, &segment).unwrap();
        let second = store.append(&meta("s1", 2), b"payload", true).unwrap();
        assert_eq!(second.segment, 3, "number 2 is skipped");
        assert!(
            fs::symlink_metadata(&segment)
                .unwrap()
                .file_type()
                .is_symlink(),
            "an existing entry is never replaced"
        );
        assert_eq!(
            files(&dir),
            [
                segment_name(0, "tmp"),
                segment_name(1, "log"),
                segment_name(2, "log"),
                segment_name(3, "log")
            ],
            "the temporary files this process created are gone, the others are left"
        );
        for at in [first, second] {
            assert_eq!(read_payload(&dir, &at).unwrap(), b"payload");
        }
        assert_eq!(fs::read(&outside).unwrap(), b"not ours");
    }

    /// Codex review (third round): a file that another process creates under the segment's
    /// final name after the number was chosen, right before the segment is published, is left
    /// as it is, and the segment goes to the next number. Publishing with a check and then
    /// `rename` replaced it; the hard link fails instead.
    #[test]
    fn a_name_taken_right_before_publishing_is_left_alone() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        let (mut store, _) = DiskStore::open(&dir, 1024 * 1024, usize::MAX).unwrap();
        store.taken_before_publishing = Some(0);
        let at = store.append(&meta("s1", 1), b"payload", true).unwrap();
        assert_eq!(at.segment, 1, "the taken number is skipped");
        let taken = dir.join(segment_name(0, "log"));
        assert_eq!(fs::read(&taken).unwrap(), b"not ours");
        assert_eq!(
            files(&dir),
            [segment_name(0, "log"), segment_name(1, "log")],
            "no temporary file is left"
        );
        assert_eq!(read_payload(&dir, &at).unwrap(), b"payload");
        drop(store);
        // The next process leaves the foreign file alone and finds the record.
        let (_store, recovery) = DiskStore::open(&dir, 1024 * 1024, usize::MAX).unwrap();
        assert_eq!(recovery.ignored, [segment_name(0, "log")]);
        assert_eq!(recovery.found.len(), 1);
        assert_eq!(fs::read(&taken).unwrap(), b"not ours");
    }

    /// Codex review (third round): a crash between publishing a segment (the hard link) and
    /// removing its temporary name leaves that name as a second name of the live segment. The
    /// next start removes the name only: the segment and its records stay, and the segment's
    /// number is not used again.
    #[test]
    fn a_temporary_name_left_by_a_crash_after_publishing_is_removed_alone() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        let (mut store, _) = DiskStore::open(&dir, 1024 * 1024, usize::MAX).unwrap();
        for seq in 1..=3 {
            store
                .append(&meta("s1", seq), &[u8::try_from(seq).unwrap(); 100], true)
                .unwrap();
        }
        drop(store);
        let segment = dir.join(segment_name(0, "log"));
        fs::hard_link(&segment, dir.join(segment_name(0, "tmp"))).unwrap();
        assert_eq!(fs::metadata(&segment).unwrap().nlink(), 2);

        let (mut store, recovery) = DiskStore::open(&dir, 1024 * 1024, usize::MAX).unwrap();
        assert_eq!(recovery.removed_temporary, 1);
        assert_eq!(files(&dir), [segment_name(0, "log")]);
        assert_eq!(fs::metadata(&segment).unwrap().nlink(), 1);
        let found: Vec<u64> = recovery
            .found
            .iter()
            .map(|found| found.meta.seq.get())
            .collect();
        assert_eq!(found, [1, 2, 3]);
        for found in &recovery.found {
            assert_eq!(
                read_payload(&dir, &found.at).unwrap(),
                [u8::try_from(found.meta.seq.get()).unwrap(); 100]
            );
        }
        let at = store.append(&meta("s2", 1), b"payload", true).unwrap();
        assert_eq!(at.segment, 1);
    }

    /// Independent review, finding 4: a file named with the highest segment number leaves no
    /// number for a new segment. Appending fails rather than reusing a number.
    #[test]
    fn the_highest_segment_number_is_never_used() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        fs::create_dir(&dir).unwrap();
        let foreign = dir.join(segment_name(u64::MAX, "log"));
        fs::write(&foreign, b"an operator's file").unwrap();
        let payload = [7_u8; 1000];
        let record = record_len(&StreamId::new("s1").unwrap(), payload.len());
        let (mut store, recovery) =
            DiskStore::open(&dir, SEGMENT_HEADER_LEN + record, usize::MAX).unwrap();
        assert_eq!(recovery.ignored, [segment_name(u64::MAX, "log")]);
        for seq in 1..=2 {
            let error = store.append(&meta("s1", seq), &payload, true).unwrap_err();
            assert!(error.to_string().contains("no segment number"), "{error}");
        }
        assert_eq!(fs::read(&foreign).unwrap(), b"an operator's file");
        assert_eq!(files(&dir), [segment_name(u64::MAX, "log")]);
    }

    /// Independent review, finding 10: an existing buffer directory that other users can enter
    /// is made private when the agent owns it.
    #[test]
    fn an_existing_directory_open_to_others_is_made_private() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, Permissions::from_mode(0o755)).unwrap();
        DiskStore::open(&dir, 1024 * 1024, usize::MAX).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o7777, 0o700);
    }

    #[test]
    fn unknown_files_are_left_alone_and_leftover_temporaries_removed() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        let (mut store, _) = DiskStore::open(&dir, 1024 * 1024, usize::MAX).unwrap();
        store.append(&meta("s1", 1), b"payload", true).unwrap();
        drop(store);
        let outside = scratch.write("outside.log", b"not ours");
        fs::write(dir.join("notes.txt"), b"an operator's notes").unwrap();
        fs::write(dir.join(segment_name(7, "tmp")), b"TIOTSEG").unwrap();
        fs::create_dir(dir.join(segment_name(8, "log"))).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join(segment_name(9, "log"))).unwrap();
        fs::write(dir.join(segment_name(10, "log")), b"something else").unwrap();

        let (mut store, recovery) = DiskStore::open(&dir, 1024 * 1024, usize::MAX).unwrap();
        assert_eq!(recovery.found.len(), 1);
        assert_eq!(recovery.removed_temporary, 1);
        let mut ignored = recovery.ignored.clone();
        ignored.sort();
        assert_eq!(
            ignored,
            [
                "notes.txt".to_owned(),
                segment_name(8, "log"),
                segment_name(9, "log"),
                segment_name(10, "log"),
            ]
        );
        assert_eq!(
            files(&dir),
            [
                "notes.txt".to_owned(),
                segment_name(0, "log"),
                segment_name(8, "log"),
                segment_name(9, "log"),
                segment_name(10, "log"),
            ],
            "only the temporary file is gone"
        );
        assert_eq!(fs::read(&outside).unwrap(), b"not ours");
        // New segments never reuse a number seen in the directory.
        let next = store.append(&meta("s2", 1), b"payload", true).unwrap();
        assert_eq!(next.segment, 11);
    }

    #[test]
    fn a_symbolic_link_or_a_file_is_not_a_buffer_directory() {
        let scratch = scratch();
        let target = scratch.path().join("elsewhere");
        fs::create_dir(&target).unwrap();
        let link = scratch.path().join("buffer");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error = DiskStore::open(&link, 1024, usize::MAX).unwrap_err();
        assert!(error.to_string().contains("is not a directory"), "{error}");
        let file = scratch.write("file", b"");
        assert!(DiskStore::open(&file, 1024, usize::MAX).is_err());
        // The parent must exist: the state directory is systemd's to create.
        assert!(DiskStore::open(&scratch.path().join("missing/buffer"), 1024, usize::MAX).is_err());
    }

    #[test]
    fn a_failed_write_closes_the_segment_and_the_next_needs_a_new_one() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        let (mut store, _) = DiskStore::open(&dir, 1024 * 1024, usize::MAX).unwrap();
        store.append(&meta("s1", 1), b"payload", true).unwrap();
        // The directory disappears: appending to the open segment still works (its file is
        // open), but a new segment cannot be created.
        store.seal_writer();
        fs::remove_dir_all(&dir).unwrap();
        assert!(store.append(&meta("s1", 2), b"payload", true).is_err());
        assert!(store.writer.is_none());
    }

    /// Independent review, second round, finding B: a directory whose records are all on
    /// different streams. Reading it keeps only the stream IDs of the records it keeps, so its
    /// memory stays within the limit whatever the directory holds.
    #[test]
    fn recovery_keeps_only_the_stream_ids_of_the_records_it_keeps() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        {
            let (mut store, _) = DiskStore::open(&dir, 1 << 20, usize::MAX).unwrap();
            for n in 1..=1_000_u64 {
                let mut meta = meta(&format!("{n:064}"), 1);
                meta.formed_at = n;
                store.append(&meta, &[1], false).unwrap();
            }
            store.sync().unwrap();
        }
        let (_store, recovery) = DiskStore::open(&dir, 1 << 20, 10).unwrap();
        let kept: Vec<u64> = recovery
            .found
            .iter()
            .map(|found| found.meta.formed_at)
            .collect();
        assert_eq!(kept.len(), 10);
        assert!(kept.iter().all(|formed_at| *formed_at > 990), "{kept:?}");
        assert!(
            recovery.streams_peak <= prune_at(10),
            "{} stream IDs held at once for a limit of 10 records",
            recovery.streams_peak
        );
    }

    /// Independent review, second round, info 2: one store per buffer directory. A second one,
    /// in this process or in another agent, is refused while the first holds the directory's
    /// lock, and opens once the first is gone.
    #[test]
    fn a_second_store_on_the_same_directory_is_refused_while_the_first_is_open() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        let (first, _) = DiskStore::open(&dir, 1 << 20, usize::MAX).unwrap();
        let error = DiskStore::open(&dir, 1 << 20, usize::MAX).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "{error}");
        assert!(error.to_string().contains("in use"), "{error}");
        let error = DiskStore::open_for_appends(&dir, 1 << 20, &mut || true).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "{error}");
        drop(first);
        DiskStore::open(&dir, 1 << 20, usize::MAX).unwrap();
    }

    /// Independent review, second round, info 1: a stop that comes while the directory is read
    /// ends the read at the next record: the open fails and nothing is deleted or changed.
    #[test]
    fn a_stop_ends_reading_the_directory_at_the_next_record() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        {
            let (mut store, _) = DiskStore::open(&dir, 1 << 20, usize::MAX).unwrap();
            for seq in 1..=20 {
                store.append(&meta("s1", seq), b"payload", true).unwrap();
            }
        }
        let path = dir.join(segment_name(0, "log"));
        let before = fs::read(&path).unwrap();
        let mut asked = 0;
        let error = DiskStore::open_until(&dir, 1 << 20, usize::MAX, &mut || {
            asked += 1;
            asked <= 5
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
        assert_eq!(asked, 6, "it ended at the first record after the stop");
        assert_eq!(fs::read(&path).unwrap(), before);
        let (_store, recovery) = DiskStore::open(&dir, 1 << 20, usize::MAX).unwrap();
        assert_eq!(recovery.found.len(), 20);
    }

    /// Independent review, second round, info 1: the stop's open lists the names and sizes in
    /// the directory and reads only the last segment, for its newest formation time; it
    /// removes nothing (a leftover temporary file included) and takes no number in use.
    #[test]
    fn the_stop_open_reads_and_removes_nothing() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        {
            let (mut store, _) = DiskStore::open(&dir, 1 << 20, usize::MAX).unwrap();
            for seq in 1..=3 {
                store.append(&meta("s1", seq), b"payload", true).unwrap();
            }
        }
        let segment = dir.join(segment_name(0, "log"));
        let before = fs::read(&segment).unwrap();
        let leftover = dir.join(segment_name(4, "tmp"));
        fs::write(&leftover, b"left by a crash").unwrap();
        let (mut store, newest) = DiskStore::open_for_appends(&dir, 1 << 20, &mut || true).unwrap();
        assert_eq!(newest, Some(meta("s1", 3).formed_at));
        assert_eq!(store.bytes(), before.len() as u64);
        let at = store.append(&meta("s2", 1), b"payload", true).unwrap();
        assert_eq!(at.segment, 5);
        assert!(leftover.exists());
        assert_eq!(fs::read(&segment).unwrap(), before);
    }

    /// Independent review, second round, info 3: a leftover temporary file that cannot be
    /// removed (here the directory is read-only) does not keep the directory from opening: it is
    /// left, with a warning, and its number stays taken.
    #[test]
    fn a_leftover_temporary_file_that_cannot_be_removed_is_left_and_its_number_skipped() {
        /// Makes the directory writable again, also when the test fails.
        struct Writable(PathBuf);
        impl Drop for Writable {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, Permissions::from_mode(0o700));
            }
        }
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        {
            let (mut store, _) = DiskStore::open(&dir, 1 << 20, usize::MAX).unwrap();
            store.append(&meta("s1", 1), b"payload", true).unwrap();
        }
        let leftover = dir.join(segment_name(5, "tmp"));
        fs::write(&leftover, b"left by a crash").unwrap();
        fs::set_permissions(&dir, Permissions::from_mode(0o500)).unwrap();
        let _writable = Writable(dir.clone());
        if fs::write(dir.join("probe"), b"").is_ok() {
            eprintln!("skipped: this user can write to a read-only directory (root)");
            return;
        }
        let (store, recovery) = DiskStore::open(&dir, 1 << 20, usize::MAX).unwrap();
        assert_eq!(recovery.removed_temporary, 0);
        assert!(leftover.exists());
        assert_eq!(recovery.found.len(), 1);
        assert_eq!(store.next_segment, 6, "the leftover's number stays taken");
    }

    /// Independent review, second round, info 4: the segment header's reserved bytes (12..16)
    /// are zero in every segment this format writes. A file with anything else there is not
    /// ours: left alone, never read or deleted.
    #[test]
    fn a_segment_whose_reserved_header_bytes_are_not_zero_is_left_alone() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        {
            let (mut store, _) = DiskStore::open(&dir, 1 << 20, usize::MAX).unwrap();
            for seq in 1..=2 {
                store.append(&meta("s1", seq), b"payload", true).unwrap();
            }
        }
        let path = dir.join(segment_name(0, "log"));
        let mut bytes = fs::read(&path).unwrap();
        bytes[12] = 1;
        fs::write(&path, &bytes).unwrap();
        let (mut store, recovery) = DiskStore::open(&dir, 1 << 20, usize::MAX).unwrap();
        assert!(
            recovery.found.is_empty(),
            "{} records read",
            recovery.found.len()
        );
        assert_eq!(recovery.ignored, [segment_name(0, "log")]);
        let at = store.append(&meta("s1", 3), b"payload", true).unwrap();
        assert_eq!(at.segment, 1);
        store.release(at.segment);
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    /// Codex review (fifth round): the stop's open counts the formation time of a last record
    /// whose header is intact but whose payload was cut short.
    #[test]
    fn the_stop_open_counts_a_torn_last_record_for_the_newest_time() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        {
            let (mut store, _) = DiskStore::open(&dir, 1 << 20, usize::MAX).unwrap();
            for seq in 1..=3 {
                store.append(&meta("s1", seq), b"payload", true).unwrap();
            }
        }
        let segment = dir.join(segment_name(0, "log"));
        let len = fs::metadata(&segment).unwrap().len();
        OpenOptions::new()
            .write(true)
            .open(&segment)
            .unwrap()
            .set_len(len - 3)
            .unwrap();
        let (store, newest) = DiskStore::open_for_appends(&dir, 1 << 20, &mut || true).unwrap();
        assert_eq!(newest, Some(meta("s1", 3).formed_at));
        drop(store);
        // And the full open: two records kept, the torn one counted, its time in `newest`.
        let (_store, recovery) = DiskStore::open(&dir, 1 << 20, usize::MAX).unwrap();
        assert_eq!(recovery.found.len(), 2);
        assert_eq!(recovery.torn_records, 3);
        assert_eq!(recovery.newest, Some(meta("s1", 3).formed_at));
    }

    /// Codex review (sixth round): the clock file keeps the age clock's time for the next
    /// start, through both opens; a damaged one, or a symbolic link, gives no time and is
    /// replaced at the next save; neither it nor a leftover new one is listed as ignored.
    #[test]
    fn the_clock_file_keeps_the_age_clocks_time_and_a_damaged_one_is_ignored() {
        let scratch = scratch();
        let dir = scratch.path().join("buffer");
        let path = dir.join(CLOCK_FILE);
        let (store, recovery) = DiskStore::open(&dir, 1 << 20, usize::MAX).unwrap();
        assert_eq!(recovery.clock, None);
        store.save_clock(1_234).unwrap();
        store.save_clock(5_678).unwrap();
        drop(store);
        assert_eq!(files(&dir), [CLOCK_FILE.to_owned()]);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::read(&path).unwrap(), encode_clock(5_678));
        fs::write(dir.join(CLOCK_TEMPORARY), b"left by a crash").unwrap();
        let (store, recovery) = DiskStore::open(&dir, 1 << 20, usize::MAX).unwrap();
        assert_eq!(recovery.clock, Some(5_678));
        assert!(recovery.ignored.is_empty(), "{:?}", recovery.ignored);
        drop(store);
        // The stop's open takes it too, without any segment.
        let (store, floor) = DiskStore::open_for_appends(&dir, 1 << 20, &mut || true).unwrap();
        assert_eq!(floor, Some(5_678));
        drop(store);
        // A flipped bit in the time: no time.
        let mut bytes = fs::read(&path).unwrap();
        bytes[16] ^= 0x01;
        fs::write(&path, &bytes).unwrap();
        let (store, recovery) = DiskStore::open(&dir, 1 << 20, usize::MAX).unwrap();
        assert_eq!(recovery.clock, None);
        // The next save replaces it, and the leftover new one.
        store.save_clock(9_999).unwrap();
        drop(store);
        assert_eq!(files(&dir), [CLOCK_FILE.to_owned()]);
        assert_eq!(read_clock(&dir), Some(9_999));
        // A symbolic link to a valid clock file elsewhere is not followed; the save replaces
        // the link, not its target.
        let outside = scratch.write("elsewhere", encode_clock(u64::MAX));
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        let (store, recovery) = DiskStore::open(&dir, 1 << 20, usize::MAX).unwrap();
        assert_eq!(recovery.clock, None);
        store.save_clock(10_000).unwrap();
        assert_eq!(read_clock(&dir), Some(10_000));
        assert_eq!(fs::read(&outside).unwrap(), encode_clock(u64::MAX));
    }
}
