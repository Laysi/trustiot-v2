//! The offline buffer (rebuild checklist G1, decision 9): encoded telemetry batches waiting for
//! the cloud's acknowledgement, in memory while they fit and on the data partition otherwise.
//!
//! One FIFO holds every batch in the order it was formed, wherever it lives: the uploader takes
//! the oldest one ([`Buffer::front`]). A new batch stays in memory while the batches in memory
//! total at most the memory limit; one that does not fit is written to disk (`disk`, fsynced).
//! When neither has room, the oldest batches are dropped until the new one fits: the oldest of
//! all while the disk takes batches, the oldest in memory while it does not (its writes failed,
//! or no new batch is written there with a disk limit of 0). A batch that cannot fit at all is
//! dropped itself. A failed disk is tried again after [`DISK_RETRY`]. Batches older than
//! [`protocol::telemetry::BUFFER_MAX_AGE`] on the age clock are dropped ([`Buffer::expire`]).
//! At most [`MAX_BATCHES`] batches are kept, in memory and on disk together; beyond that the
//! oldest are dropped, once the new batch has its place (one that fits nowhere drops no other). Every dropped batch's readings are counted in `dropped_records`.
//!
//! Memory: the payloads in memory (at most the memory limit) and one index entry per buffered
//! batch, wherever its payload is (about 130 bytes each, measured; see the README), so at most the
//! memory limit plus [`MAX_BATCHES`] entries, whatever the disk limit and the upload interval;
//! reading the disk at start holds at most one and a half times [`MAX_BATCHES`] records' headers
//! at once.
//!
//! Opening the disk raises the age clock's floor to the newest batch found there or the age
//! clock's time saved there, whichever is later (so that those batches never look younger than
//! when they were stored), and the batches formed before the open move forward by as much as
//! that moved the clock, so that a late open ages none of them. While batches are on disk,
//! housekeeping saves the age clock's time there every [`CLOCK_SAVE_EVERY`], and a stop saves
//! it too: a forward step of the wall clock that the age clock counted stays counted after a
//! restart, even when the wall clock was corrected back before it. Once the agent is stopping
//! no full read of the disk starts, and one under way ends; the stop opens a disk that had not
//! opened for appending only ([`Buffer::flush`]).
//!
//! The disk limit decides whether a new batch may be written to disk, never whether a batch
//! found there at start is loaded: an earlier run may have had a larger limit. Every batch found
//! is kept and sent (up to [`MAX_BATCHES`]). While the disk holds more than the limit, no new
//! batch is written there; once memory is full and a new batch needs the disk, the oldest
//! batches are dropped (counted, `disk_full`) until the disk is under the limit, as for any full
//! disk. With a limit of 0 no batch is ever written, so batches found are removed only as they
//! are acknowledged or expire.
//!
//! The buffer does synchronous file I/O and reports what it did as [`Event`]s, which the caller
//! logs: the upload pipeline runs it on tokio's blocking threads.

mod disk;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use protocol::health::BufferStats;
use protocol::telemetry::BUFFER_MAX_AGE;
use protocol::{Seq, StreamId, UnixMillis};

pub(crate) use self::disk::{DiskRef, Meta, read_payload};
use self::disk::{DiskStore, SEGMENT_HEADER_LEN, record_len};
use crate::clock::AgeClock;

/// How long the buffer waits before using the disk again after a failure (a full, read-only or
/// failing data partition).
pub const DISK_RETRY: Duration = Duration::from_secs(60);
/// How often housekeeping saves the age clock's time in the buffer directory while batches
/// are on disk ([`Buffer::tick`]); a stop saves it too. A crash loses what the age clock
/// counted after the last save (normally less than this much real time, but also a forward
/// step of the wall clock taken and reverted meanwhile), and only when the wall clock after
/// the restart is behind the age clock.
pub const CLOCK_SAVE_EVERY: Duration = Duration::from_secs(60);
/// The largest segment file. Smaller with a small disk limit: a segment is deleted only once
/// all of its batches are released, so the limit must hold several of them.
pub const MAX_SEGMENT_BYTES: u64 = 4 * 1024 * 1024;
/// The smallest segment size target; a larger batch gets a segment of its own.
const MIN_SEGMENT_BYTES: u64 = 16 * 1024;
/// The most batches the buffer keeps, in memory and on disk together; beyond it the oldest are
/// dropped (`too_many_batches`). It bounds the memory of the index of buffered batches whatever
/// the disk limit and the upload interval, and the memory reading the disk at start needs. A
/// small site in a 30-day outage forms about 8,640 batches (one per five-minute window, see
/// [`crate::upload`]); with full 256 KiB batches it is reached at 16 GiB on disk.
pub const MAX_BATCHES: usize = 65_536;

/// Why batches were dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DropReason {
    /// Memory was full and the disk took no batches (it failed or is switched off).
    MemoryFull,
    /// Memory and the disk limit were both full.
    DiskFull,
    /// More batches than [`MAX_BATCHES`].
    TooManyBatches,
    /// Older than the longest buffering time.
    Expired,
    /// A record on disk was torn or corrupt.
    Corrupt,
    /// Readings, before forming batches: the collection window was full.
    WindowFull,
}

impl std::fmt::Display for DropReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::MemoryFull => "memory_full",
            Self::DiskFull => "disk_full",
            Self::TooManyBatches => "too_many_batches",
            Self::Expired => "expired",
            Self::Corrupt => "corrupt",
            Self::WindowFull => "window_full",
        })
    }
}

/// What the buffer did, for the caller to log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// Batches dropped and their readings counted in `dropped_records`.
    Dropped {
        reason: DropReason,
        batches: u64,
        records: u64,
    },
    /// Writing to the disk failed; it is used again after [`DISK_RETRY`].
    DiskFailed(String),
    /// The buffer directory could not be opened; it is tried again after [`DISK_RETRY`].
    DiskOpenFailed(String),
    /// A stop opened the buffer directory, which had not opened before, for appending only:
    /// the segments already there (`bytes`) are left for the next start.
    DiskOpenedForStop { bytes: u64 },
    /// The age clock's time could not be saved in the buffer directory; it is tried again
    /// after [`CLOCK_SAVE_EVERY`].
    ClockNotSaved(String),
    /// The buffer directory was opened and its batches loaded.
    DiskOpened {
        batches: u64,
        records: u64,
        bytes: u64,
        removed_temporary: usize,
        ignored: Vec<String>,
        unreadable_bytes: u64,
    },
    /// Shutting down, these batches could not be made durable on disk and are lost: they did
    /// not fit, writing or syncing them failed, or the stop's time ran out (`error` says which).
    NotFlushed {
        batches: u64,
        records: u64,
        error: Option<String>,
    },
}

/// A batch the collector formed.
#[derive(Debug)]
pub(crate) struct Formed {
    pub(crate) meta: Meta,
    pub(crate) payload: Vec<u8>,
}

#[derive(Clone, Debug)]
enum Place {
    Memory(Bytes),
    Disk(DiskRef),
}

/// One buffered batch: the index entry kept for every batch, wherever its payload is.
#[derive(Clone, Debug)]
struct Entry {
    id: u64,
    meta: Meta,
    /// The payload's length (at most the telemetry channel's limit).
    len: u32,
    place: Place,
}

/// Where the payload of the batch to send is.
#[derive(Clone, Debug)]
pub(crate) enum Source {
    Memory(Bytes),
    Disk { dir: PathBuf, at: DiskRef },
}

/// The oldest batch, to send.
#[derive(Clone, Debug)]
pub(crate) struct Outgoing {
    pub(crate) id: u64,
    pub(crate) stream: StreamId,
    pub(crate) seq: Seq,
    /// Readings in the batch.
    pub(crate) records: u32,
    /// When it was formed, on the age clock.
    pub(crate) formed_at: u64,
    pub(crate) source: Source,
}

/// Whether a batch formed at `formed_at` is older than the longest buffering time at `now`
/// (both on the age clock): it is never sent again, and [`Buffer::expire`] drops it.
pub(crate) fn expired(formed_at: u64, now: u64) -> bool {
    now.saturating_sub(formed_at) > millis(BUFFER_MAX_AGE)
}

/// Collects events, merging consecutive drops for the same reason.
#[derive(Debug, Default)]
pub(crate) struct Events(pub(crate) Vec<Event>);

impl Events {
    fn dropped(&mut self, reason: DropReason, records: u32) {
        if let Some(Event::Dropped {
            reason: last,
            batches,
            records: total,
        }) = self.0.last_mut()
            && *last == reason
        {
            *batches += 1;
            *total += u64::from(records);
            return;
        }
        self.0.push(Event::Dropped {
            reason,
            batches: 1,
            records: u64::from(records),
        });
    }

    fn push(&mut self, event: Event) {
        self.0.push(event);
    }
}

/// The buffer's state. Not shared: the upload pipeline owns it.
#[derive(Debug)]
pub(crate) struct Buffer {
    entries: VecDeque<Entry>,
    memory_limit: usize,
    disk_limit: u64,
    max_batches: usize,
    memory_used: usize,
    dir: PathBuf,
    disk: Option<DiskStore>,
    /// While set, the disk takes no batches (and, while it is not open, is not opened) until
    /// the age clock reaches it.
    disk_down_until: Option<u64>,
    /// With a disk limit of 0 the directory did not exist: nothing to recover, nothing to open.
    nothing_to_recover: bool,
    next_id: u64,
    pending_records: u64,
    dropped_records: u64,
    /// The batches' oldest sample times, as a multiset.
    oldest: BTreeMap<u64, u32>,
    segment_target: u64,
    /// The age clock whose floor an opened disk raises (see [`Buffer::open_disk`]).
    age: Option<AgeClock>,
    /// Set once the agent is stopping: no full read of the disk starts, and one under way ends.
    stopping: Option<Arc<AtomicBool>>,
    /// When the age clock's time was last saved (or its save tried), on the age clock.
    clock_saved: Option<u64>,
    /// Tests only: the most records reading the disk held at once.
    #[cfg(test)]
    recovery_peak: usize,
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl Entry {
    fn in_memory(&self) -> bool {
        matches!(self.place, Place::Memory(_))
    }

    /// The memory its payload takes.
    fn memory(&self) -> usize {
        match self.place {
            Place::Memory(_) => self.len as usize,
            Place::Disk(_) => 0,
        }
    }
}

impl Buffer {
    /// An empty buffer that keeps its files in `dir` once [`Buffer::open_disk`] opens it, and at
    /// most [`MAX_BATCHES`] batches. With a disk limit of 0 no batch is written to disk, but
    /// batches an earlier process left there are still loaded and sent.
    pub(crate) fn new(dir: PathBuf, memory_limit: usize, disk_limit: u64) -> Self {
        Self {
            entries: VecDeque::new(),
            memory_limit,
            disk_limit,
            max_batches: MAX_BATCHES,
            memory_used: 0,
            dir,
            disk: None,
            disk_down_until: None,
            nothing_to_recover: false,
            next_id: 0,
            pending_records: 0,
            dropped_records: 0,
            oldest: BTreeMap::new(),
            segment_target: (disk_limit / 16).clamp(MIN_SEGMENT_BYTES, MAX_SEGMENT_BYTES),
            age: None,
            stopping: None,
            clock_saved: None,
            #[cfg(test)]
            recovery_peak: 0,
        }
    }

    /// The same buffer keeping at most `max` batches (at least 1).
    pub(crate) fn with_batch_limit(mut self, max: usize) -> Self {
        self.max_batches = max.max(1);
        self
    }

    /// The same buffer measuring age on `age`: opening the disk raises its floor to the newest
    /// batch found there, and moves the formation time of the batches buffered before by as
    /// much as that moved the clock, so that their age stays what it was.
    pub(crate) fn with_age_clock(mut self, age: AgeClock) -> Self {
        self.age = Some(age);
        self
    }

    /// The same buffer, stopping once `stopping` is set: no full read of the disk starts after
    /// that, and one under way ends at the next record.
    pub(crate) fn with_stop(mut self, stopping: Arc<AtomicBool>) -> Self {
        self.stopping = Some(stopping);
        self
    }

    fn is_stopping(&self) -> bool {
        self.stopping
            .as_ref()
            .is_some_and(|stopping| stopping.load(Ordering::Relaxed))
    }

    /// Raises the age clock's floor to `newest` (the newest formation time on disk) and adds
    /// how far that moved the clock to the formation time of `entries`, batches this process
    /// formed before, so that the raise does not age them.
    fn raise_floor(&self, newest: Option<u64>, entries: &mut VecDeque<Entry>) {
        let (Some(age), Some(newest)) = (&self.age, newest) else {
            return;
        };
        let moved = age.raise_floor(newest);
        if moved > 0 {
            for entry in entries {
                entry.meta.formed_at = entry.meta.formed_at.saturating_add(moved);
            }
        }
    }

    /// Opens the buffer directory and puts the batches found there before every batch already
    /// buffered (they come from an earlier process, so they are older): grouped by stream, the
    /// streams in the order of their oldest formation time, each stream in ascending sequence
    /// numbers. A batch found twice is kept once. Returns the floor of the age clock: the later
    /// of the newest formation time in any record header there that verified (kept or not:
    /// [`disk::Recovery::newest`]) and the time in the clock file ([`disk::Recovery::clock`]);
    /// with an age clock ([`Buffer::with_age_clock`]) the floor is raised here, and the batches
    /// buffered before keep their age. Once the agent is stopping nothing is read: the open
    /// fails and is tried again as usual.
    ///
    /// Every batch found is loaded, whatever the disk limit: they were written under the limit
    /// of an earlier run, which may have been larger. With a limit of 0 the directory is opened
    /// only to send them, and not created when it does not exist. At most [`MAX_BATCHES`] are
    /// kept in all: the oldest go first (counted, `too_many_batches`).
    pub(crate) fn open_disk(&mut self, now: u64, events: &mut Events) -> Option<u64> {
        if self.disk.is_some() || self.nothing_to_recover {
            return None;
        }
        // With a limit of 0, a directory that is missing now stays empty of batches for the whole
        // run, so it is not looked at again: only the agent writes in its state directory (the
        // unit's sandbox), this run never creates the directory, and a run with a limit above 0
        // creates it when it opens the disk (`disk::prepare_dir`), before it writes any batch. So
        // no batch of an earlier run can be missing here now and turn up later.
        if self.disk_limit == 0
            && let Err(error) = std::fs::symlink_metadata(&self.dir)
            && error.kind() == std::io::ErrorKind::NotFound
        {
            self.nothing_to_recover = true;
            return None;
        }
        let stopping = self.stopping.clone();
        let mut more = move || {
            stopping
                .as_ref()
                .is_none_or(|stopping| !stopping.load(Ordering::Relaxed))
        };
        let (mut store, recovery) = match DiskStore::open_until(
            &self.dir,
            self.segment_target,
            self.max_batches,
            &mut more,
        ) {
            Ok(opened) => opened,
            Err(error) => {
                events.push(Event::DiskOpenFailed(format!(
                    "{}: {error}",
                    self.dir.display()
                )));
                self.disk_down_until = Some(now.saturating_add(millis(DISK_RETRY)));
                return None;
            }
        };
        #[cfg(test)]
        {
            self.recovery_peak = recovery.peak;
        }
        self.disk_down_until = None;
        if recovery.torn_records > 0 {
            self.dropped_records += recovery.torn_records;
            events.push(Event::Dropped {
                reason: DropReason::Corrupt,
                batches: 1,
                records: recovery.torn_records,
            });
        }
        let (over_batches, over_records) = recovery.over_limit;
        if over_batches > 0 {
            self.dropped_records += over_records;
            events.push(Event::Dropped {
                reason: DropReason::TooManyBatches,
                batches: over_batches,
                records: over_records,
            });
        }
        let disk::Recovery {
            mut found,
            removed_temporary,
            ignored,
            unreadable_bytes,
            newest,
            clock,
            ..
        } = recovery;
        let floor = newest.max(clock);
        // Each stream (its records share one ID) by its oldest formation time, then by where it
        // first appears; within a stream by sequence number; a batch found twice, first copy
        // first.
        let mut streams: HashMap<*const StreamId, (u64, (u64, u64))> = HashMap::new();
        for found in &found {
            let at = (found.at.segment, found.at.offset);
            streams
                .entry(Arc::as_ptr(&found.meta.stream))
                .and_modify(|(oldest, first)| {
                    *oldest = (*oldest).min(found.meta.formed_at);
                    *first = (*first).min(at);
                })
                .or_insert((found.meta.formed_at, at));
        }
        found.sort_unstable_by_key(|found| {
            (
                streams.get(&Arc::as_ptr(&found.meta.stream)).copied(),
                found.meta.seq,
                found.at.segment,
                found.at.offset,
            )
        });
        let mut newer = std::mem::take(&mut self.entries);
        self.entries.reserve(found.len() + newer.len());
        let mut batches = 0;
        let mut records = 0;
        let mut previous = None;
        for found in found {
            let key = (Arc::as_ptr(&found.meta.stream), found.meta.seq);
            if previous == Some(key) {
                store.release(found.at.segment);
                continue;
            }
            previous = Some(key);
            batches += 1;
            records += u64::from(found.meta.records);
            let entry = Entry {
                id: self.next_id,
                len: found.at.len,
                meta: found.meta,
                place: Place::Disk(found.at),
            };
            self.next_id += 1;
            self.count_in(&entry);
            self.entries.push_back(entry);
        }
        self.raise_floor(floor, &mut newer);
        self.entries.extend(newer);
        events.push(Event::DiskOpened {
            batches,
            records,
            bytes: store.bytes(),
            removed_temporary,
            ignored,
            unreadable_bytes,
        });
        self.disk = Some(store);
        // Batches formed before the disk opened count too.
        while self.entries.len() > self.max_batches {
            self.drop_at(0, DropReason::TooManyBatches, events);
        }
        floor
    }

    fn disk_usable(&self, now: u64) -> bool {
        self.disk.is_some() && self.disk_down_until.is_none_or(|until| now >= until)
    }

    fn count_in(&mut self, entry: &Entry) {
        self.pending_records += u64::from(entry.meta.records);
        *self.oldest.entry(entry.meta.oldest.get()).or_default() += 1;
        self.memory_used += entry.memory();
    }

    /// Undoes [`Buffer::count_in`] and releases the entry's record on disk.
    fn count_out(&mut self, entry: &Entry) {
        self.uncount(entry);
        if let Place::Disk(at) = entry.place
            && let Some(disk) = &mut self.disk
        {
            disk.release(at.segment);
        }
    }

    /// Undoes [`Buffer::count_in`] only.
    fn uncount(&mut self, entry: &Entry) {
        self.pending_records = self
            .pending_records
            .saturating_sub(u64::from(entry.meta.records));
        let key = entry.meta.oldest.get();
        if let Some(count) = self.oldest.get_mut(&key) {
            *count -= 1;
            if *count == 0 {
                self.oldest.remove(&key);
            }
        }
        self.memory_used = self.memory_used.saturating_sub(entry.memory());
    }

    /// Appends a new batch whose payload has its place, after dropping the oldest batches while
    /// [`Buffer::with_batch_limit`]'s number is reached. Only a batch that is stored makes room
    /// this way: one that fits nowhere drops no other.
    fn insert(&mut self, meta: Meta, len: usize, place: Place, events: &mut Events) {
        while self.entries.len() >= self.max_batches {
            self.drop_at(0, DropReason::TooManyBatches, events);
        }
        let entry = Entry {
            id: self.next_id,
            meta,
            len: u32::try_from(len).unwrap_or(u32::MAX),
            place,
        };
        self.next_id += 1;
        self.count_in(&entry);
        self.entries.push_back(entry);
    }

    fn remove_at(&mut self, index: usize) -> Option<Entry> {
        let entry = self.entries.remove(index)?;
        self.count_out(&entry);
        Some(entry)
    }

    fn drop_at(&mut self, index: usize, reason: DropReason, events: &mut Events) {
        if let Some(entry) = self.remove_at(index) {
            self.dropped_records += u64::from(entry.meta.records);
            events.dropped(reason, entry.meta.records);
        }
    }

    /// Adds a batch the collector formed (see the module documentation for where it goes).
    pub(crate) fn push(&mut self, formed: Formed, now: u64, events: &mut Events) {
        let Formed { meta, payload } = formed;
        let len = payload.len();
        let record = record_len(&meta.stream, len);
        let fits_memory = len <= self.memory_limit;
        let mut disk_takes =
            self.disk_usable(now) && SEGMENT_HEADER_LEN + record <= self.disk_limit;
        loop {
            if fits_memory && self.memory_used + len <= self.memory_limit {
                self.insert(meta, len, Place::Memory(Bytes::from(payload)), events);
                return;
            }
            if disk_takes
                && let Some(disk) = &mut self.disk
                && disk.bytes() + disk.space_for(record) <= self.disk_limit
            {
                match disk.append(&meta, &payload, true) {
                    Ok(at) => {
                        self.insert(meta, len, Place::Disk(at), events);
                        return;
                    }
                    Err(error) => {
                        events.push(Event::DiskFailed(format!(
                            "{}: {error}",
                            disk.dir().display()
                        )));
                        self.disk_down_until = Some(now.saturating_add(millis(DISK_RETRY)));
                        disk_takes = false;
                        continue;
                    }
                }
            }
            // Make room: the oldest batch of all while the disk takes batches, otherwise the
            // oldest in memory (dropping one on disk would not help).
            let (victim, reason) = if disk_takes {
                (
                    (!self.entries.is_empty()).then_some(0),
                    DropReason::DiskFull,
                )
            } else {
                (
                    self.entries.iter().position(Entry::in_memory),
                    DropReason::MemoryFull,
                )
            };
            let can_fit = fits_memory || disk_takes;
            match victim {
                Some(index) if can_fit => self.drop_at(index, reason, events),
                _ => {
                    // Nothing left to drop, or the batch fits nowhere: drop the new batch.
                    self.dropped_records += u64::from(meta.records);
                    events.dropped(reason, meta.records);
                    return;
                }
            }
        }
    }

    /// The oldest batch.
    pub(crate) fn front(&self) -> Option<Outgoing> {
        let entry = self.entries.front()?;
        Some(Outgoing {
            id: entry.id,
            stream: StreamId::clone(&entry.meta.stream),
            seq: entry.meta.seq,
            records: entry.meta.records,
            formed_at: entry.meta.formed_at,
            source: self.source_of(entry),
        })
    }

    /// Where the payload of batch `id` is, if it is still buffered.
    pub(crate) fn source(&self, id: u64) -> Option<Source> {
        let entry = self.entries.iter().find(|entry| entry.id == id)?;
        Some(self.source_of(entry))
    }

    fn source_of(&self, entry: &Entry) -> Source {
        match &entry.place {
            Place::Memory(payload) => Source::Memory(payload.clone()),
            Place::Disk(at) => Source::Disk {
                dir: self.dir.clone(),
                at: *at,
            },
        }
    }

    /// The batch in flight is almost always the oldest, found at once.
    fn position(&self, id: u64) -> Option<usize> {
        self.entries.iter().position(|entry| entry.id == id)
    }

    /// Removes the batch `id` once the cloud acknowledged it; `false` when it is no longer in
    /// the buffer (dropped meanwhile).
    pub(crate) fn release(&mut self, id: u64) -> bool {
        match self.position(id) {
            Some(index) => self.remove_at(index).is_some(),
            None => false,
        }
    }

    /// Drops the batch `id` because its record on disk is unreadable.
    pub(crate) fn discard(&mut self, id: u64, events: &mut Events) {
        if let Some(index) = self.position(id) {
            self.drop_at(index, DropReason::Corrupt, events);
        }
    }

    /// Drops every batch older than the longest buffering time at `now` on the age clock.
    pub(crate) fn expire(&mut self, now: u64, events: &mut Events) {
        let mut gone = Vec::new();
        self.entries.retain(|entry| {
            let keep = !expired(entry.meta.formed_at, now);
            if !keep {
                gone.push(entry.clone());
            }
            keep
        });
        for entry in gone {
            self.count_out(&entry);
            self.dropped_records += u64::from(entry.meta.records);
            events.dropped(DropReason::Expired, entry.meta.records);
        }
    }

    /// Housekeeping on every upload interval: opens the disk when it is due (returning the age
    /// clock's new floor) unless the agent is stopping, deletes released segments whose
    /// deletion failed before, and saves the age clock's time `now` every
    /// [`CLOCK_SAVE_EVERY`] while batches are on disk ([`Buffer::save_clock`]).
    pub(crate) fn tick(&mut self, now: u64, events: &mut Events) -> Option<u64> {
        if self.disk.is_none() {
            if !self.nothing_to_recover
                && !self.is_stopping()
                && self.disk_down_until.is_none_or(|until| now >= until)
            {
                return self.open_disk(now, events);
            }
        } else if let Some(disk) = &mut self.disk {
            disk.sweep_all();
        }
        if self
            .clock_saved
            .is_none_or(|saved| now >= saved.saturating_add(millis(CLOCK_SAVE_EVERY)))
        {
            self.save_clock(now, events);
        }
        None
    }

    /// Saves the age clock's time in the buffer directory while segments are there
    /// ([`DiskStore::save_clock`]), so that the next start's age clock starts no lower: the
    /// batches on disk then never look younger after a restart than they did before it by
    /// more than what the age clock counted after the save, whatever the wall clock says at
    /// the restart (for example after a forward step that was corrected back while they
    /// waited). The time saved is read from the age clock right before writing, never the
    /// caller's `now` alone, which was taken earlier: a reading in between (an upload's) may
    /// have seen a forward step. Without an age clock it is `now`.
    fn save_clock(&mut self, now: u64, events: &mut Events) {
        let Some(disk) = &self.disk else {
            return;
        };
        if disk.bytes() == 0 {
            return;
        }
        let at = self.age.as_ref().map_or(now, |age| age.now().max(now));
        self.clock_saved = Some(at);
        if let Err(error) = disk.save_clock(at) {
            events.push(Event::ClockNotSaved(error.to_string()));
        }
    }

    /// Writes every batch in memory to disk, oldest first, for a graceful stop, while `more`
    /// says there is time for another one (the caller's deadline): one pass, stopping between
    /// records. A disk that failed to open earlier is tried once more first, for appending
    /// only ([`DiskStore::open_for_appends`]): the stop never reads what is already there,
    /// which can take long; the next start does. Room is made as for a new batch, so when the
    /// disk limit is reached the oldest batches are dropped.
    /// Records are appended without their own fsync; each segment the flush fills is synced
    /// before the next is started, and the last one at the end, so the final sync has at most
    /// one segment to write. Then, while there is time, the age clock's time is saved
    /// ([`Buffer::save_clock`]).
    ///
    /// What is not durable afterwards is reported as [`Event::NotFlushed`]: the batches that
    /// did not fit, whose write failed or that the deadline left in memory, and, when a sync
    /// fails, every batch written since the last sync that succeeded. Those written but not
    /// durable leave the buffer as lost; their records stay in the segment files without being
    /// released, so a later process still sends any that did reach the disk intact.
    pub(crate) fn flush(&mut self, more: &mut dyn FnMut() -> bool, events: &mut Events) {
        if self.disk.is_none() && self.disk_limit > 0 && self.entries.iter().any(Entry::in_memory) {
            // Memory is lost otherwise: whatever the retry time says, try the disk once more.
            self.open_for_stop(more, events);
        }
        // Written since the last sync that succeeded.
        let mut unsynced: Vec<u64> = Vec::new();
        let mut not_durable: HashSet<u64> = HashSet::new();
        let mut lost = (0_u64, 0_u64);
        let mut error: Option<String> = None;
        let mut out_of_time = false;
        let mut index = 0;
        while index < self.entries.len() {
            let entry = &self.entries[index];
            if !entry.in_memory() {
                index += 1;
                continue;
            }
            let record = record_len(&entry.meta.stream, entry.len as usize);
            let fits = self.disk.is_some() && SEGMENT_HEADER_LEN + record <= self.disk_limit;
            if fits && error.is_none() && !out_of_time && !more() {
                out_of_time = true;
            }
            if !fits || error.is_some() || out_of_time {
                if self.disk.is_none() && error.is_none() {
                    error = Some(format!("{}: not open", self.dir.display()));
                }
                lost.0 += 1;
                lost.1 += u64::from(entry.meta.records);
                index += 1;
                continue;
            }
            // Make room as for a new batch: the oldest of all first, this one last.
            let mut itself = false;
            while self
                .disk
                .as_ref()
                .is_some_and(|disk| disk.bytes() + disk.space_for(record) > self.disk_limit)
            {
                itself = index == 0;
                self.drop_at(0, DropReason::DiskFull, events);
                if itself {
                    break;
                }
                index -= 1;
            }
            if itself {
                continue;
            }
            let Some(disk) = &mut self.disk else {
                break;
            };
            let entry = &self.entries[index];
            let Place::Memory(payload) = &entry.place else {
                index += 1;
                continue;
            };
            match disk.append(&entry.meta, payload, false) {
                Ok(at) => {
                    self.memory_used = self.memory_used.saturating_sub(entry.memory());
                    unsynced.push(entry.id);
                    self.entries[index].place = Place::Disk(at);
                    index += 1;
                    // A segment was filled: sync it (and this record) before going on.
                    if disk.has_closed_unsynced() {
                        match disk.sync() {
                            Ok(()) => unsynced.clear(),
                            Err(failure) => {
                                error = Some(format!("{}: {failure}", disk.dir().display()));
                                not_durable.extend(unsynced.drain(..));
                            }
                        }
                    }
                }
                Err(failure) => {
                    // Nothing more is written; what was written is still synced below.
                    error = Some(format!("{}: {failure}", disk.dir().display()));
                    lost.0 += 1;
                    lost.1 += u64::from(entry.meta.records);
                    index += 1;
                }
            }
        }
        if !unsynced.is_empty()
            && let Some(disk) = &mut self.disk
            && let Err(failure) = disk.sync()
        {
            error = Some(format!("{}: {failure}", disk.dir().display()));
            not_durable.extend(unsynced.drain(..));
        }
        if !not_durable.is_empty() {
            let mut gone = Vec::new();
            self.entries.retain(|entry| {
                let keep = !not_durable.contains(&entry.id);
                if !keep {
                    gone.push(entry.clone());
                }
                keep
            });
            for entry in gone {
                // Not released: the record stays in its segment for a later process.
                self.uncount(&entry);
                lost.0 += 1;
                lost.1 += u64::from(entry.meta.records);
            }
        }
        if !out_of_time
            && more()
            && let Some(now) = self.age.as_ref().map(AgeClock::now)
        {
            self.save_clock(now, events);
        }
        let error = error.or_else(|| {
            out_of_time.then(|| "the time for the stop ran out before they were written".to_owned())
        });
        if lost.0 > 0 {
            events.push(Event::NotFlushed {
                batches: lost.0,
                records: lost.1,
                error,
            });
        } else if let Some(error) = error {
            events.push(Event::DiskFailed(error));
        }
    }

    /// Opens the disk for a stop's appends ([`Buffer::flush`]). The age clock's floor is
    /// raised to the newest batch of the last segment written there or the age clock's time
    /// saved there, whichever is later, and every batch here
    /// (all formed by this process: the disk never opened) keeps its age, so that the next
    /// start does not take them for older than they are.
    fn open_for_stop(&mut self, more: &mut dyn FnMut() -> bool, events: &mut Events) {
        match DiskStore::open_for_appends(&self.dir, self.segment_target, more) {
            Ok((store, newest)) => {
                let mut entries = std::mem::take(&mut self.entries);
                self.raise_floor(newest, &mut entries);
                self.entries = entries;
                self.disk_down_until = None;
                events.push(Event::DiskOpenedForStop {
                    bytes: store.bytes(),
                });
                self.disk = Some(store);
            }
            Err(error) => events.push(Event::DiskOpenFailed(format!(
                "{}: {error}",
                self.dir.display()
            ))),
        }
    }

    /// The heartbeat's view.
    pub(crate) fn stats(&self) -> BufferStats {
        BufferStats {
            pending_records: self.pending_records,
            pending_batches: self.entries.len() as u64,
            oldest_pending: self.oldest.keys().next().map(|at| UnixMillis::new(*at)),
            disk_bytes: self.disk.as_ref().map_or(0, DiskStore::bytes),
            dropped_records: self.dropped_records,
        }
    }

    /// Bytes of batches in memory.
    #[cfg(test)]
    fn memory_used(&self) -> usize {
        self.memory_used
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;

    const DAY: u64 = 86_400_000;

    fn formed(stream: &str, seq: u64, len: usize, formed_at: u64) -> Formed {
        Formed {
            meta: Meta {
                stream: Arc::new(StreamId::new(stream).unwrap()),
                seq: Seq::new(seq).unwrap(),
                records: 10,
                oldest: UnixMillis::new(1_000 + seq),
                formed_at,
            },
            payload: vec![u8::try_from(seq % 251).unwrap(); len],
        }
    }

    fn seqs(buffer: &Buffer) -> Vec<(String, u64, bool)> {
        buffer
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.meta.stream.to_string(),
                    entry.meta.seq.get(),
                    matches!(entry.place, Place::Disk(_)),
                )
            })
            .collect()
    }

    fn opened(dir: &Path, memory: usize, disk: u64) -> (Buffer, Events) {
        let mut buffer = Buffer::new(dir.to_owned(), memory, disk);
        let mut events = Events::default();
        buffer.open_disk(0, &mut events);
        (buffer, events)
    }

    #[test]
    fn batches_stay_in_memory_until_it_is_full_then_go_to_disk_in_one_fifo() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let (mut buffer, mut events) = opened(&dir, 2_000, 1 << 20);
        for seq in 1..=4 {
            buffer.push(formed("s1", seq, 1_000, 0), 0, &mut events);
        }
        assert_eq!(
            seqs(&buffer),
            [
                ("s1".into(), 1, false),
                ("s1".into(), 2, false),
                ("s1".into(), 3, true),
                ("s1".into(), 4, true)
            ]
        );
        assert_eq!(buffer.memory_used(), 2_000);
        let stats = buffer.stats();
        assert_eq!(stats.pending_batches, 4);
        assert_eq!(stats.pending_records, 40);
        assert_eq!(stats.oldest_pending, Some(UnixMillis::new(1_001)));
        assert!(stats.disk_bytes > 2_000);
        // Releasing the oldest frees memory, so the next batch goes to memory although older
        // ones are on disk: the FIFO still sends them first.
        let first = buffer.front().unwrap();
        assert!(matches!(first.source, Source::Memory(_)));
        assert!(buffer.release(first.id));
        buffer.push(formed("s1", 5, 1_000, 0), 0, &mut events);
        let order: Vec<u64> = seqs(&buffer).iter().map(|(_, seq, _)| *seq).collect();
        assert_eq!(order, [2, 3, 4, 5]);
        for _ in 0..4 {
            let next = buffer.front().unwrap();
            if let Source::Disk { dir, at } = &next.source {
                assert_eq!(
                    read_payload(dir, at).unwrap(),
                    vec![u8::try_from(next.seq.get()).unwrap(); 1_000]
                );
            }
            assert!(buffer.release(next.id));
        }
        let stats = buffer.stats();
        assert_eq!(
            (
                stats.pending_batches,
                stats.pending_records,
                stats.disk_bytes
            ),
            (0, 0, 0)
        );
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            0,
            "released segments are deleted"
        );
        assert!(
            events
                .0
                .iter()
                .all(|event| !matches!(event, Event::Dropped { .. }))
        );
    }

    #[test]
    fn a_full_disk_drops_the_oldest_batches_first() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let record = record_len(&StreamId::new("s1").unwrap(), 1_000);
        // Memory for one batch, disk for four. A segment is deleted only once all of its
        // batches are gone, so making room may drop several.
        let (mut buffer, mut events) = opened(&dir, 1_000, 4 * (record + SEGMENT_HEADER_LEN) + 10);
        for seq in 1..=10 {
            buffer.push(formed("s1", seq, 1_000, 0), 0, &mut events);
        }
        let order: Vec<u64> = seqs(&buffer).iter().map(|(_, seq, _)| *seq).collect();
        assert!(
            order.windows(2).all(|pair| pair[0] < pair[1]) && order.last() == Some(&10),
            "the newest batches are kept, in order: {order:?}"
        );
        let kept = order.len() as u64;
        assert!(kept >= 4, "{order:?}");
        let stats = buffer.stats();
        assert_eq!(stats.dropped_records, (10 - kept) * 10);
        assert!(stats.disk_bytes <= 4 * (record + SEGMENT_HEADER_LEN) + 10);
        assert_eq!(
            events
                .0
                .iter()
                .filter_map(|event| match event {
                    Event::Dropped {
                        reason, records, ..
                    } => Some((*reason, *records)),
                    _ => None,
                })
                .fold(0, |sum, (reason, records)| {
                    assert_eq!(reason, DropReason::DiskFull);
                    sum + records
                }),
            (10 - kept) * 10
        );
        assert_eq!(order[0], 10 - kept + 1, "exactly the oldest were dropped");
    }

    #[test]
    fn a_failing_disk_keeps_the_newest_batches_in_memory_and_is_tried_again_later() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let (mut buffer, mut events) = opened(&dir, 2_000, 1 << 20);
        buffer.push(formed("s1", 1, 1_000, 0), 0, &mut events);
        buffer.push(formed("s1", 2, 1_000, 0), 0, &mut events);
        // The data partition fails: the next segment cannot be created.
        fs::remove_dir_all(&dir).unwrap();
        for seq in 3..=5 {
            buffer.push(formed("s1", seq, 1_000, 1_000), 1_000, &mut events);
        }
        let order: Vec<(u64, bool)> = seqs(&buffer)
            .iter()
            .map(|(_, seq, disk)| (*seq, *disk))
            .collect();
        assert_eq!(order, [(4, false), (5, false)], "the oldest in memory went");
        assert_eq!(buffer.stats().dropped_records, 30);
        assert_eq!(
            events
                .0
                .iter()
                .filter(|event| matches!(event, Event::DiskFailed(_)))
                .count(),
            1,
            "the disk is not tried again before the retry time: {:?}",
            events.0
        );
        // After the retry time it is tried again; once it works, batches go to disk again.
        fs::create_dir(&dir).unwrap();
        let later = 1_000 + millis(DISK_RETRY);
        buffer.push(formed("s1", 6, 1_000, later), later, &mut events);
        let order: Vec<(u64, bool)> = seqs(&buffer)
            .iter()
            .map(|(_, seq, disk)| (*seq, *disk))
            .collect();
        assert_eq!(order, [(4, false), (5, false), (6, true)]);
    }

    /// Codex review before the commit (eighth round): at the batch limit, a new batch that
    /// cannot be stored (larger than the memory limit, with no disk, or with a disk whose write
    /// fails) is dropped alone. The oldest batch used to make room first and was lost with it.
    #[test]
    fn at_the_batch_limit_a_batch_that_cannot_be_stored_drops_no_other() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        for disk_limit in [0, 1 << 20] {
            let dir = scratch.path().join(format!("buffer-{disk_limit}"));
            let (buffer, _) = opened(&dir, 1_000, disk_limit);
            let mut buffer = buffer.with_batch_limit(2);
            let mut events = Events::default();
            buffer.push(formed("s1", 1, 100, 0), 0, &mut events);
            buffer.push(formed("s1", 2, 100, 0), 0, &mut events);
            if disk_limit > 0 {
                // The data partition fails: no segment can be created.
                fs::remove_dir_all(&dir).unwrap();
            }
            buffer.push(formed("s1", 3, 2_000, 0), 0, &mut events);
            let kept: Vec<u64> = seqs(&buffer).iter().map(|(_, seq, _)| *seq).collect();
            assert_eq!(kept, [1, 2], "disk limit {disk_limit}: {:?}", events.0);
            assert_eq!(
                buffer.stats().dropped_records,
                10,
                "disk limit {disk_limit}"
            );
            assert!(
                events.0.iter().all(|event| !matches!(
                    event,
                    Event::Dropped {
                        reason: DropReason::TooManyBatches,
                        ..
                    }
                )),
                "disk limit {disk_limit}: {:?}",
                events.0
            );
        }
    }

    #[test]
    fn without_a_disk_the_buffer_is_memory_only_and_a_batch_that_fits_nowhere_is_dropped() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let (mut buffer, mut events) = opened(&dir, 2_000, 0);
        assert!(!dir.exists(), "a disk limit of 0 never touches the disk");
        for seq in 1..=3 {
            buffer.push(formed("s1", seq, 1_000, 0), 0, &mut events);
        }
        buffer.push(formed("s1", 4, 3_000, 0), 0, &mut events);
        let order: Vec<u64> = seqs(&buffer).iter().map(|(_, seq, _)| *seq).collect();
        assert_eq!(
            order,
            [2, 3],
            "the oversized batch is dropped, not the others"
        );
        assert_eq!(buffer.stats().dropped_records, 20);
        assert_eq!(buffer.stats().disk_bytes, 0);
    }

    #[test]
    fn batches_expire_after_thirty_days_on_the_age_clock() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let (mut buffer, mut events) = opened(&dir, 1_000, 1 << 20);
        buffer.push(formed("s1", 1, 1_000, 0), 0, &mut events);
        buffer.push(formed("s1", 2, 1_000, DAY), DAY, &mut events);
        buffer.expire(30 * DAY, &mut events);
        assert_eq!(
            buffer.stats().pending_batches,
            2,
            "exactly 30 days is not over"
        );
        buffer.expire(30 * DAY + 1, &mut events);
        let order: Vec<(u64, bool)> = seqs(&buffer)
            .iter()
            .map(|(_, seq, disk)| (*seq, *disk))
            .collect();
        assert_eq!(order, [(2, true)]);
        assert_eq!(buffer.stats().dropped_records, 10);
        let mut events = Events::default();
        buffer.expire(31 * DAY + 1, &mut events);
        assert_eq!(buffer.stats().pending_batches, 0);
        assert_eq!(buffer.stats().disk_bytes, 0, "its segment is deleted");
        assert_eq!(
            events.0.last(),
            Some(&Event::Dropped {
                reason: DropReason::Expired,
                batches: 1,
                records: 10
            })
        );
    }

    #[test]
    fn reopening_orders_streams_by_age_and_each_stream_by_sequence_number() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let (mut buffer, mut events) = opened(&dir, 2_000, 1 << 20);
        // Stream a fills memory (1, 2), then spills (3, 4); a graceful stop writes 1 and 2
        // after 3 and 4. A second process adds stream b on disk.
        for seq in 1..=4 {
            buffer.push(formed("a", seq, 1_000, 100 + seq), 100, &mut events);
        }
        buffer.flush(&mut || true, &mut events);
        assert!(seqs(&buffer).iter().all(|(_, _, disk)| *disk));
        drop(buffer);
        let (mut buffer, mut events) = opened(&dir, 0, 1 << 20);
        for seq in 1..=2 {
            buffer.push(formed("b", seq, 1_000, 500 + seq), 500, &mut events);
        }
        drop(buffer);

        let mut buffer = Buffer::new(dir.clone(), 1_000, 1 << 20);
        let newest = buffer.open_disk(0, &mut events);
        assert_eq!(newest, Some(502));
        buffer.push(formed("c", 1, 1_000, 600), 600, &mut events);
        let order: Vec<(String, u64)> = seqs(&buffer)
            .into_iter()
            .map(|(stream, seq, _)| (stream, seq))
            .collect();
        assert_eq!(
            order,
            [
                ("a".into(), 1),
                ("a".into(), 2),
                ("a".into(), 3),
                ("a".into(), 4),
                ("b".into(), 1),
                ("b".into(), 2),
                ("c".into(), 1)
            ]
        );
        assert!(
            matches!(
                events.0.last(),
                Some(Event::DiskOpened {
                    batches: 6,
                    records: 60,
                    ..
                })
            ),
            "{:?}",
            events.0
        );
    }

    fn segment_files(dir: &Path) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".log")
            })
            .count()
    }

    /// Codex review before the commit (2026-10-28), finding 1: a disk limit of 0 returned
    /// before opening the directory, so batches an earlier process wrote under a larger limit
    /// were never sent and never counted.
    #[test]
    fn batches_an_earlier_process_left_are_sent_with_the_disk_switched_off() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let (mut buffer, mut events) = opened(&dir, 0, 1 << 20);
        for seq in 1..=3 {
            buffer.push(formed("s1", seq, 1_000, 100), 100, &mut events);
        }
        drop(buffer);

        let mut buffer = Buffer::new(dir.clone(), 1 << 20, 0);
        let mut events = Events::default();
        assert_eq!(buffer.open_disk(200, &mut events), Some(100));
        assert_eq!(
            seqs(&buffer),
            [
                ("s1".into(), 1, true),
                ("s1".into(), 2, true),
                ("s1".into(), 3, true)
            ]
        );
        let stats = buffer.stats();
        assert_eq!((stats.pending_batches, stats.pending_records), (3, 30));
        let on_disk = stats.disk_bytes;
        assert!(on_disk > 3_000, "{stats:?}");
        // A new batch is never written to disk.
        buffer.push(formed("s2", 1, 1_000, 200), 200, &mut events);
        assert_eq!(buffer.stats().disk_bytes, on_disk);
        // Sent and released, the batches leave the disk.
        for _ in 0..3 {
            let id = buffer.front().unwrap().id;
            assert!(buffer.release(id));
        }
        assert_eq!(seqs(&buffer), [("s2".into(), 1, false)]);
        assert_eq!(buffer.stats().disk_bytes, 0);
        assert_eq!(segment_files(&dir), 0);
        assert_eq!(buffer.stats().dropped_records, 0);
    }

    #[test]
    fn with_the_disk_switched_off_a_missing_directory_is_neither_created_nor_an_error() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let mut buffer = Buffer::new(dir.clone(), 1 << 20, 0);
        let mut events = Events::default();
        assert_eq!(buffer.open_disk(0, &mut events), None);
        assert_eq!(buffer.tick(millis(DISK_RETRY) * 2, &mut events), None);
        assert!(events.0.is_empty(), "{:?}", events.0);
        assert!(!dir.exists());
    }

    /// The limit was lowered between two runs: the batches found are all kept and sent; only
    /// when memory is full and a new batch needs the disk are the oldest dropped (counted)
    /// until the disk is under the new limit, as for any full disk.
    #[test]
    fn batches_above_a_lowered_disk_limit_are_kept_until_the_disk_needs_room() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        // A 256 KiB limit: 16 KiB segments.
        let (mut buffer, mut events) = opened(&dir, 0, 256 * 1024);
        for seq in 1..=40 {
            buffer.push(formed("s1", seq, 2_000, 100), 100, &mut events);
        }
        drop(buffer);

        let mut buffer = Buffer::new(dir.clone(), 4_000, 32 * 1024);
        let mut events = Events::default();
        buffer.open_disk(200, &mut events);
        let found = buffer.stats();
        assert_eq!((found.pending_batches, found.dropped_records), (40, 0));
        assert!(found.disk_bytes > 64 * 1024, "{found:?}");
        // Memory takes two batches; nothing is dropped yet.
        for seq in 1..=2 {
            buffer.push(formed("s2", seq, 2_000, 200), 200, &mut events);
        }
        assert_eq!(buffer.stats().dropped_records, 0);
        // The third needs the disk: the oldest found batches go until it fits under 32 KiB.
        buffer.push(formed("s2", 3, 2_000, 200), 200, &mut events);
        let after = buffer.stats();
        assert!(after.disk_bytes <= 32 * 1024, "{after:?}");
        assert!(after.dropped_records > 0, "{after:?}");
        let order = seqs(&buffer);
        let kept: Vec<u64> = order
            .iter()
            .filter(|(stream, _, _)| stream == "s1")
            .map(|(_, seq, _)| *seq)
            .collect();
        assert_eq!(kept.last(), Some(&40), "the newest found batch is kept");
        assert_eq!(
            u64::try_from(40 - kept.len()).unwrap() * 10,
            after.dropped_records,
            "only the oldest found batches were dropped, all counted"
        );
        assert!(kept.windows(2).all(|pair| pair[1] == pair[0] + 1));
        assert_eq!(order.last(), Some(&("s2".into(), 3, true)));
        assert!(events.0.iter().any(|event| matches!(
            event,
            Event::Dropped {
                reason: DropReason::DiskFull,
                ..
            }
        )));
    }

    /// Codex review before the commit (2026-10-28), finding 2: when the stop's final sync
    /// failed, the batches it wrote stayed counted as on disk and nothing reported them lost.
    #[test]
    fn a_failed_sync_at_the_stop_reports_its_batches_as_lost() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let (mut buffer, mut events) = opened(&dir, 1 << 20, 1 << 20);
        for seq in 1..=3 {
            buffer.push(formed("s1", seq, 1_000, 100), 100, &mut events);
        }
        assert!(seqs(&buffer).iter().all(|(_, _, disk)| !disk));
        if let Some(disk) = &mut buffer.disk {
            disk.fail_sync = true;
        }
        let mut events = Events::default();
        buffer.flush(&mut || true, &mut events);
        let lost: Vec<&Event> = events
            .0
            .iter()
            .filter(|event| matches!(event, Event::NotFlushed { .. }))
            .collect();
        assert!(
            matches!(
                lost.as_slice(),
                [Event::NotFlushed {
                    batches: 3,
                    records: 30,
                    error: Some(error),
                }] if error.contains("injected sync failure")
            ),
            "{:?}",
            events.0
        );
        let stats = buffer.stats();
        assert_eq!(
            (stats.pending_batches, stats.pending_records),
            (0, 0),
            "{stats:?}"
        );
    }

    /// Found with finding 2: a stop that fills a segment and goes on in the next one must make
    /// every segment it wrote durable, not only the last.
    #[test]
    fn a_stop_that_spans_segments_syncs_every_segment_it_wrote() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        // 16 KiB segments; 20 batches of 2 KB in memory take three of them.
        let (mut buffer, mut events) = opened(&dir, 1 << 20, 256 * 1024);
        for seq in 1..=20 {
            buffer.push(formed("s1", seq, 2_000, 100), 100, &mut events);
        }
        buffer.flush(&mut || true, &mut events);
        assert!(seqs(&buffer).iter().all(|(_, _, disk)| *disk));
        let disk = buffer.disk.as_ref().unwrap();
        let segments = disk.segment_ids();
        assert!(segments.len() >= 3, "{segments:?}");
        // Independent review, finding 1: each segment is synced as soon as the flush goes on to
        // the next (with the next one's first record), so the final sync has one segment to
        // write, not the whole flush.
        assert_eq!(
            disk.synced[..4],
            [segments[0], segments[1], segments[1], segments[2]],
            "{:?}",
            disk.synced
        );
        let mut synced = disk.synced.clone();
        synced.sort_unstable();
        synced.dedup();
        assert_eq!(synced, segments);
        assert!(
            !events
                .0
                .iter()
                .any(|event| matches!(event, Event::NotFlushed { .. }))
        );
    }

    /// A buffer with `count` batches of `len` bytes in memory, ready for a stop.
    fn flush_small_batches(dir: &Path, count: u64, len: usize) -> (Buffer, Events) {
        let (mut buffer, mut events) = opened(dir, usize::try_from(count).unwrap() * len, 1 << 30);
        let stream = "0123456789abcdef0123456789abcdef";
        for seq in 1..=count {
            buffer.push(formed(stream, seq, len, 100), 100, &mut events);
        }
        assert_eq!(buffer.stats().pending_batches, count);
        assert!(seqs(&buffer).iter().all(|(_, _, disk)| !*disk));
        (buffer, Events::default())
    }

    /// Independent review of Laysi/trustiot-v2#90 (2026-10-28), finding 1: a stop with a full
    /// default memory limit (16 MiB) of small batches (62,836 of 267 bytes) writes all of them
    /// within the pipeline's time for a stop, in one pass. Before the fix each batch was looked
    /// up by a linear scan, twice: 7 to 9 seconds in a release build.
    #[test]
    fn a_stop_writes_a_full_memory_of_small_batches_within_its_time() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let count = (crate::upload::DEFAULT_MEMORY_LIMIT / 267) as u64;
        let (mut buffer, mut events) = flush_small_batches(&dir, count, 267);
        let deadline = std::time::Instant::now() + crate::upload::FLUSH_LIMIT;
        buffer.flush(&mut || std::time::Instant::now() < deadline, &mut events);
        assert!(
            !events
                .0
                .iter()
                .any(|event| matches!(event, Event::NotFlushed { .. })),
            "not every batch was written within {:?}: {:?}",
            crate::upload::FLUSH_LIMIT,
            events.0
        );
        assert!(seqs(&buffer).iter().all(|(_, _, disk)| *disk));
        assert_eq!(buffer.stats().pending_batches, count);
        assert_eq!(buffer.memory_used(), 0);
    }

    /// Finding 1: when the stop's time runs out, the flush stops between two records, what it
    /// wrote is synced and readable, and the rest is reported as not flushed.
    #[test]
    fn a_stop_that_runs_out_of_time_reports_the_rest_as_not_flushed() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let (mut buffer, mut events) = flush_small_batches(&dir, 10, 1_000);
        let mut left = 4;
        let mut more = || {
            left -= 1;
            left >= 0
        };
        buffer.flush(&mut more, &mut events);
        assert!(
            matches!(
                &events.0[..],
                [Event::NotFlushed {
                    batches: 6,
                    records: 60,
                    error: Some(error),
                }] if error.contains("time")
            ),
            "{:?}",
            events.0
        );
        let on_disk: Vec<bool> = seqs(&buffer).iter().map(|(_, _, disk)| *disk).collect();
        assert_eq!(
            on_disk,
            [
                true, true, true, true, false, false, false, false, false, false
            ]
        );
        assert!(!buffer.disk.as_ref().unwrap().synced.is_empty(), "synced");
        drop(buffer);
        // A later process finds exactly the four, intact.
        let (buffer, events) = opened(&dir, 0, 1 << 30);
        assert_eq!(seqs(&buffer).len(), 4);
        assert!(
            matches!(
                &events.0[..],
                [Event::DiskOpened {
                    batches: 4,
                    unreadable_bytes: 0,
                    ..
                }]
            ),
            "{:?}",
            events.0
        );
    }

    /// Finding 6: the disk could not be opened at start (the state directory was missing) and
    /// works by the time of the stop, before the next retry: the stop tries it once more instead
    /// of losing memory.
    #[test]
    fn a_stop_tries_a_disk_that_failed_to_open_earlier() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let state = scratch.path().join("state");
        let dir = state.join("buffer");
        let mut buffer = Buffer::new(dir.clone(), 1 << 20, 1 << 20);
        let mut events = Events::default();
        assert!(buffer.open_disk(0, &mut events).is_none());
        assert!(matches!(events.0.last(), Some(Event::DiskOpenFailed(_))));
        fs::create_dir(&state).unwrap();
        for seq in 1..=3 {
            buffer.push(formed("s1", seq, 100, 10), 10, &mut events);
        }
        // The stop comes before DISK_RETRY; it tries the disk anyway.
        let mut events = Events::default();
        buffer.flush(&mut || true, &mut events);
        assert!(
            !events
                .0
                .iter()
                .any(|event| matches!(event, Event::NotFlushed { .. })),
            "{:?}",
            events.0
        );
        assert!(seqs(&buffer).iter().all(|(_, _, disk)| *disk));
        drop(buffer);
        let (buffer, _) = opened(&dir, 0, 1 << 20);
        assert_eq!(seqs(&buffer).len(), 3);
    }

    /// Finding 3: at most the batch limit is kept, in memory and on disk together; a new batch
    /// beyond it drops the oldest, counted.
    #[test]
    fn the_batch_limit_drops_the_oldest() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let mut buffer = Buffer::new(dir, 3_000, 1 << 20).with_batch_limit(5);
        let mut events = Events::default();
        buffer.open_disk(0, &mut events);
        for seq in 1..=8 {
            buffer.push(formed("s1", seq, 1_000, 0), 0, &mut events);
        }
        let kept: Vec<u64> = seqs(&buffer).iter().map(|(_, seq, _)| *seq).collect();
        assert_eq!(kept, [4, 5, 6, 7, 8]);
        assert_eq!(buffer.stats().dropped_records, 30);
        assert!(events.0.contains(&Event::Dropped {
            reason: DropReason::TooManyBatches,
            batches: 3,
            records: 30,
        }));
    }

    /// Finding 3: start-up keeps at most the batch limit of what the disk holds, the newest,
    /// and never reads much more than that into memory at once.
    #[test]
    fn start_up_keeps_at_most_the_batch_limit_of_the_newest_batches() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        // 256 KiB segments, 1,000 batches of 1,000 bytes.
        let (mut buffer, mut events) = opened(&dir, 0, 4 << 20);
        for seq in 1..=1_000 {
            buffer.push(formed("s1", seq, 1_000, seq), seq, &mut events);
        }
        assert_eq!(buffer.stats().pending_batches, 1_000);
        drop(buffer);
        let mut buffer = Buffer::new(dir.clone(), 0, 4 << 20).with_batch_limit(100);
        let mut events = Events::default();
        assert_eq!(buffer.open_disk(0, &mut events), Some(1_000));
        let kept: Vec<u64> = seqs(&buffer).iter().map(|(_, seq, _)| *seq).collect();
        assert_eq!(kept, (901..=1_000).collect::<Vec<_>>());
        assert!(
            buffer.recovery_peak <= 150,
            "start-up held {} records at once",
            buffer.recovery_peak
        );
        assert_eq!(buffer.stats().dropped_records, 9_000);
        assert!(events.0.contains(&Event::Dropped {
            reason: DropReason::TooManyBatches,
            batches: 900,
            records: 9_000,
        }));
        assert!(segment_files(&dir) <= 2, "{}", segment_files(&dir));
    }

    /// Finding 3: the index entry of a buffered batch stays small; with [`MAX_BATCHES`] the
    /// index is bounded (the README states the bound with these sizes).
    #[test]
    fn an_index_entry_stays_small() {
        assert!(size_of::<Entry>() <= 96, "{}", size_of::<Entry>());
        assert!(
            size_of::<disk::Found>() <= 80,
            "{}",
            size_of::<disk::Found>()
        );
    }

    /// Finding 3, measured: resident memory of the index after reading more than
    /// [`MAX_BATCHES`] small batches from disk, and its peak while reading. The batches are
    /// written straight to the disk store, so no index of them exists before the start-up
    /// being measured, and the peak is measured by resetting the process's high-water mark
    /// (`/proc/self/clear_refs`). Run alone, in a release build:
    /// `cargo test --release -p agent --lib -- --ignored --test-threads=1 --nocapture index_memory`.
    #[test]
    #[ignore = "a measurement; run explicitly"]
    fn index_memory_of_a_full_batch_limit() {
        fn status_kib(field: &str) -> u64 {
            fs::read_to_string("/proc/self/status")
                .unwrap()
                .lines()
                .find_map(|line| line.strip_prefix(field))
                .and_then(|rest| rest.trim().trim_end_matches(" kB").trim().parse().ok())
                .unwrap()
        }
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let stream = "0123456789abcdef0123456789abcdef";
        // Three times the limit, so that reading has to thin out the records it holds.
        let count = 3 * MAX_BATCHES as u64;
        {
            let (mut store, _) = DiskStore::open(&dir, MAX_SEGMENT_BYTES, usize::MAX).unwrap();
            for seq in 1..=count {
                let batch = formed(stream, seq, 267, seq);
                store.append(&batch.meta, &batch.payload, false).unwrap();
            }
            store.sync().unwrap();
        }
        // Writing 5 resets VmHWM to the current resident size.
        fs::write("/proc/self/clear_refs", "5").unwrap();
        let before = status_kib("VmRSS:");
        let mut buffer = Buffer::new(dir, 0, 1 << 30);
        let mut events = Events::default();
        buffer.open_disk(0, &mut events);
        let after = status_kib("VmRSS:");
        let peak = status_kib("VmHWM:");
        let kept = buffer.stats().pending_batches;
        let per_batch = (after.saturating_sub(before) * 1024) / kept;
        eprintln!(
            "{count} batches on disk, {kept} kept: RSS {before} kB -> {after} kB ({per_batch} B per kept batch), peak {peak} kB; at most {} records held while reading; size_of Entry {} B, Found {} B",
            buffer.recovery_peak,
            size_of::<Entry>(),
            size_of::<disk::Found>(),
        );
        assert_eq!(kept, MAX_BATCHES as u64);
        assert!(after.saturating_sub(before) < 16 * 1024);
        assert!(peak.saturating_sub(before) < 32 * 1024);
    }

    /// Independent review, second round, finding B, measured: the peak memory of start-up for
    /// a directory of a million one-byte records, each on its own 64-character stream. Run
    /// alone, in a release build:
    /// `cargo test --release -p agent --lib -- --ignored --test-threads=1 --nocapture distinct_streams`.
    #[test]
    #[ignore = "a measurement; run explicitly"]
    fn recovery_memory_of_records_on_distinct_streams() {
        fn status_kib(field: &str) -> u64 {
            fs::read_to_string("/proc/self/status")
                .unwrap()
                .lines()
                .find_map(|line| line.strip_prefix(field))
                .and_then(|rest| rest.trim().trim_end_matches(" kB").trim().parse().ok())
                .unwrap()
        }
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let count = 1_000_000_u64;
        {
            let (mut store, _) = DiskStore::open(&dir, MAX_SEGMENT_BYTES, usize::MAX).unwrap();
            for n in 1..=count {
                let mut batch = formed(&format!("{n:064}"), 1, 1, n);
                batch.meta.formed_at = n;
                store.append(&batch.meta, &batch.payload, false).unwrap();
            }
            store.sync().unwrap();
        }
        let on_disk: u64 = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        fs::write("/proc/self/clear_refs", "5").unwrap();
        let before = status_kib("VmRSS:");
        let started = std::time::Instant::now();
        let mut buffer = Buffer::new(dir, 16 << 20, 64 << 30);
        let mut events = Events::default();
        buffer.open_disk(0, &mut events);
        let took = started.elapsed();
        let after = status_kib("VmRSS:");
        let peak = status_kib("VmHWM:");
        eprintln!(
            "{count} records on distinct streams, {on_disk} B on disk: open took {took:?}; RSS {before} kB -> {after} kB, peak {peak} kB; kept {}, at most {} records held while reading",
            buffer.stats().pending_batches,
            buffer.recovery_peak,
        );
        assert_eq!(buffer.stats().pending_batches, MAX_BATCHES as u64);
        assert!(
            peak.saturating_sub(before) < 64 * 1024,
            "start-up peaked {} MiB above where it started",
            peak.saturating_sub(before) / 1024
        );
    }

    /// Finding 2, through the buffer: a batch formed while the wall clock was 40 days ahead is
    /// dropped 31 real days later, after the wall clock was corrected.
    #[tokio::test(start_paused = true)]
    async fn a_batch_formed_during_a_forward_step_expires_within_thirty_real_days() {
        struct Settable(Arc<std::sync::atomic::AtomicU64>);
        impl crate::clock::WallClock for Settable {
            fn now(&self) -> UnixMillis {
                UnixMillis::new(self.0.load(Ordering::Relaxed))
            }
        }
        let start = 1_790_000_000_000_u64;
        let wall = Arc::new(std::sync::atomic::AtomicU64::new(start));
        let clock = crate::clock::GatewayClock::new(
            Settable(Arc::clone(&wall)),
            "/nonexistent/trustiot-agent-test/synchronized".into(),
        );
        let age = AgeClock::new(clock);
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let mut buffer = Buffer::new(scratch.path().join("buffer"), 1 << 20, 0);
        let mut events = Events::default();
        wall.store(start + 40 * DAY, Ordering::Relaxed);
        let formed_at = age.now();
        buffer.push(formed("s1", 1, 100, formed_at), formed_at, &mut events);
        wall.store(start, Ordering::Relaxed);
        tokio::time::advance(Duration::from_millis(31 * DAY)).await;
        wall.store(start + 31 * DAY, Ordering::Relaxed);
        buffer.expire(age.now(), &mut events);
        assert_eq!(
            buffer.stats().pending_batches,
            0,
            "a batch formed 31 real days ago is still buffered (age clock {}, formed at {formed_at})",
            age.now()
        );
    }

    /// Finding 5, through the buffer: a flipped bit in one record header loses that record
    /// only; its bytes are reported, every other batch is kept with its readings.
    #[test]
    fn records_after_a_corrupt_header_are_kept() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let (mut buffer, mut events) = opened(&dir, 0, 1 << 20);
        for seq in 1..=50 {
            buffer.push(formed("s1", seq, 100, 0), 0, &mut events);
        }
        drop(buffer);
        let record = record_len(&StreamId::new("s1").unwrap(), 100);
        let path = dir.join("seg-00000000000000000000.log");
        let mut bytes = fs::read(&path).unwrap();
        let header_of_second = usize::try_from(SEGMENT_HEADER_LEN + record).unwrap();
        bytes[header_of_second + 8] ^= 0x01;
        fs::write(&path, &bytes).unwrap();
        let (buffer, events) = opened(&dir, 0, 1 << 20);
        let stats = buffer.stats();
        assert_eq!(
            (
                stats.pending_batches,
                stats.pending_records,
                stats.dropped_records
            ),
            (49, 490, 0),
            "{stats:?}"
        );
        assert!(
            matches!(
                &events.0[..],
                [Event::DiskOpened { batches: 49, unreadable_bytes, .. }] if *unreadable_bytes == record
            ),
            "{:?}",
            events.0
        );
    }

    /// A wall clock the test sets.
    struct TestWall(Arc<std::sync::atomic::AtomicU64>);

    impl crate::clock::WallClock for TestWall {
        fn now(&self) -> UnixMillis {
            UnixMillis::new(self.0.load(Ordering::Relaxed))
        }
    }

    /// An age clock on a wall clock that says `at`.
    fn age_clock_at(at: u64) -> AgeClock {
        let wall = Arc::new(std::sync::atomic::AtomicU64::new(at));
        AgeClock::new(crate::clock::GatewayClock::new(
            TestWall(wall),
            "/nonexistent/trustiot-agent-test/synchronized".into(),
        ))
    }

    /// `count` batches an earlier process left in `dir`, formed at `formed_at` on stream "old";
    /// the files there afterwards, with their contents.
    fn earlier_batches(dir: &Path, count: u64, formed_at: u64) -> Vec<(String, Vec<u8>)> {
        {
            let (mut store, _) = DiskStore::open(dir, 1 << 20, usize::MAX).unwrap();
            for seq in 1..=count {
                let batch = formed("old", seq, 100, formed_at);
                store.append(&batch.meta, &batch.payload, true).unwrap();
            }
        }
        let mut files: Vec<(String, Vec<u8>)> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    fs::read(entry.path()).unwrap(),
                )
            })
            .collect();
        files.sort();
        files
    }

    /// Moves the buffer directory away and puts a file in its place, so that it cannot be
    /// opened; the returned closure puts it back.
    fn hide(dir: &Path) -> impl FnOnce() + use<> {
        let hidden = dir.with_extension("hidden");
        fs::rename(dir, &hidden).unwrap();
        fs::write(dir, b"not a directory").unwrap();
        let dir = dir.to_owned();
        move || {
            fs::remove_file(&dir).unwrap();
            fs::rename(&hidden, &dir).unwrap();
        }
    }

    /// Independent review, second round, finding A: the disk failed to open at start and opens
    /// a minute later, holding a batch formed 400 days ahead of the wall clock (the real-time
    /// clock lost its time and NTP is not reachable yet). Raising the age clock's floor to that
    /// batch must not age the batch this process formed before the open: both are a minute
    /// old, and neither expires.
    #[tokio::test(start_paused = true)]
    async fn a_late_disk_open_keeps_the_age_of_batches_formed_before_it() {
        let t = 1_790_000_000_000_u64;
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        earlier_batches(&dir, 1, t);
        let restore = hide(&dir);
        let age = age_clock_at(t - 400 * DAY);
        let mut buffer = Buffer::new(dir.clone(), 1 << 20, 1 << 20).with_age_clock(age.clone());
        let mut events = Events::default();
        assert!(buffer.open_disk(age.now(), &mut events).is_none());
        let formed_at = age.now();
        buffer.push(formed("new", 1, 100, formed_at), formed_at, &mut events);
        tokio::time::advance(Duration::from_secs(61)).await;
        restore();
        let floor = buffer.tick(age.now(), &mut events);
        assert_eq!(floor, Some(t), "the disk opened late: {:?}", events.0);
        let now = age.now();
        buffer.expire(now, &mut events);
        let ages: Vec<(String, u64)> = buffer
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.meta.stream.to_string(),
                    now.saturating_sub(entry.meta.formed_at),
                )
            })
            .collect();
        assert_eq!(
            ages,
            [("old".to_owned(), 61_000), ("new".to_owned(), 61_000)],
            "{:?}",
            events.0
        );
    }

    /// Independent review, second round, info 1: a stop whose disk did not open before opens it
    /// for appending only and reads nothing already there (which can take long on an SD card);
    /// that is left as it was for the next start. The age clock's floor still comes from the
    /// last segment there, so the batch written at the stop keeps its age at the next start
    /// although the wall clock is 400 days behind the batches on disk.
    #[tokio::test(start_paused = true)]
    async fn a_stop_opens_a_disk_that_failed_before_for_appends_only() {
        let t = 1_790_000_000_000_u64;
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let before = earlier_batches(&dir, 3, t);
        let restore = hide(&dir);
        let age = age_clock_at(t - 400 * DAY);
        let mut buffer = Buffer::new(dir.clone(), 1 << 20, 1 << 20).with_age_clock(age.clone());
        let mut events = Events::default();
        assert!(buffer.open_disk(age.now(), &mut events).is_none());
        let formed_at = age.now();
        buffer.push(formed("new", 1, 100, formed_at), formed_at, &mut events);
        tokio::time::advance(Duration::from_secs(61)).await;
        restore();
        let mut events = Events::default();
        buffer.flush(&mut || true, &mut events);
        let on_disk: u64 = before.iter().map(|(_, content)| content.len() as u64).sum();
        assert!(
            matches!(&events.0[..], [Event::DiskOpenedForStop { bytes }] if *bytes == on_disk),
            "{:?}",
            events.0
        );
        assert_eq!(
            seqs(&buffer),
            [("new".to_owned(), 1, true)],
            "nothing already on disk was read"
        );
        for (name, content) in &before {
            assert_eq!(
                &fs::read(dir.join(name)).unwrap(),
                content,
                "{name} changed"
            );
        }
        drop(buffer);
        // The next start, with the wall clock still behind: all four batches, none expired.
        let age = age_clock_at(t - 400 * DAY + 61_000);
        let mut buffer = Buffer::new(dir, 1 << 20, 1 << 20).with_age_clock(age.clone());
        let mut events = Events::default();
        buffer.open_disk(age.now(), &mut events);
        buffer.expire(age.now(), &mut events);
        assert_eq!(buffer.stats().pending_batches, 4, "{:?}", events.0);
    }

    /// Independent review, second round, info 1: once the agent is stopping, housekeeping starts
    /// no read of the disk (the stop's flush opens it for appending only).
    #[test]
    fn housekeeping_opens_no_disk_once_stopping() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let state = scratch.path().join("state");
        let dir = state.join("buffer");
        let stopping = Arc::new(AtomicBool::new(false));
        let mut buffer =
            Buffer::new(dir.clone(), 1 << 20, 1 << 20).with_stop(Arc::clone(&stopping));
        let mut events = Events::default();
        assert!(buffer.open_disk(0, &mut events).is_none());
        fs::create_dir(&state).unwrap();
        let retry = millis(DISK_RETRY);
        stopping.store(true, Ordering::Relaxed);
        buffer.tick(retry, &mut events);
        assert!(buffer.disk.is_none(), "{:?}", events.0);
        assert!(!dir.exists(), "the directory was not even created");
        stopping.store(false, Ordering::Relaxed);
        buffer.tick(retry, &mut events);
        assert!(
            buffer.disk.is_some(),
            "without the stop it opens: {:?}",
            events.0
        );
    }

    /// Writes `batches` to a new buffer directory `dir` as an earlier process did, and cuts the
    /// last `cut` bytes off its segment.
    fn written(dir: &Path, batches: &[Formed], cut: u64) {
        {
            let (mut store, _) = DiskStore::open(dir, 1 << 20, usize::MAX).unwrap();
            for batch in batches {
                store.append(&batch.meta, &batch.payload, true).unwrap();
            }
        }
        let segment = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|extension| extension == "log"))
            .unwrap();
        let len = fs::metadata(&segment).unwrap().len();
        fs::OpenOptions::new()
            .write(true)
            .open(&segment)
            .unwrap()
            .set_len(len - cut)
            .unwrap();
    }

    /// Codex review (fifth round): the newest record on disk has an intact header but its
    /// payload was cut short (the process died while writing it). It was formed 40 days after
    /// the batch before it, during a forward step of the wall clock that was corrected before
    /// the restart. Its header proves the age clock got that far, so the floor starts there:
    /// the older batch is 40 days old and expires, instead of looking new and staying 40 days
    /// longer.
    #[test]
    fn a_torn_record_still_raises_the_floor_to_its_formation_time() {
        let t = 1_790_000_000_000_u64;
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        written(
            &dir,
            &[formed("s1", 1, 100, t), formed("s1", 2, 100, t + 40 * DAY)],
            10,
        );
        let age = age_clock_at(t + 60_000);
        let mut buffer = Buffer::new(dir, 1 << 20, 1 << 20).with_age_clock(age.clone());
        let mut events = Events::default();
        assert_eq!(buffer.open_disk(age.now(), &mut events), Some(t + 40 * DAY));
        assert_eq!(
            buffer.stats().pending_batches,
            1,
            "the torn one is not kept"
        );
        buffer.expire(age.now(), &mut events);
        assert_eq!(buffer.stats().pending_batches, 0, "{:?}", events.0);
    }

    /// Codex review (fifth round): a batch found twice keeps its first copy, and the later
    /// copy's formation time still reaches the floor.
    #[test]
    fn a_batch_found_twice_raises_the_floor_to_its_newest_copy() {
        let t = 1_790_000_000_000_u64;
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        written(
            &dir,
            &[formed("s1", 1, 100, t), formed("s1", 1, 100, t + 40 * DAY)],
            0,
        );
        let age = age_clock_at(t + 60_000);
        let mut buffer = Buffer::new(dir, 1 << 20, 1 << 20).with_age_clock(age.clone());
        let mut events = Events::default();
        assert_eq!(buffer.open_disk(age.now(), &mut events), Some(t + 40 * DAY));
        assert_eq!(buffer.stats().pending_batches, 1, "kept once");
        assert!(age.now() >= t + 40 * DAY);
    }

    /// An age clock on a wall clock the test sets, and that wall clock.
    fn settable_age_clock(at: u64) -> (AgeClock, Arc<std::sync::atomic::AtomicU64>) {
        let wall = Arc::new(std::sync::atomic::AtomicU64::new(at));
        let age = AgeClock::new(crate::clock::GatewayClock::new(
            TestWall(Arc::clone(&wall)),
            "/nonexistent/trustiot-agent-test/synchronized".into(),
        ));
        (age, wall)
    }

    /// Codex review (sixth round): a batch goes to disk; then the wall clock steps 20 days
    /// forward (it was behind, and NTP corrects it) and back again (a wrong correction), and
    /// the agent dies. The age clock counted the 20 days while it ran, and the restart must
    /// not forget them although no batch was formed after the step: the batch is 20 days old
    /// after the restart and expires 10 days later, not 30.
    #[tokio::test(start_paused = true)]
    async fn a_restart_keeps_a_forward_step_the_age_clock_counted_while_batches_were_on_disk() {
        let t = 1_790_000_000_000_u64;
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let (age, wall) = settable_age_clock(t);
        let mut buffer = Buffer::new(dir.clone(), 0, 1 << 20).with_age_clock(age.clone());
        buffer.open_disk(age.now(), &mut Events::default());
        let mut events = Events::default();
        let formed_at = age.now();
        buffer.push(formed("s1", 1, 100, formed_at), formed_at, &mut events);
        assert_eq!(
            segment_files(&dir),
            1,
            "the batch is on disk: {:?}",
            events.0
        );
        wall.store(t + 20 * DAY, Ordering::Relaxed);
        buffer.tick(age.now(), &mut events);
        wall.store(t, Ordering::Relaxed);
        tokio::time::advance(Duration::from_secs(60)).await;
        wall.store(t + 60_000, Ordering::Relaxed);
        buffer.tick(age.now(), &mut events);
        assert!(events.0.is_empty(), "{:?}", events.0);
        drop(buffer);
        // The restart, with the wall clock a minute and a half after the batch.
        let age = age_clock_at(t + 90_000);
        let mut buffer = Buffer::new(dir, 0, 1 << 20).with_age_clock(age.clone());
        let mut events = Events::default();
        buffer.open_disk(age.now(), &mut events);
        assert_eq!(buffer.stats().pending_batches, 1, "{:?}", events.0);
        assert!(
            age.now() - formed_at >= 20 * DAY,
            "the batch counts as {} ms old after the restart",
            age.now() - formed_at
        );
        tokio::time::advance(Duration::from_millis(10 * DAY)).await;
        buffer.expire(age.now(), &mut events);
        assert_eq!(
            buffer.stats().pending_batches,
            0,
            "a batch 30 days old on the age clock is still buffered: {:?}",
            events.0
        );
    }

    /// Codex review (sixth round): a stop saves the age clock's time with the batches it
    /// writes, so a forward step the age clock counted before the stop counts after the
    /// restart too.
    #[tokio::test(start_paused = true)]
    async fn a_stop_saves_the_age_clocks_time_with_the_batches_it_writes() {
        let t = 1_790_000_000_000_u64;
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let (age, wall) = settable_age_clock(t);
        let mut buffer = Buffer::new(dir.clone(), 1 << 20, 1 << 20).with_age_clock(age.clone());
        buffer.open_disk(age.now(), &mut Events::default());
        let mut events = Events::default();
        let formed_at = age.now();
        buffer.push(formed("s1", 1, 100, formed_at), formed_at, &mut events);
        wall.store(t + 20 * DAY, Ordering::Relaxed);
        let stepped = age.now();
        wall.store(t, Ordering::Relaxed);
        buffer.flush(&mut || true, &mut events);
        assert_eq!(
            segment_files(&dir),
            1,
            "the stop wrote the batch: {:?}",
            events.0
        );
        assert!(events.0.is_empty(), "{:?}", events.0);
        drop(buffer);
        let age = age_clock_at(t + 60_000);
        let mut buffer = Buffer::new(dir, 1 << 20, 1 << 20).with_age_clock(age.clone());
        let mut events = Events::default();
        let floor = buffer.open_disk(age.now(), &mut events);
        assert!(
            floor.is_some_and(|floor| floor >= stepped),
            "floor {floor:?}, the age clock was at {stepped} before the stop"
        );
    }

    /// Codex review before the commit (twelfth round): housekeeping is handed the age clock's
    /// time as the pipeline took it before the work started, and another reading of the shared
    /// clock (an upload's) can see a forward step of the wall clock in between. The save must
    /// not store that earlier time: the clock file holds at least what the age clock has
    /// returned by then, so the step stays counted after a restart.
    #[tokio::test(start_paused = true)]
    async fn housekeeping_saves_the_age_clocks_time_as_it_is_when_saving() {
        let t = 1_790_000_000_000_u64;
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let (age, wall) = settable_age_clock(t);
        let mut buffer = Buffer::new(dir.clone(), 0, 1 << 20).with_age_clock(age.clone());
        buffer.open_disk(age.now(), &mut Events::default());
        let mut events = Events::default();
        let formed_at = age.now();
        buffer.push(formed("s1", 1, 100, formed_at), formed_at, &mut events);
        // The pipeline takes the time for housekeeping...
        let taken = age.now();
        // ...and before housekeeping saves, an upload reads the clock during a forward step.
        wall.store(t + 20 * DAY, Ordering::Relaxed);
        let stepped = age.now();
        wall.store(t, Ordering::Relaxed);
        buffer.tick(taken, &mut events);
        assert!(events.0.is_empty(), "{:?}", events.0);
        let saved = disk::read_clock(&dir);
        assert!(
            saved.is_some_and(|saved| saved >= stepped),
            "saved {saved:?}; the age clock had returned {stepped}"
        );
    }

    /// Codex review (sixth round): housekeeping saves the age clock's time at most every
    /// [`CLOCK_SAVE_EVERY`], and only while batches are on disk.
    #[test]
    fn housekeeping_saves_the_age_clocks_time_every_minute_while_batches_are_on_disk() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-buffer");
        let dir = scratch.path().join("buffer");
        let (mut buffer, mut events) = opened(&dir, 0, 1 << 20);
        buffer.tick(1_000, &mut events);
        assert_eq!(
            disk::read_clock(&dir),
            None,
            "no batch on disk, nothing saved"
        );
        buffer.push(formed("s1", 1, 100, 1_000), 1_000, &mut events);
        let every = millis(CLOCK_SAVE_EVERY);
        for (now, saved) in [
            (2_000, 2_000),
            (2_000 + every - 1, 2_000),
            (2_000 + every, 2_000 + every),
        ] {
            buffer.tick(now, &mut events);
            assert_eq!(
                disk::read_clock(&dir),
                Some(saved),
                "after the tick at {now}"
            );
        }
        assert!(
            events
                .0
                .iter()
                .all(|event| !matches!(event, Event::ClockNotSaved(_)))
        );
    }
}
