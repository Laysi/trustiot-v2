//! What keeps one check's page from reaching a later check (README.md, 「威脅模型」).
//!
//! The page's code may run with the renderer's full rights: Chromium runs without its own
//! sandbox, as the service's user. Whatever it leaves behind could read a later check's data,
//! which may belong to another tenant, and pass it out through that check's verdict. So between
//! checks nothing of the browser may remain:
//!
//! - processes: the browser's process group is killed, and when the service is the container's
//!   PID 1 every other process of the service's user is killed and reaped too, so a process that
//!   left the group (`setsid`) does not survive;
//! - files and IPC objects: when the service is PID 1, everything in the work directory,
//!   `/dev/shm` and `/dev/mqueue` is removed, `/dev/termination-log` is emptied, and the System V
//!   shared memory segments, message queues and semaphores of the IPC namespace are removed.
//!   The pod's volumes and IPC namespace outlive a container restart, so startup does the same.
//!
//! When any of it fails, the service stops (README.md, 「清理失敗」) rather than run a check
//! next to something it could not remove.
//!
//! Outside a container (the service is not PID 1) only the browser's own process group and its
//! own directory are removed: there the other processes and files are not the service's.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long the processes may take to go away after `SIGKILL`.
pub const PROCESS_DEADLINE: Duration = Duration::from_secs(3);

/// Directories whose whole content goes between checks, besides the work directory.
pub const SHARED_DIRS: [&str; 2] = ["/dev/shm", "/dev/mqueue"];

/// Files that are emptied between checks: Kubernetes makes the termination log writable by any
/// user.
pub const SHARED_FILES: [&str; 1] = ["/dev/termination-log"];

/// Why the cleanup did not leave a clean pod.
#[derive(Debug, thiserror::Error)]
pub enum CleanupError {
    #[error("processes of the service's user survived SIGKILL")]
    Processes,
    #[error("the process table could not be read")]
    ProcessTable,
    #[error("{0} could not be emptied")]
    Files(PathBuf),
    #[error("System V IPC objects could not be removed")]
    Ipc,
}

/// The cleanup the service does in its environment.
#[derive(Clone, Debug)]
pub struct Cleanup {
    work_dir: PathBuf,
    whole_container: bool,
}

impl Cleanup {
    /// The cleanup for this process: the whole container's when it is PID 1 on Linux.
    #[must_use]
    pub fn for_this_process(work_dir: &Path) -> Self {
        Self {
            work_dir: work_dir.to_owned(),
            whole_container: cfg!(target_os = "linux") && std::process::id() == 1,
        }
    }

    /// Whether the cleanup covers the whole container (the service is PID 1).
    #[must_use]
    pub fn whole_container(&self) -> bool {
        self.whole_container
    }

    /// The work directory the browser's directories go in.
    #[must_use]
    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    /// Kills and reaps every other process of the service's user, removes every file the
    /// service's user can write, and every System V IPC object. Does nothing unless
    /// [`Self::whole_container`].
    ///
    /// # Errors
    ///
    /// When anything survives.
    pub fn container(&self) -> Result<(), CleanupError> {
        if !self.whole_container {
            return Ok(());
        }
        sweep_processes(Instant::now() + PROCESS_DEADLINE)?;
        empty_dir(&self.work_dir)?;
        for dir in SHARED_DIRS {
            let dir = Path::new(dir);
            if dir.is_dir() {
                empty_dir(dir)?;
            }
        }
        for file in SHARED_FILES {
            empty_file(Path::new(file))?;
        }
        remove_ipc()
    }
}

/// Removes everything inside `dir`, keeping `dir`. Entries are removed through descriptors
/// relative to their directory, so neither long paths nor symbolic links lead it elsewhere, and
/// directories the owner made unreadable are opened up first.
///
/// # Errors
///
/// When something remains.
pub fn empty_dir(dir: &Path) -> Result<(), CleanupError> {
    let failed = || CleanupError::Files(dir.to_owned());
    let entries = std::fs::read_dir(dir).map_err(|_| failed())?;
    for entry in entries {
        let entry = entry.map_err(|_| failed())?;
        remove_tree(&entry.path()).map_err(|_| failed())?;
    }
    let left = std::fs::read_dir(dir)
        .map_err(|_| failed())?
        .next()
        .is_some();
    if left { Err(failed()) } else { Ok(()) }
}

/// Removes `path` and, for a directory, everything under it, without following symbolic links
/// and without recursion (a page could build a tree deeper than any stack).
fn remove_tree(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // The standard library's removal works through directory descriptors, so long paths do not
    // stop it; it fails on directories the owner made unreadable, which the walk below opens up.
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() && std::fs::remove_dir_all(path).is_ok() {
        return Ok(());
    }
    let mut stack = vec![(path.to_owned(), false)];
    while let Some((path, emptied)) = stack.pop() {
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !metadata.is_dir() {
            remove_file(&path)?;
            continue;
        }
        if emptied {
            std::fs::remove_dir(&path)?;
            continue;
        }
        if metadata.permissions().mode() & 0o700 != 0o700 {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        stack.push((path.clone(), true));
        for entry in std::fs::read_dir(&path)? {
            stack.push((entry?.path(), false));
        }
    }
    Ok(())
}

fn remove_file(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn empty_file(path: &Path) -> Result<(), CleanupError> {
    match std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
    {
        Ok(_) => Ok(()),
        // Missing, or not writable by the service's user and so not by the browser either.
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            Ok(())
        }
        // A read-only file system: nobody writes it.
        Err(error) if error.raw_os_error() == Some(libc::EROFS) => Ok(()),
        Err(_) => Err(CleanupError::Files(path.to_owned())),
    }
}

#[cfg(target_os = "linux")]
fn sweep_processes(deadline: Instant) -> Result<(), CleanupError> {
    let me = std::process::id();
    let uid = linux::uid();
    loop {
        reap_children();
        let survivors = linux::live_processes(uid, me).map_err(|_| CleanupError::ProcessTable)?;
        if survivors.is_empty() {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(CleanupError::Processes);
        }
        for pid in survivors {
            linux::kill(pid);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(not(target_os = "linux"))]
fn sweep_processes(_deadline: Instant) -> Result<(), CleanupError> {
    Ok(())
}

/// Reaps every child that has exited. As PID 1 the service inherits every orphan.
#[cfg(target_os = "linux")]
fn reap_children() {
    while linux::reap_one() {}
}

#[cfg(target_os = "linux")]
fn remove_ipc() -> Result<(), CleanupError> {
    for _ in 0..3 {
        let left = linux::ipc_ids().map_err(|_| CleanupError::Ipc)?;
        if left.iter().all(Vec::is_empty) {
            return Ok(());
        }
        linux::remove_ipc(&left);
    }
    Err(CleanupError::Ipc)
}

#[cfg(not(target_os = "linux"))]
fn remove_ipc() -> Result<(), CleanupError> {
    Ok(())
}

/// Makes the service's memory, descriptors and environment unreadable to processes of its own
/// user (`PR_SET_DUMPABLE` 0): the browser runs as that user and its renderer runs the page's
/// code, so without it a renderer could read or trace the service. Linux only.
///
/// # Errors
///
/// When the kernel refuses.
pub fn protect_this_process() -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::not_dumpable()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io;

    /// Processes other than `me` that belong to `uid` and are not zombies.
    pub(super) fn live_processes(uid: u32, me: u32) -> io::Result<Vec<i32>> {
        let mut pids = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == me {
                continue;
            }
            let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
                // Gone since the directory was read.
                continue;
            };
            if belongs_and_lives(&status, uid) {
                pids.push(i32::try_from(pid).map_err(|_| io::Error::other("pid"))?);
            }
        }
        Ok(pids)
    }

    /// Whether `/proc/<pid>/status` names a live process whose real user is `uid`.
    pub(super) fn belongs_and_lives(status: &str, uid: u32) -> bool {
        let mut zombie = false;
        let mut owned = false;
        for line in status.lines() {
            if let Some(state) = line.strip_prefix("State:") {
                zombie = state.trim_start().starts_with(['Z', 'X']);
            } else if let Some(uids) = line.strip_prefix("Uid:") {
                owned = uids
                    .split_whitespace()
                    .next()
                    .and_then(|real| real.parse::<u32>().ok())
                    == Some(uid);
            }
        }
        owned && !zombie
    }

    #[allow(unsafe_code, reason = "getuid(2) has no safe binding in std")]
    pub(super) fn uid() -> u32 {
        // SAFETY: getuid(2) takes no arguments, touches no memory and cannot fail.
        unsafe { libc::getuid() }
    }

    /// Reaps one exited child; false when there is none.
    #[allow(
        unsafe_code,
        reason = "waitpid(2) for any child has no safe binding in std"
    )]
    pub(super) fn reap_one() -> bool {
        let mut status = 0;
        // SAFETY: waitpid(2) writes only to `status`, a valid local integer.
        let reaped = unsafe { libc::waitpid(-1, &raw mut status, libc::WNOHANG) };
        reaped > 0
    }

    #[allow(unsafe_code, reason = "kill(2) has no safe binding in std")]
    pub(super) fn kill(pid: i32) {
        if pid <= 1 {
            return;
        }
        // SAFETY: kill(2) takes two integers and touches no memory of this process; `pid` is a
        // single process above 1, never a group or every process.
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    /// The ids in `/proc/sysvipc/{shm,msg,sem}`, in that order.
    pub(super) fn ipc_ids() -> io::Result<[Vec<i32>; 3]> {
        let read = |name: &str| -> io::Result<Vec<i32>> {
            match std::fs::read_to_string(format!("/proc/sysvipc/{name}")) {
                Ok(table) => Ok(parse_ipc_ids(&table)),
                // A kernel without System V IPC has none to remove.
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
                Err(error) => Err(error),
            }
        };
        Ok([read("shm")?, read("msg")?, read("sem")?])
    }

    /// The second column (the id) of every row after the header.
    pub(super) fn parse_ipc_ids(table: &str) -> Vec<i32> {
        table
            .lines()
            .skip(1)
            .filter_map(|row| row.split_whitespace().nth(1)?.parse().ok())
            .collect()
    }

    #[allow(
        unsafe_code,
        reason = "System V IPC removal has no safe binding in std or the workspace"
    )]
    pub(super) fn remove_ipc([shm, msg, sem]: &[Vec<i32>; 3]) {
        // SAFETY: each call takes an id and IPC_RMID with a null buffer, which IPC_RMID never
        // reads or writes; an id that is gone or not removable only returns an error.
        unsafe {
            for id in shm {
                libc::shmctl(*id, libc::IPC_RMID, std::ptr::null_mut());
            }
            for id in msg {
                libc::msgctl(*id, libc::IPC_RMID, std::ptr::null_mut());
            }
            for id in sem {
                libc::semctl(*id, 0, libc::IPC_RMID);
            }
        }
    }

    #[allow(unsafe_code, reason = "prctl(2) has no safe binding in std")]
    pub(super) fn not_dumpable() -> io::Result<()> {
        // SAFETY: prctl(2) with PR_SET_DUMPABLE takes integers only and touches no memory.
        let result = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn reads_the_owner_and_state_from_status() {
            let status =
                "Name:\tchromium\nState:\tS (sleeping)\nUid:\t65532\t65532\t65532\t65532\n";
            assert!(belongs_and_lives(status, 65532));
            assert!(!belongs_and_lives(status, 0));
            let zombie = "Name:\tchromium\nState:\tZ (zombie)\nUid:\t65532\t65532\t65532\t65532\n";
            assert!(!belongs_and_lives(zombie, 65532));
            let other = "State:\tR (running)\nUid:\t0\t65532\t65532\t65532\n";
            assert!(!belongs_and_lives(other, 65532));
        }

        #[test]
        fn reads_ipc_ids() {
            let table = "       key      shmid perms                  size  cpid  lpid nattch   uid   gid  cuid  cgid      atime      dtime      ctime                   rss                  swap\n\
                         0          32769   600                  4096    12    12      0 65532 65532 65532 65532          0          0 1700000000                     0                     0\n\
                         1234       65538   600                  4096    12    12      0 65532 65532 65532 65532          0          0 1700000000                     0                     0\n";
            assert_eq!(parse_ipc_ids(table), [32769, 65538]);
            assert_eq!(parse_ipc_ids("key msqid\n"), Vec::<i32>::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dashboard-check-cleanup-{name}-{}",
            std::process::id()
        ));
        if dir.exists() {
            empty_dir(&dir).expect("previous run's files");
        } else {
            std::fs::create_dir_all(&dir).expect("mkdir");
        }
        dir
    }

    #[test]
    fn empties_a_directory_whatever_was_left_in_it() {
        let dir = temp_dir("empty");
        let outside = temp_dir("outside");
        std::fs::write(outside.join("keep"), "kept").expect("write");
        // Plain files, nested directories, a deep chain, an unreadable directory, and links
        // that point outside, which must not be followed.
        std::fs::write(dir.join("file"), "x").expect("write");
        let mut deep = dir.join("deep");
        for _ in 0..200 {
            deep = deep.join("d");
        }
        std::fs::create_dir_all(&deep).expect("mkdir");
        std::fs::write(deep.join("leaf"), "x").expect("write");
        let locked = dir.join("locked");
        std::fs::create_dir(&locked).expect("mkdir");
        std::fs::write(locked.join("secret"), "x").expect("write");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        symlink(&outside, dir.join("link-dir")).expect("symlink");
        symlink(outside.join("keep"), dir.join("link-file")).expect("symlink");

        empty_dir(&dir).expect("empties");

        assert_eq!(std::fs::read_dir(&dir).expect("reads").count(), 0);
        assert_eq!(
            std::fs::read_to_string(outside.join("keep")).expect("kept"),
            "kept"
        );
        std::fs::remove_dir(&dir).expect("cleanup");
        std::fs::remove_dir_all(&outside).expect("cleanup");
    }

    #[test]
    fn empties_writable_files_and_skips_missing_ones() {
        let dir = temp_dir("files");
        let log = dir.join("termination-log");
        std::fs::write(&log, "left by a page").expect("write");
        empty_file(&log).expect("empties");
        assert_eq!(std::fs::read_to_string(&log).expect("reads"), "");
        empty_file(&dir.join("missing")).expect("nothing to do");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn outside_a_container_only_the_browser_is_cleaned() {
        // The test process is never PID 1, so the container cleanup must leave everything.
        let dir = temp_dir("not-init");
        std::fs::write(dir.join("other"), "x").expect("write");
        let cleanup = Cleanup::for_this_process(&dir);
        assert!(!cleanup.whole_container());
        cleanup.container().expect("nothing to do");
        assert!(dir.join("other").exists());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
