//! Helpers shared by the integration tests: running `example-service` as a real process, a
//! minimal HTTP/1.1 client, and container runtimes for tests that need Postgres or PID 1.
#![allow(
    dead_code,
    reason = "each test binary includes this module and uses a different subset of it"
)]

pub mod postgres;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// The example service's binary, built by Cargo for these tests.
pub const EXAMPLE_SERVICE: &str = env!("CARGO_BIN_EXE_example-service");

/// How long the reader threads get to consume what an exited process left in its pipes. The
/// data is already in the kernel, so this only bounds scheduling delay on a loaded machine.
pub const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// A W3C trace context whose trace id the tests look for.
pub const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
pub const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

/// Lines a child process printed, shared with the threads that read them.
#[derive(Clone, Default)]
pub struct Output {
    lines: Arc<(Mutex<Vec<String>>, Condvar)>,
    /// Set once the reader thread has seen the end of the stream (or a read error).
    closed: Arc<(Mutex<bool>, Condvar)>,
}

impl Output {
    fn collect(self, stream: impl Read + Send + 'static) {
        thread::spawn(move || {
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                let (lines, changed) = &*self.lines;
                lines.lock().expect("output lock").push(line);
                changed.notify_all();
            }
            let (closed, changed) = &*self.closed;
            *closed.lock().expect("output lock") = true;
            changed.notify_all();
        });
    }

    /// Waits until the reader thread has read the stream to its end. A process that has exited
    /// can still have output in its pipe that the reader thread has not consumed yet, so read
    /// `lines`, `text` and `json_lines` for the complete output only after this returns true.
    pub fn wait_closed(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let (closed, changed) = &*self.closed;
        let mut guard = closed.lock().expect("output lock");
        while !*guard {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            guard = changed
                .wait_timeout(guard, deadline - now)
                .expect("output lock")
                .0;
        }
        true
    }

    /// Every line so far.
    pub fn lines(&self) -> Vec<String> {
        self.lines.0.lock().expect("output lock").clone()
    }

    /// Everything so far, newline-separated.
    pub fn text(&self) -> String {
        self.lines().join("\n")
    }

    /// Waits for a JSON line that satisfies `matches`.
    pub fn wait_for_json(
        &self,
        timeout: Duration,
        matches: impl Fn(&serde_json::Value) -> bool,
    ) -> Option<serde_json::Value> {
        let deadline = Instant::now() + timeout;
        let (lines, changed) = &*self.lines;
        let mut guard = lines.lock().expect("output lock");
        loop {
            if let Some(found) = guard
                .iter()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find(|value| matches(value))
            {
                return Some(found);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            guard = changed
                .wait_timeout(guard, deadline - now)
                .expect("output lock")
                .0;
        }
    }

    /// Waits for the JSON line whose `message` is `message`.
    pub fn wait_for_message(&self, message: &str, timeout: Duration) -> Option<serde_json::Value> {
        self.wait_for_json(timeout, |line| line["message"] == message)
    }

    /// Every line, parsed; panics on a line that is not one JSON object.
    pub fn json_lines(&self) -> Vec<serde_json::Value> {
        self.lines()
            .iter()
            .map(|line| {
                let value: serde_json::Value = serde_json::from_str(line)
                    .unwrap_or_else(|error| panic!("not a JSON line ({error}): {line}"));
                assert!(value.is_object(), "not a JSON object: {line}");
                value
            })
            .collect()
    }
}

/// `example-service` running as a child process.
pub struct Service {
    child: Child,
    pub stdout: Output,
    pub stderr: Output,
    pub address: SocketAddr,
}

impl Service {
    /// Starts the service on a free local port with a short drain delay, plus `env`. The
    /// environment is otherwise empty, so the developer's own variables cannot leak in.
    pub fn start(env: &[(&str, &str)]) -> Self {
        let (child, stdout, stderr) = spawn_example(env);
        let listening = stdout
            .wait_for_message("listening", Duration::from_secs(20))
            .unwrap_or_else(|| {
                panic!(
                    "the service did not start listening\nstdout:\n{}\nstderr:\n{}",
                    stdout.text(),
                    stderr.text()
                )
            });
        let address = listening["fields"]["address"]
            .as_str()
            .and_then(|address| address.parse().ok())
            .unwrap_or_else(|| panic!("no address in {listening}"));
        Self {
            child,
            stdout,
            stderr,
            address,
        }
    }

    /// The service's process id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Sends a signal by name, for example `TERM`.
    pub fn signal(&self, signal: &str) {
        send_signal(self.pid(), signal);
    }

    /// Waits for the process to exit, then for its stdout and stderr to be read completely, so
    /// the output is whole when this returns.
    pub fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let status = wait_exit(&mut self.child, timeout)?;
        for (name, output) in [("stdout", &self.stdout), ("stderr", &self.stderr)] {
            assert!(
                output.wait_closed(OUTPUT_DRAIN_TIMEOUT),
                "{name} of the exited service was not read to its end"
            );
        }
        Some(status)
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Spawns the example service without waiting for it to listen.
pub fn spawn_example(env: &[(&str, &str)]) -> (Child, Output, Output) {
    let mut command = Command::new(EXAMPLE_SERVICE);
    command
        .env_clear()
        .env("TRUSTIOT__GREETING", "hello from the tests")
        .env("TRUSTIOT__HTTP__LISTEN_ADDR", "127.0.0.1:0")
        .env("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "300ms")
        .env("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "10s")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("start example-service");
    let stdout = Output::default();
    let stderr = Output::default();
    stdout
        .clone()
        .collect(child.stdout.take().expect("piped stdout"));
    stderr
        .clone()
        .collect(child.stderr.take().expect("piped stderr"));
    (child, stdout, stderr)
}

pub fn send_signal(pid: u32, signal: &str) {
    let status = Command::new("kill")
        .args(["-s", signal, &pid.to_string()])
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -s {signal} {pid} failed");
}

pub fn wait_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("poll the child") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// A parsed HTTP response.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("body is not JSON ({error}): {}", self.body))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Sends one HTTP/1.1 request on a fresh connection and reads the whole response.
pub fn request(
    address: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> std::io::Result<Response> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;
    let mut head =
        format!("{method} {target} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(body) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if let Some(body) = body {
        stream.write_all(body.as_bytes())?;
    }
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    parse_response(&raw)
}

pub fn get(address: SocketAddr, target: &str) -> std::io::Result<Response> {
    request(address, "GET", target, &[], None)
}

fn parse_response(raw: &[u8]) -> std::io::Result<Response> {
    let invalid =
        |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_owned());
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| invalid("no end of headers"))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| invalid("no status line"))?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let chunked = headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding") && value.eq_ignore_ascii_case("chunked")
    });
    let body = if chunked {
        decode_chunked(body)
    } else {
        body.to_owned()
    };
    Ok(Response {
        status,
        headers,
        body,
    })
}

fn decode_chunked(mut rest: &str) -> String {
    let mut body = String::new();
    while let Some((size, after)) = rest.split_once("\r\n") {
        let Ok(size) = usize::from_str_radix(size.trim(), 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        let Some(chunk) = after.get(..size) else {
            break;
        };
        body.push_str(chunk);
        rest = after.get(size + 2..).unwrap_or("");
    }
    body
}

/// Fails the test in CI, where every prerequisite must be present; skips it elsewhere.
#[allow(
    clippy::print_stderr,
    reason = "tells the developer why a test did not run"
)]
pub fn unavailable(what: &str) {
    assert!(
        std::env::var_os("CI").is_none(),
        "{what} is required when CI is set"
    );
    eprintln!("skipped: {what} is not available (set CI to make this a failure)");
}

/// `podman` or `docker`, whichever works.
pub fn container_runtime() -> Option<&'static str> {
    ["podman", "docker"].into_iter().find(|runtime| {
        Command::new(runtime)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// Runs a container runtime command and returns its trimmed stdout.
pub fn runtime_output(runtime: &str, args: &[&str]) -> String {
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

/// A unique name for a container or image created by this test process.
pub fn unique_name(kind: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "service-kit-test-{kind}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// A container that is removed when dropped.
pub struct Container {
    pub runtime: &'static str,
    pub name: String,
}

impl Container {
    /// The host address mapped to `port` inside the container.
    pub fn host_address(&self, port: &str) -> SocketAddr {
        let mapping = runtime_output(self.runtime, &["port", &self.name, port]);
        mapping
            .lines()
            .find_map(|line| line.trim().parse().ok())
            .unwrap_or_else(|| panic!("no host address for {port}: {mapping}"))
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new(self.runtime)
            .args(["rm", "--force", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}
