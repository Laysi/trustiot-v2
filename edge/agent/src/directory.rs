//! A directory held open by its descriptor, for `provision install`, which runs as root on paths
//! that come from a file.
//!
//! [`Directory::open`] walks an absolute path from `/` one component at a time, each opened with
//! `O_DIRECTORY | O_NOFOLLOW`, so a symbolic link anywhere in the path is refused rather than
//! followed. Every later step (reading, creating, renaming, removing, flushing and locking) works
//! on that descriptor with the `*at` system calls and a single file name, never on the path text
//! again: a directory renamed or replaced by a link after it was opened and checked cannot send
//! a write anywhere else.

use std::ffi::{CString, OsStr};
use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Component, Path, PathBuf};

/// An open directory and the path it was opened by (for messages only).
#[derive(Debug)]
pub(crate) struct Directory {
    file: File,
    path: PathBuf,
}

impl Directory {
    /// Opens the directory at the absolute `path` without following any symbolic link in it.
    ///
    /// # Errors
    ///
    /// `path` is relative or has a `..`, a component is a symbolic link or not a
    /// directory (`ELOOP` or `ENOTDIR`: Linux answers a link with either), or it cannot be opened.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let mut components = path.components();
        if components.next() != Some(Component::RootDir) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not an absolute path",
            ));
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/")?;
        for component in components {
            match component {
                Component::Normal(name) => {
                    file = openat(
                        &file,
                        name,
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                        0,
                    )
                    .map_err(|error| {
                        if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
                            io::Error::other(format!(
                                "{name:?} is a symbolic link or not a directory"
                            ))
                        } else {
                            error
                        }
                    })?;
                }
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "the path has \"..\"",
                    ));
                }
            }
        }
        Ok(Self {
            file,
            path: path.to_owned(),
        })
    }

    /// The path it was opened by.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The directory's own metadata (`fstat`).
    pub(crate) fn metadata(&self) -> io::Result<Metadata> {
        self.file.metadata()
    }

    /// Whether `name` is a regular file (not a link, directory, FIFO, socket or device), by
    /// `fstatat` without following a link; `None` when nothing is there. The owner, mode and
    /// size are read from the opened file instead ([`Directory::open_read`], then `fstat`).
    #[allow(
        unsafe_code,
        reason = "fstatat(2) has no safe wrapper in std; `std::fs::symlink_metadata` takes a path"
    )]
    pub(crate) fn is_file(&self, name: &OsStr) -> io::Result<Option<bool>> {
        let name = c_name(name)?;
        // SAFETY: `stat` is plain C data (integers); all zeroes is a valid value, and fstatat
        // overwrites it before it is read.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: the descriptor stays open for the call (borrowed from `self.file`), `name` is
        // NUL-terminated and `stat` is valid for writes; fstatat keeps neither pointer.
        let code = unsafe {
            libc::fstatat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                &raw mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if code != 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(error)
            };
        }
        Ok(Some((stat.st_mode & libc::S_IFMT) == libc::S_IFREG))
    }

    /// Opens `name` for reading without following a link or blocking (`O_NOFOLLOW`,
    /// `O_NONBLOCK`): a link is `ELOOP`, a FIFO opens at once.
    pub(crate) fn open_read(&self, name: &OsStr) -> io::Result<File> {
        openat(
            &self.file,
            name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0,
        )
    }

    /// Creates `name`, which must not exist (`O_EXCL`, `O_NOFOLLOW`), for writing with `mode`.
    pub(crate) fn create_new(&self, name: &OsStr, mode: libc::c_uint) -> io::Result<File> {
        openat(
            &self.file,
            name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            mode,
        )
    }

    /// Renames `from` over `to`, both in this directory (`renameat`).
    #[allow(unsafe_code, reason = "renameat(2) has no safe wrapper in std")]
    pub(crate) fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
        let from = c_name(from)?;
        let to = c_name(to)?;
        let fd = self.file.as_raw_fd();
        // SAFETY: the descriptor stays open for the call (borrowed from `self.file`); both names
        // are NUL-terminated and outlive the call, which keeps neither.
        if unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Removes the file `name` (`unlinkat`).
    #[allow(unsafe_code, reason = "unlinkat(2) has no safe wrapper in std")]
    pub(crate) fn remove(&self, name: &OsStr) -> io::Result<()> {
        let name = c_name(name)?;
        // SAFETY: the descriptor stays open for the call (borrowed from `self.file`); `name` is
        // NUL-terminated and outlives the call, which does not keep it.
        if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Takes the directory's exclusive lock (`flock`), waiting while another process holds it;
    /// it is released when this `Directory` is dropped. The agent's renewal takes the same lock
    /// to write the certificate ([`crate::renewal::write_atomically`]).
    pub(crate) fn lock(&self) -> io::Result<()> {
        self.file.lock()
    }

    /// Whether this process may create, rename and remove files here: write and search
    /// permission for the effective user and groups (`faccessat` with `AT_EACCESS`), which is also
    /// refused on a read-only file system: Linux answers a write check with `EROFS` both for a
    /// file system mounted read-only and for a read-only bind mount of a writable one (access(2),
    /// <https://man7.org/linux/man-pages/man2/access.2.html>, "EROFS"; `do_faccessat` in the
    /// kernel's `fs/open.c` checks the mount), so no `fstatvfs` is needed. Nothing is created.
    ///
    /// # Errors
    ///
    /// Why not, for example `EACCES` or `EROFS`.
    #[allow(unsafe_code, reason = "faccessat(2) has no safe wrapper in std")]
    pub(crate) fn writable(&self) -> io::Result<()> {
        // SAFETY: the descriptor stays open for the call (borrowed from `self.file`); the name is
        // a NUL-terminated literal, which the call does not keep.
        let code = unsafe {
            libc::faccessat(
                self.file.as_raw_fd(),
                c".".as_ptr(),
                libc::W_OK | libc::X_OK,
                libc::AT_EACCESS,
            )
        };
        if code != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Flushes the directory's entries to disk (`fsync`).
    pub(crate) fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    /// The same open directory under a second descriptor (`dup`): the same directory whatever
    /// happened to the path since, and the same lock (a `flock` belongs to the open directory,
    /// which both descriptors share), so taking it through one does not wait for the other.
    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            path: self.path.clone(),
        })
    }
}

/// `name` as a C string: one path component, so not empty, `.`, `..`, or with a `/` or NUL.
fn c_name(name: &OsStr) -> io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} is not a file name"),
        ));
    }
    CString::new(bytes).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// `openat(directory, name, flags | O_CLOEXEC, mode)`.
#[allow(unsafe_code, reason = "openat(2) has no safe wrapper in std")]
fn openat(
    directory: &File,
    name: &OsStr,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> io::Result<File> {
    let name = c_name(name)?;
    // SAFETY: the descriptor stays open for the call (borrowed from `directory`); `name` is
    // NUL-terminated and outlives the call, which does not keep it. `mode` is passed as an unsigned
    // int, as the variadic argument is read.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new descriptor that nothing else owns or closes.
    Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::MetadataExt as _;

    use testkit::mqtt::ScratchDir;

    use super::*;

    fn scratch(kind: &str) -> (ScratchDir, PathBuf) {
        let dir = ScratchDir::new(kind);
        // The system's temporary directory may itself be reached through a link (macOS).
        let path = dir.path().canonicalize().unwrap();
        (dir, path)
    }

    #[test]
    fn a_path_with_a_link_or_dotdot_is_refused() {
        let (_dir, path) = scratch("directory-open");
        std::fs::create_dir(path.join("real")).unwrap();
        std::os::unix::fs::symlink(path.join("real"), path.join("link")).unwrap();
        std::fs::create_dir(path.join("real").join("inner")).unwrap();
        let opened = Directory::open(&path.join("real").join("inner")).unwrap();
        assert_eq!(
            opened.metadata().unwrap().ino(),
            std::fs::metadata(path.join("real").join("inner"))
                .unwrap()
                .ino()
        );
        for refused in [
            path.join("link"),
            path.join("link").join("inner"),
            path.join("real").join("..").join("real"),
            PathBuf::from("relative"),
        ] {
            assert!(Directory::open(&refused).is_err(), "{}", refused.display());
        }
        let error = Directory::open(&path.join("link")).unwrap_err();
        assert!(error.to_string().contains("symbolic link"), "{error}");
        std::fs::write(path.join("file"), "x").unwrap();
        assert!(Directory::open(&path.join("file")).is_err());
    }

    #[test]
    fn names_are_used_in_the_opened_directory_only() {
        let (_dir, path) = scratch("directory-names");
        let directory = Directory::open(&path).unwrap();
        // The directory is renamed after it was opened: its descriptor still names it.
        let moved = path.with_extension("moved");
        std::fs::rename(&path, &moved).unwrap();
        std::fs::create_dir(&path).unwrap();
        let mut file = directory.create_new(OsStr::new("new"), 0o600).unwrap();
        file.write_all(b"contents").unwrap();
        drop(file);
        assert!(directory.create_new(OsStr::new("new"), 0o600).is_err());
        assert!(moved.join("new").exists() && !path.join("new").exists());
        assert_eq!(directory.is_file(OsStr::new("new")).unwrap(), Some(true));
        directory
            .rename(OsStr::new("new"), OsStr::new("renamed"))
            .unwrap();
        let mut text = String::new();
        directory
            .open_read(OsStr::new("renamed"))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "contents");
        std::os::unix::fs::symlink(moved.join("renamed"), moved.join("link")).unwrap();
        assert_eq!(directory.is_file(OsStr::new("link")).unwrap(), Some(false));
        assert_eq!(
            directory
                .open_read(OsStr::new("link"))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ELOOP)
        );
        directory.remove(OsStr::new("renamed")).unwrap();
        assert_eq!(directory.is_file(OsStr::new("renamed")).unwrap(), None);
        directory.sync().unwrap();
        for bad in ["", ".", "..", "a/b"] {
            assert!(directory.is_file(OsStr::new(bad)).is_err(), "{bad:?}");
        }
        std::fs::remove_dir_all(&moved).unwrap();
    }
}
