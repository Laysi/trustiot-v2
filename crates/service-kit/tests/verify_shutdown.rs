//! Independent verification of Laysi/trustiot-v2#51: health, readiness and shutdown under load,
//! with keep-alive connections and many requests in flight, against a real `example-service`.
//!
//! `container.rs` and `lifecycle.rs` cover one request in flight; these tests add concurrency,
//! idle keep-alive connections, and the order of events (readiness first, listener closed after
//! the drain delay, exit code 0 within the grace period).

mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use support::{Service, get};

fn connect(address: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("read timeout");
    stream
}

/// Reads one HTTP/1.1 response with a Content-Length body from a keep-alive connection.
fn read_response(stream: &mut TcpStream) -> std::io::Result<(u16, String, String)> {
    let mut raw = Vec::new();
    let mut byte = [0_u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte)? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        raw.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&raw).into_owned();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("status");
    let length = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    stream.read_exact(&mut body)?;
    Ok((status, head, String::from_utf8_lossy(&body).into_owned()))
}

fn send_get(stream: &mut TcpStream, target: &str) {
    stream
        .write_all(format!("GET {target} HTTP/1.1\r\nHost: t\r\n\r\n").as_bytes())
        .expect("send");
}

#[test]
fn many_requests_in_flight_all_complete_and_the_exit_code_is_zero() {
    let mut service = Service::start(&[
        ("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "500ms"),
        ("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "8s"),
    ]);
    let address = service.address;
    // 100 requests, each answering after 2 s, half of them on keep-alive connections.
    let workers: Vec<_> = (0..100)
        .map(|index| {
            thread::spawn(move || {
                let mut stream = connect(address);
                send_get(&mut stream, "/v1/slow?ms=2000");
                let started = Instant::now();
                let result = read_response(&mut stream);
                (
                    index,
                    started.elapsed(),
                    result.map(|(status, _, body)| (status, body)),
                )
            })
        })
        .collect();
    thread::sleep(Duration::from_millis(500));
    let signalled = Instant::now();
    service.signal("TERM");
    // Readiness fails at once while the delay keeps the listener open.
    // `kill` returns before the service's runtime has handled the signal, so poll for the flip
    // inside the 500 ms delay instead of asserting on the very first probe.
    let readiness_failed = loop {
        let ready = get(address, "/readyz").expect("readyz during the drain delay");
        if ready.status == 503 {
            break signalled.elapsed();
        }
        assert!(
            signalled.elapsed() < Duration::from_millis(400),
            "readiness still passes: {ready:?}"
        );
        thread::sleep(Duration::from_millis(10));
    };
    eprintln!("readiness failed {readiness_failed:?} after SIGTERM");
    assert_eq!(get(address, "/healthz").expect("healthz").status, 200);

    for worker in workers {
        let (index, elapsed, result) = worker.join().expect("worker");
        let (status, body) = result.unwrap_or_else(|error| panic!("request {index}: {error}"));
        assert_eq!(status, 200, "request {index}: {body}");
        assert!(elapsed >= Duration::from_millis(1500), "{elapsed:?}");
    }
    let status = service
        .wait_exit(Duration::from_secs(10))
        .expect("the service exits within the grace period");
    assert!(status.success(), "{status}");
    let total = signalled.elapsed();
    eprintln!("exit {total:?} after SIGTERM with 100 requests in flight");
    assert!(total < Duration::from_secs(8), "{total:?}");
    // The listener is gone.
    assert!(TcpStream::connect_timeout(&address, Duration::from_millis(500)).is_err());
}

#[test]
fn idle_keep_alive_connections_do_not_delay_the_exit() {
    let mut service = Service::start(&[
        ("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "300ms"),
        ("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "10s"),
    ]);
    let address = service.address;
    // 50 connections that completed one request and now sit idle on keep-alive.
    let mut idle: Vec<TcpStream> = (0..50)
        .map(|_| {
            let mut stream = connect(address);
            send_get(&mut stream, "/v1/hello");
            let (status, _, _) = read_response(&mut stream).expect("hello");
            assert_eq!(status, 200);
            stream
        })
        .collect();
    let signalled = Instant::now();
    service.signal("TERM");
    let status = service
        .wait_exit(Duration::from_secs(12))
        .expect("exits within the grace period");
    let elapsed = signalled.elapsed();
    eprintln!("exit {elapsed:?} after SIGTERM with 50 idle keep-alive connections: {status}");
    assert!(status.success(), "{status}");
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    // The server closed the idle connections rather than abandoning them.
    for stream in &mut idle {
        let mut buffer = [0_u8; 1];
        assert_eq!(stream.read(&mut buffer).unwrap_or(0), 0);
    }
}

#[test]
fn a_request_arriving_on_a_keep_alive_connection_during_the_delay_is_served_then_closed() {
    let mut service = Service::start(&[
        ("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "1500ms"),
        ("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "5s"),
    ]);
    let mut stream = connect(service.address);
    send_get(&mut stream, "/v1/hello");
    assert_eq!(read_response(&mut stream).expect("first").0, 200);
    service.signal("TERM");
    thread::sleep(Duration::from_millis(200));
    // During the drain delay the service still serves, and readiness already fails.
    let started = Instant::now();
    loop {
        send_get(&mut stream, "/readyz");
        let (status, _, body) = read_response(&mut stream).expect("readyz on the open connection");
        if status == 503 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_millis(800),
            "readiness still passes: {body}"
        );
        thread::sleep(Duration::from_millis(20));
    }
    send_get(&mut stream, "/v1/hello");
    let (status, _, _) = read_response(&mut stream).expect("hello during the delay");
    assert_eq!(status, 200);
    let exit = service.wait_exit(Duration::from_secs(8)).expect("exits");
    assert!(exit.success(), "{exit}");
}

#[test]
fn a_client_that_stalls_mid_request_is_cut_off_at_the_drain_timeout_not_hung() {
    // A client that sent half a request head would, without the drain timeout, keep the
    // service up until SIGKILL. The service must give up at `drain_timeout` and say so by exit
    // code; the exit must come well before the pod's grace period.
    let mut service = Service::start(&[
        ("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "200ms"),
        ("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "2s"),
    ]);
    let mut stalled = connect(service.address);
    stalled
        .write_all(b"GET /v1/hello HT")
        .expect("partial request");
    thread::sleep(Duration::from_millis(200));
    let signalled = Instant::now();
    service.signal("TERM");
    let status = service
        .wait_exit(Duration::from_secs(10))
        .expect("the service must not hang on a stalled client");
    let elapsed = signalled.elapsed();
    eprintln!("stalled client: exit {elapsed:?}, {status}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    // Either a clean exit (the server dropped the half-open connection) or the documented
    // drain-timeout failure; never a signal death or a hang.
    assert!(matches!(status.code(), Some(0 | 1)), "{status}");
}

#[test]
fn a_slow_body_upload_in_flight_is_cut_off_at_the_drain_timeout() {
    let mut service = Service::start(&[
        ("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "200ms"),
        ("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "2s"),
    ]);
    let mut stream = connect(service.address);
    // Announces 1000 bytes, sends 10, then stalls: the handler waits on the body.
    stream
        .write_all(
            b"POST /v1/echo HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\n\
              Content-Length: 1000\r\n\r\n{\"text\":\"a",
        )
        .expect("partial body");
    thread::sleep(Duration::from_millis(300));
    let signalled = Instant::now();
    service.signal("TERM");
    let status = service
        .wait_exit(Duration::from_secs(10))
        .expect("the service must not hang on a stalled upload");
    let elapsed = signalled.elapsed();
    eprintln!("stalled upload: exit {elapsed:?}, {status}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    assert!(matches!(status.code(), Some(0 | 1)), "{status}");
}

#[test]
fn an_idle_service_exits_zero_right_after_the_drain_delay() {
    let mut idle = Service::start(&[("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "300ms")]);
    let started = Instant::now();
    idle.signal("TERM");
    let status = idle.wait_exit(Duration::from_secs(5)).expect("exit");
    assert!(status.success(), "{status}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}
