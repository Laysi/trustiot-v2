//! A minimal Chrome DevTools Protocol client over the browser's pipe transport
//! (`--remote-debugging-pipe`): JSON messages, each ended by a NUL byte, written to the
//! browser's file descriptor 3 and read from its descriptor 4.
//!
//! Commands get their answers through [`Cdp::call`]. Events go, as they are read, to an
//! [`EventSink`], which keeps what it needs and may answer at once with [`Cdp::notify`]
//! (request interception must not wait for the check's flow). Nothing queues events, so a page
//! that makes the browser emit many cannot make the service hold them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// The largest message read from the browser. Larger ones are skipped, and every command then
/// waiting fails with [`CdpError::TooLarge`]: the browser answers a command about the page
/// (such as the document's tree) with a message that grows with the page.
pub const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

/// Why a command got no result. The browser's own error text is never kept: it can quote the
/// page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CdpError {
    #[error("the browser closed the protocol pipe")]
    Closed,
    #[error("the browser answered the command with an error")]
    Protocol,
    #[error("a message from the browser was larger than the limit")]
    TooLarge,
    #[error("a message from the browser was not a protocol message")]
    Malformed,
    #[error("a command could not be encoded")]
    Encode,
}

/// Receives the browser's events, in order, on the reading task.
pub trait EventSink: Send + Sync + 'static {
    /// One event; `session` is the target session it belongs to, `None` for the browser's own.
    fn event(&self, cdp: &Cdp, session: Option<&str>, method: &str, params: Value);

    /// The pipe closed or failed: no more events will come.
    fn closed(&self);
}

/// A connection to one browser.
#[derive(Clone, Debug)]
pub struct Cdp {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    out: mpsc::UnboundedSender<Vec<u8>>,
    next_id: AtomicU64,
    pending: Mutex<Pending>,
}

#[derive(Debug, Default)]
struct Pending {
    closed: bool,
    waiters: HashMap<u64, oneshot::Sender<Result<Value, CdpError>>>,
}

/// The connection's reading and writing tasks, aborted when dropped.
#[derive(Debug)]
pub struct Tasks {
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl Drop for Tasks {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

#[derive(Serialize)]
struct Outgoing<'a, P> {
    id: u64,
    method: &'a str,
    params: P,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
}

#[derive(Deserialize)]
struct Incoming {
    id: Option<u64>,
    method: Option<String>,
    #[serde(default)]
    params: Value,
    result: Option<Value>,
    error: Option<serde::de::IgnoredAny>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

impl Cdp {
    /// Starts a connection on the browser's pipe ends: `from_browser` is read, `to_browser`
    /// written. Must be called inside a Tokio runtime.
    pub fn start<R, W>(from_browser: R, to_browser: W, sink: Arc<dyn EventSink>) -> (Self, Tasks)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out, outgoing) = mpsc::unbounded_channel();
        let cdp = Self {
            inner: Arc::new(Inner {
                out,
                next_id: AtomicU64::new(1),
                pending: Mutex::new(Pending::default()),
            }),
        };
        let writer = tokio::spawn(write_loop(to_browser, outgoing, cdp.clone()));
        let reader = tokio::spawn(read_loop(from_browser, cdp.clone(), sink));
        (cdp, Tasks { reader, writer })
    }

    /// Sends a command and waits for its result. Dropping the future forgets the command.
    ///
    /// # Errors
    ///
    /// When the browser answers with an error, the pipe closes first, or the answer is larger
    /// than [`MAX_MESSAGE_BYTES`].
    pub async fn call<P: Serialize>(
        &self,
        session: Option<&str>,
        method: &str,
        params: &P,
    ) -> Result<Value, CdpError> {
        let id = self.next_id();
        let frame = encode(id, session, method, params)?;
        let (sender, answer) = oneshot::channel();
        {
            let mut pending = self.pending();
            if pending.closed {
                return Err(CdpError::Closed);
            }
            pending.waiters.insert(id, sender);
        }
        let _forget = Forget { cdp: self, id };
        if self.inner.out.send(frame).is_err() {
            return Err(CdpError::Closed);
        }
        answer.await.unwrap_or(Err(CdpError::Closed))
    }

    /// Sends a command without waiting for its result.
    pub fn notify<P: Serialize>(&self, session: Option<&str>, method: &str, params: &P) {
        let id = self.next_id();
        if let Ok(frame) = encode(id, session, method, params) {
            // A closed pipe drops it; the reader reports the close.
            let _ = self.inner.out.send(frame);
        }
    }

    fn next_id(&self) -> u64 {
        self.inner.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn pending(&self) -> MutexGuard<'_, Pending> {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn answer(&self, id: u64, result: Result<Value, CdpError>) {
        if let Some(waiter) = self.pending().waiters.remove(&id) {
            let _ = waiter.send(result);
        }
    }

    fn fail_all(&self, error: CdpError, close: bool) {
        let waiters = {
            let mut pending = self.pending();
            pending.closed |= close;
            std::mem::take(&mut pending.waiters)
        };
        for (_, waiter) in waiters {
            let _ = waiter.send(Err(error));
        }
    }
}

/// Removes a command's waiter when its caller stops waiting.
struct Forget<'a> {
    cdp: &'a Cdp,
    id: u64,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.cdp.pending().waiters.remove(&self.id);
    }
}

fn encode<P: Serialize>(
    id: u64,
    session: Option<&str>,
    method: &str,
    params: &P,
) -> Result<Vec<u8>, CdpError> {
    let mut frame = serde_json::to_vec(&Outgoing {
        id,
        method,
        params,
        session_id: session,
    })
    .map_err(|_| CdpError::Encode)?;
    frame.push(0);
    Ok(frame)
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut to_browser: W,
    mut outgoing: mpsc::UnboundedReceiver<Vec<u8>>,
    cdp: Cdp,
) {
    while let Some(frame) = outgoing.recv().await {
        if to_browser.write_all(&frame).await.is_err() || to_browser.flush().await.is_err() {
            break;
        }
    }
    cdp.fail_all(CdpError::Closed, true);
}

async fn read_loop<R: AsyncRead + Unpin>(mut from_browser: R, cdp: Cdp, sink: Arc<dyn EventSink>) {
    let mut chunk = vec![0_u8; 64 * 1024];
    let mut message = Vec::new();
    let mut skipping = false;
    loop {
        let read = match from_browser.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let mut rest = chunk.get(..read).unwrap_or_default();
        while !rest.is_empty() {
            let end = rest.iter().position(|byte| *byte == 0);
            let (part, after) = match end {
                Some(end) => (
                    rest.get(..end).unwrap_or_default(),
                    rest.get(end + 1..).unwrap_or_default(),
                ),
                None => (rest, &[][..]),
            };
            if !skipping {
                if message.len() + part.len() > MAX_MESSAGE_BYTES {
                    message = Vec::new();
                    skipping = true;
                } else {
                    message.extend_from_slice(part);
                }
            }
            if end.is_some() {
                if skipping {
                    skipping = false;
                    cdp.fail_all(CdpError::TooLarge, false);
                } else {
                    dispatch(&cdp, sink.as_ref(), &std::mem::take(&mut message));
                }
            }
            rest = after;
        }
    }
    cdp.fail_all(CdpError::Closed, true);
    sink.closed();
}

fn dispatch(cdp: &Cdp, sink: &dyn EventSink, message: &[u8]) {
    let Ok(incoming) = serde_json::from_slice::<Incoming>(message) else {
        cdp.fail_all(CdpError::Malformed, false);
        return;
    };
    if let Some(id) = incoming.id {
        let result = match (incoming.error, incoming.result) {
            (None, Some(result)) => Ok(result),
            (Some(_), _) => Err(CdpError::Protocol),
            (None, None) => Err(CdpError::Malformed),
        };
        cdp.answer(id, result);
    } else if let Some(method) = incoming.method {
        sink.event(
            cdp,
            incoming.session_id.as_deref(),
            &method,
            incoming.params,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, BufReader, DuplexStream, duplex};

    use super::*;

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<(Option<String>, String, Value)>>,
        closed: Mutex<bool>,
    }

    impl EventSink for Recorder {
        fn event(&self, cdp: &Cdp, session: Option<&str>, method: &str, params: Value) {
            if method == "Test.reply" {
                cdp.notify(session, "Test.replied", &params);
            }
            self.events.lock().expect("lock").push((
                session.map(str::to_owned),
                method.to_owned(),
                params,
            ));
        }

        fn closed(&self) {
            *self.closed.lock().expect("lock") = true;
        }
    }

    /// The browser's side of the pipes: reads commands, writes messages.
    struct FakeBrowser {
        commands: BufReader<DuplexStream>,
        messages: DuplexStream,
    }

    impl FakeBrowser {
        async fn command(&mut self) -> Value {
            let mut frame = Vec::new();
            let read = tokio::time::timeout(
                Duration::from_secs(30),
                self.commands.read_until(0, &mut frame),
            )
            .await
            .expect("a command arrives")
            .expect("reads");
            assert!(read > 0, "the pipe closed");
            assert_eq!(frame.pop(), Some(0), "commands end with NUL");
            serde_json::from_slice(&frame).expect("JSON")
        }

        async fn send(&mut self, message: &[u8]) {
            self.messages.write_all(message).await.expect("writes");
            self.messages.write_all(&[0]).await.expect("writes");
        }
    }

    fn connect() -> (Cdp, Tasks, FakeBrowser, Arc<Recorder>) {
        let (to_browser, commands) = duplex(1 << 20);
        let (messages, from_browser) = duplex(1 << 20);
        let recorder = Arc::new(Recorder::default());
        let (cdp, tasks) = Cdp::start(from_browser, to_browser, recorder.clone());
        (
            cdp,
            tasks,
            FakeBrowser {
                commands: BufReader::new(commands),
                messages,
            },
            recorder,
        )
    }

    #[tokio::test]
    async fn commands_get_their_own_answers() {
        let (cdp, _tasks, mut browser, _recorder) = connect();
        let first = tokio::spawn({
            let cdp = cdp.clone();
            async move { cdp.call(None, "Browser.getVersion", &json!({})).await }
        });
        let one = browser.command().await;
        assert_eq!(one["method"], "Browser.getVersion");
        assert!(one.get("sessionId").is_none(), "{one}");
        let second = tokio::spawn({
            let cdp = cdp.clone();
            async move { cdp.call(Some("S1"), "Page.enable", &json!({"a": 1})).await }
        });
        let two = browser.command().await;
        assert_eq!(two["sessionId"], "S1");
        assert_eq!(two["params"], json!({"a": 1}));
        // Answered out of order.
        browser
            .send(format!(r#"{{"id":{},"result":{{"ok":2}}}}"#, two["id"]).as_bytes())
            .await;
        browser
            .send(
                format!(
                    r#"{{"id":{},"error":{{"code":-32000,"message":"page text"}}}}"#,
                    one["id"]
                )
                .as_bytes(),
            )
            .await;
        assert_eq!(second.await.expect("joins"), Ok(json!({"ok": 2})));
        assert_eq!(first.await.expect("joins"), Err(CdpError::Protocol));
    }

    #[tokio::test]
    async fn events_reach_the_sink_and_it_can_answer() {
        let (_cdp, _tasks, mut browser, recorder) = connect();
        browser
            .send(br#"{"method":"Test.reply","params":{"n":1},"sessionId":"S2"}"#)
            .await;
        let reply = browser.command().await;
        assert_eq!(reply["method"], "Test.replied");
        assert_eq!(reply["sessionId"], "S2");
        assert_eq!(reply["params"], json!({"n": 1}));
        let events = recorder.events.lock().expect("lock").clone();
        assert_eq!(
            events,
            vec![(
                Some("S2".to_owned()),
                "Test.reply".to_owned(),
                json!({"n": 1})
            )]
        );
    }

    #[tokio::test]
    async fn an_oversized_message_fails_the_waiting_commands_and_reading_goes_on() {
        let (cdp, _tasks, mut browser, recorder) = connect();
        let waiting = tokio::spawn({
            let cdp = cdp.clone();
            async move { cdp.call(None, "DOM.getDocument", &json!({})).await }
        });
        let command = browser.command().await;
        let big = vec![b' '; MAX_MESSAGE_BYTES + 1];
        let writer = tokio::spawn(async move {
            browser.send(&big).await;
            browser
                .send(br#"{"method":"Test.after","params":{}}"#)
                .await;
            browser
        });
        assert_eq!(waiting.await.expect("joins"), Err(CdpError::TooLarge));
        let mut browser = writer.await.expect("joins");
        // The connection still works.
        let next = tokio::spawn({
            let cdp = cdp.clone();
            async move { cdp.call(None, "Next", &json!({})).await }
        });
        let next_command = browser.command().await;
        assert_ne!(next_command["id"], command["id"]);
        browser
            .send(format!(r#"{{"id":{},"result":{{}}}}"#, next_command["id"]).as_bytes())
            .await;
        assert_eq!(next.await.expect("joins"), Ok(json!({})));
        let methods: Vec<String> = recorder
            .events
            .lock()
            .expect("lock")
            .iter()
            .map(|(_, method, _)| method.clone())
            .collect();
        assert_eq!(methods, ["Test.after"]);
    }

    #[tokio::test]
    async fn a_closed_pipe_fails_every_command_and_tells_the_sink() {
        let (cdp, _tasks, browser, recorder) = connect();
        let waiting = tokio::spawn({
            let cdp = cdp.clone();
            async move { cdp.call(None, "Slow", &json!({})).await }
        });
        drop(browser);
        assert_eq!(waiting.await.expect("joins"), Err(CdpError::Closed));
        assert_eq!(
            cdp.call(None, "Later", &json!({})).await,
            Err(CdpError::Closed)
        );
        let closed = tokio::time::timeout(Duration::from_secs(30), async {
            while !*recorder.closed.lock().expect("lock") {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(closed.is_ok(), "the sink hears of the close");
    }

    #[tokio::test]
    async fn malformed_messages_fail_the_waiting_commands() {
        let (cdp, _tasks, mut browser, _recorder) = connect();
        let waiting = tokio::spawn({
            let cdp = cdp.clone();
            async move { cdp.call(None, "X", &json!({})).await }
        });
        let _ = browser.command().await;
        browser.send(b"not json").await;
        assert_eq!(waiting.await.expect("joins"), Err(CdpError::Malformed));
    }

    #[tokio::test]
    async fn messages_split_across_reads_are_joined() {
        let (cdp, _tasks, mut browser, _recorder) = connect();
        let waiting = tokio::spawn({
            let cdp = cdp.clone();
            async move { cdp.call(None, "X", &json!({})).await }
        });
        let command = browser.command().await;
        let message = format!(r#"{{"id":{},"result":{{"value":"abc"}}}}"#, command["id"]);
        let (head, tail) = message.as_bytes().split_at(7);
        browser.messages.write_all(head).await.expect("writes");
        browser.messages.flush().await.expect("flushes");
        tokio::task::yield_now().await;
        browser.messages.write_all(tail).await.expect("writes");
        browser.messages.write_all(&[0]).await.expect("writes");
        assert_eq!(waiting.await.expect("joins"), Ok(json!({"value": "abc"})));
    }
}
