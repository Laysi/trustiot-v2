//! Files and directories: secret files are created with mode 0600 and refused when their mode
//! is wider; replacements are atomic (a temporary file in the same directory, then `rename`), so
//! an interrupted write never leaves half a file behind.
//!
//! Errors name the file and the problem, never its contents.

use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use zeroize::Zeroizing;

use crate::Error;

/// The mode of every file that holds a secret.
pub const SECRET_MODE: u32 = 0o600;

/// The mode of public files the tool writes (the CSR and the certificate).
pub const PUBLIC_MODE: u32 = 0o644;

/// The mode of directories the tool creates.
pub const DIRECTORY_MODE: u32 = 0o700;

/// Bits a secret file must not have: anything for the group or others, and execute.
const SECRET_FORBIDDEN: u32 = 0o177;

/// Bits a directory the tool keeps files in must not have: write for the group or others, who
/// could otherwise replace the files in it.
const DIRECTORY_FORBIDDEN: u32 = 0o022;

fn io_error(action: &'static str, path: &Path, source: io::Error) -> Error {
    Error::Io {
        action,
        path: path.to_owned(),
        source,
    }
}

/// Creates `path` (and missing parents) with mode 0700 when it does not exist; refuses a path
/// that is not a directory or that the group or others can write to.
///
/// # Errors
///
/// When it cannot be created or inspected, or is refused.
pub fn ensure_directory(path: &Path) -> Result<(), Error> {
    match DirBuilder::new()
        .recursive(true)
        .mode(DIRECTORY_MODE)
        .create(path)
    {
        Ok(()) => {}
        Err(error) => return Err(io_error("create the directory", path, error)),
    }
    let metadata = fs::metadata(path).map_err(|error| io_error("inspect", path, error))?;
    if !metadata.is_dir() {
        return Err(Error::BadFile {
            path: path.to_owned(),
            reason: "is not a directory",
        });
    }
    let mode = metadata.permissions().mode() & 0o7777;
    if mode & DIRECTORY_FORBIDDEN != 0 {
        return Err(Error::DirectoryTooOpen {
            path: path.to_owned(),
            mode,
        });
    }
    Ok(())
}

/// Opens `path` for reading without following a symbolic link.
fn open_no_follow(path: &Path) -> Result<File, Error> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| io_error("open", path, error))
}

/// Reads at most `max_bytes` of `file` into `buffer`; refuses a larger file.
fn read_limited(
    file: &mut File,
    path: &Path,
    max_bytes: usize,
    buffer: &mut Vec<u8>,
) -> Result<(), Error> {
    let limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    file.take(limit)
        .read_to_end(buffer)
        .map_err(|error| io_error("read", path, error))?;
    if buffer.len() > max_bytes {
        return Err(Error::BadFile {
            path: path.to_owned(),
            reason: "is larger than the tool accepts",
        });
    }
    Ok(())
}

/// Reads a file that holds a secret, at most `max_bytes`. Refused unless it is a regular file
/// (not a symbolic link) of mode 0600 or narrower. The bytes are wiped when dropped.
///
/// # Errors
///
/// When the file cannot be read, is refused, or is too large.
pub fn read_secret(path: &Path, max_bytes: usize) -> Result<Zeroizing<Vec<u8>>, Error> {
    let mut file = open_no_follow(path)?;
    // The checks use the opened file, so the file read is the file checked.
    let metadata = file
        .metadata()
        .map_err(|error| io_error("inspect", path, error))?;
    if !metadata.is_file() {
        return Err(Error::BadFile {
            path: path.to_owned(),
            reason: "is not a regular file",
        });
    }
    let mode = metadata.permissions().mode() & 0o7777;
    if mode & SECRET_FORBIDDEN != 0 {
        return Err(Error::TooOpen {
            path: path.to_owned(),
            mode,
        });
    }
    // Reserve the whole buffer first: growing it would leave unwiped copies behind.
    let mut buffer = Zeroizing::new(Vec::with_capacity(max_bytes.saturating_add(1)));
    read_limited(&mut file, path, max_bytes, &mut buffer)?;
    Ok(buffer)
}

/// Reads a public file, at most `max_bytes`.
///
/// # Errors
///
/// When the file cannot be read or is too large.
pub fn read_public(path: &Path, max_bytes: usize) -> Result<Vec<u8>, Error> {
    let mut file = File::open(path).map_err(|error| io_error("open", path, error))?;
    let mut buffer = Vec::new();
    read_limited(&mut file, path, max_bytes, &mut buffer)?;
    Ok(buffer)
}

/// Whether [`create_secret`] made the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Created {
    /// The file is new and holds the contents.
    Yes,
    /// A file (or link) already had the name; nothing was written.
    AlreadyExists,
}

/// Creates `path` with mode 0600 and `contents`, unless something already has that name: an
/// existing file is never overwritten. A failed write removes the file again.
///
/// # Errors
///
/// When the file cannot be created or written.
pub fn create_secret(path: &Path, contents: &[u8]) -> Result<Created, Error> {
    let file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(SECRET_MODE)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Ok(Created::AlreadyExists);
        }
        Err(error) => return Err(io_error("create", path, error)),
    };
    if let Err(error) = fill(file, path, SECRET_MODE, contents) {
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(Created::Yes)
}

/// Sets the mode (the umask may have narrowed it), writes `contents` and flushes them to disk.
fn fill(mut file: File, path: &Path, mode: u32, contents: &[u8]) -> Result<(), Error> {
    file.set_permissions(Permissions::from_mode(mode))
        .map_err(|error| io_error("set the mode of", path, error))?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|error| io_error("write", path, error))
}

/// Replaces `path` atomically with a file of mode 0600 holding `contents`.
///
/// # Errors
///
/// When the file cannot be written or renamed.
pub fn replace_secret(path: &Path, contents: &[u8]) -> Result<(), Error> {
    replace(path, contents, SECRET_MODE)
}

/// Replaces `path` atomically with a file of mode 0644 holding `contents`.
///
/// # Errors
///
/// When the file cannot be written or renamed.
pub fn replace_public(path: &Path, contents: &[u8]) -> Result<(), Error> {
    replace(path, contents, PUBLIC_MODE)
}

fn replace(path: &Path, contents: &[u8], mode: u32) -> Result<(), Error> {
    let temporary = temporary_name(path)?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)
        .map_err(|error| io_error("create", &temporary, error))?;
    let written = fill(file, &temporary, mode, contents).and_then(|()| {
        fs::rename(&temporary, path).map_err(|error| io_error("rename into", path, error))
    });
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written?;
    // Make the rename itself durable; a directory that cannot be synced is not an error.
    if let Some(parent) = path.parent()
        && let Ok(directory) = File::open(parent)
    {
        let _ = directory.sync_all();
    }
    Ok(())
}

/// `.<name>.tmp-<pid>-<nanos>` next to `path`.
fn temporary_name(path: &Path) -> Result<PathBuf, Error> {
    let name = path.file_name().ok_or_else(|| Error::BadFile {
        path: path.to_owned(),
        reason: "has no file name",
    })?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let mut temporary = std::ffi::OsString::from(".");
    temporary.push(name);
    temporary.push(format!(".tmp-{}-{nanos}", std::process::id()));
    Ok(path.with_file_name(temporary))
}

/// Removes `path`; `false` when there was nothing to remove.
///
/// # Errors
///
/// When it exists and cannot be removed.
pub fn remove(path: &Path) -> Result<bool, Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("remove", path, error)),
    }
}

/// The mode bits of `path`, for tests and checks.
///
/// # Errors
///
/// When it cannot be inspected.
pub fn mode(path: &Path) -> Result<u32, Error> {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o7777)
        .map_err(|error| io_error("inspect", path, error))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "workstation-files-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("after 1970")
                .as_nanos()
        ));
        ensure_directory(&path).expect("a scratch directory");
        path
    }

    #[test]
    fn secret_files_are_created_0600_and_never_overwritten() {
        let directory = scratch("create");
        let path = directory.join("key");
        assert_eq!(create_secret(&path, b"one").expect("created"), Created::Yes);
        assert_eq!(mode(&path).expect("a mode"), 0o600);
        assert_eq!(
            create_secret(&path, b"two").expect("checked"),
            Created::AlreadyExists
        );
        assert_eq!(read_secret(&path, 16).expect("read").as_slice(), b"one");
        fs::remove_dir_all(directory).expect("cleaned up");
    }

    #[test]
    fn wider_secret_files_are_refused_by_name_without_contents() {
        let directory = scratch("open");
        let path = directory.join("session");
        create_secret(&path, b"the-secret-contents").expect("created");
        for wider in [0o640, 0o604, 0o700, 0o660, 0o666] {
            fs::set_permissions(&path, Permissions::from_mode(wider)).expect("chmod");
            let error = read_secret(&path, 64).expect_err("refused");
            let message = error.to_string();
            assert!(matches!(error, Error::TooOpen { .. }), "{message}");
            assert!(message.contains(&path.display().to_string()), "{message}");
            assert!(!message.contains("the-secret-contents"), "{message}");
        }
        fs::set_permissions(&path, Permissions::from_mode(0o400)).expect("chmod");
        assert!(read_secret(&path, 64).is_ok(), "narrower than 0600 is fine");
        fs::remove_dir_all(directory).expect("cleaned up");
    }

    #[test]
    fn links_and_oversized_files_are_refused() {
        let directory = scratch("link");
        let target = directory.join("target");
        create_secret(&target, b"0123456789").expect("created");
        let link = directory.join("link");
        symlink(&target, &link).expect("a link");
        assert!(read_secret(&link, 64).is_err(), "a link is not followed");
        assert_eq!(
            create_secret(&link, b"x").expect("checked"),
            Created::AlreadyExists
        );
        assert!(matches!(
            read_secret(&target, 4),
            Err(Error::BadFile { .. })
        ));
        fs::remove_dir_all(directory).expect("cleaned up");
    }

    #[test]
    fn replacements_are_whole_files_with_the_mode_asked_for() {
        let directory = scratch("replace");
        let path = directory.join("session");
        replace_secret(&path, b"first").expect("written");
        replace_secret(&path, b"second").expect("replaced");
        assert_eq!(mode(&path).expect("a mode"), 0o600);
        assert_eq!(read_secret(&path, 64).expect("read").as_slice(), b"second");
        let public = directory.join("cert.pem");
        replace_public(&public, b"cert").expect("written");
        assert_eq!(mode(&public).expect("a mode"), 0o644);
        let left: Vec<_> = fs::read_dir(&directory)
            .expect("listed")
            .map(|entry| entry.expect("an entry").file_name())
            .collect();
        assert_eq!(left.len(), 2, "no temporary file is left: {left:?}");
        fs::remove_dir_all(directory).expect("cleaned up");
    }

    #[test]
    fn directories_writable_by_others_are_refused() {
        let directory = scratch("directory");
        assert_eq!(mode(&directory).expect("a mode"), 0o700);
        fs::set_permissions(&directory, Permissions::from_mode(0o777)).expect("chmod");
        assert!(matches!(
            ensure_directory(&directory),
            Err(Error::DirectoryTooOpen { .. })
        ));
        fs::set_permissions(&directory, Permissions::from_mode(0o755)).expect("chmod");
        ensure_directory(&directory).expect("not writable by others is fine");
        fs::remove_dir_all(directory).expect("cleaned up");
    }
}
