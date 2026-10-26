//! One browser process per check: `chromium-headless-shell` with a fresh directory, driven over
//! the DevTools pipe, killed with its whole process group when the check ends however it ends
//! (README.md, 「瀏覽器旗標」 for every flag).

use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::unix::pipe;

use crate::cdp::{Cdp, EventSink, Tasks};
use crate::cleanup::{Cleanup, CleanupError};

/// The browser's flags, besides `--user-data-dir`. Each one is explained in README.md,
/// 「瀏覽器旗標」; `tests` keeps the two lists equal.
pub const FLAGS: [&str; 27] = [
    "--no-sandbox",
    "--remote-debugging-pipe",
    "--no-zygote",
    "--disable-gpu",
    "--in-process-gpu",
    "--disable-dev-shm-usage",
    "--host-resolver-rules=MAP * ~NOTFOUND",
    "--proxy-server=http://proxy.invalid:9",
    "--proxy-bypass-list=<-loopback>",
    "--force-webrtc-ip-handling-policy",
    "--webrtc-ip-handling-policy=disable_non_proxied_udp",
    "--dns-prefetch-disable",
    "--disable-background-networking",
    "--disable-component-update",
    "--disable-domain-reliability",
    "--disable-client-side-phishing-detection",
    "--no-pings",
    "--disable-breakpad",
    "--disable-extensions",
    "--no-first-run",
    "--deny-permission-prompts",
    "--block-new-web-contents",
    "--mute-audio",
    "--disable-background-timer-throttling",
    "--disable-renderer-backgrounding",
    "--window-size=1280,1024",
    "--lang=en-US",
];

/// Makes each check's directory name unique within this process.
static CHECKS: AtomicU64 = AtomicU64::new(0);

/// Why the browser could not start.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("the check's directory could not be created")]
    Directory,
    #[error("the protocol pipes could not be created")]
    Pipes,
    #[error("the browser could not be started")]
    Spawn,
}

/// A running browser. Dropping it kills the browser and cleans up on a blocking thread; prefer
/// [`Browser::close`], which waits for that.
#[derive(Debug)]
pub struct Browser {
    cdp: Cdp,
    teardown: Option<Teardown>,
}

/// Everything that has to go when the check ends.
#[derive(Debug)]
struct Teardown {
    child: Child,
    dir: PathBuf,
    cleanup: Arc<Cleanup>,
    tasks: Tasks,
}

impl Browser {
    /// Starts a browser for one check in a new directory under the cleanup's work directory.
    /// Must be called inside a Tokio runtime.
    ///
    /// # Errors
    ///
    /// When the directory, the pipes or the process cannot be created.
    pub fn launch(
        chromium: &Path,
        cleanup: Arc<Cleanup>,
        sink: Arc<dyn EventSink>,
    ) -> Result<Self, LaunchError> {
        let dir = cleanup.work_dir().join(format!(
            "check-{}-{}",
            std::process::id(),
            CHECKS.fetch_add(1, Ordering::Relaxed)
        ));
        // Fails when it exists: nothing left by an earlier process is reused.
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|_| LaunchError::Directory)?;
        let profile = dir.join("profile");
        let tmp = dir.join("tmp");
        for sub in [&profile, &tmp] {
            if std::fs::DirBuilder::new().mode(0o700).create(sub).is_err() {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(LaunchError::Directory);
            }
        }
        let spawned = spawn(chromium, &dir, &profile, &tmp);
        let (child, to_browser, from_browser) = match spawned {
            Ok(spawned) => spawned,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(error);
            }
        };
        let (cdp, tasks) = Cdp::start(from_browser, to_browser, sink);
        Ok(Self {
            cdp,
            teardown: Some(Teardown {
                child,
                dir,
                cleanup,
                tasks,
            }),
        })
    }

    /// The protocol connection.
    #[must_use]
    pub fn cdp(&self) -> &Cdp {
        &self.cdp
    }

    /// Kills the browser and waits for the cleanup.
    ///
    /// # Errors
    ///
    /// When something of the browser survived; the caller must not run another check.
    pub async fn close(mut self) -> Result<(), CleanupError> {
        let Some(teardown) = self.teardown.take() else {
            return Ok(());
        };
        match tokio::task::spawn_blocking(move || teardown.run()).await {
            Ok(result) => result,
            // The cleanup panicked: nothing says the pod is clean.
            Err(_) => Err(CleanupError::Processes),
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let Some(teardown) = self.teardown.take() else {
            return;
        };
        // The check was abandoned (the request was dropped, or the check failed): kill the
        // group now, finish the cleanup where blocking is allowed.
        teardown.kill();
        let finish = move || {
            if let Err(error) = teardown.run() {
                cleanup_failed(&error);
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(finish);
            }
            Err(_) => finish(),
        }
    }
}

/// Stops the service after a failed cleanup: the container's restart leaves no process behind,
/// and startup cleans the pod's volumes again before serving (README.md, 「清理失敗」).
pub fn cleanup_failed(error: &CleanupError) -> ! {
    tracing::error!(error = %error, "the cleanup after a check failed; stopping the service");
    std::process::exit(70)
}

impl Teardown {
    /// Kills the browser's process group.
    fn kill(&self) {
        if let Ok(pid) = i32::try_from(self.child.id()) {
            kill_group(pid);
        }
    }

    /// Kills the group, reaps the browser, removes its directory, then cleans the container.
    fn run(mut self) -> Result<(), CleanupError> {
        self.kill();
        let _ = self.child.wait();
        drop(self.tasks);
        let removed = crate::cleanup::empty_dir(&self.dir).and_then(|()| {
            std::fs::remove_dir(&self.dir).map_err(|_| CleanupError::Files(self.dir.clone()))
        });
        // The container cleanup removes the directory too; only report what it could not.
        let container = self.cleanup.container();
        if self.cleanup.whole_container() {
            container
        } else {
            container.and(removed)
        }
    }
}

type Spawned = (Child, pipe::Sender, pipe::Receiver);

/// Starts the browser with its protocol pipes on descriptors 3 (commands) and 4 (messages).
fn spawn(chromium: &Path, dir: &Path, profile: &Path, tmp: &Path) -> Result<Spawned, LaunchError> {
    let (commands_read, commands_write) = std::io::pipe().map_err(|_| LaunchError::Pipes)?;
    let (messages_read, messages_write) = std::io::pipe().map_err(|_| LaunchError::Pipes)?;
    let commands_fd = commands_read.as_raw_fd();
    let messages_fd = messages_write.as_raw_fd();
    let mut command = Command::new(chromium);
    command
        .args(FLAGS)
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("about:blank")
        .env_clear()
        .env("HOME", dir)
        .env("TMPDIR", tmp)
        .env("XDG_CONFIG_HOME", dir.join(".config"))
        .env("XDG_CACHE_HOME", dir.join(".cache"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    pipes_on_3_and_4(&mut command, commands_fd, messages_fd);
    let child = command.spawn().map_err(|_| LaunchError::Spawn)?;
    // The browser's ends stay with the browser only.
    drop(commands_read);
    drop(messages_write);
    let to_browser = pipe::Sender::from_owned_fd(OwnedFd::from(commands_write));
    let from_browser = pipe::Receiver::from_owned_fd(OwnedFd::from(messages_read));
    match (to_browser, from_browser) {
        (Ok(to_browser), Ok(from_browser)) => Ok((child, to_browser, from_browser)),
        _ => {
            let mut child = child;
            if let Ok(pid) = i32::try_from(child.id()) {
                kill_group(pid);
            }
            let _ = child.wait();
            Err(LaunchError::Pipes)
        }
    }
}

/// Puts `commands` on descriptor 3 and `messages` on descriptor 4 of the child, where Chromium's
/// pipe transport expects them, and raises the child's OOM score so that the kernel kills the
/// browser rather than the service when the pod runs out of memory. Both originals are
/// close-on-exec, so the child keeps only 0–4.
#[allow(
    unsafe_code,
    reason = "CommandExt::pre_exec is unsafe; std has no safe way to place a descriptor at a \
              chosen number in the child"
)]
fn pipes_on_3_and_4(command: &mut Command, commands: i32, messages: i32) {
    // SAFETY: the closure runs in the child between fork and exec, and calls only fcntl(2),
    // dup2(2), close(2), open(2) and write(2), which are async-signal-safe; it allocates nothing
    // and touches only its own copies of two integers and static byte strings.
    unsafe {
        command.pre_exec(move || {
            // Above 3 and 4 first, so that placing one cannot close the other.
            let commands_high = libc::fcntl(commands, libc::F_DUPFD_CLOEXEC, 10);
            let messages_high = libc::fcntl(messages, libc::F_DUPFD_CLOEXEC, 10);
            if commands_high < 0
                || messages_high < 0
                || libc::dup2(commands_high, 3) < 0
                || libc::dup2(messages_high, 4) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(commands_high);
            libc::close(messages_high);
            let score = libc::open(
                c"/proc/self/oom_score_adj".as_ptr(),
                libc::O_WRONLY | libc::O_CLOEXEC,
            );
            if score >= 0 {
                libc::write(score, b"1000".as_ptr().cast(), 4);
                libc::close(score);
            }
            Ok(())
        });
    }
}

/// Sends SIGKILL to the process group `pgid` (the browser's own pid, as it leads its group).
#[allow(unsafe_code, reason = "killpg(2) has no safe binding in std")]
fn kill_group(pgid: i32) {
    if pgid <= 1 {
        return;
    }
    // SAFETY: killpg(2) takes two integers and touches no memory; `pgid` is the browser's group,
    // above 1, so never this service's group or every process.
    let _ = unsafe { libc::killpg(pgid, libc::SIGKILL) };
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::*;

    struct Silent;

    impl EventSink for Silent {
        fn event(&self, _: &Cdp, _: Option<&str>, _: &str, _: Value) {}
        fn closed(&self) {}
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dashboard-check-browser-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// A stand-in browser: answers every command on descriptor 3 with an empty result on
    /// descriptor 4, records its arguments and environment, and leaves a child of its own in
    /// its process group.
    fn fake_browser(dir: &Path) -> PathBuf {
        let script = dir.join("fake-browser");
        let record = dir.join("record");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$@\" > '{record}.args'\n\
                 env > '{record}.env'\n\
                 sleep 600 &\n\
                 echo $! > '{record}.grandchild'\n\
                 exec perl -e 'open(my $in, \"<&=3\") or die; open(my $out, \">&=4\") or die; \
                   $/ = \"\\0\"; select($out); $| = 1; \
                   while (my $m = <$in>) {{ my ($id) = $m =~ /\"id\":(\\d+)/; print \"{{\\\"id\\\":$id,\\\"result\\\":{{}}}}\\0\"; }}'\n",
                record = record.display()
            ),
        )
        .expect("write");
        let mut permissions = std::fs::metadata(&script).expect("stat").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod");
        script
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: kill(2) with signal 0 only checks that `pid` exists.
        #[allow(unsafe_code, reason = "a test checks whether a process exists")]
        let result = unsafe { libc::kill(pid, 0) };
        result == 0
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_browser_gets_the_pipes_a_fresh_directory_and_dies_with_its_group() {
        let dir = temp_dir("launch");
        let script = fake_browser(&dir);
        let work = dir.join("work");
        std::fs::create_dir(&work).expect("mkdir");
        let cleanup = Arc::new(Cleanup::for_this_process(&work));
        let browser = Browser::launch(&script, cleanup, Arc::new(Silent)).expect("launches");
        let answer = tokio::time::timeout(
            Duration::from_secs(60),
            browser.cdp().call(None, "Browser.getVersion", &json!({})),
        )
        .await
        .expect("the fake browser answers");
        assert_eq!(answer, Ok(json!({})));

        let args = std::fs::read_to_string(dir.join("record.args")).expect("args");
        let args: Vec<&str> = args.lines().collect();
        assert_eq!(args.get(..FLAGS.len()), Some(&FLAGS[..]));
        let profile = args
            .iter()
            .find_map(|arg| arg.strip_prefix("--user-data-dir="))
            .expect("a profile directory");
        assert!(Path::new(profile).starts_with(&work), "{profile}");
        assert_eq!(args.last(), Some(&"about:blank"));
        let env = std::fs::read_to_string(dir.join("record.env")).expect("env");
        // The shell that stands in for the browser sets these itself.
        let variables: Vec<(&str, &str)> = env
            .lines()
            .filter_map(|line| line.split_once('='))
            .filter(|(name, _)| !["PWD", "SHLVL", "_", "OLDPWD"].contains(name))
            .collect();
        let mut names: Vec<&str> = variables.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["HOME", "TMPDIR", "XDG_CACHE_HOME", "XDG_CONFIG_HOME"],
            "the browser gets no variable of the service"
        );
        let grandchild: i32 = std::fs::read_to_string(dir.join("record.grandchild"))
            .expect("pid")
            .trim()
            .parse()
            .expect("number");
        assert!(alive(grandchild));
        let check_dirs: Vec<PathBuf> = std::fs::read_dir(&work)
            .expect("reads")
            .map(|entry| entry.expect("entry").path())
            .collect();
        assert_eq!(check_dirs.len(), 1);
        // Everything the browser writes on its own (its profile, temporary files, and what
        // HOME and the XDG directories lead to, such as the fontconfig cache) is in this
        // check's directory, which the cleanup removes.
        let check_dir = check_dirs.first().expect("one directory");
        assert!(Path::new(profile).starts_with(check_dir), "{profile}");
        for (name, value) in &variables {
            assert!(Path::new(value).starts_with(check_dir), "{name}={value}");
        }

        browser.close().await.expect("cleans up");

        assert_eq!(std::fs::read_dir(&work).expect("reads").count(), 0);
        // The grandchild was in the group; it is gone or a zombie waiting for its parent.
        let gone = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let state = std::fs::read_to_string(format!("/proc/{grandchild}/stat")).ok();
                let zombie = state.as_deref().is_some_and(|stat| stat.contains(") Z "));
                if !alive(grandchild) || zombie {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(gone.is_ok(), "the browser's process group was killed");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_a_browser_kills_it() {
        let dir = temp_dir("drop");
        let script = fake_browser(&dir);
        let work = dir.join("work");
        std::fs::create_dir(&work).expect("mkdir");
        let cleanup = Arc::new(Cleanup::for_this_process(&work));
        let browser = Browser::launch(&script, cleanup, Arc::new(Silent)).expect("launches");
        let cdp = browser.cdp().clone();
        tokio::time::timeout(
            Duration::from_secs(60),
            cdp.call(None, "Browser.getVersion", &json!({})),
        )
        .await
        .expect("answers")
        .expect("ok");
        drop(browser);
        let closed = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if std::fs::read_dir(&work).expect("reads").count() == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(closed.is_ok(), "the check's directory was removed");
        assert_eq!(
            cdp.call(None, "Browser.getVersion", &json!({})).await,
            Err(crate::cdp::CdpError::Closed)
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_missing_browser_leaves_no_directory() {
        let dir = temp_dir("missing");
        let work = dir.join("work");
        std::fs::create_dir(&work).expect("mkdir");
        let cleanup = Arc::new(Cleanup::for_this_process(&work));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(async {
                Browser::launch(&dir.join("no-such-browser"), cleanup, Arc::new(Silent))
            })
            .expect_err("cannot start");
        assert!(matches!(error, LaunchError::Spawn), "{error}");
        assert_eq!(std::fs::read_dir(&work).expect("reads").count(), 0);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
