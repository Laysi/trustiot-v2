//! The applied configuration on the data partition, so that the gateway polls with it after a
//! restart before (or without) reaching the cloud.
//!
//! The file holds the `down/config` payload exactly as received, after it decoded, passed
//! validation and was planned: loading it runs the same [`protocol::config::DesiredConfig`]
//! checks again, so a damaged file is refused like a bad delivery. It is replaced atomically:
//! the new payload goes to a temporary file in the same directory, which is flushed to the disk,
//! renamed over the old file, and the directory is flushed too. A power cut at any moment leaves
//! either the old file or the new one, never a mix; a leftover temporary file is overwritten by
//! the next save and never read.
//!
//! The directory is the one systemd prepares for the service ([`state_directory`]): the unit's
//! sandbox (`ProtectSystem=strict`) leaves only its `StateDirectory` writable, so the agent takes
//! the path from systemd rather than from its configuration file, where it could name a
//! directory the agent can never write.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use protocol::topic::Channel;

/// The file's name in the state directory.
pub const FILE_NAME: &str = "desired-config.cbor";
/// The temporary file a save writes before renaming it to [`FILE_NAME`].
const TEMPORARY_NAME: &str = "desired-config.cbor.tmp";
/// The largest file loaded: the largest `down/config` payload. A larger file cannot be one the
/// agent wrote.
pub const MAX_FILE_BYTES: usize = Channel::Config.max_payload_bytes();

/// The environment variable in which systemd passes the unit's `StateDirectory`
/// (systemd.exec(5)).
pub const STATE_DIRECTORY_VARIABLE: &str = "STATE_DIRECTORY";
/// The state directory without [`STATE_DIRECTORY_VARIABLE`]: the packaged unit's
/// `StateDirectory=trustiot-agent`.
pub const DEFAULT_STATE_DIRECTORY: &str = "/var/lib/trustiot-agent";

/// The agent's state directory, from the value of [`STATE_DIRECTORY_VARIABLE`]: the path systemd
/// passes (the first one, should the unit ever list several, which systemd joins with `:`), or
/// [`DEFAULT_STATE_DIRECTORY`] when the variable is not set.
///
/// # Errors
///
/// When the path is empty or not absolute.
pub fn state_directory(variable: Option<&OsStr>) -> Result<PathBuf, String> {
    let Some(variable) = variable else {
        return Ok(PathBuf::from(DEFAULT_STATE_DIRECTORY));
    };
    let first = variable
        .as_bytes()
        .split(|&byte| byte == b':')
        .next()
        .unwrap_or_default();
    let directory = Path::new(OsStr::from_bytes(first));
    if directory.is_absolute() {
        Ok(directory.to_path_buf())
    } else {
        Err(format!(
            "{STATE_DIRECTORY_VARIABLE}={} is not an absolute path",
            variable.to_string_lossy()
        ))
    }
}

/// Where the applied configuration is kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Store {
    directory: PathBuf,
}

impl Store {
    /// The store in `directory`, the agent's state directory on the data partition. Nothing is
    /// read or created until [`Store::load`] or [`Store::save`].
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    /// The file that holds the applied configuration.
    pub fn path(&self) -> PathBuf {
        self.directory.join(FILE_NAME)
    }

    /// The payload saved last; `None` when nothing was ever saved.
    ///
    /// # Errors
    ///
    /// When the file cannot be read, or is larger than [`MAX_FILE_BYTES`]
    /// ([`io::ErrorKind::InvalidData`]).
    pub fn load(&self) -> io::Result<Option<Vec<u8>>> {
        let file = match File::open(self.path()) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        // Read one byte past the limit to tell a full-size file from a larger one, without
        // trusting the size the file system reports.
        let limit = u64::try_from(MAX_FILE_BYTES).unwrap_or(u64::MAX);
        let mut payload = Vec::new();
        file.take(limit.saturating_add(1))
            .read_to_end(&mut payload)?;
        if payload.len() > MAX_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("larger than the {MAX_FILE_BYTES} bytes a configuration can have"),
            ));
        }
        Ok(Some(payload))
    }

    /// Replaces the saved payload with `payload`, atomically and durably: when this returns
    /// `Ok`, the new payload survives a power cut; when it fails or the power is cut first, the
    /// previous payload is still there. The file is readable by the agent's user only.
    ///
    /// # Errors
    ///
    /// When writing, flushing or renaming fails; the previous payload is then unchanged.
    pub fn save(&self, payload: &[u8]) -> io::Result<()> {
        let temporary = self.directory.join(TEMPORARY_NAME);
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(payload)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, self.path())?;
        // The rename is durable only once the directory entry is on the disk.
        File::open(&self.directory)?.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use testkit::mqtt::ScratchDir;

    use super::*;

    #[test]
    fn the_state_directory_is_the_one_systemd_passes() {
        assert_eq!(
            state_directory(None),
            Ok(PathBuf::from("/var/lib/trustiot-agent"))
        );
        assert_eq!(
            state_directory(Some(OsStr::new("/var/lib/trustiot-agent"))),
            Ok(PathBuf::from("/var/lib/trustiot-agent"))
        );
        assert_eq!(
            state_directory(Some(OsStr::new("/var/lib/a:/var/lib/b"))),
            Ok(PathBuf::from("/var/lib/a"))
        );
        for refused in ["", "state", ":/var/lib/a"] {
            let error = state_directory(Some(OsStr::new(refused))).unwrap_err();
            assert!(
                error.contains("is not an absolute path"),
                "{refused}: {error}"
            );
        }
    }

    #[test]
    fn nothing_saved_loads_as_none() {
        let dir = ScratchDir::new("store-empty");
        assert_eq!(Store::new(dir.path()).load().unwrap(), None);
    }

    #[test]
    fn a_save_replaces_the_previous_payload_and_is_private() {
        let dir = ScratchDir::new("store-save");
        let store = Store::new(dir.path());
        store.save(b"first").unwrap();
        store.save(b"second").unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some(&b"second"[..]));
        let mode = fs::metadata(store.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode {mode:o}");
        assert!(
            !dir.path().join(TEMPORARY_NAME).exists(),
            "the temporary file is renamed, not left behind"
        );
    }

    /// A power cut in the middle of a save leaves a partial temporary file: loading ignores it
    /// and returns the previous payload, and the next save overwrites it.
    #[test]
    fn a_save_cut_short_leaves_the_previous_payload() {
        let dir = ScratchDir::new("store-cut");
        let store = Store::new(dir.path());
        store.save(b"applied").unwrap();
        dir.write(TEMPORARY_NAME, b"half of a new con");
        assert_eq!(store.load().unwrap().as_deref(), Some(&b"applied"[..]));
        store.save(b"next").unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some(&b"next"[..]));
    }

    #[test]
    fn a_file_larger_than_any_configuration_is_refused() {
        let dir = ScratchDir::new("store-large");
        dir.write(FILE_NAME, vec![0_u8; MAX_FILE_BYTES + 1]);
        let error = Store::new(dir.path()).load().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
        dir.write(FILE_NAME, vec![0_u8; MAX_FILE_BYTES]);
        assert_eq!(
            Store::new(dir.path()).load().unwrap().map(|p| p.len()),
            Some(MAX_FILE_BYTES)
        );
    }

    #[test]
    fn a_missing_directory_fails_the_save() {
        let dir = ScratchDir::new("store-missing");
        let store = Store::new(dir.path().join("absent"));
        assert!(store.save(b"x").is_err());
        assert_eq!(store.load().unwrap(), None);
    }
}
