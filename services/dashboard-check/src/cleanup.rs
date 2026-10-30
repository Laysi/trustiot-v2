//! What keeps one check's page from reaching a later check (README.md, 「威脅模型」).
//!
//! The page's code may run with the renderer's full rights: Chromium runs without its own
//! sandbox, as the service's user. Whatever it leaves behind could read a later check's data,
//! which may belong to another tenant, and pass it out through that check's verdict. So between
//! checks nothing of the browser may remain:
//!
//! - processes: the browser's process group is killed, and when the service is the container's
//!   PID 1 every other process in its PID namespace is killed and reaped too, whatever its state,
//!   so a process that left the group (`setsid`) does not survive, nor does one whose main thread
//!   has exited while its other threads still run (it shows as a zombie);
//! - files and IPC objects: when the service is PID 1, everything in the work directory,
//!   `/dev/shm` and `/dev/mqueue` is removed, `/dev/termination-log` is emptied, and the System V
//!   shared memory segments, message queues and semaphores of the IPC namespace are removed.
//!   The pod's volumes and IPC namespace outlive a container restart, so startup does the same.
//!
//! When any of it fails, the service stops (README.md, 「清理失敗」) rather than run a check
//! next to something it could not remove.
//!
//! The setting `cleanup` ([`CleanupScope`]) says which of the two cleanups the service must do,
//! and it refuses to start when it cannot ([`ScopeError`]): `container`, the default and what
//! the chart sets, needs the service to be PID 1, which a pod that shares its process namespace
//! or a container started with an init process takes away. `browser` is only for a run outside
//! a container, and refused when anything says the process runs in one ([`container_sign`]);
//! then only the browser's own process group and its own directory are removed: there the other
//! processes and files are not the service's. The signal to every process needs an `Init`,
//! which exists only in PID 1 on Linux, so no other path can reach it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::CleanupScope;

/// How long the other processes in the PID namespace may take to go away after `SIGKILL`.
pub const PROCESS_DEADLINE: Duration = Duration::from_secs(3);

/// Directories whose whole content goes between checks, besides the work directory.
pub const SHARED_DIRS: [&str; 2] = ["/dev/shm", "/dev/mqueue"];

/// Files that are emptied between checks: Kubernetes makes the termination log writable by any
/// user.
pub const SHARED_FILES: [&str; 1] = ["/dev/termination-log"];

/// Why the cleanup did not leave a clean pod.
#[derive(Debug, thiserror::Error)]
pub enum CleanupError {
    #[error("processes survived SIGKILL")]
    Processes,
    #[error("the whole-container cleanup ran in a process that is not PID 1")]
    NotInit,
    #[error("the process table could not be read")]
    ProcessTable,
    #[error("{0} could not be emptied")]
    Files(PathBuf),
    #[error("System V IPC objects could not be removed")]
    Ipc,
}

/// Why this process cannot do the cleanup its settings ask for; the service does not start.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ScopeError {
    #[error(
        "cleanup `container` needs the service to be PID 1 of its container on Linux, and it is \
         not (a pod that shares its process namespace, or a container started with an init \
         process, puts another process there); only a run outside a container may set \
         TRUSTIOT__CLEANUP=browser"
    )]
    NotInit,
    #[error(
        "cleanup `browser` is for a run outside a container, but the service is its \
         container's PID 1, which cleans the whole container: leave TRUSTIOT__CLEANUP unset"
    )]
    Init,
    #[error(
        "cleanup `browser` is for a run outside a container, but the service runs in one \
         ({0}); in a container the service must be PID 1 and clean the whole container"
    )]
    InContainer(&'static str),
}

/// The initial PID namespace's inode number on Linux (`PROC_PID_INIT_INO`): a process outside
/// any container sees `/proc/self/ns/pid` as `pid:[4026531836]`.
const INITIAL_PID_NAMESPACE: &str = "pid:[4026531836]";

/// The first sign that this process runs in a container, or `None` when there is none: a PID
/// namespace other than the initial one (or one that cannot be read), the files Docker and
/// Podman leave in a container's root, or the variables a container engine or Kubernetes sets.
/// Only a refusal follows from a sign; having none proves nothing, it only lets a run outside a
/// container (development, tests) use `browser`.
#[must_use]
pub fn container_sign() -> Option<&'static str> {
    if cfg!(target_os = "linux") {
        match std::fs::read_link("/proc/self/ns/pid") {
            Ok(namespace) if namespace.as_os_str() == INITIAL_PID_NAMESPACE => {}
            Ok(_) => return Some("a PID namespace of its own"),
            Err(_) => return Some("its PID namespace cannot be read"),
        }
    }
    if Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists() {
        return Some("a container engine's marker file");
    }
    if std::env::var_os("container").is_some() {
        return Some("the variable `container`");
    }
    if std::env::var_os("KUBERNETES_SERVICE_HOST").is_some() {
        return Some("a Kubernetes pod's variables");
    }
    None
}

/// The cleanup the service does in its environment.
#[derive(Clone, Debug)]
pub struct Cleanup {
    work_dir: PathBuf,
    /// Present only when this process is PID 1 on Linux.
    init: Option<Init>,
}

/// Proof that this process is PID 1 of its PID namespace on Linux, that is the container's
/// init: the only process allowed to signal every other process. Only [`Init::here`] makes
/// one (the field is private to this module), and the signal checks again before it is sent.
#[derive(Clone, Copy, Debug)]
struct Init(());

impl Init {
    fn here() -> Option<Self> {
        (cfg!(target_os = "linux") && is_init()).then_some(Self(()))
    }
}

fn is_init() -> bool {
    std::process::id() == 1
}

impl Cleanup {
    /// The cleanup `scope` asks for, in this process: the whole container's needs PID 1 on
    /// Linux; the browser's alone is refused there and wherever [`container_sign`] finds one.
    ///
    /// # Errors
    ///
    /// When this process cannot do the cleanup `scope` asks for.
    pub fn for_this_process(work_dir: &Path, scope: CleanupScope) -> Result<Self, ScopeError> {
        Self::for_process(work_dir, scope, Init::here(), container_sign)
    }

    fn for_process(
        work_dir: &Path,
        scope: CleanupScope,
        init: Option<Init>,
        container_sign: impl FnOnce() -> Option<&'static str>,
    ) -> Result<Self, ScopeError> {
        match (scope, init) {
            (CleanupScope::Container, None) => Err(ScopeError::NotInit),
            (CleanupScope::Browser, Some(_)) => Err(ScopeError::Init),
            (CleanupScope::Browser, None) => match container_sign() {
                Some(sign) => Err(ScopeError::InContainer(sign)),
                None => Ok(Self::browser_only(work_dir)),
            },
            (CleanupScope::Container, Some(_)) => Ok(Self {
                work_dir: work_dir.to_owned(),
                init,
            }),
        }
    }

    /// The browser's cleanup alone, whatever the environment: for the unit tests of what
    /// uses it, which may run in a container.
    #[cfg(test)]
    pub(crate) fn browser_only_for_tests(work_dir: &Path) -> Self {
        Self::browser_only(work_dir)
    }

    fn browser_only(work_dir: &Path) -> Self {
        Self {
            work_dir: work_dir.to_owned(),
            init: None,
        }
    }

    /// Whether the cleanup covers the whole container (the service is PID 1).
    #[must_use]
    pub fn whole_container(&self) -> bool {
        self.init.is_some()
    }

    /// The work directory the browser's directories go in.
    #[must_use]
    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    /// Kills and reaps every other process in the PID namespace, removes every file the
    /// service's user can write, and every System V IPC object. Does nothing unless
    /// [`Self::whole_container`].
    ///
    /// # Errors
    ///
    /// When anything survives.
    pub fn container(&self) -> Result<(), CleanupError> {
        let Some(init) = self.init else {
            return Ok(());
        };
        sweep_processes(init, Instant::now() + PROCESS_DEADLINE)?;
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

/// Kills every other process in the PID namespace and reaps them until only this one is left.
/// A process still listed in `/proc` at the deadline, in any state, is a survivor: a zombie is
/// listed until it is reaped, and a process whose main thread has exited stays a zombie while
/// any of its threads runs. Every round signals again, so a process forked while the previous
/// signal went out is caught by the next.
#[cfg(target_os = "linux")]
fn sweep_processes(init: Init, deadline: Instant) -> Result<(), CleanupError> {
    let me = std::process::id();
    loop {
        kill_every_other_process(init)?;
        reap_children();
        let left = linux::other_processes(me).map_err(|_| CleanupError::ProcessTable)?;
        if left.is_empty() {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(CleanupError::Processes);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(not(target_os = "linux"))]
fn sweep_processes(_init: Init, _deadline: Instant) -> Result<(), CleanupError> {
    Ok(())
}

/// Sends `SIGKILL` to every process in the PID namespace but this one. Only PID 1 may: the
/// [`Init`] it takes exists only there, and a copy of it in a forked child is no proof, so the
/// process id is checked again right before the signal.
#[cfg(any(target_os = "linux", test))]
fn kill_every_other_process(_init: Init) -> Result<(), CleanupError> {
    if !is_init() {
        return Err(CleanupError::NotInit);
    }
    signal::kill_all_but_init();
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
mod signal {
    /// `kill(-1, SIGKILL)`: as PID 1 it reaches every process in the PID namespace the
    /// service's user may signal; the kernel leaves out the caller, which is also init.
    #[cfg(all(target_os = "linux", not(test)))]
    #[allow(unsafe_code, reason = "kill(2) has no safe binding in std")]
    pub(super) fn kill_all_but_init() {
        // SAFETY: kill(2) takes two integers and touches no memory of this process. Only
        // `kill_every_other_process` calls it, after checking that this process is PID 1.
        let _ = unsafe { libc::kill(-1, libc::SIGKILL) };
    }

    /// How often the unit tests reached the signal. They never run as PID 1, and on a developer's
    /// machine the real signal would reach every process of the developer, so the unit test
    /// build only counts.
    #[cfg(test)]
    pub(super) static CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    #[cfg(test)]
    pub(super) fn kill_all_but_init() {
        CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
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

    /// Every process listed in `/proc` other than `me`, whatever its state or owner.
    pub(super) fn other_processes(me: u32) -> io::Result<Vec<u32>> {
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
            if pid != me {
                pids.push(pid);
            }
        }
        Ok(pids)
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
        fn lists_every_other_process() {
            let me = std::process::id();
            let others = other_processes(me).expect("reads /proc");
            assert!(!others.contains(&me));
            // The parent (the test runner's caller) is another process and is listed.
            let parent = std::os::unix::process::parent_id();
            assert!(others.contains(&parent), "{parent} not in {others:?}");
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
        let cleanup = Cleanup::browser_only_for_tests(&dir);
        assert!(!cleanup.whole_container());
        cleanup.container().expect("nothing to do");
        assert!(dir.join("other").exists());
        assert_eq!(signal::CALLS.load(std::sync::atomic::Ordering::SeqCst), 0);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn the_service_refuses_a_cleanup_it_cannot_do_here() {
        let dir = Path::new("/nonexistent-work-dir");
        // The default needs PID 1: outside it there is no falling back to the browser's cleanup.
        assert_eq!(
            Cleanup::for_this_process(dir, CleanupScope::default()).err(),
            Some(ScopeError::NotInit)
        );
        let no_sign: fn() -> Option<&'static str> = || None;
        let sign: fn() -> Option<&'static str> = || Some("a PID namespace of its own");
        for container_sign in [no_sign, sign] {
            assert_eq!(
                Cleanup::for_process(dir, CleanupScope::Container, None, container_sign).err(),
                Some(ScopeError::NotInit)
            );
        }
        // As PID 1, `browser` would leave the rest of the container alone: refused.
        assert_eq!(
            Cleanup::for_process(dir, CleanupScope::Browser, Some(Init(())), no_sign).err(),
            Some(ScopeError::Init)
        );
        // In a container that is not PID 1 neither cleanup is possible, so the service does
        // not start: `browser` is refused on any sign of a container.
        assert_eq!(
            Cleanup::for_process(dir, CleanupScope::Browser, None, sign).err(),
            Some(ScopeError::InContainer("a PID namespace of its own"))
        );
        // The two that fit. Neither cleans here, so the forged proof never reaches the signal.
        let whole = Cleanup::for_process(dir, CleanupScope::Container, Some(Init(())), sign)
            .expect("PID 1 cleans the container");
        assert!(whole.whole_container());
        let browser = Cleanup::for_process(dir, CleanupScope::Browser, None, no_sign)
            .expect("outside a container");
        assert!(!browser.whole_container());
        assert_eq!(signal::CALLS.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn browser_is_refused_wherever_a_container_shows() {
        // This process is never PID 1, so `browser` is what decides; whether it is accepted
        // depends only on the signs, which outside Linux are only files and variables.
        let dir = Path::new("/nonexistent-work-dir");
        let sign = container_sign();
        let scope = Cleanup::for_this_process(dir, CleanupScope::Browser);
        match sign {
            Some(sign) => assert_eq!(scope.err(), Some(ScopeError::InContainer(sign))),
            None => assert!(scope.is_ok_and(|cleanup| !cleanup.whole_container())),
        }
    }

    #[test]
    fn only_pid_1_may_signal_every_process() {
        // No proof outside PID 1, so the cleanup above never had one.
        assert!(!is_init());
        assert!(Init::here().is_none());
        // A proof that does not hold (as a copy in a forked child would be) is refused before
        // the signal. The unit test build only counts the signal; it never sends it.
        let forged = Init(());
        assert!(matches!(
            kill_every_other_process(forged),
            Err(CleanupError::NotInit)
        ));
        assert_eq!(signal::CALLS.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
