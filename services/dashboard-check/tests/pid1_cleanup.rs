//! The cleanup between checks as the container's PID 1 (README.md, 「威脅模型」 point 3).
//!
//! A process whose main thread has exited while another of its threads keeps running shows as a
//! zombie (`State: Z`) in `/proc`, yet still runs code. After the cleanup it must be gone, and no
//! process but the service may be left in the PID namespace.
//!
//! Only the container's PID 1 may signal every process, so the test runs its own binary as PID 1
//! in a container, as crates/service-kit's PID 1 tests run `example-service`: anywhere else the
//! cleanup leaves other processes alone (src/cleanup.rs). Linux only; needs podman or docker and
//! is skipped without one unless `CI` is set. The binary has its own `main` instead of libtest's,
//! because the same binary is the driver on the host, the container's PID 1 and the lingering
//! process, and the lingering process must end its main thread.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "without libtest the binary reports its result on stdout and stderr itself"
)]

use std::process::ExitCode;

/// Which part the binary plays; unset on the host.
const ROLE: &str = "DASHBOARD_CHECK_PID1_TEST_ROLE";
/// The one test this binary runs, as libtest would name it.
const TEST_NAME: &str = "cleanup_as_pid_1_leaves_no_process_behind";

fn main() -> ExitCode {
    match std::env::var(ROLE).as_deref() {
        #[cfg(target_os = "linux")]
        Ok("init") => inside::init(),
        #[cfg(target_os = "linux")]
        Ok("linger") => inside::linger(),
        Ok(other) => {
            eprintln!("unknown {ROLE}: {other}");
            ExitCode::FAILURE
        }
        Err(_) => driver(),
    }
}

/// Runs the test from the host, honouring the filter and options Cargo passes to every test
/// binary.
fn driver() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--list") {
        println!("{TEST_NAME}: test");
        return ExitCode::SUCCESS;
    }
    if !selected(&args) {
        println!("\nrunning 0 tests\n");
        return ExitCode::SUCCESS;
    }
    println!("\nrunning 1 test");
    #[cfg(target_os = "linux")]
    host::run();
    #[cfg(not(target_os = "linux"))]
    eprintln!("skipped: the PID 1 cleanup exists on Linux only");
    println!("test {TEST_NAME} ... ok\n\ntest result: ok. 1 passed; 0 failed\n");
    ExitCode::SUCCESS
}

/// Whether libtest would run this test given `args`: no filter or a matching one, not skipped,
/// and not a run of ignored tests only.
fn selected(args: &[String]) -> bool {
    // libtest options followed by a value.
    const VALUED: [&str; 7] = [
        "--test-threads",
        "--skip",
        "--format",
        "--color",
        "--logfile",
        "--shuffle-seed",
        "-Z",
    ];
    let mut exact = false;
    let mut filters = Vec::new();
    let mut skips = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--exact" => exact = true,
            // Only ignored tests, and this one is not.
            "--ignored" => return false,
            "--skip" => skips.extend(args.next().map(String::as_str)),
            option if VALUED.contains(&option) => {
                args.next();
            }
            option => match option.strip_prefix("--skip=") {
                Some(skip) => skips.push(skip),
                None if option.starts_with('-') => {}
                None => filters.push(option),
            },
        }
    }
    let matches = |pattern: &str| {
        if exact {
            pattern == TEST_NAME
        } else {
            TEST_NAME.contains(pattern)
        }
    };
    (filters.is_empty() || filters.iter().any(|filter| matches(filter)))
        && !skips.iter().any(|skip| matches(skip))
}

#[cfg(target_os = "linux")]
mod host {
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::ROLE;

    /// A minimal glibc base without a shell, so the test binary itself is PID 1; the same pin as
    /// every other distroless/cc copy in the repository (Renovate moves them together).
    const BASE_IMAGE: &str = "gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2";
    /// The bound on the container's whole run. Inside, the slowest steps are a process starting
    /// and the cleanup's own deadline (3 s), so only a hang reaches it.
    const RUN_LIMIT: Duration = Duration::from_secs(180);

    struct Removed {
        runtime: &'static str,
        args: &'static [&'static str],
        name: String,
    }

    impl Drop for Removed {
        fn drop(&mut self) {
            let _ = Command::new(self.runtime)
                .args(self.args)
                .arg(&self.name)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    fn container_runtime() -> Option<&'static str> {
        ["podman", "docker"].into_iter().find(|runtime| {
            Command::new(runtime)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
    }

    fn output(runtime: &str, args: &[&str]) -> String {
        let output = Command::new(runtime)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("{runtime} {args:?}: {error}"));
        assert!(
            output.status.success(),
            "{runtime} {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    pub(super) fn run() {
        let Some(runtime) = container_runtime() else {
            assert!(
                std::env::var_os("CI").is_none(),
                "podman or docker is required when CI is set"
            );
            eprintln!("skipped: podman or docker is not available (set CI to make this a failure)");
            return;
        };
        let unique = format!("dashboard-check-pid1-{}", std::process::id());
        let context = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(&unique);
        std::fs::create_dir_all(&context).expect("create the build context");
        let binary = std::env::current_exe().expect("this test's binary");
        std::fs::copy(&binary, context.join("pid1-cleanup")).expect("copy the binary");
        std::fs::write(
            context.join("Containerfile"),
            format!(
                "FROM {BASE_IMAGE}\nCOPY pid1-cleanup /usr/local/bin/pid1-cleanup\n\
                 ENTRYPOINT [\"/usr/local/bin/pid1-cleanup\"]\n"
            ),
        )
        .expect("write the Containerfile");
        let image = Removed {
            runtime,
            args: &["rmi", "--force"],
            name: format!("localhost/{unique}:test"),
        };
        let containerfile = context.join("Containerfile");
        let context_arg = context.to_str().expect("a UTF-8 path");
        output(
            runtime,
            &[
                "build",
                "--quiet",
                "--file",
                containerfile.to_str().expect("a UTF-8 path"),
                "--tag",
                &image.name,
                context_arg,
            ],
        );
        let _ = std::fs::remove_dir_all(&context);
        let container = Removed {
            runtime,
            // `--volumes`: the image's anonymous volumes go with the container.
            args: &["rm", "--force", "--volumes"],
            name: unique.clone(),
        };
        let role = format!("{ROLE}=init");
        output(
            runtime,
            &[
                "run",
                "--detach",
                "--name",
                &container.name,
                // As the chart runs the checker: a read-only root with a memory-backed /tmp, the
                // image's non-root user, no capabilities, no privilege gain, and no network.
                "--read-only",
                "--tmpfs",
                "/tmp:rw,mode=1777",
                "--user",
                "65532:65532",
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges",
                "--network",
                "none",
                "--env",
                &role,
                &image.name,
            ],
        );
        let started = Instant::now();
        loop {
            let state = output(
                runtime,
                &["inspect", "--format", "{{.State.Status}}", &unique],
            );
            if state == "exited" {
                break;
            }
            assert!(
                started.elapsed() < RUN_LIMIT,
                "the container did not exit within {RUN_LIMIT:?} (state {state}); its output: {}",
                logs(runtime, &unique)
            );
            std::thread::sleep(Duration::from_millis(200));
        }
        let code = output(
            runtime,
            &["inspect", "--format", "{{.State.ExitCode}}", &unique],
        );
        let logs = logs(runtime, &unique);
        println!("{logs}");
        assert_eq!(
            code, "0",
            "the cleanup as PID 1 left something behind: {logs}"
        );
        assert!(
            logs.contains("no process left besides PID 1"),
            "the container ended without its verdict: {logs}"
        );
        drop(container);
        drop(image);
    }

    fn logs(runtime: &str, name: &str) -> String {
        let output = Command::new(runtime)
            .args(["logs", name])
            .output()
            .map(|output| {
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )
            });
        output.unwrap_or_else(|error| format!("(no logs: {error})"))
    }
}

/// The parts that run inside the container.
#[cfg(target_os = "linux")]
mod inside {
    use std::path::Path;
    use std::process::{Command, ExitCode, Stdio};
    use std::time::{Duration, Instant};

    use dashboard_check::cleanup::{Cleanup, ScopeError};
    use dashboard_check::config::CleanupScope;

    use super::ROLE;

    /// Where the lingering process counts while its main thread is gone.
    const HEARTBEAT: &str = "/tmp/heartbeat";
    /// How long the lingering process may take to start and end its main thread.
    const LINGER_LIMIT: Duration = Duration::from_secs(60);

    /// PID 1: starts a process whose main thread exits while another thread keeps counting,
    /// confirms the kernel shows it as a zombie that still runs, cleans up, and checks that no
    /// other process is left.
    pub(super) fn init() -> ExitCode {
        if std::process::id() != 1 {
            println!("FAIL: not PID 1 (pid {})", std::process::id());
            return ExitCode::FAILURE;
        }
        let work = Path::new("/tmp/work");
        std::fs::create_dir_all(work).expect("create the work directory");
        // As PID 1 the browser's cleanup alone is refused, and the container's is the one.
        let browser = Cleanup::for_this_process(work, CleanupScope::Browser).err();
        if browser != Some(ScopeError::Init) {
            println!("FAIL: cleanup `browser` as PID 1 gave {browser:?}, not ScopeError::Init");
            return ExitCode::FAILURE;
        }
        let cleanup = match Cleanup::for_this_process(work, CleanupScope::Container) {
            Ok(cleanup) if cleanup.whole_container() => cleanup,
            other => {
                println!("FAIL: the cleanup does not cover the container as PID 1: {other:?}");
                return ExitCode::FAILURE;
            }
        };
        let child = Command::new(std::env::current_exe().expect("this binary"))
            .env(ROLE, "linger")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("start the lingering process");
        let pid = child.id();
        // Never waited for here: the cleanup reaps it.
        drop(child);

        let started = Instant::now();
        let mut first_beat = None;
        let (thread, beats) = loop {
            assert!(
                started.elapsed() < LINGER_LIMIT,
                "process {pid} never showed a zombie main thread with a running thread within \
                 {LINGER_LIMIT:?}: {:?}",
                state(pid)
            );
            std::thread::sleep(Duration::from_millis(20));
            if !state(pid).is_some_and(|state| state.starts_with('Z')) {
                continue;
            }
            let Some(thread) = running_thread(pid) else {
                continue;
            };
            let Some(beat) = heartbeat() else { continue };
            // The count goes on after the main thread was seen gone: code still runs.
            match first_beat {
                None => first_beat = Some(beat),
                Some(first) if beat > first => break (thread, (first, beat)),
                Some(_) => {}
            }
        };
        println!(
            "confirmed: process {pid} shows State {:?} while its thread {} ({:?}) keeps \
             running (heartbeat {} -> {})",
            state(pid).unwrap_or_default(),
            thread.0,
            thread.1,
            beats.0,
            beats.1
        );

        let result = cleanup.container();
        let left = other_processes();
        println!("cleanup: {result:?}; processes left: {left:?}");
        if result.is_ok() && left.is_empty() {
            println!("PASS: no process left besides PID 1");
            ExitCode::SUCCESS
        } else {
            println!("FAIL: the cleanup left processes or failed");
            ExitCode::FAILURE
        }
    }

    /// The lingering process: leaves the session, starts a thread that counts forever, then
    /// ends only its main thread.
    #[allow(
        unsafe_code,
        reason = "setsid(2) and ending only the calling thread with exit(2) have no safe binding"
    )]
    pub(super) fn linger() -> ExitCode {
        // SAFETY: setsid(2) takes no arguments and touches no memory.
        unsafe { libc::setsid() };
        std::thread::spawn(|| {
            let mut count = 0_u64;
            loop {
                count += 1;
                let _ = std::fs::write(HEARTBEAT, count.to_string());
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        // SAFETY: SYS_exit (not exit_group) ends only the calling thread, here the main thread,
        // without unwinding; nothing in this frame needs dropping and the counting thread only
        // uses what it owns.
        unsafe { libc::syscall(libc::SYS_exit, 0) };
        unreachable!("exit(2) returned")
    }

    /// The `State:` of `/proc/<pid>/status`, without the label.
    fn state(pid: u32) -> Option<String> {
        status_state(&std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?)
    }

    fn status_state(status: &str) -> Option<String> {
        status
            .lines()
            .find_map(|line| line.strip_prefix("State:"))
            .map(|state| state.trim().to_owned())
    }

    /// A thread of `pid` other than its main thread that is not a zombie, with its state.
    fn running_thread(pid: u32) -> Option<(u32, String)> {
        std::fs::read_dir(format!("/proc/{pid}/task"))
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            .filter(|&tid| tid != pid)
            .find_map(|tid| {
                let status =
                    std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/status")).ok()?;
                let state = status_state(&status)?;
                (!state.starts_with(['Z', 'X'])).then_some((tid, state))
            })
    }

    fn heartbeat() -> Option<u64> {
        std::fs::read_to_string(HEARTBEAT).ok()?.trim().parse().ok()
    }

    /// Every process in `/proc` other than PID 1, with its state.
    fn other_processes() -> Vec<(u32, Option<String>)> {
        let mut left: Vec<_> = std::fs::read_dir("/proc")
            .expect("read /proc")
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            .filter(|&pid| pid != 1)
            .map(|pid| (pid, state(pid)))
            .collect();
        left.sort();
        left
    }
}
