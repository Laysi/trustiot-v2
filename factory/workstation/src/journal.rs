//! What `register` keeps about each gateway it registers, in the workstation directory:
//!
//! | Path | Content |
//! | --- | --- |
//! | `registrations/<hardware ID>.json` | the gateway's latest state ([`Record`]): `pending` from before the first request until the platform's answer passed its checks, then `registered` with the registration as the platform answered it |
//! | `registrations.jsonl` | one JSON line per event ([`Event`]), appended, never rewritten |
//!
//! Both are mode 0600 and must belong to the user running the tool (the directory is 0700).
//! A record is replaced atomically ([`files::replace_secret`]: a temporary file in the same
//! directory, flushed, renamed over the record, then the directory flushed), so an interrupted
//! write leaves the previous record whole; a temporary file a crash leaves behind (a name that
//! starts with `.`) is ignored. Each log line is written with one `write` on a file opened with
//! `O_APPEND` and flushed to disk before the tool goes on.
//!
//! Neither holds a secret: no cookie, no key, no signature header. The registration is public
//! (the gateway's certificate, its chain and the root certificate). An error from the platform
//! is kept as its status, M1 message key and `details.reason` only.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use contract::certs::GatewayRegistration;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::Error;
use crate::files::{self, SECRET_MODE};

/// The directory of the records, in the workstation directory.
pub const RECORDS_DIRECTORY: &str = "registrations";

/// The log, in the workstation directory.
pub const LOG_FILE: &str = "registrations.jsonl";

/// The version of [`Record`]'s format.
pub const RECORD_VERSION: u32 = 1;

/// Largest record read or written: a registration with its PEM certificates is a few KiB, and
/// `register` refuses an answer whose fields could make it more than a fraction of this.
pub const MAX_RECORD_BYTES: usize = 256 * 1024;

/// Where a gateway's registration stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// A request was or is about to be sent; no answer passed its checks yet. Running the same
    /// `register` again resolves it: the platform answers a repeat with the registration.
    Pending,
    /// The platform's answer passed every check and is in [`Record::registration`].
    Registered,
}

/// A gateway's latest state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// [`RECORD_VERSION`].
    pub version: u32,
    pub hardware_id: String,
    pub state: State,
    /// The platform's origin the request went to.
    pub platform: String,
    /// The SHA-256 of the platform's root certificate (DER) the operator pinned with
    /// `--root-sha256`, 64 lower-case hex digits. A `registered` record's `root_certificate` has
    /// it.
    pub root_sha256: String,
    /// The SHA-256 of the gateway's public key (its DER SubjectPublicKeyInfo), lower-case hex.
    pub public_key_sha256: String,
    /// The SHA-256 of the endorsement key (its DER SubjectPublicKeyInfo), lower-case hex.
    pub endorsement_key_sha256: String,
    /// The serial of the workstation certificate that signed the request.
    pub workstation_serial: String,
    /// When the record was written.
    pub updated_at: DateTime<Utc>,
    /// The registration, once [`State::Registered`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration: Option<GatewayRegistration>,
}

/// One line of the log: when, the hardware ID, what happened, and the event's own fields.
#[derive(Clone, Debug)]
pub struct Event {
    pub at: DateTime<Utc>,
    pub hardware_id: String,
    /// `started`, `attempt_failed`, `registered`, `stopped` or `gave_up`.
    pub event: &'static str,
    /// Fields of this event: numbers, statuses, message keys, reasons, serials. Never a secret.
    pub fields: Map<String, Value>,
}

impl Event {
    /// An event without fields.
    #[must_use]
    pub fn new(at: DateTime<Utc>, hardware_id: &str, event: &'static str) -> Self {
        Self {
            at,
            hardware_id: hardware_id.to_owned(),
            event,
            fields: Map::new(),
        }
    }

    /// Adds `name: value`.
    #[must_use]
    pub fn with(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.fields.insert(name.to_owned(), value.into());
        self
    }

    fn line(&self) -> Result<Vec<u8>, Error> {
        let mut object = Map::new();
        object.insert("at".to_owned(), Value::String(self.at.to_rfc3339()));
        object.insert(
            "hardware_id".to_owned(),
            Value::String(self.hardware_id.clone()),
        );
        object.insert("event".to_owned(), Value::String(self.event.to_owned()));
        for (name, value) in &self.fields {
            object.insert(name.clone(), value.clone());
        }
        let mut line = serde_json::to_vec(&Value::Object(object))
            .map_err(|_| Error::Failed("cannot encode a log line".to_owned()))?;
        line.push(b'\n');
        Ok(line)
    }
}

/// The records and the log of one workstation directory.
#[derive(Clone, Debug)]
pub struct Journal {
    records: PathBuf,
    log: PathBuf,
}

impl Journal {
    /// The journal in `workstation_directory`; nothing is created yet.
    #[must_use]
    pub fn new(workstation_directory: &Path) -> Self {
        Self {
            records: workstation_directory.join(RECORDS_DIRECTORY),
            log: workstation_directory.join(LOG_FILE),
        }
    }

    /// The records' directory.
    #[must_use]
    pub fn records_directory(&self) -> &Path {
        &self.records
    }

    /// The log file.
    #[must_use]
    pub fn log_path(&self) -> &Path {
        &self.log
    }

    /// The record file of `hardware_id` (a valid hardware ID, so a plain file name).
    #[must_use]
    pub fn record_path(&self, hardware_id: &str) -> PathBuf {
        self.records.join(format!("{hardware_id}.json"))
    }

    /// Creates the records' directory (0700) when missing and checks it and its parent.
    ///
    /// # Errors
    ///
    /// When a directory cannot be created or is refused (see [`files::ensure_directory`]).
    pub fn prepare(&self) -> Result<(), Error> {
        if let Some(parent) = self.records.parent() {
            files::ensure_directory(parent)?;
        }
        files::ensure_directory(&self.records)
    }

    /// The record of `hardware_id`, if there is one.
    ///
    /// # Errors
    ///
    /// When the file is refused (mode, owner, not a regular file) or does not hold a record of
    /// that hardware ID.
    pub fn load(&self, hardware_id: &str) -> Result<Option<Record>, Error> {
        let path = self.record_path(hardware_id);
        let bytes = match files::read_secret(&path, MAX_RECORD_BYTES) {
            Ok(bytes) => bytes,
            Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        parse(&path, &bytes, hardware_id).map(Some)
    }

    /// Every record, in no particular order. Files whose name starts with `.` (temporary files a
    /// crash left) or does not end in `.json` are skipped.
    ///
    /// # Errors
    ///
    /// When the directory cannot be listed, or a record file is refused or unreadable: the
    /// message names that file and what to do about it.
    pub fn records(&self) -> Result<Vec<Record>, Error> {
        let entries = match fs::read_dir(&self.records) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(Error::Io {
                    action: "list",
                    path: self.records.clone(),
                    source,
                });
            }
        };
        let mut records = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| Error::Io {
                action: "list",
                path: self.records.clone(),
                source,
            })?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let Some(hardware_id) = name.strip_suffix(".json") else {
                continue;
            };
            let path = entry.path();
            let record = files::read_secret(&path, MAX_RECORD_BYTES)
                .and_then(|bytes| parse(&path, &bytes, hardware_id))
                .map_err(|error| {
                    Error::Failed(format!(
                        "the registration record {} cannot be used: {error}. register reads every \
                         record in {} to make sure a gateway key is not registered under two \
                         hardware IDs, so it stops until this one is fixed: give it back mode \
                         0600 and this user as owner if that is the problem, or move it out of \
                         that directory (keep it, and compare it with {}) and run the same \
                         command again",
                        path.display(),
                        self.records.display(),
                        self.log.display()
                    ))
                })?;
            records.push(record);
        }
        Ok(records)
    }

    /// Writes `record` as its hardware ID's record, atomically (mode 0600).
    ///
    /// # Errors
    ///
    /// When it cannot be encoded, written or renamed; the previous record is then unchanged.
    pub fn write(&self, record: &Record) -> Result<(), Error> {
        let mut text = serde_json::to_vec_pretty(record)
            .map_err(|_| Error::Failed("cannot encode the registration record".to_owned()))?;
        text.push(b'\n');
        if text.len() > MAX_RECORD_BYTES {
            return Err(Error::Failed(format!(
                "the registration record of {} would be larger than {} KiB; it is not written",
                record.hardware_id,
                MAX_RECORD_BYTES / 1024
            )));
        }
        files::replace_secret(&self.record_path(&record.hardware_id), &text)
    }

    /// Appends `event` to the log as one line and flushes it to disk. The log is created with
    /// mode 0600; an existing one must be a regular file of mode 0600 or narrower that belongs to
    /// the user running the tool.
    ///
    /// # Errors
    ///
    /// When the log is refused or cannot be written.
    pub fn append(&self, event: &Event) -> Result<(), Error> {
        let path = &self.log;
        let io_error = |action: &'static str| {
            move |source: io::Error| Error::Io {
                action,
                path: path.clone(),
                source,
            }
        };
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(SECRET_MODE)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(io_error("open"))?;
        let metadata = file.metadata().map_err(io_error("inspect"))?;
        if !metadata.is_file() {
            return Err(Error::BadFile {
                path: path.clone(),
                reason: "is not a regular file",
            });
        }
        files::check_owner(path, metadata.uid(), files::effective_uid())?;
        let mode = metadata.permissions().mode() & 0o7777;
        if mode & 0o177 != 0 {
            return Err(Error::TooOpen {
                path: path.clone(),
                mode,
            });
        }
        let line = event.line()?;
        // One write of the whole line: with O_APPEND it lands after everything else.
        file.write_all(&line).map_err(io_error("append to"))?;
        file.sync_data().map_err(io_error("flush"))
    }
}

/// The record in `bytes` (from `path`), which must be of `hardware_id`.
fn parse(path: &Path, bytes: &[u8], hardware_id: &str) -> Result<Record, Error> {
    let record: Record = serde_json::from_slice(bytes).map_err(|_| Error::BadFile {
        path: path.to_owned(),
        reason: "is not a registration record",
    })?;
    if record.hardware_id != hardware_id {
        return Err(Error::BadFile {
            path: path.to_owned(),
            reason: "holds the record of another hardware ID",
        });
    }
    if record.version != RECORD_VERSION {
        return Err(Error::BadFile {
            path: path.to_owned(),
            reason: "is a registration record of another version of the tool",
        });
    }
    let pinned = record.root_sha256.len() == 64
        && record
            .root_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !pinned {
        return Err(Error::BadFile {
            path: path.to_owned(),
            reason: "is a registration record without a valid root_sha256",
        });
    }
    if record.state == State::Registered && record.registration.is_none() {
        return Err(Error::BadFile {
            path: path.to_owned(),
            reason: "is a registered record without its registration",
        });
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "workstation-journal-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("after 1970")
                .as_nanos()
        ));
        files::ensure_directory(&path).expect("a scratch directory");
        path
    }

    fn record(hardware_id: &str, state: State) -> Record {
        Record {
            version: RECORD_VERSION,
            hardware_id: hardware_id.to_owned(),
            state,
            platform: "https://app.example".to_owned(),
            root_sha256: "c".repeat(64),
            public_key_sha256: "a".repeat(64),
            endorsement_key_sha256: "b".repeat(64),
            workstation_serial: "4f3a".to_owned(),
            updated_at: DateTime::from_timestamp(1_793_000_000, 0).expect("a time"),
            registration: None,
        }
    }

    #[test]
    fn records_round_trip_with_mode_0600_and_temporary_files_are_ignored() {
        let directory = scratch("records");
        let journal = Journal::new(&directory);
        journal.prepare().expect("prepared");
        assert_eq!(
            files::mode(journal.records_directory()).expect("a mode"),
            0o700
        );
        assert_eq!(journal.load("gw-1").expect("read"), None);
        let pending = record("gw-1", State::Pending);
        journal.write(&pending).expect("written");
        assert_eq!(
            files::mode(&journal.record_path("gw-1")).expect("a mode"),
            0o600
        );
        assert_eq!(journal.load("gw-1").expect("read"), Some(pending.clone()));

        // What a crash in the middle of replacing the record leaves: a partial temporary file
        // next to it. The record itself is the previous, whole one.
        let partial = journal.records_directory().join(".gw-1.json.tmp-4242-17");
        fs::write(
            &partial,
            &b"{\"version\":1,\"hardware_id\":\"gw-1\",\"sta"[..],
        )
        .expect("a partial temporary file");
        assert_eq!(journal.load("gw-1").expect("read"), Some(pending.clone()));
        assert_eq!(journal.records().expect("listed"), [pending]);
        fs::remove_dir_all(directory).expect("cleaned up");
    }

    #[test]
    fn a_record_under_another_name_or_unreadable_is_refused() {
        let directory = scratch("refused");
        let journal = Journal::new(&directory);
        journal.prepare().expect("prepared");
        journal
            .write(&record("gw-1", State::Pending))
            .expect("written");
        fs::rename(journal.record_path("gw-1"), journal.record_path("gw-2")).expect("renamed");
        assert!(matches!(
            journal.load("gw-2"),
            Err(Error::BadFile { reason, .. }) if reason.contains("another hardware ID")
        ));
        files::replace_secret(&journal.record_path("gw-3"), b"{").expect("written");
        assert!(matches!(journal.load("gw-3"), Err(Error::BadFile { .. })));
        let mut registered = record("gw-4", State::Registered);
        registered.registration = None;
        files::replace_secret(
            &journal.record_path("gw-4"),
            &serde_json::to_vec(&registered).expect("encoded"),
        )
        .expect("written");
        assert!(matches!(journal.load("gw-4"), Err(Error::BadFile { .. })));
        let mut unpinned = record("gw-5", State::Pending);
        unpinned.root_sha256 = "C".repeat(64);
        files::replace_secret(
            &journal.record_path("gw-5"),
            &serde_json::to_vec(&unpinned).expect("encoded"),
        )
        .expect("written");
        assert!(matches!(journal.load("gw-5"), Err(Error::BadFile { .. })));
        fs::set_permissions(
            journal.record_path("gw-2"),
            fs::Permissions::from_mode(0o644),
        )
        .expect("chmod");
        assert!(matches!(journal.load("gw-2"), Err(Error::TooOpen { .. })));
        fs::remove_dir_all(directory).expect("cleaned up");
    }

    #[test]
    fn the_log_is_appended_one_line_per_event_and_refused_when_wider_than_0600() {
        let directory = scratch("log");
        let journal = Journal::new(&directory);
        let at = DateTime::from_timestamp(1_793_000_000, 0).expect("a time");
        journal
            .append(&Event::new(at, "gw-1", "started"))
            .expect("appended");
        journal
            .append(
                &Event::new(at, "gw-1", "attempt_failed")
                    .with("attempt", 1)
                    .with("status", 503),
            )
            .expect("appended");
        assert_eq!(files::mode(journal.log_path()).expect("a mode"), 0o600);
        let text = fs::read_to_string(journal.log_path()).expect("read");
        let lines: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("a JSON line"))
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["event"], "started");
        assert_eq!(lines[1]["status"], 503);
        assert_eq!(lines[1]["at"], "2026-10-26T07:33:20+00:00");
        fs::set_permissions(journal.log_path(), fs::Permissions::from_mode(0o640)).expect("chmod");
        assert!(matches!(
            journal.append(&Event::new(at, "gw-1", "started")),
            Err(Error::TooOpen { .. })
        ));
        fs::remove_dir_all(directory).expect("cleaned up");
    }
}
