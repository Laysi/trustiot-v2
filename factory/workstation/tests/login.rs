//! The platform administrator's login and logout, `enroll`, and `--headers-file`, run as the
//! binary against stand-ins on loopback (Laysi/trustiot-v2#614). No test contacts a real
//! environment or mail server.

mod support;

use axum::http::{Method, StatusCode};
use support::{
    Behaviour, Environment, FakeSecrets, Profile, Run, Scratch, Silence, Silent, StandIn, mode,
    pem_body_lines, run, set_mode,
};

/// Writes the `--headers-file` for `secrets` with mode 0600 and returns its path.
fn headers_file(scratch: &Scratch, secrets: &FakeSecrets) -> String {
    let path = scratch.join("access.headers");
    std::fs::write(&path, secrets.headers_file()).expect("written");
    set_mode(&path, 0o600);
    path.display().to_string()
}

/// Fails when any run's stdout or stderr contains one of `secrets`.
fn assert_nothing_leaks(runs: &[&Run], secrets: &[(&str, String)]) {
    for run in runs {
        let output = run.output();
        for (what, value) in secrets {
            assert!(
                !output.contains(value.as_str()),
                "the {what} shows in the output:\n{output}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_login_enroll_and_logout_at_trace_leak_no_secret() {
    let scratch = Scratch::new("flow");
    let environment = Environment::in_scratch(&scratch);
    let platform = StandIn::start(FakeSecrets::new(), Behaviour::default()).await;
    let secrets = platform.secrets();
    let headers = headers_file(&scratch, secrets);
    let dir = scratch.join("ws").display().to_string();
    let url = platform.url.as_str();

    let init = run(&environment, &["init", "--dir", &dir], "").await;
    assert!(init.success, "{init:?}");

    let login = run(
        &environment,
        &[
            "login",
            "--url",
            url,
            "--email",
            "admin@example.com",
            "--headers-file",
            &headers,
        ],
        &format!("{}\n", secrets.code),
    )
    .await;
    assert!(login.success, "{login:?}");
    assert!(login.stdout.contains("logged in to"), "{login:?}");
    let session = environment.session_file();
    assert_eq!(mode(&session), 0o600, "the session file is 0600");
    assert_eq!(
        mode(session.parent().expect("a directory")),
        0o700,
        "its directory is 0700"
    );
    let stored = std::fs::read_to_string(&session).expect("the session file");
    assert!(stored.contains(&secrets.cookie), "the cookie is stored");
    assert!(stored.contains(url), "with its origin");

    let enroll = run(
        &environment,
        &[
            "enroll",
            "--name",
            "line-a-ws-01",
            "--dir",
            &dir,
            "--headers-file",
            &headers,
        ],
        "",
    )
    .await;
    assert!(enroll.success, "{enroll:?}");
    assert!(
        enroll.stdout.contains("installed workstation:line-a-ws-01"),
        "{enroll:?}"
    );
    assert!(scratch.join("ws/workstation.pem").exists());

    let logout = run(&environment, &["logout", "--headers-file", &headers], "").await;
    assert!(logout.success, "{logout:?}");
    assert!(logout.stdout.contains("logged out"), "{logout:?}");
    assert!(!session.exists(), "the session file is removed");
    assert!(platform.revoked(), "the stand-in revoked the session");

    // What the stand-in received: send_code, verify_code, the issue request and the DELETE,
    // each once, each with both Access headers; the cookie on the last two only.
    let requests = platform.requests();
    let summary: Vec<(Method, String, Option<String>)> = requests
        .iter()
        .map(|request| (request.method.clone(), request.path.clone(), request.step()))
        .collect();
    assert_eq!(
        summary,
        [
            (
                Method::POST,
                "/auth/login".to_owned(),
                Some("send_code".to_owned())
            ),
            (
                Method::POST,
                "/auth/login".to_owned(),
                Some("verify_code".to_owned())
            ),
            (
                Method::POST,
                "/api/certs/v1/workstation-certificates".to_owned(),
                None
            ),
            (
                Method::DELETE,
                "/api/identity/v1/sessions/current".to_owned(),
                None
            ),
        ]
    );
    let cookie = format!("__Host-trustiot-session={}", secrets.cookie);
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(
            request.header("cf-access-client-id"),
            Some(secrets.access_id.as_str()),
            "request {index}"
        );
        assert_eq!(
            request.header("cf-access-client-secret"),
            Some(secrets.access_secret.as_str()),
            "request {index}"
        );
        assert_eq!(
            request
                .headers
                .get_all("cf-access-client-id")
                .iter()
                .count(),
            1
        );
        assert!(request.header("origin").is_none() && request.header("sec-fetch-site").is_none());
        let expected_cookie = (index >= 2).then_some(cookie.as_str());
        assert_eq!(request.header("cookie"), expected_cookie, "request {index}");
    }
    let verify: serde_json::Value = serde_json::from_slice(&requests[1].body).expect("a JSON body");
    assert_eq!(verify["code"], secrets.code.as_str());
    assert_eq!(verify["email"], "admin@example.com");

    // TRACE was on, and no output holds the code, the cookie, the private key or the
    // --headers-file values. (The key file is gone from the checks' view only if it were
    // printed; it is read here to search for it.)
    let key = std::fs::read_to_string(scratch.join("ws/workstation-key.pem")).expect("the key");
    let all = [&init, &login, &enroll, &logout];
    assert!(
        all.iter().any(|run| run.stderr.contains("TRACE")),
        "TRACE lines were written"
    );
    let mut forbidden = vec![
        ("login code", secrets.code.clone()),
        ("session cookie", secrets.cookie.clone()),
        ("Access client id", secrets.access_id.clone()),
        ("Access client secret", secrets.access_secret.clone()),
    ];
    forbidden.extend(
        pem_body_lines(&key)
            .into_iter()
            .map(|line| ("private key", line)),
    );
    assert_nothing_leaks(&all, &forbidden);

    // For the test report (`cargo test -p workstation --test login -- --nocapture`): what an
    // operator sees at TRACE, already checked to hold no secret.
    for (name, run) in ["init", "login", "enroll", "logout"].iter().zip(all) {
        println!("===== {name} (exit {:?})\n{}", run.code, run.output());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_code_can_be_entered_again_without_a_new_email() {
    let scratch = Scratch::new("retype");
    let environment = Environment::in_scratch(&scratch);
    let platform = StandIn::start(FakeSecrets::new(), Behaviour::default()).await;
    let secrets = platform.secrets();
    let wrong = if secrets.code == "11111111" {
        "22222222"
    } else {
        "11111111"
    };
    let login = run(
        &environment,
        &[
            "login",
            "--url",
            &platform.url,
            "--email",
            "admin@example.com",
        ],
        &format!("not-a-code\n{wrong}\n{}\n", secrets.code),
    )
    .await;
    assert!(login.success, "{login:?}");
    let steps: Vec<Option<String>> = platform.requests().iter().map(|r| r.step()).collect();
    assert_eq!(
        steps,
        [
            Some("send_code".to_owned()),
            Some("verify_code".to_owned()),
            Some("verify_code".to_owned())
        ],
        "one email; the malformed entry was never sent"
    );
    assert!(login.stderr.contains("8 digits"), "{login:?}");
    assert_nothing_leaks(&[&login], &[("session cookie", secrets.cookie.clone())]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_code_is_sent_once_when_the_connection_drops() {
    let scratch = Scratch::new("drop");
    let environment = Environment::in_scratch(&scratch);
    let silent = Silent::start(Silence::Drop).await;
    let login = run(
        &environment,
        &[
            "login",
            "--url",
            &silent.url,
            "--email",
            "admin@example.com",
        ],
        "",
    )
    .await;
    assert_eq!(login.code, Some(1), "{login:?}");
    assert_eq!(silent.accepted(), 1, "no second attempt");
    assert!(
        login.stderr.contains("may or may not have been sent"),
        "{login:?}"
    );
    assert!(login.stderr.contains("60 seconds"), "{login:?}");
    assert!(!environment.session_file().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_code_is_sent_once_when_the_answer_times_out() {
    let scratch = Scratch::new("timeout");
    let environment = Environment::in_scratch(&scratch);
    let silent = Silent::start(Silence::Hold).await;
    // The tool's own timeout, set by the test; only the number of attempts is checked.
    let login = run(
        &environment,
        &[
            "login",
            "--url",
            &silent.url,
            "--email",
            "admin@example.com",
            "--timeout",
            "2",
        ],
        "",
    )
    .await;
    assert_eq!(login.code, Some(1), "{login:?}");
    assert_eq!(silent.accepted(), 1, "no second attempt");
    assert!(login.stderr.contains("no complete response"), "{login:?}");
    assert!(
        login.stderr.contains("may or may not have been sent"),
        "{login:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_code_is_sent_once_when_the_service_is_unavailable() {
    let scratch = Scratch::new("unavailable");
    let environment = Environment::in_scratch(&scratch);
    let platform = StandIn::start(
        FakeSecrets::new(),
        Behaviour {
            send_code_status: StatusCode::SERVICE_UNAVAILABLE,
            ..Behaviour::default()
        },
    )
    .await;
    let login = run(
        &environment,
        &[
            "login",
            "--url",
            &platform.url,
            "--email",
            "admin@example.com",
        ],
        "",
    )
    .await;
    assert_eq!(login.code, Some(1), "{login:?}");
    assert_eq!(platform.requests().len(), 1, "no second attempt");
    assert!(
        login.stderr.contains("503 unavailable (error.unavailable)"),
        "{login:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accepted_send_code_with_an_unreadable_answer_still_asks_for_the_code() {
    let scratch = Scratch::new("unreadable");
    let environment = Environment::in_scratch(&scratch);
    let platform = StandIn::start(
        FakeSecrets::new(),
        Behaviour {
            unreadable_send_code_answer: true,
            ..Behaviour::default()
        },
    )
    .await;
    let secrets = platform.secrets();
    let login = run(
        &environment,
        &[
            "login",
            "--url",
            &platform.url,
            "--email",
            "admin@example.com",
        ],
        &format!("{}\n", secrets.code),
    )
    .await;
    assert!(login.success, "{login:?}");
    let steps: Vec<Option<String>> = platform.requests().iter().map(|r| r.step()).collect();
    assert_eq!(
        steps,
        [Some("send_code".to_owned()), Some("verify_code".to_owned())],
        "one email, and the code it carried was used"
    );
    assert!(
        login
            .stderr
            .contains("accepted the request but its answer could not be read"),
        "{login:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_that_cannot_be_stored_is_revoked() {
    let scratch = Scratch::new("unstored");
    let environment = Environment::in_scratch(&scratch);
    let directory = environment
        .runtime
        .as_ref()
        .expect("a runtime directory")
        .join("trustiot-workstation");
    // Root ignores the mode, so the session could be stored after all: nothing to test then.
    let probe = scratch.join("probe");
    std::fs::create_dir(&probe).expect("a probe directory");
    set_mode(&probe, 0o500);
    let privileged = std::fs::write(probe.join("file"), "x").is_ok();
    set_mode(&probe, 0o700);
    if privileged {
        eprintln!("skipped: running as a user whom directory modes do not restrict");
        return;
    }
    let platform = StandIn::start(
        FakeSecrets::new(),
        Behaviour {
            lock_on_verify: Some(directory.clone()),
            ..Behaviour::default()
        },
    )
    .await;
    let secrets = platform.secrets();
    let login = run(
        &environment,
        &[
            "login",
            "--url",
            &platform.url,
            "--email",
            "admin@example.com",
        ],
        &format!("{}\n", secrets.code),
    )
    .await;
    set_mode(&directory, 0o700);
    assert_eq!(login.code, Some(1), "{login:?}");
    assert!(
        login.stderr.contains("the new session was revoked"),
        "{login:?}"
    );
    let calls: Vec<(String, String)> = platform
        .requests()
        .iter()
        .map(|r| (r.method.as_str().to_owned(), r.path.clone()))
        .collect();
    assert_eq!(
        calls.last(),
        Some(&(
            "DELETE".to_owned(),
            "/api/identity/v1/sessions/current".to_owned()
        )),
        "{calls:?}"
    );
    assert!(platform.revoked());
    assert!(!directory.join("session").exists());
    assert_nothing_leaks(&[&login], &[("session cookie", secrets.cookie.clone())]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_refuses_to_start_without_a_runtime_directory() {
    let scratch = Scratch::new("no-runtime");
    let mut environment = Environment::in_scratch(&scratch);
    environment.runtime = None;
    let platform = StandIn::start(FakeSecrets::new(), Behaviour::default()).await;
    let login = run(
        &environment,
        &[
            "login",
            "--url",
            &platform.url,
            "--email",
            "admin@example.com",
        ],
        "",
    )
    .await;
    assert_eq!(login.code, Some(1), "{login:?}");
    assert!(login.stderr.contains("XDG_RUNTIME_DIR"), "{login:?}");
    assert!(platform.requests().is_empty(), "no code was requested");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_file_wider_than_0600_is_refused() {
    let scratch = Scratch::new("session-mode");
    let environment = Environment::in_scratch(&scratch);
    let platform = StandIn::start(FakeSecrets::new(), Behaviour::default()).await;
    let secrets = platform.secrets();
    let login = run(
        &environment,
        &[
            "login",
            "--url",
            &platform.url,
            "--email",
            "admin@example.com",
        ],
        &format!("{}\n", secrets.code),
    )
    .await;
    assert!(login.success, "{login:?}");
    let session = environment.session_file();
    let before = platform.requests().len();
    for wider in [0o640, 0o644, 0o604] {
        set_mode(&session, wider);
        for arguments in [&["logout"][..], &["enroll", "--name", "ws-1"]] {
            let refused = run(&environment, arguments, "").await;
            assert_eq!(refused.code, Some(1), "{refused:?}");
            assert!(
                refused.stderr.contains(&session.display().to_string()),
                "{refused:?}"
            );
            assert!(refused.stderr.contains("0600"), "{refused:?}");
            assert_nothing_leaks(&[&refused], &[("session cookie", secrets.cookie.clone())]);
        }
    }
    assert_eq!(
        platform.requests().len(),
        before,
        "the cookie was never sent"
    );
    assert!(session.exists(), "the file is left for the operator");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_of_an_ended_session_removes_the_file() {
    let scratch = Scratch::new("ended");
    let environment = Environment::in_scratch(&scratch);
    let platform = StandIn::start(FakeSecrets::new(), Behaviour::default()).await;
    let secrets = platform.secrets();
    let login = run(
        &environment,
        &[
            "login",
            "--url",
            &platform.url,
            "--email",
            "admin@example.com",
        ],
        &format!("{}\n", secrets.code),
    )
    .await;
    assert!(login.success, "{login:?}");
    let first = run(&environment, &["logout"], "").await;
    assert!(first.success, "{first:?}");
    // Put the (now revoked) session back: the stand-in answers 401.
    let session = environment.session_file();
    std::fs::write(
        &session,
        format!("origin={}\ncookie={}\n", platform.url, secrets.cookie),
    )
    .expect("written");
    set_mode(&session, 0o600);
    let second = run(&environment, &["logout"], "").await;
    assert!(second.success, "{second:?}");
    assert!(second.stdout.contains("already ended"), "{second:?}");
    assert!(!session.exists());
    let none = run(&environment, &["logout"], "").await;
    assert!(
        none.success && none.stdout.contains("nothing to log out"),
        "{none:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_session_is_only_sent_to_its_own_origin() {
    let scratch = Scratch::new("origin");
    let environment = Environment::in_scratch(&scratch);
    let platform = StandIn::start(FakeSecrets::new(), Behaviour::default()).await;
    let other = StandIn::start(FakeSecrets::new(), Behaviour::default()).await;
    let login = run(
        &environment,
        &[
            "login",
            "--url",
            &platform.url,
            "--email",
            "admin@example.com",
        ],
        &format!("{}\n", platform.secrets().code),
    )
    .await;
    assert!(login.success, "{login:?}");
    let refused = run(&environment, &["logout", "--url", &other.url], "").await;
    assert_eq!(refused.code, Some(1), "{refused:?}");
    assert!(refused.stderr.contains("belongs to"), "{refused:?}");

    // A login to another platform does not replace the stored session, which would leave it
    // active and unreachable: it is refused before a code is mailed.
    let elsewhere = run(
        &environment,
        &["login", "--url", &other.url, "--email", "admin@example.com"],
        &format!("{}\n", other.secrets().code),
    )
    .await;
    assert_eq!(elsewhere.code, Some(1), "{elsewhere:?}");
    assert!(
        elsewhere.stderr.contains("run logout first"),
        "{elsewhere:?}"
    );
    assert!(other.requests().is_empty());
    assert_eq!(
        platform.requests().len(),
        2,
        "send_code and verify_code only"
    );

    // After logout, the other platform can be logged in to.
    let logout = run(&environment, &["logout"], "").await;
    assert!(logout.success, "{logout:?}");
    let elsewhere = run(
        &environment,
        &["login", "--url", &other.url, "--email", "admin@example.com"],
        &format!("{}\n", other.secrets().code),
    )
    .await;
    assert!(elsewhere.success, "{elsewhere:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_headers_file_is_checked_before_any_request() {
    let scratch = Scratch::new("headers");
    let environment = Environment::in_scratch(&scratch);
    let platform = StandIn::start(FakeSecrets::new(), Behaviour::default()).await;
    let secrets = platform.secrets();
    let path = scratch.join("access.headers");
    let path_text = path.display().to_string();
    let arguments = [
        "login",
        "--url",
        &platform.url,
        "--email",
        "admin@example.com",
        "--headers-file",
        &path_text,
    ];

    // Another header name.
    std::fs::write(
        &path,
        format!(
            "{}authorization: Bearer {}\n",
            secrets.headers_file(),
            secrets.access_secret
        ),
    )
    .expect("written");
    set_mode(&path, 0o600);
    let refused = run(&environment, &arguments, "").await;
    assert_eq!(refused.code, Some(1), "{refused:?}");
    assert!(
        refused.stderr.contains("only cf-access-client-id"),
        "{refused:?}"
    );

    // The right names, but readable by others.
    std::fs::write(&path, secrets.headers_file()).expect("written");
    set_mode(&path, 0o644);
    let refused_mode = run(&environment, &arguments, "").await;
    assert_eq!(refused_mode.code, Some(1), "{refused_mode:?}");
    assert!(refused_mode.stderr.contains(&path_text), "{refused_mode:?}");

    assert!(platform.requests().is_empty(), "nothing was sent");
    assert_nothing_leaks(
        &[&refused, &refused_mode],
        &[
            ("Access client id", secrets.access_id.clone()),
            ("Access client secret", secrets.access_secret.clone()),
        ],
    );
}

/// Runs `init` into `<scratch>/ws` and logs in to `platform`; returns the `--dir` value.
async fn init_and_login(
    scratch: &Scratch,
    environment: &Environment,
    platform: &StandIn,
) -> String {
    let dir = scratch.join("ws").display().to_string();
    assert!(run(environment, &["init", "--dir", &dir], "").await.success);
    let login = run(
        environment,
        &[
            "login",
            "--url",
            &platform.url,
            "--email",
            "admin@example.com",
        ],
        &format!("{}\n", platform.secrets().code),
    )
    .await;
    assert!(login.success, "{login:?}");
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_session_is_revoked_before_its_file_is_removed() {
    let scratch = Scratch::new("enroll-rejected");
    let environment = Environment::in_scratch(&scratch);
    let platform = StandIn::start(
        FakeSecrets::new(),
        Behaviour {
            issue_rejects_session: true,
            ..Behaviour::default()
        },
    )
    .await;
    let dir = init_and_login(&scratch, &environment, &platform).await;
    let enroll = run(
        &environment,
        &["enroll", "--name", "ws-1", "--dir", &dir],
        "",
    )
    .await;
    assert_eq!(enroll.code, Some(1), "{enroll:?}");
    let calls: Vec<String> = platform
        .requests()
        .iter()
        .map(|r| format!("{} {}", r.method, r.path))
        .collect();
    assert_eq!(
        calls.last().map(String::as_str),
        Some("DELETE /api/identity/v1/sessions/current"),
        "{calls:?}"
    );
    assert!(platform.revoked(), "the live session was revoked first");
    assert!(!environment.session_file().exists(), "{enroll:?}");
    assert!(enroll.stderr.contains("log in again"), "{enroll:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_proxys_401_keeps_the_session_file() {
    let scratch = Scratch::new("enroll-proxy");
    let environment = Environment::in_scratch(&scratch);
    let platform = StandIn::start(
        FakeSecrets::new(),
        Behaviour {
            proxy_401_on: Some("/api/certs/v1/workstation-certificates"),
            ..Behaviour::default()
        },
    )
    .await;
    let dir = init_and_login(&scratch, &environment, &platform).await;
    let enroll = run(
        &environment,
        &["enroll", "--name", "ws-1", "--dir", &dir],
        "",
    )
    .await;
    assert_eq!(enroll.code, Some(1), "{enroll:?}");
    assert!(
        enroll.stderr.contains("without an M1 error body"),
        "{enroll:?}"
    );
    assert!(
        environment.session_file().exists(),
        "the only copy of a live cookie is kept"
    );
    assert!(!platform.revoked());
    assert!(
        platform
            .requests()
            .iter()
            .all(|r| r.method != Method::DELETE),
        "a proxy's 401 says nothing about the session"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_session_that_cannot_be_revoked_keeps_its_file() {
    let scratch = Scratch::new("enroll-unrevoked");
    let environment = Environment::in_scratch(&scratch);
    let platform = StandIn::start(
        FakeSecrets::new(),
        Behaviour {
            issue_rejects_session: true,
            proxy_401_on: Some("/api/identity/v1/sessions/current"),
            ..Behaviour::default()
        },
    )
    .await;
    let dir = init_and_login(&scratch, &environment, &platform).await;
    let enroll = run(
        &environment,
        &["enroll", "--name", "ws-1", "--dir", &dir],
        "",
    )
    .await;
    assert_eq!(enroll.code, Some(1), "{enroll:?}");
    assert!(enroll.stderr.contains("run logout"), "{enroll:?}");
    assert!(environment.session_file().exists(), "{enroll:?}");

    // logout itself keeps the file on a proxy's 401 too.
    let logout = run(&environment, &["logout"], "").await;
    assert_eq!(logout.code, Some(1), "{logout:?}");
    assert!(environment.session_file().exists(), "{logout:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_whose_serial_is_not_the_certificates_is_refused() {
    for (case, serial) in [
        ("another serial", "0123456789abcdef".to_owned()),
        ("terminal escape", "\u{1b}[2J4f3a".to_owned()),
        ("upper case", "4F3A9C1E2B7D6058A1C3E5F708192A3B".to_owned()),
    ] {
        let scratch = Scratch::new("enroll-serial");
        let environment = Environment::in_scratch(&scratch);
        let platform = StandIn::start(
            FakeSecrets::new(),
            Behaviour {
                issued_serial: Some(serial.clone()),
                ..Behaviour::default()
            },
        )
        .await;
        let dir = init_and_login(&scratch, &environment, &platform).await;
        let enroll = run(
            &environment,
            &["enroll", "--name", "ws-1", "--dir", &dir],
            "",
        )
        .await;
        assert_eq!(enroll.code, Some(1), "{case}: {enroll:?}");
        assert!(
            !scratch.join("ws/workstation.pem").exists(),
            "{case}: nothing is installed"
        );
        assert!(
            !enroll.output().chars().any(|c| c.is_control() && c != '\n'),
            "{case}: {enroll:?}"
        );
        if case == "another serial" {
            assert!(
                enroll.stderr.contains("revoke that serial")
                    && enroll
                        .stderr
                        .contains(&Profile::workstation("ws-1").serial_hex()),
                "{case}: the certificate's own serial is named: {enroll:?}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_after_the_session_ended_removes_the_session_file() {
    let scratch = Scratch::new("enroll-401");
    let environment = Environment::in_scratch(&scratch);
    let platform = StandIn::start(FakeSecrets::new(), Behaviour::default()).await;
    let secrets = platform.secrets();
    let dir = scratch.join("ws").display().to_string();
    assert!(
        run(&environment, &["init", "--dir", &dir], "")
            .await
            .success
    );
    let login = run(
        &environment,
        &[
            "login",
            "--url",
            &platform.url,
            "--email",
            "admin@example.com",
        ],
        &format!("{}\n", secrets.code),
    )
    .await;
    assert!(login.success, "{login:?}");
    // Revoke it behind the tool's back, as an expiry would.
    let session = environment.session_file();
    let stored = std::fs::read_to_string(&session).expect("the session");
    assert!(run(&environment, &["logout"], "").await.success);
    std::fs::write(&session, stored).expect("restored");
    set_mode(&session, 0o600);

    let enroll = run(
        &environment,
        &["enroll", "--name", "ws-1", "--dir", &dir],
        "",
    )
    .await;
    assert_eq!(enroll.code, Some(1), "{enroll:?}");
    assert!(enroll.stderr.contains("log in again"), "{enroll:?}");
    assert!(!session.exists(), "the ended session is forgotten");
    assert!(!scratch.join("ws/workstation.pem").exists());
}
