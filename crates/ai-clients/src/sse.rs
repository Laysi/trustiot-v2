//! Incremental parser for server-sent events, as the WHATWG HTML standard defines the
//! `text/event-stream` format (<https://html.spec.whatwg.org/multipage/server-sent-events.html#parsing-an-event-stream>).
//!
//! Bytes arrive in arbitrary chunks; [`SseParser::push`] returns every event a chunk completes.
//! Only `event` and `data` matter to the providers here; `id` and `retry` are ignored, as are
//! comment lines (starting with `:`).

use std::collections::VecDeque;

/// One dispatched event: its type (`event:` field, `message` when absent) and its data, the
/// `data:` lines joined with `\n`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

impl SseEvent {
    /// The event in wire format (`event:` line, one `data:` line per data line, blank line),
    /// for relaying it unchanged.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.event.len() + self.data.len() + 16);
        out.extend_from_slice(b"event: ");
        out.extend_from_slice(self.event.as_bytes());
        out.push(b'\n');
        for line in self.data.split('\n') {
            out.extend_from_slice(b"data: ");
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
        out.push(b'\n');
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SseError {
    #[error("a server-sent event exceeds {limit} bytes")]
    EventTooLarge { limit: usize },
    #[error("a server-sent event line is not valid UTF-8")]
    InvalidUtf8,
}

/// Turns a byte stream into [`SseEvent`]s. Memory is bounded: an event (its pending line plus
/// the data collected so far) larger than `max_event_bytes` is an error, not an allocation.
#[derive(Debug)]
pub struct SseParser {
    line: Vec<u8>,
    data: String,
    has_data: bool,
    event: Option<String>,
    /// The previous chunk ended in `\r`; a `\n` at the start of the next one belongs to it.
    skip_lf: bool,
    at_start: bool,
    /// Bytes of the byte order mark matched so far at the start of the stream.
    bom_seen: usize,
    max_event_bytes: usize,
}

const BOM: &[u8] = b"\xEF\xBB\xBF";

impl SseParser {
    pub fn new(max_event_bytes: usize) -> Self {
        Self {
            line: Vec::new(),
            data: String::new(),
            has_data: false,
            event: None,
            skip_lf: false,
            at_start: true,
            bom_seen: 0,
            max_event_bytes,
        }
    }

    /// Feeds one chunk and appends the events it completes to `out`.
    pub fn push(&mut self, chunk: &[u8], out: &mut VecDeque<SseEvent>) -> Result<(), SseError> {
        let mut chunk = chunk;
        if self.at_start {
            // The stream may start with a byte order mark, possibly split across chunks.
            let mut matched = 0;
            while let (Some(byte), Some(expected)) = (chunk.get(matched), BOM.get(self.bom_seen)) {
                if byte != expected {
                    break;
                }
                self.bom_seen += 1;
                matched += 1;
            }
            let (_, rest) = chunk.split_at(matched);
            if self.bom_seen == BOM.len() {
                self.at_start = false;
            } else if rest.is_empty() {
                return Ok(());
            } else {
                // Not a byte order mark after all: the bytes held back are line content.
                self.at_start = false;
                let (held, _) = BOM.split_at(self.bom_seen);
                self.feed(held, out)?;
            }
            chunk = rest;
        }
        self.feed(chunk, out)
    }

    fn feed(&mut self, bytes: &[u8], out: &mut VecDeque<SseEvent>) -> Result<(), SseError> {
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => self.end_line(out)?,
                b'\r' => {
                    self.skip_lf = true;
                    self.end_line(out)?;
                }
                _ => {
                    if self.line.len() + self.data.len() >= self.max_event_bytes {
                        return Err(SseError::EventTooLarge {
                            limit: self.max_event_bytes,
                        });
                    }
                    self.line.push(byte);
                }
            }
        }
        Ok(())
    }

    /// Whether bytes of an event that was never completed (no blank line) are pending. The
    /// standard discards such an event at the end of the stream.
    pub fn has_partial_event(&self) -> bool {
        !self.line.is_empty() || self.has_data || self.event.is_some()
    }

    fn end_line(&mut self, out: &mut VecDeque<SseEvent>) -> Result<(), SseError> {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            self.dispatch(out);
            return Ok(());
        }
        let line = String::from_utf8(line).map_err(|_| SseError::InvalidUtf8)?;
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_str(), ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
                if self.data.len() > self.max_event_bytes {
                    return Err(SseError::EventTooLarge {
                        limit: self.max_event_bytes,
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self, out: &mut VecDeque<SseEvent>) {
        let event = self.event.take();
        if !self.has_data {
            self.data.clear();
            return;
        }
        self.has_data = false;
        out.push_back(SseEvent {
            event: event.unwrap_or_else(|| "message".to_owned()),
            data: std::mem::take(&mut self.data),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_in_chunks(input: &[u8], size: usize) -> Vec<SseEvent> {
        let mut parser = SseParser::new(1 << 20);
        let mut out = VecDeque::new();
        for chunk in input.chunks(size) {
            parser.push(chunk, &mut out).unwrap();
        }
        out.into_iter().collect()
    }

    #[test]
    fn every_chunking_and_line_ending_gives_the_same_events() {
        let lf = "\u{FEFF}: comment\nevent: message_start\ndata: {\"a\":\n data:1}\n\nevent: ping\ndata: {}\n\ndata: x\n\n";
        let expected = vec![
            SseEvent {
                event: "message_start".into(),
                data: "{\"a\":".into(),
            },
            SseEvent {
                event: "ping".into(),
                data: "{}".into(),
            },
            SseEvent {
                event: "message".into(),
                data: "x".into(),
            },
        ];
        // " data:1" is a field named " data", which is ignored.
        for text in [
            lf.to_owned(),
            lf.replace('\n', "\r\n"),
            lf.replace('\n', "\r"),
        ] {
            for size in 1..=text.len() {
                assert_eq!(
                    parse_in_chunks(text.as_bytes(), size),
                    expected,
                    "chunk size {size}, input {text:?}"
                );
            }
        }
    }

    #[test]
    fn multi_line_data_is_joined_and_reencoded_unchanged() {
        let events = parse_in_chunks(b"event: e\ndata: one\ndata:two\n\n", 3);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "one\ntwo");
        assert_eq!(
            parse_in_chunks(&events[0].to_bytes(), 5),
            events,
            "re-encoded event parses to itself"
        );
    }

    #[test]
    fn multi_byte_characters_split_across_chunks_survive() {
        let text = "data: 閘道器 ✓\n\n".as_bytes();
        for size in 1..=text.len() {
            assert_eq!(parse_in_chunks(text, size)[0].data, "閘道器 ✓");
        }
    }

    #[test]
    fn an_oversized_event_is_an_error() {
        let mut parser = SseParser::new(16);
        let mut out = VecDeque::new();
        let error = parser
            .push(b"data: 0123456789\ndata: 0123456789\n", &mut out)
            .unwrap_err();
        assert_eq!(error, SseError::EventTooLarge { limit: 16 });
        let mut parser = SseParser::new(16);
        assert!(parser.push(&[b'a'; 17], &mut out).is_err());
    }

    #[test]
    fn invalid_utf8_is_an_error_and_a_partial_event_is_reported() {
        let mut parser = SseParser::new(1024);
        let mut out = VecDeque::new();
        assert_eq!(
            parser.push(b"data: \xff\n", &mut out),
            Err(SseError::InvalidUtf8)
        );
        let mut parser = SseParser::new(1024);
        parser.push(b"event: x\ndata: y\n", &mut out).unwrap();
        assert!(out.is_empty());
        assert!(parser.has_partial_event());
    }
}
