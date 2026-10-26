//! What the browser reports during one check, kept as it arrives on the protocol's reading task:
//! the frames and execution contexts, the host page's reports, the page's exceptions,
//! navigations and requests. Request interception is answered here, at once: every request gets
//! a file of the site or fails (`frontend/sandbox/README.md`, 「檢查服務的流程」 step 3).

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use contract::api::dashboard::RuntimeVersion;
use contract::api::dashboard::check::MAX_SCRIPT_MESSAGE_CHARS;
use contract::api::dashboard::sandbox::{CHECK_HOST_PATH, SandboxFile};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::cdp::{Cdp, EventSink};
use crate::site::{self, FileKind, ResponseHeader, Site};

/// The binding the host page reports through (`frontend/sandbox/README.md`, 「檢查服務的流程」
/// step 4): `tqCheckReport(JSON.stringify(report))`.
pub const REPORT_BINDING: &str = "tqCheckReport";

/// The name of the checker's isolated world in the sandbox frame.
pub const WORLD_NAME: &str = "tq-check";

/// The most requests a page may make before the rest are left unanswered: the first already
/// fails the check.
pub const MAX_ANSWERED_REQUESTS: usize = 1_000;

/// The most exceptions kept; one is enough to fail the check.
pub const MAX_EXCEPTIONS: usize = 32;

/// The largest report the host page may send; its only free text is a message of at most 200
/// characters.
pub const MAX_REPORT_BYTES: usize = 4096;

/// A report of the host page.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Report {
    /// The sandbox's runtime loaded; the host sent `init` and `data`.
    Ready,
    /// The runtime's render-finished rule held.
    Rendered,
    /// A page script or `render` threw.
    Error {
        #[serde(default)]
        message: Option<String>,
    },
}

/// What happened in the browser so far.
#[derive(Debug, Default)]
pub struct State {
    pub session: Option<String>,
    pub target: Option<String>,
    pub main_frame: Option<String>,
    pub sandbox_frame: Option<String>,
    /// The sandbox frame and every frame inside it.
    pub page_frames: HashSet<String>,
    host_document_served: bool,
    host_script_served: bool,
    sandbox_document_served: bool,
    sandbox_navigation_requested: bool,
    sandbox_navigated: bool,
    host_navigated: bool,
    pub host_context: Option<i64>,
    own_worlds: HashSet<i64>,
    pub loaded: bool,
    pub ready: Option<Instant>,
    pub rendered: bool,
    /// The runtime's error report, with its message.
    pub page_error: Option<Option<String>>,
    pub crashed: bool,
    pub closed: bool,
    pub navigated: bool,
    pub requested: bool,
    /// The host page (trusted code) misbehaved: it threw, sent an invalid report, or the
    /// browser did something only the host page could cause.
    pub host_failed: bool,
    /// The page's uncaught exceptions: id and text, cut by [`clip_message`].
    pub exceptions: Vec<(i64, Option<String>)>,
    answered: usize,
}

impl State {
    /// The check cannot go on: the browser or the page's renderer is gone, or the page left.
    #[must_use]
    pub fn ended(&self) -> bool {
        self.closed || self.crashed || self.navigated
    }
}

/// The observer of one check.
#[derive(Debug)]
pub struct Observer {
    runtime: RuntimeVersion,
    site: Arc<Site>,
    state: Mutex<State>,
    changes: watch::Sender<u64>,
}

impl Observer {
    #[must_use]
    pub fn new(runtime: RuntimeVersion, site: Arc<Site>) -> Self {
        Self {
            runtime,
            site,
            state: Mutex::new(State::default()),
            changes: watch::Sender::new(0),
        }
    }

    /// The state, locked.
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Changes the state and wakes the waiters.
    pub fn update(&self, change: impl FnOnce(&mut State)) {
        change(&mut self.state());
        self.changes
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// Waits until `done` returns something, the browser ends, or `deadline` passes; `None` in
    /// the last two cases.
    pub async fn wait_until<T>(
        &self,
        deadline: Instant,
        mut done: impl FnMut(&State) -> Option<T>,
    ) -> Option<T> {
        let mut changes = self.changes.subscribe();
        loop {
            changes.borrow_and_update();
            {
                let state = self.state();
                if let Some(value) = done(&state) {
                    return Some(value);
                }
                if state.ended() {
                    return None;
                }
            }
            match tokio::time::timeout_at(deadline, changes.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => return None,
            }
        }
    }

    fn expected_sandbox_url(&self) -> String {
        site::url(&SandboxFile::Document.path(self.runtime))
    }

    /// Decides a paused request: a file of the site, or a failure and the problem it means.
    fn intercept(&self, state: &mut State, params: &Value) -> Interception {
        let url = str_at(params, &["request", "url"]).unwrap_or_default();
        let method = str_at(params, &["request", "method"]).unwrap_or_default();
        let frame = str_at(params, &["frameId"]).unwrap_or_default();
        let document = str_at(params, &["resourceType"]) == Some("Document");
        let path = site::path_of(url);
        let served = path.and_then(|path| self.site.get(path));
        let is_main = state.main_frame.as_deref() == Some(frame);
        let is_sandbox = state.sandbox_frame.as_deref() == Some(frame);
        let in_page = state.page_frames.contains(frame);
        let refuse = |state: &mut State| {
            if document {
                state.navigated = true;
            } else if in_page || !is_main {
                state.requested = true;
            } else {
                // The host page asked for something it does not have.
                state.host_failed = true;
            }
            Interception::Fail
        };
        let Some(served) = served.filter(|_| method == "GET") else {
            return refuse(state);
        };
        match served.kind {
            FileKind::HostPage if is_main && document && !state.host_document_served => {
                state.host_document_served = true;
                Interception::Serve(path.unwrap_or_default().to_owned())
            }
            FileKind::HostScript if is_main && !document && !state.host_script_served => {
                state.host_script_served = true;
                Interception::Serve(path.unwrap_or_default().to_owned())
            }
            FileKind::Sandbox(runtime, SandboxFile::Document)
                if runtime == self.runtime
                    && is_sandbox
                    && document
                    && !state.sandbox_document_served =>
            {
                state.sandbox_document_served = true;
                Interception::Serve(path.unwrap_or_default().to_owned())
            }
            FileKind::Sandbox(runtime, file)
                if runtime == self.runtime
                    && file != SandboxFile::Document
                    && in_page
                    && !document =>
            {
                Interception::Serve(path.unwrap_or_default().to_owned())
            }
            _ => refuse(state),
        }
    }

    fn on_frame_attached(&self, state: &mut State, params: &Value) {
        let (Some(frame), Some(parent)) = (
            str_at(params, &["frameId"]),
            str_at(params, &["parentFrameId"]),
        ) else {
            return;
        };
        if state.main_frame.as_deref() == Some(parent) {
            if state.sandbox_frame.is_none() {
                state.sandbox_frame = Some(frame.to_owned());
                state.page_frames.insert(frame.to_owned());
            } else if state.sandbox_frame.as_deref() != Some(frame) {
                // The host page makes exactly one frame; the sandbox cannot add frames to it.
                state.host_failed = true;
            }
        } else if state.page_frames.contains(parent) {
            state.page_frames.insert(frame.to_owned());
        }
    }

    fn on_navigation_requested(&self, state: &mut State, params: &Value) {
        let frame = str_at(params, &["frameId"]).unwrap_or_default();
        let url = str_at(params, &["url"]).unwrap_or_default();
        if state.sandbox_frame.as_deref() == Some(frame)
            && !state.sandbox_navigation_requested
            && url == self.expected_sandbox_url()
        {
            state.sandbox_navigation_requested = true;
        } else if state.page_frames.contains(frame)
            || (state.main_frame.as_deref() == Some(frame) && state.host_navigated)
        {
            state.navigated = true;
        }
    }

    fn on_navigated(&self, state: &mut State, params: &Value) {
        let frame = str_at(params, &["frame", "id"]).unwrap_or_default();
        let url = str_at(params, &["frame", "url"]).unwrap_or_default();
        if state.sandbox_frame.as_deref() == Some(frame) {
            if !state.sandbox_navigated && url == self.expected_sandbox_url() {
                state.sandbox_navigated = true;
            } else {
                state.navigated = true;
            }
        } else if state.page_frames.contains(frame) {
            state.navigated = true;
        } else if state.main_frame.as_deref() == Some(frame) && url != "about:blank" {
            if !state.host_navigated && url == site::url(CHECK_HOST_PATH) {
                state.host_navigated = true;
            } else {
                state.navigated = true;
            }
        }
    }

    fn on_context(&self, state: &mut State, params: &Value) {
        let Some(context) = params.get("context") else {
            return;
        };
        let Some(id) = context.get("id").and_then(Value::as_i64) else {
            return;
        };
        let frame = str_at(context, &["auxData", "frameId"]).unwrap_or_default();
        let default = context
            .pointer("/auxData/isDefault")
            .and_then(Value::as_bool)
            == Some(true);
        let isolated = str_at(context, &["auxData", "type"]) == Some("isolated");
        if isolated && str_at(context, &["name"]) == Some(WORLD_NAME) {
            state.own_worlds.insert(id);
        } else if default
            && state.main_frame.as_deref() == Some(frame)
            && str_at(context, &["origin"]) == Some(site::ORIGIN)
        {
            state.host_context = Some(id);
        }
    }

    fn on_exception(state: &mut State, params: &Value) {
        let Some(details) = params.get("exceptionDetails") else {
            return;
        };
        let context = details.get("executionContextId").and_then(Value::as_i64);
        if context.is_some_and(|id| state.own_worlds.contains(&id)) {
            // The checker's own world: the comparison's call reports its failure itself.
            return;
        }
        if context.is_some() && context == state.host_context {
            state.host_failed = true;
            return;
        }
        if state.exceptions.len() < MAX_EXCEPTIONS {
            let id = details
                .get("exceptionId")
                .and_then(Value::as_i64)
                .unwrap_or(-1);
            state.exceptions.push((id, exception_text(details)));
        }
    }

    fn on_binding(state: &mut State, params: &Value) {
        if str_at(params, &["name"]) != Some(REPORT_BINDING) {
            return;
        }
        let context = params.get("executionContextId").and_then(Value::as_i64);
        if context.is_none() || context != state.host_context {
            // Only the host page's own world may report; the binding is visible in the
            // sandbox's world too, and what the page sends there means nothing.
            return;
        }
        let payload = str_at(params, &["payload"]).unwrap_or_default();
        let report = (payload.len() <= MAX_REPORT_BYTES)
            .then(|| serde_json::from_str::<Report>(payload).ok())
            .flatten();
        match report {
            Some(Report::Ready) if state.ready.is_none() => state.ready = Some(Instant::now()),
            Some(Report::Rendered) if state.ready.is_some() => state.rendered = true,
            Some(Report::Error { message }) => {
                if state.page_error.is_none() {
                    state.page_error = Some(message);
                }
            }
            _ => state.host_failed = true,
        }
    }
}

enum Interception {
    Serve(String),
    Fail,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Fulfill<'a> {
    request_id: &'a str,
    response_code: u16,
    response_headers: &'a [ResponseHeader],
    body: &'a str,
}

impl EventSink for Observer {
    fn event(&self, cdp: &Cdp, session: Option<&str>, method: &str, params: Value) {
        let mut answer = None;
        {
            let mut state = self.state();
            let own_session = session.is_some() && session == state.session.as_deref();
            match method {
                "Fetch.requestPaused" if own_session => {
                    if state.answered < MAX_ANSWERED_REQUESTS {
                        state.answered += 1;
                        let id = str_at(&params, &["requestId"])
                            .unwrap_or_default()
                            .to_owned();
                        answer = Some((id, self.intercept(&mut state, &params)));
                    } else {
                        state.requested = true;
                    }
                }
                "Page.frameAttached" if own_session => self.on_frame_attached(&mut state, &params),
                "Page.frameRequestedNavigation" if own_session => {
                    self.on_navigation_requested(&mut state, &params);
                }
                "Page.frameNavigated" if own_session => self.on_navigated(&mut state, &params),
                "Page.navigatedWithinDocument" if own_session => {
                    let frame = str_at(&params, &["frameId"]).unwrap_or_default();
                    if state.page_frames.contains(frame) {
                        state.navigated = true;
                    }
                }
                "Page.windowOpen" => state.navigated = true,
                "Page.loadEventFired" if own_session => state.loaded = true,
                "Page.javascriptDialogOpening" if own_session => {
                    cdp.notify(
                        session,
                        "Page.handleJavaScriptDialog",
                        &json!({"accept": false}),
                    );
                }
                "Runtime.executionContextCreated" if own_session => {
                    self.on_context(&mut state, &params);
                }
                "Runtime.exceptionThrown" if own_session => Self::on_exception(&mut state, &params),
                "Runtime.exceptionRevoked" if own_session => {
                    if let Some(id) = params.get("exceptionId").and_then(Value::as_i64) {
                        state.exceptions.retain(|(kept, _)| *kept != id);
                    }
                }
                "Runtime.bindingCalled" if own_session => Self::on_binding(&mut state, &params),
                "Inspector.targetCrashed" if own_session => state.crashed = true,
                "Target.targetCrashed" => {
                    if str_at(&params, &["targetId"]) == state.target.as_deref() {
                        state.crashed = true;
                    }
                }
                // A target attached from the page's session: a frame in another process or a
                // worker. The sandbox's CSP allows neither, so the page loaded something it may
                // not; it stays paused (waitForDebuggerOnStart).
                "Target.attachedToTarget" if own_session => {
                    if str_at(&params, &["targetInfo", "type"]) == Some("page") {
                        state.navigated = true;
                    } else {
                        state.requested = true;
                    }
                }
                // A new page target once ours exists: a window the page opened.
                "Target.targetCreated" if session.is_none() => {
                    let created = str_at(&params, &["targetInfo", "targetId"]);
                    let page = str_at(&params, &["targetInfo", "type"]) == Some("page");
                    if page && state.target.is_some() && created != state.target.as_deref() {
                        state.navigated = true;
                    }
                }
                _ => return,
            }
        }
        if let Some((request_id, decision)) = answer {
            match decision {
                Interception::Serve(path) => {
                    if let Some(served) = self.site.get(&path) {
                        cdp.notify(
                            session,
                            "Fetch.fulfillRequest",
                            &Fulfill {
                                request_id: &request_id,
                                response_code: 200,
                                response_headers: &served.headers,
                                body: &served.body,
                            },
                        );
                    }
                }
                Interception::Fail => cdp.notify(
                    session,
                    "Fetch.failRequest",
                    &json!({"requestId": request_id, "errorReason": "BlockedByClient"}),
                ),
            }
        }
        self.changes
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    fn closed(&self) {
        self.update(|state| state.closed = true);
    }
}

fn str_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut at = value;
    for key in path {
        at = at.get(key)?;
    }
    at.as_str()
}

/// The text of an exception as the browser describes it: an error's description (its first line
/// is `Name: message`), a thrown primitive's value, or the browser's summary; cut by
/// [`clip_message`], so that a page throwing huge strings cannot make the service hold them.
fn exception_text(details: &Value) -> Option<String> {
    if let Some(exception) = details.get("exception") {
        if let Some(description) = exception.get("description").and_then(Value::as_str) {
            return Some(clip_message(description));
        }
        match exception.get("value") {
            Some(Value::String(text)) => return Some(clip_message(text)),
            Some(value @ (Value::Number(_) | Value::Bool(_))) => {
                return Some(clip_message(&value.to_string()));
            }
            _ => {}
        }
    }
    let text = details.get("text").and_then(Value::as_str)?;
    let text = text
        .strip_prefix("Uncaught (in promise)")
        .or_else(|| text.strip_prefix("Uncaught"))
        .unwrap_or(text)
        .trim();
    (!text.is_empty()).then(|| clip_message(text))
}

/// What the checker keeps of a script's message: its first line without control or invisible
/// characters, at most [`MAX_SCRIPT_MESSAGE_CHARS`] characters, which is all a verdict can
/// carry.
pub(crate) fn clip_message(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| !hidden(*c))
        .take(MAX_SCRIPT_MESSAGE_CHARS)
        .collect()
}

/// Characters a message may not carry: control characters and the invisible ones the contract
/// lists (`contract::api::dashboard::spec`); the check also asks the contract itself.
fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
        )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use contract::api::dashboard::sandbox::{CHECK_HOST_PATH, CHECK_HOST_SCRIPT_PATH};
    use tokio::io::{AsyncBufReadExt, BufReader, duplex};

    use super::*;
    use crate::cdp::Tasks;

    const SESSION: &str = "S";
    const MAIN: &str = "MAIN";
    const SANDBOX: &str = "SANDBOX";

    fn site() -> Arc<Site> {
        let dir = std::env::temp_dir().join(format!(
            "dashboard-check-observer-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("t")
                .replace("::", "-")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        site::tests::write_site(&dir);
        let site = Site::load(&dir).expect("loads");
        std::fs::remove_dir_all(&dir).expect("cleanup");
        Arc::new(site)
    }

    /// An observer set up as the check sets it up, with the sandbox frame attached.
    fn observer() -> Observer {
        let observer = Observer::new(RuntimeVersion::CURRENT, site());
        observer.update(|state| {
            state.session = Some(SESSION.to_owned());
            state.target = Some("T".to_owned());
            state.main_frame = Some(MAIN.to_owned());
        });
        observer
    }

    /// Feeds events to an observer and returns the commands it sent.
    struct Harness {
        observer: Arc<Observer>,
        cdp: Cdp,
        _tasks: Tasks,
        commands: BufReader<tokio::io::DuplexStream>,
        _messages: tokio::io::DuplexStream,
    }

    impl Harness {
        fn new() -> Self {
            let observer = Arc::new(observer());
            let (to_browser, commands) = duplex(1 << 22);
            let (messages, from_browser) = duplex(1 << 16);
            let (cdp, tasks) = Cdp::start(from_browser, to_browser, observer.clone());
            Self {
                observer,
                cdp,
                _tasks: tasks,
                commands: BufReader::new(commands),
                _messages: messages,
            }
        }

        fn event(&self, method: &str, params: Value) {
            self.observer
                .event(&self.cdp, Some(SESSION), method, params);
        }

        async fn command(&mut self) -> Value {
            let mut frame = Vec::new();
            tokio::time::timeout(
                Duration::from_secs(30),
                self.commands.read_until(0, &mut frame),
            )
            .await
            .expect("a command arrives")
            .expect("reads");
            frame.pop();
            serde_json::from_slice(&frame).expect("JSON")
        }

        async fn request(&mut self, frame: &str, path: &str, resource: &str) -> Value {
            self.event(
                "Fetch.requestPaused",
                json!({
                    "requestId": "R",
                    "request": {"url": site::url(path), "method": "GET"},
                    "frameId": frame,
                    "resourceType": resource,
                }),
            );
            self.command().await
        }

        fn attach_sandbox(&self) {
            self.event(
                "Page.frameAttached",
                json!({"frameId": SANDBOX, "parentFrameId": MAIN}),
            );
        }
    }

    fn served_path(command: &Value) -> Option<String> {
        if command["method"] != "Fetch.fulfillRequest" {
            return None;
        }
        let body = command["params"]["body"].as_str()?;
        let bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, body).ok()?;
        String::from_utf8(bytes).ok()
    }

    #[tokio::test]
    async fn serves_the_host_and_the_runtime_version_files_once_each_as_expected() {
        let mut harness = Harness::new();
        let runtime = RuntimeVersion::CURRENT;
        let host = harness.request(MAIN, CHECK_HOST_PATH, "Document").await;
        assert_eq!(served_path(&host).as_deref(), Some(CHECK_HOST_PATH));
        let headers = host["params"]["responseHeaders"]
            .as_array()
            .expect("headers");
        assert!(
            headers
                .iter()
                .any(|header| header["name"] == "content-security-policy")
        );
        let script = harness
            .request(MAIN, CHECK_HOST_SCRIPT_PATH, "Script")
            .await;
        assert_eq!(
            served_path(&script).as_deref(),
            Some(CHECK_HOST_SCRIPT_PATH)
        );
        harness.attach_sandbox();
        let document = SandboxFile::Document.path(runtime);
        let served = harness.request(SANDBOX, &document, "Document").await;
        assert_eq!(served_path(&served), Some(document));
        for (file, resource) in [
            (SandboxFile::Runtime, "Script"),
            (SandboxFile::D3, "Script"),
            (SandboxFile::Style, "Stylesheet"),
            (SandboxFile::Compare, "Script"),
            // Again: a version's own files are no request of the page's.
            (SandboxFile::Runtime, "Script"),
        ] {
            let path = file.path(runtime);
            let served = harness.request(SANDBOX, &path, resource).await;
            assert_eq!(served_path(&served), Some(path));
        }
        let state = harness.observer.state();
        assert!(!state.navigated && !state.requested && !state.host_failed);
    }

    #[tokio::test]
    async fn any_other_request_of_the_page_fails_as_requested() {
        for (path, resource) in [
            (CHECK_HOST_SCRIPT_PATH, "Script"),
            (CHECK_HOST_PATH, "Stylesheet"),
            ("/sandbox/v1/", "Script"),
            ("/sandbox/v2/runtime.js", "Script"),
            ("/api/v1/dashboards", "XHR"),
            ("/sandbox/v1/runtime.js?x", "Script"),
            ("/favicon.ico", "Image"),
        ] {
            let mut harness = Harness::new();
            harness.attach_sandbox();
            let command = harness.request(SANDBOX, path, resource).await;
            assert_eq!(command["method"], "Fetch.failRequest", "{path}");
            assert_eq!(command["params"]["errorReason"], "BlockedByClient");
            let state = harness.observer.state();
            assert!(state.requested, "{path}");
            assert!(!state.navigated && !state.host_failed, "{path}");
        }
        // Another origin, and a method other than GET.
        let mut harness = Harness::new();
        harness.attach_sandbox();
        harness.event(
            "Fetch.requestPaused",
            json!({"requestId": "R", "request": {"url": "https://example.com/x.js", "method": "GET"},
                   "frameId": SANDBOX, "resourceType": "Script"}),
        );
        assert_eq!(harness.command().await["method"], "Fetch.failRequest");
        harness.event(
            "Fetch.requestPaused",
            json!({"requestId": "R2", "request": {"url": site::url(&SandboxFile::Runtime.path(RuntimeVersion::CURRENT)), "method": "POST"},
                   "frameId": SANDBOX, "resourceType": "Script"}),
        );
        assert_eq!(harness.command().await["method"], "Fetch.failRequest");
        assert!(harness.observer.state().requested);
    }

    #[tokio::test]
    async fn a_second_document_of_the_sandbox_frame_is_a_navigation() {
        let mut harness = Harness::new();
        harness.attach_sandbox();
        let document = SandboxFile::Document.path(RuntimeVersion::CURRENT);
        let first = harness.request(SANDBOX, &document, "Document").await;
        assert_eq!(first["method"], "Fetch.fulfillRequest");
        let again = harness.request(SANDBOX, &document, "Document").await;
        assert_eq!(again["method"], "Fetch.failRequest");
        assert!(harness.observer.state().navigated);

        let mut harness = Harness::new();
        harness.attach_sandbox();
        let host = harness.request(SANDBOX, CHECK_HOST_PATH, "Document").await;
        assert_eq!(host["method"], "Fetch.failRequest");
        let state = harness.observer.state();
        assert!(state.navigated && !state.requested);
    }

    #[tokio::test]
    async fn the_host_files_are_served_only_to_the_main_frame_once() {
        let mut harness = Harness::new();
        harness.attach_sandbox();
        let sandbox_doc = harness
            .request(
                SANDBOX,
                &SandboxFile::Document.path(RuntimeVersion::CURRENT),
                "Document",
            )
            .await;
        assert_eq!(sandbox_doc["method"], "Fetch.fulfillRequest");
        let script = harness
            .request(MAIN, CHECK_HOST_SCRIPT_PATH, "Script")
            .await;
        assert_eq!(script["method"], "Fetch.fulfillRequest");
        let twice = harness
            .request(MAIN, CHECK_HOST_SCRIPT_PATH, "Script")
            .await;
        assert_eq!(twice["method"], "Fetch.failRequest");
        // The host page asking for something else is the host's failure, not the page's.
        assert!(harness.observer.state().host_failed);
        assert!(!harness.observer.state().requested);
    }

    #[test]
    fn navigations_of_the_page_frames_are_navigated() {
        let observer = observer();
        let expected = site::url(&SandboxFile::Document.path(RuntimeVersion::CURRENT));
        let mut state = observer.state();
        observer.on_frame_attached(
            &mut state,
            &json!({"frameId": SANDBOX, "parentFrameId": MAIN}),
        );
        observer.on_navigated(
            &mut state,
            &json!({"frame": {"id": MAIN, "url": site::url(CHECK_HOST_PATH)}}),
        );
        observer.on_navigation_requested(&mut state, &json!({"frameId": SANDBOX, "url": expected}));
        observer.on_navigated(
            &mut state,
            &json!({"frame": {"id": SANDBOX, "url": expected}}),
        );
        assert!(!state.navigated, "the expected first loads");
        observer.on_navigation_requested(&mut state, &json!({"frameId": SANDBOX, "url": expected}));
        assert!(state.navigated, "a second request");

        for event in [
            json!({"frame": {"id": SANDBOX, "url": "about:blank"}}),
            json!({"frame": {"id": SANDBOX, "url": "chrome-error://chromewebdata/"}}),
            json!({"frame": {"id": MAIN, "url": "https://check.invalid/elsewhere"}}),
            json!({"frame": {"id": "NESTED", "url": "about:blank"}}),
        ] {
            let observer = observer_with_frames();
            let mut state = observer.state();
            observer.on_navigated(&mut state, &event);
            assert!(state.navigated, "{event}");
        }
        let observer = observer_with_frames();
        let mut state = observer.state();
        observer.on_navigation_requested(
            &mut state,
            &json!({"frameId": "NESTED", "url": "about:blank"}),
        );
        assert!(state.navigated);
    }

    fn observer_with_frames() -> Observer {
        let observer = observer();
        {
            let mut state = observer.state();
            observer.on_frame_attached(
                &mut state,
                &json!({"frameId": SANDBOX, "parentFrameId": MAIN}),
            );
            observer.on_frame_attached(
                &mut state,
                &json!({"frameId": "NESTED", "parentFrameId": SANDBOX}),
            );
            observer.on_navigated(
                &mut state,
                &json!({"frame": {"id": MAIN, "url": site::url(CHECK_HOST_PATH)}}),
            );
            observer.on_navigated(&mut state, &json!({"frame": {"id": SANDBOX, "url": site::url(&SandboxFile::Document.path(RuntimeVersion::CURRENT))}}));
            assert!(!state.navigated);
            assert!(state.page_frames.contains("NESTED"));
        }
        observer
    }

    #[tokio::test]
    async fn windows_and_other_targets_fail_the_page() {
        let harness = Harness::new();
        harness.event("Page.windowOpen", json!({"url": "https://example.com/"}));
        assert!(harness.observer.state().navigated);

        let harness = Harness::new();
        harness.event(
            "Target.attachedToTarget",
            json!({"sessionId": "W", "targetInfo": {"type": "worker"}, "waitingForDebugger": true}),
        );
        assert!(harness.observer.state().requested);

        let harness = Harness::new();
        harness.observer.event(
            &harness.cdp,
            None,
            "Target.targetCreated",
            json!({"targetInfo": {"targetId": "T", "type": "page"}}),
        );
        assert!(!harness.observer.state().navigated, "our own target");
        harness.observer.event(
            &harness.cdp,
            None,
            "Target.targetCreated",
            json!({"targetInfo": {"targetId": "T2", "type": "page"}}),
        );
        assert!(harness.observer.state().navigated);
    }

    #[test]
    fn only_the_host_world_reports() {
        let observer = observer();
        let mut state = observer.state();
        observer.on_context(
            &mut state,
            &json!({"context": {"id": 7, "origin": site::ORIGIN, "name": "", "auxData": {"isDefault": true, "type": "default", "frameId": MAIN}}}),
        );
        observer.on_context(
            &mut state,
            &json!({"context": {"id": 9, "origin": "null", "name": "", "auxData": {"isDefault": true, "type": "default", "frameId": SANDBOX}}}),
        );
        assert_eq!(state.host_context, Some(7));
        let report = |context: i64, payload: &str| json!({"name": REPORT_BINDING, "payload": payload, "executionContextId": context});
        // The sandbox's world calls the binding too: ignored.
        Observer::on_binding(&mut state, &report(9, r#"{"type":"ready"}"#));
        Observer::on_binding(&mut state, &report(9, r#"{"type":"error","message":"x"}"#));
        assert!(state.ready.is_none() && state.page_error.is_none());
        // Rendered before ready means nothing.
        Observer::on_binding(
            &mut state,
            &report(7, r#"{"type":"rendered","generation":1}"#),
        );
        assert!(state.host_failed);
        state.host_failed = false;
        Observer::on_binding(&mut state, &report(7, r#"{"type":"ready","runtime":1}"#));
        Observer::on_binding(
            &mut state,
            &report(7, r#"{"type":"rendered","generation":1}"#),
        );
        assert!(state.ready.is_some() && state.rendered);
        Observer::on_binding(
            &mut state,
            &report(7, r#"{"type":"error","phase":"render","message":"boom"}"#),
        );
        assert_eq!(state.page_error, Some(Some("boom".to_owned())));
        assert!(!state.host_failed);
        for bad in [
            r#"{"type":"done"}"#,
            "not json",
            &format!(
                r#"{{"type":"error","message":"{}"}}"#,
                "x".repeat(MAX_REPORT_BYTES)
            ),
        ] {
            state.host_failed = false;
            Observer::on_binding(&mut state, &report(7, bad));
            assert!(state.host_failed, "{bad}");
        }
    }

    #[test]
    fn exceptions_are_the_pages_unless_they_come_from_the_host_or_the_checker() {
        let observer = observer();
        let mut state = observer.state();
        observer.on_context(
            &mut state,
            &json!({"context": {"id": 7, "origin": site::ORIGIN, "name": "", "auxData": {"isDefault": true, "type": "default", "frameId": MAIN}}}),
        );
        observer.on_context(
            &mut state,
            &json!({"context": {"id": 11, "origin": "", "name": WORLD_NAME, "auxData": {"isDefault": false, "type": "isolated", "frameId": SANDBOX}}}),
        );
        let thrown = |context: i64, id: i64, exception: Value| json!({"exceptionDetails": {"exceptionId": id, "text": "Uncaught", "executionContextId": context, "exception": exception}});
        Observer::on_exception(
            &mut state,
            &thrown(
                11,
                1,
                json!({"type": "object", "description": "Error: checker"}),
            ),
        );
        assert!(state.exceptions.is_empty() && !state.host_failed);
        Observer::on_exception(
            &mut state,
            &thrown(
                9,
                2,
                json!({"type": "object", "subtype": "error", "description": "Error: boom\n    at render"}),
            ),
        );
        Observer::on_exception(
            &mut state,
            &thrown(9, 3, json!({"type": "string", "value": "thrown text"})),
        );
        Observer::on_exception(
            &mut state,
            &thrown(9, 4, json!({"type": "number", "value": 42})),
        );
        assert_eq!(
            state.exceptions,
            vec![
                (2, Some("Error: boom".to_owned())),
                (3, Some("thrown text".to_owned())),
                (4, Some("42".to_owned())),
            ]
        );
        Observer::on_exception(
            &mut state,
            &thrown(
                7,
                5,
                json!({"type": "object", "description": "Error: host"}),
            ),
        );
        assert!(state.host_failed);
        assert_eq!(state.exceptions.len(), 3);
    }

    #[test]
    fn huge_exceptions_are_cut_before_they_are_kept() {
        let observer = observer();
        let mut state = observer.state();
        let huge = format!(
            "Error: {}\n{}",
            "\u{200B}x".repeat(1_000_000),
            "y".repeat(1_000)
        );
        for id in 0..40 {
            Observer::on_exception(
                &mut state,
                &json!({"exceptionDetails": {"exceptionId": id, "text": "Uncaught", "executionContextId": 9, "exception": {"type": "string", "value": huge}}}),
            );
        }
        assert_eq!(state.exceptions.len(), MAX_EXCEPTIONS);
        for (_, text) in &state.exceptions {
            let text = text.as_deref().expect("kept");
            assert_eq!(text.chars().count(), MAX_SCRIPT_MESSAGE_CHARS);
            assert!(text.starts_with("Error: xxx") && !text.contains(['\u{200B}', 'y']));
        }
    }

    #[test]
    fn exception_text_falls_back_to_the_summary() {
        assert_eq!(
            exception_text(&json!({"text": "Uncaught (in promise) late"})),
            Some("late".to_owned())
        );
        assert_eq!(exception_text(&json!({"text": "Uncaught"})), None);
        assert_eq!(
            exception_text(&json!({"exception": {"type": "undefined"}, "text": "Uncaught"})),
            None
        );
    }

    #[tokio::test]
    async fn requests_past_the_limit_stay_unanswered() {
        let mut harness = Harness::new();
        harness.attach_sandbox();
        for _ in 0..MAX_ANSWERED_REQUESTS {
            let command = harness.request(SANDBOX, "/x.js", "Script").await;
            assert_eq!(command["method"], "Fetch.failRequest");
        }
        harness.event(
            "Fetch.requestPaused",
            json!({"requestId": "late", "request": {"url": site::url("/x.js"), "method": "GET"},
                   "frameId": SANDBOX, "resourceType": "Script"}),
        );
        // An event that is answered: the next command is its answer, not the request's.
        harness.event("Page.javascriptDialogOpening", json!({"type": "alert"}));
        assert_eq!(
            harness.command().await["method"],
            "Page.handleJavaScriptDialog"
        );
        assert!(harness.observer.state().requested);
    }

    #[tokio::test]
    async fn waits_end_on_the_condition_the_deadline_or_the_browser_closing() {
        let observer = Arc::new(observer());
        let far = Instant::now() + Duration::from_secs(600);
        let waiting = tokio::spawn({
            let observer = observer.clone();
            async move {
                observer
                    .wait_until(far, |state| state.loaded.then_some(1))
                    .await
            }
        });
        tokio::task::yield_now().await;
        observer.update(|state| state.loaded = true);
        assert_eq!(waiting.await.expect("joins"), Some(1));

        let waiting = tokio::spawn({
            let observer = observer.clone();
            async move {
                observer
                    .wait_until(far, |state| state.rendered.then_some(()))
                    .await
            }
        });
        tokio::task::yield_now().await;
        observer.update(|state| state.closed = true);
        assert_eq!(waiting.await.expect("joins"), None);

        let fresh = Arc::new(Observer::new(RuntimeVersion::CURRENT, site()));
        let now = Instant::now();
        assert_eq!(
            fresh
                .wait_until(now, |state| state.rendered.then_some(()))
                .await,
            None
        );
    }
}
