//! One check, from a fresh browser to the verdict (`frontend/sandbox/README.md`,
//! 「檢查服務的流程」).
//!
//! The browser loads the checker's host page; the host page gets the request through
//! `tqCheckHost.start(request)`, frames the sandbox, sends it `init` and `data`, and reports
//! `ready`, `rendered` and `error` through the [`REPORT_BINDING`]. The checker trusts none of
//! the page: it confirms the quiet period itself from an isolated world of the sandbox frame,
//! and runs the comparison there, where the page's scripts cannot reach, with the request's data
//! as the argument. The page never sees the values it is compared with.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use contract::api::dashboard::RuntimeVersion;
use contract::api::dashboard::check::{
    CHECK_TIME_LIMIT, CheckRequest, CheckVerdict, ContentKind, Locale, MAX_ELEMENTS, MAX_PROBLEMS,
    Problem, QUIET_PERIOD, RENDER_TIME_LIMIT, SizeLimit, TimeoutPhase, VerdictError,
};
use contract::api::dashboard::result::DatasetResult;
use contract::api::dashboard::sandbox::CHECK_HOST_PATH;
use contract::api::dashboard::spec::PublicSpec;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::browser::{Browser, LaunchError};
use crate::cdp::{Cdp, CdpError};
use crate::cleanup::{Cleanup, CleanupError};
use crate::observer::{Observer, REPORT_BINDING, WORLD_NAME, clip_message};
use crate::site::{self, Site};

/// The host page's entry point (`frontend/sandbox/README.md`, 「檢查服務的流程」 step 4).
pub const START_FUNCTION: &str = "function (request) { globalThis.tqCheckHost.start(request); }";

/// Calls the comparison (`compare.js` defines `globalThis.tqCompare`).
pub const COMPARE_FUNCTION: &str = "function (input) { return globalThis.tqCompare(input); }";

/// Installed in the isolated world as soon as the sandbox is ready: records when the sandbox's
/// document last changed, and waits for a quiet period. The page cannot reach this world.
pub const WATCH_SCRIPT: &str = r"(() => {
  let last = performance.now();
  new MutationObserver(() => { last = performance.now(); }).observe(document, {
    subtree: true, childList: true, attributes: true, characterData: true,
  });
  globalThis.tqCheckSinceChange = () => performance.now() - last;
  globalThis.tqCheckQuiet = (quiet, limit) => new Promise((resolve) => {
    const start = performance.now();
    const tick = () => {
      const now = performance.now();
      if (now - last >= quiet) return resolve(true);
      if (now - start >= limit) return resolve(false);
      setTimeout(tick, Math.max(1, Math.min(quiet - (now - last), limit - (now - start))));
    };
    tick();
  });
})()";

/// How long the checker waits for an answer from the sandbox's renderer once the render limit
/// passed; a page that keeps its thread busy does not answer.
pub const PROBE_LIMIT: Duration = Duration::from_secs(1);

/// The most script errors in a verdict, so the comparison's problems still fit.
pub const MAX_SCRIPT_ERRORS: usize = 5;

/// Runs checks with one browser each.
#[derive(Debug)]
pub struct Checker {
    chromium: PathBuf,
    site: Arc<Site>,
    cleanup: Arc<Cleanup>,
}

/// Why a check produced no verdict: the service's failure, never the page's.
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    #[error("the browser could not start: {0}")]
    Launch(#[from] LaunchError),
    #[error("the browser failed outside the page during {0}")]
    Browser(&'static str),
    #[error("the host page failed")]
    HostPage,
    #[error("the comparison failed")]
    Compare,
    #[error("the verdict breaks the contract: {0}")]
    Verdict(VerdictError),
    #[error("the runtime version has no comparison in the site")]
    NoComparison,
    #[error("the cleanup after the check failed: {0}")]
    Cleanup(#[from] CleanupError),
    #[error("the request was dropped before the check ended")]
    Cancelled,
}

/// How long each phase took, for the log line.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timings {
    pub launch: Duration,
    pub ready: Option<Duration>,
    pub rendered: Option<Duration>,
    pub compared: Option<Duration>,
    pub total: Duration,
}

/// A check's result.
#[derive(Debug)]
pub struct Outcome {
    pub verdict: CheckVerdict,
    pub timings: Timings,
}

/// How the flow ended, before the browser's own observations are merged in.
#[derive(Debug)]
enum FlowEnd {
    /// The comparison ran; `extra` holds what only the protocol shows (shadow roots, size).
    Compared {
        verdict: CheckVerdict,
        extra: Vec<Problem>,
    },
    /// The flow stopped before comparing, with these problems.
    Stopped(Vec<Problem>),
}

impl Checker {
    #[must_use]
    pub fn new(chromium: PathBuf, site: Arc<Site>, cleanup: Arc<Cleanup>) -> Self {
        Self {
            chromium,
            site,
            cleanup,
        }
    }

    /// Starts a browser, asks its version and closes it: proves at startup that the browser
    /// runs here.
    ///
    /// # Errors
    ///
    /// When the browser cannot start or answer.
    pub async fn browser_version(&self) -> Result<String, CheckError> {
        let observer = Arc::new(Observer::new(RuntimeVersion::CURRENT, self.site.clone()));
        let browser = Browser::launch(&self.chromium, self.cleanup.clone(), observer)?;
        let version = tokio::time::timeout(
            CHECK_TIME_LIMIT,
            browser.cdp().call(None, "Browser.getVersion", &json!({})),
        )
        .await;
        browser.close().await?;
        match version {
            Ok(Ok(version)) => Ok(version
                .get("product")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()),
            _ => Err(CheckError::Browser("startup")),
        }
    }

    /// Checks `request`, which [`CheckRequest::validate`] accepted, in a fresh browser.
    ///
    /// # Errors
    ///
    /// When the browser or the host page fails, or the comparison breaks its contract.
    pub async fn check(
        &self,
        request: &CheckRequest,
        cancelled: impl Future<Output = ()>,
    ) -> Result<Outcome, CheckError> {
        let started = Instant::now();
        let compare = self
            .site
            .compare_source(request.runtime)
            .ok_or(CheckError::NoComparison)?;
        let observer = Arc::new(Observer::new(request.runtime, self.site.clone()));
        let browser = Browser::launch(&self.chromium, self.cleanup.clone(), observer.clone())?;
        let mut timings = Timings {
            launch: started.elapsed(),
            ..Timings::default()
        };
        let flow = run(
            browser.cdp(),
            &observer,
            request,
            compare,
            started,
            &mut timings,
        );
        let ended = tokio::select! {
            ended = tokio::time::timeout_at(started + CHECK_TIME_LIMIT, flow) => Some(ended),
            () = cancelled => None,
        };
        browser.close().await?;
        let Some(ended) = ended else {
            return Err(CheckError::Cancelled);
        };
        timings.total = started.elapsed();
        let observed = observed_problems(&observer, request);
        let host_failed = observer.state().host_failed;
        let verdict = match ended {
            Ok(Ok(end)) if !host_failed => merge(observed, end),
            Ok(Ok(_)) => return Err(CheckError::HostPage),
            Ok(Err(error)) => return Err(error),
            Err(_) => merge(
                observed,
                FlowEnd::Stopped(vec![Problem::Timeout {
                    phase: TimeoutPhase::Check,
                }]),
            ),
        };
        verdict.validate(request).map_err(CheckError::Verdict)?;
        Ok(Outcome { verdict, timings })
    }
}

/// The comparison's input: the request without the page.
#[derive(Serialize)]
struct CompareInput<'a> {
    runtime: RuntimeVersion,
    spec: &'a PublicSpec,
    datasets: &'a [DatasetResult],
    locale: Locale,
    tz: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CallFunction<'a, T: Serialize> {
    function_declaration: &'a str,
    execution_context_id: i64,
    arguments: [Argument<'a, T>; 1],
    return_by_value: bool,
    await_promise: bool,
}

#[derive(Serialize)]
struct Argument<'a, T: Serialize> {
    value: &'a T,
}

async fn call(cdp: &Cdp, session: &str, method: &str, params: Value) -> Result<Value, CdpError> {
    cdp.call(Some(session), method, &params).await
}

/// The flow; every step until the host page runs is the service's, every later failure the
/// page's unless the browser says otherwise.
async fn run(
    cdp: &Cdp,
    observer: &Observer,
    request: &CheckRequest,
    compare: &str,
    started: Instant,
    timings: &mut Timings,
) -> Result<FlowEnd, CheckError> {
    let setup = |_| CheckError::Browser("setup");
    cdp.call(None, "Browser.getVersion", &json!({}))
        .await
        .map_err(setup)?;
    cdp.call(
        None,
        "Target.setDiscoverTargets",
        &json!({"discover": true}),
    )
    .await
    .map_err(setup)?;
    // The page the browser opened at start (about:blank): one renderer for the check.
    let targets = cdp
        .call(None, "Target.getTargets", &json!({}))
        .await
        .map_err(setup)?;
    let target = targets
        .get("targetInfos")
        .and_then(Value::as_array)
        .and_then(|targets| {
            targets
                .iter()
                .find(|target| target.get("type").and_then(Value::as_str) == Some("page"))
        })
        .and_then(|target| target.get("targetId"))
        .and_then(Value::as_str)
        .ok_or(CheckError::Browser("setup"))?
        .to_owned();
    observer.update(|state| state.target = Some(target.clone()));
    let attached = cdp
        .call(
            None,
            "Target.attachToTarget",
            &json!({"targetId": target, "flatten": true}),
        )
        .await
        .map_err(setup)?;
    let session = attached
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or(CheckError::Browser("setup"))?
        .to_owned();
    observer.update(|state| state.session = Some(session.clone()));
    let session = session.as_str();
    for (method, params) in [
        ("Inspector.enable", json!({})),
        ("Page.enable", json!({})),
        ("Runtime.enable", json!({})),
        ("Fetch.enable", json!({"patterns": [{"urlPattern": "*"}]})),
        (
            "Target.setAutoAttach",
            json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true}),
        ),
        ("Runtime.addBinding", json!({"name": REPORT_BINDING})),
    ] {
        call(cdp, session, method, params).await.map_err(setup)?;
    }
    let tree = call(cdp, session, "Page.getFrameTree", json!({}))
        .await
        .map_err(setup)?;
    let main = tree
        .pointer("/frameTree/frame/id")
        .and_then(Value::as_str)
        .ok_or(CheckError::Browser("setup"))?
        .to_owned();
    observer.update(|state| state.main_frame = Some(main));
    let navigated = call(
        cdp,
        session,
        "Page.navigate",
        json!({"url": site::url(CHECK_HOST_PATH)}),
    )
    .await
    .map_err(|_| CheckError::Browser("navigate"))?;
    if navigated.get("errorText").is_some() {
        return Err(CheckError::HostPage);
    }
    let host = observer
        .wait_until(started + CHECK_TIME_LIMIT, |state| {
            state.host_context.filter(|_| state.loaded)
        })
        .await
        .ok_or(CheckError::HostPage)?;
    timings.launch = started.elapsed();

    // The host page runs: from here the page can fail the check.
    let start = cdp
        .call(
            Some(session),
            "Runtime.callFunctionOn",
            &CallFunction {
                function_declaration: START_FUNCTION,
                execution_context_id: host,
                arguments: [Argument { value: request }],
                return_by_value: true,
                await_promise: false,
            },
        )
        .await;
    match start {
        Ok(result) if result.get("exceptionDetails").is_none() => {}
        Err(CdpError::TooLarge) => return Ok(too_large()),
        _ => return Err(CheckError::HostPage),
    }
    let data_sent = Instant::now();
    let ready = observer
        .wait_until(data_sent + RENDER_TIME_LIMIT, |state| {
            if state.page_error.is_some() {
                Some(false)
            } else {
                state.ready.map(|_| true)
            }
        })
        .await;
    if let Some(stopped) = stopped_by_page(observer) {
        return Ok(stopped);
    }
    if ready != Some(true) {
        return Ok(FlowEnd::Stopped(vec![Problem::NotRendered]));
    }
    timings.ready = Some(started.elapsed());
    let render_deadline = data_sent + RENDER_TIME_LIMIT;
    let Some(sandbox) = observer.state().sandbox_frame.clone() else {
        return Err(CheckError::HostPage);
    };
    let world = match call(
        cdp,
        session,
        "Page.createIsolatedWorld",
        json!({"frameId": sandbox, "worldName": WORLD_NAME, "grantUniveralAccess": false}),
    )
    .await
    {
        Ok(world) => world.get("executionContextId").and_then(Value::as_i64),
        Err(_) => None,
    };
    let Some(world) = world else {
        // The frame went away: the page navigated it, or its renderer died.
        return page_or_service(observer, "isolated world");
    };
    let watching = evaluate(cdp, session, world, WATCH_SCRIPT, false).await;
    if watching.is_err() {
        return page_or_service(observer, "isolated world");
    }

    let rendered = observer
        .wait_until(render_deadline, |state| {
            (state.rendered || state.page_error.is_some()).then_some(())
        })
        .await
        .is_some();
    if let Some(stopped) = stopped_by_page(observer) {
        return Ok(stopped);
    }
    if rendered {
        let limit = render_deadline
            .saturating_duration_since(Instant::now())
            .max(QUIET_PERIOD * 2);
        let quiet = tokio::time::timeout(
            limit + PROBE_LIMIT,
            evaluate(
                cdp,
                session,
                world,
                &format!(
                    "globalThis.tqCheckQuiet({}, {})",
                    QUIET_PERIOD.as_millis(),
                    limit.as_millis()
                ),
                true,
            ),
        )
        .await;
        if !matches!(quiet, Ok(Ok(Value::Bool(true)))) {
            return Ok(FlowEnd::Stopped(vec![Problem::Timeout {
                phase: TimeoutPhase::Render,
            }]));
        }
    } else {
        // Not finished in time: still changing means `render` returned and the page kept
        // drawing; quiet, or too busy to answer, means it never finished.
        let since = tokio::time::timeout(
            PROBE_LIMIT,
            evaluate(
                cdp,
                session,
                world,
                "globalThis.tqCheckSinceChange()",
                false,
            ),
        )
        .await;
        let changing = matches!(since, Ok(Ok(Value::Number(ref ms))) if ms.as_f64().is_some_and(|ms| ms < millis(QUIET_PERIOD)));
        return Ok(FlowEnd::Stopped(vec![if changing {
            Problem::Timeout {
                phase: TimeoutPhase::Render,
            }
        } else {
            Problem::NotRendered
        }]));
    }
    timings.rendered = Some(started.elapsed());

    if evaluate(cdp, session, world, compare, false).await.is_err() {
        return page_or_service(observer, "comparison");
    }
    let input = CompareInput {
        runtime: request.runtime,
        spec: &request.spec,
        datasets: &request.datasets,
        locale: request.locale,
        tz: &request.tz,
    };
    let compared = cdp
        .call(
            Some(session),
            "Runtime.callFunctionOn",
            &CallFunction {
                function_declaration: COMPARE_FUNCTION,
                execution_context_id: world,
                arguments: [Argument { value: &input }],
                return_by_value: true,
                await_promise: true,
            },
        )
        .await;
    let compared = match compared {
        Ok(result) if result.get("exceptionDetails").is_none() => result,
        Err(CdpError::TooLarge) => return Ok(too_large()),
        Ok(_) | Err(_) => return page_or_service(observer, "comparison"),
    };
    let verdict = compared
        .pointer("/result/value")
        .cloned()
        .and_then(|value| serde_json::from_value::<CheckVerdict>(value).ok())
        .ok_or(CheckError::Compare)?;
    if let CheckVerdict::Failed { problems } = &verdict
        && problems
            .iter()
            .any(|problem| matches!(problem, Problem::Unrecognized))
    {
        return Err(CheckError::Compare);
    }
    timings.compared = Some(started.elapsed());

    let mut extra = Vec::new();
    let sized_out = matches!(&verdict, CheckVerdict::Failed { problems }
        if problems.iter().any(|problem| matches!(problem, Problem::TooLarge { .. })));
    if !sized_out {
        match call(
            cdp,
            session,
            "DOMSnapshot.captureSnapshot",
            json!({"computedStyles": []}),
        )
        .await
        {
            Ok(snapshot) => {
                if has_page_shadow_root(&snapshot) {
                    extra.push(Problem::UnsupportedContent {
                        content: ContentKind::ShadowRoot,
                    });
                }
            }
            Err(CdpError::TooLarge) => extra.push(too_large_problem()),
            Err(_) => return page_or_service(observer, "snapshot"),
        }
    }
    Ok(FlowEnd::Compared { verdict, extra })
}

/// Evaluates `expression` in the isolated world; its value when `return_by_value`.
async fn evaluate(
    cdp: &Cdp,
    session: &str,
    world: i64,
    expression: &str,
    await_promise: bool,
) -> Result<Value, CdpError> {
    let result = cdp
        .call(
            Some(session),
            "Runtime.evaluate",
            &json!({
                "expression": expression,
                "contextId": world,
                "returnByValue": true,
                "awaitPromise": await_promise,
                "silent": true,
            }),
        )
        .await?;
    if result.get("exceptionDetails").is_some() {
        return Err(CdpError::Protocol);
    }
    Ok(result
        .pointer("/result/value")
        .cloned()
        .unwrap_or(Value::Null))
}

/// When a step failed: the page's doing if the browser saw it navigate, crash or fill a
/// message past the limit; otherwise the service's.
fn page_or_service(observer: &Observer, step: &'static str) -> Result<FlowEnd, CheckError> {
    let state = observer.state();
    if state.navigated || state.crashed || state.requested {
        Ok(FlowEnd::Stopped(if state.crashed {
            vec![Problem::NotRendered]
        } else {
            Vec::new()
        }))
    } else {
        Err(CheckError::Browser(step))
    }
}

/// The page ended the flow: its renderer crashed, it navigated away, or the runtime reported
/// an error.
fn stopped_by_page(observer: &Observer) -> Option<FlowEnd> {
    let state = observer.state();
    if state.crashed {
        return Some(FlowEnd::Stopped(vec![Problem::NotRendered]));
    }
    if state.navigated {
        // The document is gone; `navigated` is among the observed problems.
        return Some(FlowEnd::Stopped(Vec::new()));
    }
    // The runtime's error becomes a script error when the problems are merged.
    state
        .page_error
        .as_ref()
        .map(|_| FlowEnd::Stopped(Vec::new()))
}

fn too_large_problem() -> Problem {
    Problem::TooLarge {
        limit: SizeLimit::Elements,
        max: MAX_ELEMENTS,
    }
}

fn too_large() -> FlowEnd {
    FlowEnd::Stopped(vec![too_large_problem()])
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Whether a node of the snapshot is inside an open or closed shadow root (a user-agent shadow
/// root belongs to a form field or media element, which the comparison reports itself).
fn has_page_shadow_root(snapshot: &Value) -> bool {
    let strings = snapshot
        .get("strings")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let documents = snapshot
        .get("documents")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    documents.iter().any(|document| {
        document
            .pointer("/nodes/shadowRootType/value")
            .and_then(Value::as_array)
            .is_some_and(|values| {
                values.iter().any(|index| {
                    index
                        .as_u64()
                        .and_then(|index| usize::try_from(index).ok())
                        .and_then(|index| strings.get(index))
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind == "open" || kind == "closed")
                })
            })
    })
}

/// What the browser saw: navigations, requests and the page's script errors, in that order.
fn observed_problems(observer: &Observer, request: &CheckRequest) -> Vec<Problem> {
    let state = observer.state();
    let mut problems = Vec::new();
    if state.navigated {
        problems.push(Problem::Navigated);
    }
    if state.requested {
        problems.push(Problem::Requested);
    }
    let reported = state.page_error.iter().map(|message| message.as_deref());
    let thrown = state.exceptions.iter().map(|(_, text)| text.as_deref());
    let mut errors = 0;
    for text in reported.chain(thrown) {
        let problem = Problem::ScriptError {
            message: script_message(text, request),
        };
        if errors < MAX_SCRIPT_ERRORS && !problems.contains(&problem) {
            problems.push(problem);
            errors += 1;
        }
    }
    problems
}

/// A script error's message as a verdict may carry it: cut by [`clip_message`] and trimmed; left
/// out when the contract would still refuse it.
fn script_message(text: Option<&str>, request: &CheckRequest) -> Option<String> {
    let cleaned = clip_message(text?);
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return None;
    }
    let message = Some(cleaned.to_owned());
    let alone = CheckVerdict::Failed {
        problems: vec![Problem::ScriptError {
            message: message.clone(),
        }],
    };
    alone.validate(request).is_ok().then_some(message).flatten()
}

/// The verdict: what the browser saw first, then the flow's and the comparison's problems,
/// without repeats, at most [`MAX_PROBLEMS`]. Passes only when nothing failed.
fn merge(observed: Vec<Problem>, end: FlowEnd) -> CheckVerdict {
    let mut problems = observed;
    let add = |more: Vec<Problem>, problems: &mut Vec<Problem>| {
        for problem in more {
            if !problems.contains(&problem) {
                problems.push(problem);
            }
        }
    };
    match end {
        FlowEnd::Compared {
            verdict: CheckVerdict::Passed { report },
            extra,
        } => {
            add(extra, &mut problems);
            if problems.is_empty() {
                return CheckVerdict::Passed { report };
            }
        }
        FlowEnd::Compared {
            verdict: CheckVerdict::Failed { problems: found },
            extra,
        } => {
            add(extra, &mut problems);
            add(found, &mut problems);
        }
        FlowEnd::Stopped(found) => add(found, &mut problems),
    }
    if problems.is_empty() {
        // Stopped by the page without a problem of its own (it navigated away mid-step and
        // the navigation was recorded, which `observed` already holds); never pass it.
        problems.push(Problem::NotRendered);
    }
    problems.truncate(MAX_PROBLEMS);
    CheckVerdict::Failed { problems }
}

#[cfg(test)]
mod tests {
    use contract::api::dashboard::check::{CheckReport, MAX_SCRIPT_MESSAGE_CHARS, MarkReport};
    use serde_json::json;

    use super::*;

    pub(crate) fn request() -> CheckRequest {
        serde_json::from_str(include_str!(
            "../../../crates/contract/tests/fixtures/dashboard/check-request.json"
        ))
        .expect("the contract's fixture")
    }

    #[test]
    fn a_passed_comparison_with_nothing_observed_passes() {
        let report = CheckReport {
            marks: vec![MarkReport {
                reference: "k|title".to_owned(),
                value: json!("x"),
            }],
        };
        let verdict = merge(
            Vec::new(),
            FlowEnd::Compared {
                verdict: CheckVerdict::Passed {
                    report: report.clone(),
                },
                extra: Vec::new(),
            },
        );
        assert_eq!(verdict, CheckVerdict::Passed { report });
    }

    #[test]
    fn what_the_browser_saw_fails_a_passed_comparison() {
        let verdict = merge(
            vec![Problem::Requested],
            FlowEnd::Compared {
                verdict: CheckVerdict::Passed {
                    report: CheckReport::default(),
                },
                extra: vec![Problem::UnsupportedContent {
                    content: ContentKind::ShadowRoot,
                }],
            },
        );
        assert_eq!(
            verdict,
            CheckVerdict::Failed {
                problems: vec![
                    Problem::Requested,
                    Problem::UnsupportedContent {
                        content: ContentKind::ShadowRoot
                    }
                ]
            }
        );
    }

    #[test]
    fn problems_are_merged_in_order_without_repeats_and_capped() {
        let compared: Vec<Problem> = (0..30)
            .map(|i| Problem::LiteralNumber {
                place: contract::api::dashboard::check::LiteralPlace::Text,
                literal: i.to_string(),
                tag: None,
            })
            .collect();
        let verdict = merge(
            vec![Problem::Navigated, Problem::ScriptError { message: None }],
            FlowEnd::Compared {
                verdict: CheckVerdict::Failed {
                    problems: [vec![Problem::Navigated], compared].concat(),
                },
                extra: Vec::new(),
            },
        );
        let CheckVerdict::Failed { problems } = verdict else {
            panic!("failed");
        };
        assert_eq!(problems.len(), MAX_PROBLEMS);
        assert_eq!(problems.first(), Some(&Problem::Navigated));
        assert_eq!(
            problems.get(1),
            Some(&Problem::ScriptError { message: None })
        );
        assert_eq!(
            problems
                .iter()
                .filter(|p| **p == Problem::Navigated)
                .count(),
            1
        );
    }

    #[test]
    fn a_stop_without_problems_never_passes() {
        assert_eq!(
            merge(Vec::new(), FlowEnd::Stopped(Vec::new())),
            CheckVerdict::Failed {
                problems: vec![Problem::NotRendered]
            }
        );
    }

    #[test]
    fn script_messages_are_cleaned_and_bounded() {
        let request = request();
        assert_eq!(
            script_message(Some("Error: boom\n    at render (x:1:1)"), &request),
            Some("Error: boom".to_owned())
        );
        assert_eq!(
            script_message(Some("Error: a\u{200B}b\u{202E}c\u{7}d"), &request),
            Some("Error: abcd".to_owned())
        );
        let long = format!("Error: {}", "é".repeat(500));
        let cut = script_message(Some(&long), &request).expect("kept");
        assert_eq!(cut.chars().count(), MAX_SCRIPT_MESSAGE_CHARS);
        assert_eq!(script_message(Some("\u{200B}\n"), &request), None);
        assert_eq!(script_message(None, &request), None);
        // Whatever survives the cleaning, the contract accepts.
        for text in ["\u{2028}x", "a\u{FFFA}b", "x\u{E0001}y"] {
            let message = script_message(Some(text), &request);
            let verdict = CheckVerdict::Failed {
                problems: vec![Problem::ScriptError { message }],
            };
            verdict.validate(&request).expect("valid");
        }
    }

    #[test]
    fn shadow_roots_of_the_page_are_found_in_the_snapshot() {
        let snapshot = |kinds: &[Option<&str>]| {
            let mut strings = vec!["#document".to_owned()];
            let mut index = Vec::new();
            let mut value = Vec::new();
            for (node, kind) in kinds.iter().enumerate() {
                if let Some(kind) = kind {
                    strings.push((*kind).to_owned());
                    index.push(node);
                    value.push(strings.len() - 1);
                }
            }
            json!({
                "strings": strings,
                "documents": [
                    {"nodes": {"nodeName": [0]}},
                    {"nodes": {"shadowRootType": {"index": index, "value": value}}},
                ],
            })
        };
        assert!(!has_page_shadow_root(&snapshot(&[None, None])));
        assert!(!has_page_shadow_root(&snapshot(&[
            None,
            Some("user-agent")
        ])));
        assert!(has_page_shadow_root(&snapshot(&[None, Some("open")])));
        assert!(has_page_shadow_root(&snapshot(&[Some("closed")])));
        assert!(!has_page_shadow_root(&json!({})));
    }

    #[test]
    fn the_browser_sees_a_request_without_the_page() {
        let request = request();
        let input = CompareInput {
            runtime: request.runtime,
            spec: &request.spec,
            datasets: &request.datasets,
            locale: request.locale,
            tz: &request.tz,
        };
        let value = serde_json::to_value(&input).expect("serializes");
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["datasets", "locale", "runtime", "spec", "tz"]);
        let call = serde_json::to_value(CallFunction {
            function_declaration: COMPARE_FUNCTION,
            execution_context_id: 3,
            arguments: [Argument { value: &input }],
            return_by_value: true,
            await_promise: true,
        })
        .expect("serializes");
        assert_eq!(call["executionContextId"], 3);
        assert_eq!(call["arguments"][0]["value"]["tz"], json!(request.tz));
        assert_eq!(call["returnByValue"], true);
    }
}
