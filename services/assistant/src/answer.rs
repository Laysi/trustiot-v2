//! The answer tool (Laysi/trustiot-v2#147, the user's decision of 2026-11-05): the model hands
//! over its final answer by calling [`ANSWER_TOOL`], and only that answer streams before the
//! question ends, a paragraph at a time. Nothing else the model writes is ever sent: not the text
//! it writes before or between tool calls, not its thinking, not tool calls or their results.
//! An answer written as plain text (a turn without tool calls) goes out only once the question
//! answered, as before.
//!
//! - [`spec`] is the tool's definition. It is part of a new conversation's fixed preamble
//!   ([`crate::Agent::start_conversation`]); a conversation that started without it never treats
//!   a call of that name as its answer. The request asks for its input as the model writes it
//!   (`eager_input_streaming`, `model::anthropic`), so its text arrives in pieces.
//! - [`TextDecoder`] reads the tool's input, `{"text": "..."}`, from those pieces of JSON as they
//!   come, whatever the boundaries: escapes, `\uXXXX` with surrogate pairs. Anything else (another
//!   key, bad syntax, a lone surrogate, more than [`MAX_ANSWER_BYTES`]) fails it closed: it
//!   decodes nothing more, and the turn's end decides.
//! - [`Release`] decides how much of the decoded text may go out: the answer guard's cleaned
//!   text ([`answer_guard::guard_streamed`]) of the text up to [`release_point`], which is a
//!   paragraph break before which nothing can change any more. What it releases is always the
//!   start of the answer that is stored ([`crate::agent`] checks it before storing, and fails the
//!   question otherwise), so a client never has to take text back.

use serde_json::json;

use crate::answer_guard::{self, Finding};
use crate::tools::ToolSpec;

/// The answer tool's name.
pub const ANSWER_TOOL: &str = "answer";

/// The most text the answer tool takes, in UTF-8 bytes. The default output limit
/// (`model.max_output_tokens`, 16,000 tokens) writes far less; more fails the question.
pub const MAX_ANSWER_BYTES: usize = 256 << 10;

/// The answer tool's description: when to call it, and what the answer may hold (the answer
/// guard's rules, which the system prompt states too).
const DESCRIPTION: &str = "\
Delivers your final answer to the user. When you have everything you need, call this tool once, \
alone, with the whole answer in `text`, and do not call it before the other tools you need have \
returned. The user sees only this text, streamed paragraph by paragraph while you write it; \
nothing else you write is shown. Write prose for people who do not read code, in the language of \
the user's question: no SQL, HTML or other code, not even in a code block. Separate paragraphs \
with a blank line.";

/// The answer tool's definition.
#[must_use]
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: ANSWER_TOOL.to_owned(),
        description: DESCRIPTION.to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The whole answer, as the user reads it.",
                },
            },
            "required": ["text"],
            "additionalProperties": false,
        }),
    }
}

/// The most bytes of a key [`TextDecoder`] reads: the tool has one, `text`.
const MAX_KEY_BYTES: usize = 64;

/// Where [`TextDecoder`] is in `{"text": "..."}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Before the object's `{`.
    Start,
    /// After `{` or `,`: a key next (or `}` right after `{`).
    BeforeKey {
        first: bool,
    },
    /// In a key.
    Key,
    /// After a key: its `:` next.
    AfterKey,
    /// After `:`: the value next.
    BeforeValue,
    /// In the `text` string.
    Text,
    /// After a value: `,` or `}` next.
    AfterValue,
    /// After the object's `}`: only blanks.
    Done,
    Failed,
}

/// Inside a string: a pending escape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Escape {
    None,
    /// After `\`.
    Backslash,
    /// After `\u` and `digits` hex digits of `value`.
    Unicode {
        value: u32,
        digits: u8,
    },
    /// After a high surrogate's `\uXXXX`, waiting for the `\` of its low surrogate.
    LowBackslash {
        high: u32,
    },
    /// After that `\`, waiting for its `u`.
    LowU {
        high: u32,
    },
    /// After the low surrogate's `\u` and `digits` of its hex digits.
    Low {
        high: u32,
        value: u32,
        digits: u8,
    },
}

/// Decodes the answer tool's `text` from its input JSON, piece by piece (see the module
/// documentation).
#[derive(Debug)]
pub struct TextDecoder {
    state: State,
    escape: Escape,
    key: String,
    /// `text` was read once already: a second one fails (its last value would win in a parser).
    seen_text: bool,
    text: String,
}

impl Default for TextDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl TextDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: State::Start,
            escape: Escape::None,
            key: String::new(),
            seen_text: false,
            text: String::new(),
        }
    }

    /// The text decoded so far.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether the input was not the tool's: nothing more is decoded.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.state == State::Failed
    }

    /// Whether the object closed (`}`) with its `text`.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.state == State::Done && self.seen_text
    }

    /// Reads the next piece of the input.
    pub fn push(&mut self, piece: &str) {
        for character in piece.chars() {
            if self.state == State::Failed {
                return;
            }
            self.step(character);
        }
    }

    fn fail(&mut self) {
        self.state = State::Failed;
    }

    fn step(&mut self, character: char) {
        let blank = matches!(character, ' ' | '\t' | '\n' | '\r');
        match self.state {
            State::Failed => {}
            State::Start => match character {
                '{' => self.state = State::BeforeKey { first: true },
                _ if blank => {}
                _ => self.fail(),
            },
            State::BeforeKey { first } => match character {
                '"' => {
                    self.key.clear();
                    self.state = State::Key;
                }
                '}' if first => self.state = State::Done,
                _ if blank => {}
                _ => self.fail(),
            },
            State::Key => {
                if let Some(decoded) = self.string_character(character) {
                    match decoded {
                        StringPart::End => self.state = State::AfterKey,
                        StringPart::Char(c) => {
                            if self.key.len() + c.len_utf8() > MAX_KEY_BYTES {
                                self.fail();
                            } else {
                                self.key.push(c);
                            }
                        }
                    }
                }
            }
            State::AfterKey => match character {
                ':' => self.state = State::BeforeValue,
                _ if blank => {}
                _ => self.fail(),
            },
            State::BeforeValue => match character {
                '"' if self.key == "text" && !self.seen_text => {
                    self.seen_text = true;
                    self.state = State::Text;
                }
                _ if blank => {}
                _ => self.fail(),
            },
            State::Text => {
                if let Some(decoded) = self.string_character(character) {
                    match decoded {
                        StringPart::End => self.state = State::AfterValue,
                        StringPart::Char(c) => {
                            if self.text.len() + c.len_utf8() > MAX_ANSWER_BYTES {
                                self.fail();
                            } else {
                                self.text.push(c);
                            }
                        }
                    }
                }
            }
            State::AfterValue => match character {
                ',' => self.state = State::BeforeKey { first: false },
                '}' => self.state = State::Done,
                _ if blank => {}
                _ => self.fail(),
            },
            State::Done => {
                if !blank {
                    self.fail();
                }
            }
        }
    }

    /// One character inside a string: `None` while an escape is still pending or when it failed.
    fn string_character(&mut self, character: char) -> Option<StringPart> {
        let hex = |c: char| c.to_digit(16);
        match self.escape {
            Escape::None => match character {
                '"' => Some(StringPart::End),
                '\\' => {
                    self.escape = Escape::Backslash;
                    None
                }
                // JSON strings take no raw control characters.
                c if u32::from(c) < 0x20 => {
                    self.fail();
                    None
                }
                c => Some(StringPart::Char(c)),
            },
            Escape::Backslash => {
                self.escape = Escape::None;
                let decoded = match character {
                    '"' => '"',
                    '\\' => '\\',
                    '/' => '/',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    'u' => {
                        self.escape = Escape::Unicode {
                            value: 0,
                            digits: 0,
                        };
                        return None;
                    }
                    _ => {
                        self.fail();
                        return None;
                    }
                };
                Some(StringPart::Char(decoded))
            }
            Escape::Unicode { value, digits } => {
                let Some(digit) = hex(character) else {
                    self.fail();
                    return None;
                };
                let value = value * 16 + digit;
                if digits + 1 < 4 {
                    self.escape = Escape::Unicode {
                        value,
                        digits: digits + 1,
                    };
                    return None;
                }
                self.escape = Escape::None;
                match value {
                    0xD800..=0xDBFF => {
                        self.escape = Escape::LowBackslash { high: value };
                        None
                    }
                    0xDC00..=0xDFFF => {
                        self.fail();
                        None
                    }
                    _ => match char::from_u32(value) {
                        Some(c) => Some(StringPart::Char(c)),
                        None => {
                            self.fail();
                            None
                        }
                    },
                }
            }
            Escape::LowBackslash { high } => {
                if character == '\\' {
                    self.escape = Escape::LowU { high };
                } else {
                    self.fail();
                }
                None
            }
            Escape::LowU { high } => {
                if character == 'u' {
                    self.escape = Escape::Low {
                        high,
                        value: 0,
                        digits: 0,
                    };
                } else {
                    self.fail();
                }
                None
            }
            Escape::Low {
                high,
                value,
                digits,
            } => {
                let Some(digit) = hex(character) else {
                    self.fail();
                    return None;
                };
                let value = value * 16 + digit;
                if digits + 1 < 4 {
                    self.escape = Escape::Low {
                        high,
                        value,
                        digits: digits + 1,
                    };
                    return None;
                }
                self.escape = Escape::None;
                if !(0xDC00..=0xDFFF).contains(&value) {
                    self.fail();
                    return None;
                }
                let code = 0x10000 + ((high - 0xD800) << 10) + (value - 0xDC00);
                match char::from_u32(code) {
                    Some(c) => Some(StringPart::Char(c)),
                    None => {
                        self.fail();
                        None
                    }
                }
            }
        }
    }
}

enum StringPart {
    Char(char),
    End,
}

/// How much of `text`, the start of an answer still being written, is final for the stream: a
/// paragraph start (after `\n\n`) at or before [`answer_guard::pending`] that no finding of
/// [`answer_guard::find`] crosses. Before it, every finding is final and whole (`pending`), and
/// cutting there splits none, so the guard reads the start alone as it reads it in the whole
/// answer.
#[must_use]
pub fn release_point(text: &str) -> usize {
    let findings = answer_guard::find(text);
    let mut point = answer_guard::pending(text);
    while let Some(crossing) = findings.iter().find(|finding| crosses(finding, point)) {
        let start = crossing.range.start;
        point = text
            .get(..start)
            .and_then(|before| before.rfind("\n\n"))
            .map_or(0, |blank| blank + 2);
    }
    point
}

fn crosses(finding: &Finding, point: usize) -> bool {
    finding.range.start < point && finding.range.end > point
}

/// What of an answer being written has gone out (see the module documentation).
#[derive(Debug, Default)]
pub struct Release {
    released: String,
    /// The length of the decoded text when the release was last worked out.
    checked: usize,
}

impl Release {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The text released so far.
    #[must_use]
    pub fn released(&self) -> &str {
        &self.released
    }

    /// Works out the release again for `text`, the decoded answer so far, when what was added
    /// since the last time may have ended a paragraph or a tag (a line break or a `>`): the
    /// guard's scans run over the whole text, so they do not run for every piece. Returns the
    /// released text when it grew.
    pub fn advance(&mut self, text: &str) -> Option<&str> {
        let added = text.get(self.checked..)?;
        if added.is_empty() || !added.contains(['\n', '>']) {
            return None;
        }
        self.checked = text.len();
        let point = release_point(text);
        let candidate = answer_guard::guard_streamed(text.get(..point)?).text;
        // Only ever a continuation of what went out.
        if candidate.len() > self.released.len() && candidate.starts_with(&self.released) {
            self.released = candidate;
            return Some(&self.released);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `text` in `range`, which the tests take only between characters.
    fn cut<R: std::slice::SliceIndex<str, Output = str>>(text: &str, range: R) -> &str {
        text.get(range).expect("a range between characters")
    }

    fn decode(pieces: &[&str]) -> TextDecoder {
        let mut decoder = TextDecoder::new();
        for piece in pieces {
            decoder.push(piece);
        }
        decoder
    }

    /// Every split of `json` into two and three pieces decodes to what serde_json reads.
    fn decodes_like_serde(json: &str) {
        let expected = serde_json::from_str::<serde_json::Value>(json).expect("valid JSON")["text"]
            .as_str()
            .expect("a text")
            .to_owned();
        let bounds: Vec<usize> = (0..=json.len())
            .filter(|&at| json.is_char_boundary(at))
            .collect();
        for &a in &bounds {
            let whole = decode(&[cut(json, ..a), cut(json, a..)]);
            assert!(whole.complete(), "{json:?} split at {a}");
            assert_eq!(whole.text(), expected, "{json:?} split at {a}");
            for &b in bounds.iter().filter(|&&b| b >= a).step_by(3) {
                let three = decode(&[cut(json, ..a), cut(json, a..b), cut(json, b..)]);
                assert!(three.complete(), "{json:?} split at {a}, {b}");
                assert_eq!(three.text(), expected, "{json:?} split at {a}, {b}");
            }
        }
    }

    #[test]
    fn the_text_decodes_whatever_the_pieces() {
        for json in [
            r#"{"text": "Twelve meters reported."}"#,
            r#" { "text" : "a\"b\\c\/d\b\f\n\r\te" } "#,
            r#"{"text":"用電 😀 and é"}"#,
            "{\n  \"text\": \"第三層 😀 用電\\n\\n第二段\"\n}",
            r#"{"text":""}"#,
            r#"{"text":"escaped key"}"#,
        ] {
            decodes_like_serde(json);
        }
    }

    #[test]
    fn what_is_not_the_tools_input_fails_closed() {
        for json in [
            r#"["text"]"#,
            r#"{"other": "x"}"#,
            r#"{"text": 5}"#,
            r#"{"text": "a", "text": "b"}"#,
            r#"{"text": "a", "more": "b"}"#,
            r#"{"text": "lone \ud83d here"}"#,
            r#"{"text": "low first \ude00"}"#,
            r#"{"text": "bad \x escape"}"#,
            r#"{"text": "bad \u12g4"}"#,
            "{\"text\": \"raw\ncontrol\"}",
            r#"{"text": "a"} trailing"#,
            r#"{"text" "missing colon"}"#,
        ] {
            let decoder = decode(&[json]);
            assert!(decoder.failed(), "{json:?}");
            assert!(!decoder.complete(), "{json:?}");
        }
        // Text decoded before the fault stays; nothing after it is added.
        let decoder = decode(&[r#"{"text": "kept \q lost"}"#]);
        assert!(decoder.failed());
        assert_eq!(decoder.text(), "kept ");
    }

    #[test]
    fn an_unfinished_input_is_not_complete_and_keeps_what_it_decoded() {
        let decoder = decode(&[r#"{"text": "cut off at the li"#]);
        assert!(!decoder.failed());
        assert!(!decoder.complete());
        assert_eq!(decoder.text(), "cut off at the li");
        // A surrogate pair cut between its halves decodes nothing of it yet.
        let decoder = decode(&[r#"{"text": "x\ud83d"#]);
        assert!(!decoder.failed());
        assert_eq!(decoder.text(), "x");
    }

    #[test]
    fn the_text_is_capped() {
        let mut decoder = TextDecoder::new();
        decoder.push(r#"{"text": ""#);
        let line = "a".repeat(1024);
        for _ in 0..MAX_ANSWER_BYTES / 1024 {
            decoder.push(&line);
        }
        assert!(!decoder.failed());
        assert_eq!(decoder.text().len(), MAX_ANSWER_BYTES);
        decoder.push("b");
        assert!(decoder.failed());
        assert_eq!(decoder.text().len(), MAX_ANSWER_BYTES);
    }

    /// Answers with SQL, HTML (tags across paragraphs, comments, references), code fences and
    /// inline code, CRLF and blank-line runs.
    const ANSWERS: [&str; 9] = [
        "Floor 3 used 412 kWh.\n\nThe peak was 35 kW at 14:00.\n\nQuery: select value from devices.\n\nThat is all.",
        "Before <div class=\"x\"\n\ntitle='a>b'>after\n\n`x = 1` and `kwh`\n\n```sql\nSELECT kwh FROM meters\n```\n\nEnd &lt;b&gt;bold&lt;/b&gt;.",
        "a<b and\n\nc>d\n\n<!-- note\n\nstill -->\n\n請執行 select max(kwh) 看尖峰。\n\nx<y",
        "  Leading blanks\r\n\r\n\r\n\r\nmany blank lines\n\n\n\n- item\n- <b></b>\n\n> quote `f(x)`\n\nend",
        "<script>\nif (a < b) {}\n\n</script>\n\nSELECT kwh\nFROM meters\n\n<!DOCTYPE html SYSTEM \"x>y\">ok\n\nlast",
        "- item <img src=\"a.png\"\n\n  alt='x'>\n\n> quote: select device_id, ts from t\n\n```\nopen fence\n\nmore",
        "第一段，沒有任何程式碼。\n\n第二段 😀 也沒有。\n\n第三段寫完了。",
        "Intro.\n\n<div\n\nclass=x> more\n\nnext <span\n\ntitle=\"a\"\n\n> tail\n\nend",
        "Text `SELECT\n\nkwh` and ~~~\ncode\n~~~\n\nafter <!-- c\n\n--> done\n\nselect kwh from meters\n\nbye",
    ];

    /// Splits of `text` into pieces at every `step`th character boundary, from `offset`.
    fn pieces(text: &str, step: usize, offset: usize) -> Vec<&str> {
        let bounds: Vec<usize> = (0..=text.len())
            .filter(|&at| text.is_char_boundary(at))
            .collect();
        let mut cuts: Vec<usize> = bounds.iter().copied().skip(offset).step_by(step).collect();
        cuts.insert(0, 0);
        cuts.push(text.len());
        cuts.dedup();
        cuts.windows(2).map(|w| cut(text, w[0]..w[1])).collect()
    }

    /// The release invariant: whatever the chunking, everything released is the start of the
    /// cleaned whole answer, it only grows, and it never holds what the guard removes.
    #[test]
    fn what_is_released_is_always_the_start_of_the_stored_answer() {
        for answer in ANSWERS {
            let stored = answer_guard::guard_streamed(answer).text;
            for step in [1, 2, 3, 5, 8, 13, 40] {
                for offset in [0, 1, 2] {
                    let mut text = String::new();
                    let mut release = Release::new();
                    let mut last = String::new();
                    for piece in pieces(answer, step, offset) {
                        text.push_str(piece);
                        if let Some(released) = release.advance(&text) {
                            assert!(
                                stored.starts_with(released),
                                "{answer:?} step {step}: released {released:?}, stored {stored:?}"
                            );
                            assert!(released.starts_with(&last), "it only grows");
                            last = released.to_owned();
                        }
                    }
                    // What the guard removes from the whole answer never goes out.
                    for marker in [
                        "select", "SELECT", "<div", "<b>", "<script", "<!--", "f(x)", "x = 1",
                        "&lt;b",
                    ] {
                        assert!(
                            stored.contains(marker) || !release.released().contains(marker),
                            "{answer:?}: {marker} released"
                        );
                    }
                    // Nothing of the last paragraph goes out before the end.
                    let before_last = answer.rfind("\n\n").map_or("", |at| cut(answer, ..at));
                    assert!(
                        answer_guard::guard_streamed(before_last)
                            .text
                            .starts_with(release.released()),
                        "{answer:?}: the last paragraph went out early"
                    );
                }
            }
        }
    }

    /// The same invariant over answers put together at random from pieces the guard reads
    /// differently depending on what follows them, cut at random.
    #[test]
    fn random_answers_keep_the_release_invariant() {
        const PIECES: [&str; 32] = [
            "\n\n",
            "\n\n",
            "\n\n\n",
            "\r\n\r\n",
            "\n",
            "Text ",
            "用電 ",
            "😀 ",
            "<div",
            " class=\"a>b\"",
            ">",
            "</div>",
            "<!--",
            "-->",
            "<!DOCTYPE x",
            "&lt;b&gt;",
            "select ",
            "SELECT kwh ",
            "FROM meters",
            "from devices",
            " where x = 1",
            ";",
            "`",
            "``",
            "f(x)",
            "```\n",
            "~~~\n",
            "- ",
            "> ",
            "x<y ",
            "a < b ",
            "<https://example.com/a> ",
        ];
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            usize::try_from(seed % u64::try_from(bound).expect("a bound")).expect("an index")
        };
        for _ in 0..400 {
            let length = 4 + next(28);
            let answer: String = (0..length).map(|_| PIECES[next(PIECES.len())]).collect();
            let stored = answer_guard::guard_streamed(&answer).text;
            let mut text = String::new();
            let mut release = Release::new();
            let mut rest = answer.as_str();
            while !rest.is_empty() {
                let mut cut = (1 + next(12)).min(rest.len());
                while !rest.is_char_boundary(cut) {
                    cut += 1;
                }
                let (piece, after) = rest.split_at(cut);
                rest = after;
                text.push_str(piece);
                if let Some(released) = release.advance(&text) {
                    assert!(
                        stored.starts_with(released),
                        "{answer:?}: released {released:?}, stored {stored:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn whole_paragraphs_go_out_before_the_answer_ends() {
        let mut release = Release::new();
        let mut text = String::from("First paragraph.\n\nSecond");
        assert_eq!(release.advance(&text), Some("First paragraph."));
        text.push_str(" paragraph.");
        assert_eq!(release.advance(&text), None, "no line break added");
        text.push_str("\n\nThird");
        assert_eq!(
            release.advance(&text),
            Some("First paragraph.\n\nSecond paragraph.")
        );
    }

    #[test]
    fn a_tag_that_crosses_the_pending_point_holds_its_paragraph_back() {
        // The tag closes in the last paragraph: `pending` is that paragraph's start, inside the
        // tag. Cut there, the start would read `<div` as text with no `>`, and keep it.
        let text = "Intro.\n\n<div\n\nclass=x> more";
        assert_eq!(
            cut(text, ..answer_guard::pending(text)),
            "Intro.\n\n<div\n\n"
        );
        let point = release_point(text);
        assert_eq!(cut(text, ..point), "Intro.\n\n");
        let mut release = Release::new();
        assert_eq!(release.advance(text), Some("Intro."));
        // A tag closed in an earlier paragraph is final and removed.
        let text = "Intro.\n\n<div\n\nclass=x> after\n\nnext";
        assert_eq!(
            cut(text, ..release_point(text)),
            "Intro.\n\n<div\n\nclass=x> after\n\n"
        );
        let mut release = Release::new();
        assert_eq!(release.advance(text), Some("Intro.\n\n after"));
    }

    #[test]
    fn the_tool_is_one_required_text() {
        let spec = spec();
        assert_eq!(spec.name, ANSWER_TOOL);
        assert_eq!(spec.input_schema["required"], json!(["text"]));
        assert_eq!(spec.input_schema["additionalProperties"], json!(false));
        assert!(spec.description.contains("no SQL, HTML or other code"));
    }
}
