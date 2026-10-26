//! The answer guard (Laysi/trustiot-v2#150, acceptance criterion 「回答文字不出現 SQL 或 HTML」;
//! docs/architecture/ai-assistant.md, 「儀表板」): the user reads the answer, and the dashboard
//! shows the data, so an answer never carries the dashboard's SQL or page, nor any other code.
//! The prompt says so; this module makes it hold whatever the model writes.
//!
//! [`guard`] removes three closed kinds of content from an answer's text and reports which it
//! removed ([`RemovedContent`]):
//!
//! - [`Removed::Code`]: fenced code blocks (```` ``` ```` or `~~~`, CommonMark's rule, inside
//!   lists and quotes too), whatever their language; an unclosed fence runs to the end. And
//!   inline code spans that hold code: an `=`, `;`, `{`, `}`, `->` or a call such as `f(x)`.
//!   Other spans stay, such as a dataset's name, a unit or a time zone in backticks.
//! - [`Removed::Html`]: every start or end tag as HTML's tokenizer reads one: `<` or `</`, an
//!   ASCII letter, and everything up to the `>` that ends it, of any length, whatever the
//!   name (an unknown one such as `<widget>` or `<svg:path>`, one with letters of other
//!   scripts, a placeholder such as `<device>`: all are elements to the parser) and whatever
//!   the attributes hold (a `>` in a quoted value does not end it); tags written with character
//!   references (`&lt;div&gt;`); comments (to `-->`), CDATA (to `]]>`) and declarations
//!   (`<!DOCTYPE …>`, `<?xml …?>`, `<!ENTITY …>`, `</ …>`; to a `>` outside quotes), each
//!   taking the rest when its end does not follow, as the parser reads them; and the whole of a
//!   `<script>` or `<style>` element. Text between `<` and a letter and the next `>` is a tag to the parser, prose or
//!   not (「電壓<V1時跳脫，電流>5A」 loses 「<V1時跳脫，電流>」): the guard fails closed. A `<`
//!   that starts no tag stays: 「<5%」, `a < b`, a `<` and a letter that no `>` follows (`x<y`;
//!   the parser drops it, unless it had a quoted attribute value: a tag cut short, removed to
//!   the end), and CommonMark's autolinks to `http`, `https` and `mailto` URLs and to e-mail
//!   addresses (`<https://…>`, `<ops@example.com>`), which the chat shows as links.
//! - [`Removed::Sql`]: statements that start with a statement keyword (`SELECT`, `WITH`,
//!   `INSERT`, `UPDATE`, `DELETE`, `CREATE`, `DROP`, `ALTER`, `TRUNCATE`, `GRANT`, `REVOKE`,
//!   `EXPLAIN`, `SHOW`) and have that keyword's companion clause (`FROM` after `SELECT`, `INTO`
//!   after `INSERT`, …). A `SELECT` on its own line or in code needs none when only an
//!   expression follows it to the end of its line (`SELECT 1`, `select now()`), or in upper
//!   case when a parenthesis or an operator follows it; anywhere when only an expression with
//!   a call, an operator or a string follows it to the statement's end
//!   (`請執行 select max(kwh) 看尖峰`). In inline code any case counts; in
//!   prose the statement needs evidence beyond the two words (an upper case companion, an
//!   operator, a parenthesis, `WHERE`, …), more of it in lower case unless the statement stands
//!   at the start of its line with a SQL-shaped column list or without English articles, or is
//!   a `SELECT` of plain names with a `_` or commas from a table, or of plain names with the
//!   keyword or `FROM` in upper case (`SELECT kwh from devices`); capitalized keywords
//!   (`Select device_id From devices`) need SQL's structure and no English article. Where a
//!   statement stands on its own (a line's start, after a colon, a table cell), one that is only
//!   its keywords and plain names counts in any case, whatever ends it (`Query: select value
//!   from devices.`, `delete from devices`). So prose such as "Select the meter from the list",
//!   "Select meters from groups to compare them" or "- select the meter from the list" stays. A
//!   statement ends at `;`, a
//!   blank line, the backticks that close the inline code it is in (any length of run, as
//!   CommonMark reads them) or another backtick, a sentence's end or the first non-ASCII
//!   character outside a string, so the Chinese text around it stays; names quoted in backticks
//!   as ClickHouse and MySQL quote them (`` SELECT `device_id` FROM `devices` ``) are part of
//!   it.
//!
//! The agent applies it to the final answer before the answer joins the history
//! ([`crate::Agent::ask`]), so the stored history and the caller both get the cleaned text, and
//! [`crate::Answer::removed`] says what was removed; the chat UI shows [`CONTENT_REMOVED`] then.
//! [`find`] is the detector itself, for the streaming of Laysi/trustiot-v2#147, and [`pending`]
//! says how much of a text still being written is final: the stream sends that part without
//! [`find`]'s findings and holds back the rest.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;

use contract::MessageKey;
use serde::{Serialize, Serializer};

/// The notice the chat UI shows under an answer the guard changed ([`crate::Answer::removed`]
/// is not empty). The service has no chat API yet; the conversation API and the chat that show
/// it come with Laysi/trustiot-v2#645 and #646.
pub const CONTENT_REMOVED: MessageKey = MessageKey::new("assistant.answer_content_removed");

/// The text an answer gets when the guard removed all of it: the history needs a non-empty text
/// block, and [`CONTENT_REMOVED`] explains it to the user.
pub const REMOVED_PLACEHOLDER: &str = "…";

/// `text[range]`, or `""` when an end of `range` is not a character boundary of `text`. Every
/// range here ends at an ASCII byte, the end of the text, or where `find` found something, so
/// the fallback is never taken; it keeps an answer's text from ever ending the question with a
/// panic.
fn substr<R: std::slice::SliceIndex<str, Output = str>>(text: &str, range: R) -> &str {
    text.get(range).unwrap_or_default()
}

/// A kind of content the guard removes: a closed set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Removed {
    /// A fenced code block, or inline code that holds code.
    Code,
    /// An HTML or SVG tag, comment or declaration.
    Html,
    /// A SQL statement.
    Sql,
}

impl Removed {
    /// Every kind, in order.
    pub const ALL: [Self; 3] = [Self::Code, Self::Html, Self::Sql];

    /// The kind's name, as the API and the logs write it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Html => "html",
            Self::Sql => "sql",
        }
    }

    const fn bit(self) -> u8 {
        match self {
            Self::Code => 1,
            Self::Html => 2,
            Self::Sql => 4,
        }
    }
}

/// Which kinds of content were removed from an answer. Serializes as the list of their names in
/// [`Removed::ALL`]'s order, such as `["html", "sql"]`.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct RemovedContent {
    bits: u8,
}

impl RemovedContent {
    /// Adds `kind`.
    pub fn insert(&mut self, kind: Removed) {
        self.bits |= kind.bit();
    }

    #[must_use]
    pub const fn contains(self, kind: Removed) -> bool {
        self.bits & kind.bit() != 0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.bits == 0
    }

    /// Both sets together.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self {
            bits: self.bits | other.bits,
        }
    }

    /// The kinds, in [`Removed::ALL`]'s order.
    pub fn kinds(self) -> impl Iterator<Item = Removed> {
        Removed::ALL
            .into_iter()
            .filter(move |kind| self.contains(*kind))
    }

    /// The names, comma-separated, for the logs.
    #[must_use]
    pub fn names(self) -> String {
        self.kinds()
            .map(Removed::as_str)
            .collect::<Vec<_>>()
            .join(",")
    }
}

impl fmt::Debug for RemovedContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.kinds()).finish()
    }
}

impl FromIterator<Removed> for RemovedContent {
    fn from_iter<I: IntoIterator<Item = Removed>>(iter: I) -> Self {
        let mut set = Self::default();
        for kind in iter {
            set.insert(kind);
        }
        set
    }
}

impl Serialize for RemovedContent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.kinds())
    }
}

/// One part of a text the guard removes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// Byte offsets into the text, on character boundaries.
    pub range: Range<usize>,
    pub kind: Removed,
}

/// A text with the guard applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guarded {
    /// The text without what was removed, and without the list markers, quote markers and
    /// blank lines the removal left behind. Empty when nothing else was there.
    pub text: String,
    pub removed: RemovedContent,
}

/// Every part of `text` the guard removes, in order and not overlapping: fenced code first, then
/// HTML outside it (tags, then tags written with character references), then SQL outside both,
/// then inline code outside all three. A finding that ends at the end of `text` may grow when
/// more text follows (an unclosed fence, comment or statement).
#[must_use]
pub fn find(text: &str) -> Vec<Finding> {
    let mut findings = fences(text);
    let html: Vec<Finding> = gaps(text.len(), &findings)
        .into_iter()
        .flat_map(|gap| html_in(text, gap))
        .collect();
    merge(&mut findings, html);
    let escaped: Vec<Finding> = gaps(text.len(), &findings)
        .into_iter()
        .flat_map(|gap| escaped_html_in(text, gap))
        .collect();
    merge(&mut findings, escaped);
    let sql: Vec<Finding> = gaps(text.len(), &findings)
        .into_iter()
        .flat_map(|gap| sql_in(text, gap))
        .collect();
    merge(&mut findings, sql);
    let code: Vec<Finding> = gaps(text.len(), &findings)
        .into_iter()
        .flat_map(|gap| inline_code_in(text, gap))
        .collect();
    merge(&mut findings, code);
    findings
}

/// Where `text`, the start of an answer still being written (Laysi/trustiot-v2#147), stops being
/// final. Every finding of [`find`]`(text)` that starts before it, and every byte before it
/// outside those findings, stays as it is in any longer text that starts with `text`; after it,
/// more text may change what is removed. It is the start of the paragraph (after the last empty
/// line, `\n\n`; every reading but a tag's ends there) that holds the earliest of: the end of
/// the text, so the last paragraph is held back (a statement, an inline code span or a tag
/// written with references in it may still grow); the first finding that reaches the end of the
/// text (an unclosed fence, comment, declaration, or a tag cut short in a quoted value); and the
/// first `<` and letter after the last fence that no `>` has closed yet (a tag may close any
/// number of paragraphs later).
#[must_use]
pub fn pending(text: &str) -> usize {
    let open_finding = find(text)
        .into_iter()
        .filter(|finding| finding.range.end == text.len())
        .map(|finding| finding.range.start)
        .min();
    let open_tag = gaps(text.len(), &fences(text))
        .last()
        .filter(|gap| gap.end == text.len())
        .and_then(|gap| {
            let tags = Tags::of(substr(text, gap.clone()));
            tags.starts
                .iter()
                .zip(&tags.ends)
                .find(|(_, end)| end.is_none())
                .map(|(at, _)| gap.start + at)
        });
    let earliest = open_finding
        .into_iter()
        .chain(open_tag)
        .min()
        .unwrap_or(text.len());
    substr(text, ..earliest)
        .rfind("\n\n")
        .map_or(0, |blank| blank + 2)
}

/// `text` without what [`find`] finds. Returns `text` unchanged when nothing is found.
#[must_use]
pub fn guard(text: &str) -> Guarded {
    let findings = find(text);
    if findings.is_empty() {
        return Guarded {
            text: text.to_owned(),
            removed: RemovedContent::default(),
        };
    }
    let mut kept = String::with_capacity(text.len());
    let mut cuts = Vec::with_capacity(findings.len());
    let mut removed = RemovedContent::default();
    let mut last = 0;
    for finding in &findings {
        push_piece(&mut kept, substr(text, last..finding.range.start));
        cuts.push(kept.len());
        removed.insert(finding.kind);
        last = finding.range.end;
    }
    push_piece(&mut kept, substr(text, last..));
    Guarded {
        text: tidy(&kept, &cuts),
        removed,
    }
}

/// Appends `piece`, without doubling the spaces on both sides of a cut or leaving a space before
/// punctuation.
fn push_piece(kept: &mut String, piece: &str) {
    if kept.ends_with([' ', '\t']) {
        let piece = piece.trim_start_matches([' ', '\t']);
        if piece.starts_with(CLOSING_PUNCTUATION) {
            kept.truncate(kept.trim_end_matches([' ', '\t']).len());
        }
        kept.push_str(piece);
    } else {
        kept.push_str(piece);
    }
}

/// Punctuation that follows a word without a space.
const CLOSING_PUNCTUATION: [char; 16] = [
    '.', ',', ';', ':', '!', '?', ')', ']', '，', '。', '；', '：', '！', '？', '）', '、',
];

/// Drops the lines a removal left empty or with nothing but markers, collapses runs of blank lines and
/// trims the ends.
fn tidy(kept: &str, cuts: &[usize]) -> String {
    let mut lines: Vec<&str> = Vec::new();
    let mut offset = 0;
    for line in kept.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let content = line.trim_end_matches(['\n', '\r']);
        let end = start + content.len();
        let cut = cuts.iter().any(|&cut| (start..=end).contains(&cut));
        // A line the cut left empty or with only markers goes; the blank lines around it stay.
        if cut && is_residue(content) {
            continue;
        }
        lines.push(content);
    }
    let mut text = String::with_capacity(kept.len());
    let mut blank_run = 0;
    for line in lines {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        text.push_str(line);
        text.push('\n');
    }
    text.trim().to_owned()
}

/// A line with no letter or digit: what is left of a list item, a quote or a heading whose
/// content was removed.
fn is_residue(line: &str) -> bool {
    !line.chars().any(char::is_alphanumeric)
}

/// The ranges of `0..len` outside `findings` (sorted, not overlapping).
fn gaps(len: usize, findings: &[Finding]) -> Vec<Range<usize>> {
    let mut gaps = Vec::with_capacity(findings.len() + 1);
    let mut start = 0;
    for finding in findings {
        if finding.range.start > start {
            gaps.push(start..finding.range.start);
        }
        start = start.max(finding.range.end);
    }
    if start < len {
        gaps.push(start..len);
    }
    gaps
}

fn merge(findings: &mut Vec<Finding>, more: Vec<Finding>) {
    findings.extend(more);
    findings.sort_by_key(|finding| finding.range.start);
}

// ---------------------------------------------------------------------------------------------
// Fenced code

/// The fenced code blocks of `text`: from the opening fence's line to the closing fence's line,
/// both included, or to the end of `text` when no fence closes it.
fn fences(text: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    // The open block: where its line starts, the fence's character and length.
    let mut open: Option<(usize, u8, usize)> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let body = container_content(line.trim_end_matches(['\n', '\r']));
        match open {
            None => {
                let Some((fence, run)) = fence_run(body) else {
                    continue;
                };
                if fence == b'`' && substr(body, run..).contains('`') {
                    // Not a fence (CommonMark): code written inline with a fence's backticks.
                    // The whole line is code.
                    findings.push(Finding {
                        range: start..offset,
                        kind: Removed::Code,
                    });
                } else {
                    open = Some((start, fence, run));
                }
            }
            Some((opened, fence, run)) => {
                let closes = fence_run(body).is_some_and(|(character, length)| {
                    character == fence && length >= run && substr(body, length..).trim().is_empty()
                });
                if closes {
                    findings.push(Finding {
                        range: opened..offset,
                        kind: Removed::Code,
                    });
                    open = None;
                }
            }
        }
    }
    if let Some((opened, _, _)) = open {
        findings.push(Finding {
            range: opened..text.len(),
            kind: Removed::Code,
        });
    }
    findings
}

/// A line without its indentation, quote markers and list marker: where a fence would start.
fn container_content(line: &str) -> &str {
    let mut rest = line.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix('>') {
            rest = after.trim_start();
            continue;
        }
        if let Some(after) = rest
            .strip_prefix(['-', '*', '+'])
            .filter(|after| after.starts_with([' ', '\t']))
        {
            rest = after.trim_start();
            continue;
        }
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if (1..=9).contains(&digits)
            && let Some(after) = substr(rest, digits..)
                .strip_prefix(['.', ')'])
                .filter(|after| after.starts_with([' ', '\t']))
        {
            rest = after.trim_start();
            continue;
        }
        return rest;
    }
}

/// The fence a line starts with: its character and length (3 or more).
fn fence_run(body: &str) -> Option<(u8, usize)> {
    let first = *body.as_bytes().first()?;
    if first != b'`' && first != b'~' {
        return None;
    }
    let run = body.bytes().take_while(|&byte| byte == first).count();
    (run >= 3).then_some((first, run))
}

// ---------------------------------------------------------------------------------------------
// HTML

/// The tags, comments and declarations in `gap` of `text`.
fn html_in(text: &str, gap: Range<usize>) -> Vec<Finding> {
    let segment = substr(text, gap.clone());
    let quotes = Quotes::of(segment);
    let tags = Tags::of(segment);
    let mut findings = Vec::new();
    let mut from = 0;
    while let Some(found) = substr(segment, from..).find('<') {
        let at = from + found;
        match html_at(segment, at, &quotes, &tags) {
            Some(end) => {
                // A tag that is all of an inline code span takes its backticks along.
                let bytes = segment.as_bytes();
                let wrapped = at > 0
                    && bytes[at - 1] == b'`'
                    && bytes.get(end) == Some(&b'`')
                    && (at < 2 || bytes[at - 2] != b'`')
                    && bytes.get(end + 1) != Some(&b'`');
                let range = if wrapped { at - 1..end + 1 } else { at..end };
                findings.push(Finding {
                    range: gap.start + range.start..gap.start + range.end,
                    kind: Removed::Html,
                });
                from = range.end;
            }
            None => from = at + 1,
        }
    }
    findings
}

/// The end of the tag, comment or declaration that starts at `at` (a `<`), if one does, as
/// HTML's tokenizer reads it (HTML Living Standard, 「Tokenization」).
fn html_at(segment: &str, at: usize, quotes: &Quotes, tags: &Tags) -> Option<usize> {
    let rest = substr(segment, at..);
    if let Some(after) = rest.strip_prefix("<!--") {
        return Some(at + 4 + comment_end(after));
    }
    if let Some(after) = rest.strip_prefix("<![CDATA[") {
        // In SVG and MathML a CDATA section runs to `]]>`; elsewhere the parser ends it at its
        // first `>`, which is not later.
        return Some(
            after
                .find("]]>")
                .map_or(segment.len(), |end| at + 9 + end + 3),
        );
    }
    // A DOCTYPE, a processing instruction such as `<?xml …?>`, a declaration such as
    // `<!ENTITY …>`, and `</` before anything but a letter or `>`: the parser reads each up to
    // a `>` (as a comment, except the DOCTYPE), and one without a `>` swallows the rest.
    let end_tag = rest.strip_prefix("</");
    let declaration = rest.starts_with("<!")
        || rest.starts_with("<?")
        || end_tag
            .is_some_and(|after| after.starts_with(|c: char| !c.is_ascii_alphabetic() && c != '>'));
    if declaration {
        return Some(quotes.declaration_end(at + 2));
    }
    if rest.starts_with("</>") {
        // The parser drops it.
        return Some(at + 3);
    }
    let end = tags.end(at)?;
    if end_tag.is_some() {
        return Some(end);
    }
    let name = tag_name(substr(rest, 1..), b"");
    if name.eq_ignore_ascii_case("script") || name.eq_ignore_ascii_case("style") {
        // The element's content is code too: up to its end tag, or the end. HTML's parser
        // ignores a `/` before the `>` here (`<script/>` still opens the element), so the guard
        // does too, even where SVG would close it.
        return Some(closing_tag(segment, end, name));
    }
    Some(end)
}

/// How far a comment reaches into `after`, the text after its `<!--`, as HTML's tokenizer ends
/// it: just past a `>` or `->` right there (an empty comment), else past the first `-->` or
/// `--!>`, else the rest.
fn comment_end(after: &str) -> usize {
    if after.starts_with('>') {
        return 1;
    }
    if after.starts_with("->") {
        return 2;
    }
    let mut from = 0;
    while let Some(found) = substr(after, from..).find("--") {
        let dashes_end = from + found + 2;
        let tail = substr(after, dashes_end..);
        if tail.starts_with('>') {
            return dashes_end + 1;
        }
        if tail.starts_with("!>") {
            return dashes_end + 2;
        }
        from = dashes_end - 1;
    }
    after.len()
}

/// A tag name at the start of `text`, as HTML's tokenizer reads it: an ASCII letter, then
/// anything up to a blank, `/` or `>` (or one of `stops`); `""` when `text` starts with no
/// letter.
fn tag_name<'a>(text: &'a str, stops: &[u8]) -> &'a str {
    let bytes = text.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_alphabetic) {
        return "";
    }
    let len = bytes
        .iter()
        .position(|byte| {
            byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>') || stops.contains(byte)
        })
        .unwrap_or(bytes.len());
    substr(text, ..len)
}

/// The URL schemes whose autolinks stay: the ones the chat shows as links. CommonMark reads
/// `<` and any scheme with a `:` as a link, but `<svg:path>` is a tag to HTML's parser, so a
/// scheme the chat does not link makes markup.
const AUTOLINK_SCHEMES: [&str; 3] = ["http", "https", "mailto"];

/// Whether `body`, the text between `<` and `>` with no blank, control character, `<` or `>`
/// in it, is an autolink that stays: a URL with one of [`AUTOLINK_SCHEMES`], or an e-mail
/// address as CommonMark's autolinks write one.
fn is_autolink(body: &str) -> bool {
    let url = body.split_once(':').is_some_and(|(scheme, _)| {
        AUTOLINK_SCHEMES
            .iter()
            .any(|known| scheme.eq_ignore_ascii_case(known))
    });
    url || is_email_autolink(body)
}

fn is_email_autolink(body: &str) -> bool {
    let Some((local, domain)) = body.split_once('@') else {
        return false;
    };
    let local_ok = !local.is_empty()
        && local
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".!#$%&'*+/=?^_`{|}~-".contains(&byte));
    let label_ok = |label: &str| {
        let bytes = label.as_bytes();
        bytes.len() <= 63
            && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
    };
    local_ok && domain.split('.').all(label_ok)
}

/// A byte that ends an autolink's body: a blank, a control character, `<`, `>`, or one of
/// `stops`.
fn ends_autolink(byte: u8, stops: &[u8]) -> bool {
    byte <= b' ' || byte == 0x7f || matches!(byte, b'<' | b'>') || stops.contains(&byte)
}

/// Whether an autolink that stays starts at `from`, just after a `<`. Reads up to the next
/// blank, `<` or `>` at most.
fn is_autolink_at(segment: &str, from: usize) -> bool {
    let rest = substr(segment, from..);
    rest.bytes()
        .position(|byte| ends_autolink(byte, b""))
        .is_some_and(|len| {
            rest.as_bytes().get(len) == Some(&b'>') && is_autolink(substr(rest, ..len))
        })
}

/// The states of HTML's tokenizer inside a start or end tag, after its `<` (HTML Living
/// Standard, 13.2.5.8 to 13.2.5.40).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TagState {
    Name,
    BeforeAttributeName,
    AttributeName,
    AfterAttributeName,
    BeforeAttributeValue,
    DoubleQuotedValue,
    SingleQuotedValue,
    UnquotedValue,
    AfterQuotedValue,
    SelfClosing,
}

impl TagState {
    const ALL: [Self; 10] = [
        Self::Name,
        Self::BeforeAttributeName,
        Self::AttributeName,
        Self::AfterAttributeName,
        Self::BeforeAttributeValue,
        Self::DoubleQuotedValue,
        Self::SingleQuotedValue,
        Self::UnquotedValue,
        Self::AfterQuotedValue,
        Self::SelfClosing,
    ];

    /// The slots [`tag_ends`] reads in: each state, before and after the tag had a quoted
    /// value.
    const SLOTS: usize = 2 * Self::ALL.len();

    fn slot(self, quoted: bool) -> usize {
        self as usize + if quoted { Self::ALL.len() } else { 0 }
    }

    fn of_slot(slot: usize) -> (Self, bool) {
        (Self::ALL[slot % Self::ALL.len()], slot >= Self::ALL.len())
    }

    fn quoted_value(self) -> bool {
        matches!(self, Self::DoubleQuotedValue | Self::SingleQuotedValue)
    }

    /// The state after `byte`, or `None` when `byte` is the `>` that ends the tag. The
    /// tokenizer's parse errors change nothing here: a quote or `<` in a name, a `=` that starts
    /// an attribute's name, a missing blank between attributes or a `/` not before `>` all go
    /// on reading the tag, as the tokenizer does.
    fn after(self, byte: u8) -> Option<Self> {
        let blank = byte.is_ascii_whitespace();
        let next = match self {
            Self::Name => match byte {
                b'>' => return None,
                b'/' => Self::SelfClosing,
                _ if blank => Self::BeforeAttributeName,
                _ => Self::Name,
            },
            Self::BeforeAttributeName => match byte {
                b'>' => return None,
                b'/' => Self::SelfClosing,
                _ if blank => Self::BeforeAttributeName,
                _ => Self::AttributeName,
            },
            Self::AttributeName | Self::AfterAttributeName => match byte {
                b'>' => return None,
                b'/' => Self::SelfClosing,
                b'=' => Self::BeforeAttributeValue,
                _ if blank => Self::AfterAttributeName,
                _ => Self::AttributeName,
            },
            Self::BeforeAttributeValue => match byte {
                b'>' => return None,
                b'"' => Self::DoubleQuotedValue,
                b'\'' => Self::SingleQuotedValue,
                _ if blank => Self::BeforeAttributeValue,
                _ => Self::UnquotedValue,
            },
            Self::DoubleQuotedValue if byte == b'"' => Self::AfterQuotedValue,
            Self::SingleQuotedValue if byte == b'\'' => Self::AfterQuotedValue,
            Self::DoubleQuotedValue | Self::SingleQuotedValue => self,
            Self::UnquotedValue => match byte {
                b'>' => return None,
                _ if blank => Self::BeforeAttributeName,
                _ => Self::UnquotedValue,
            },
            Self::AfterQuotedValue | Self::SelfClosing => match byte {
                b'>' => return None,
                b'/' => Self::SelfClosing,
                _ if blank => Self::BeforeAttributeName,
                _ => Self::AttributeName,
            },
        };
        Some(next)
    }
}

/// The start and end tags of a segment: each `<` (or `</`) followed by an ASCII letter, except
/// an autolink that stays, with where it ends ([`tag_ends`]).
struct Tags {
    /// Each candidate's `<`, in order.
    starts: Vec<usize>,
    /// Just past each candidate's end, or `None` when it makes no tag.
    ends: Vec<Option<usize>>,
}

impl Tags {
    fn of(segment: &str) -> Self {
        let bytes = segment.as_bytes();
        let mut starts = Vec::new();
        let mut names = Vec::new();
        for (at, _) in segment.match_indices('<') {
            let end_tag = bytes.get(at + 1) == Some(&b'/');
            let name = at + if end_tag { 2 } else { 1 };
            if bytes.get(name).is_some_and(u8::is_ascii_alphabetic)
                && (end_tag || !is_autolink_at(segment, at + 1))
            {
                starts.push(at);
                names.push(name);
            }
        }
        let ends = tag_ends(bytes, &names);
        Self { starts, ends }
    }

    /// Just past the end of the tag whose `<` is at `at`, if one starts there.
    fn end(&self, at: usize) -> Option<usize> {
        let index = self.starts.binary_search(&at).ok()?;
        self.ends.get(index).copied().flatten()
    }
}

/// Where each tag whose name starts at `names[k]` (in increasing order) ends, as HTML's
/// tokenizer reads it: just past the `>` that ends it, wherever that is (a `>` in a quoted
/// value does not end it). When the text ends first, the parser makes no tag of it and drops
/// it; in an answer that is prose such as `x<y`, so it is `None`, unless the tag had a quoted
/// value (`<img src="x" onerror="…"`): a tag cut short, which ends at the end of the text,
/// failing closed.
///
/// One pass reads the segment for every candidate at once. The tokenizer's next state depends
/// only on its state and the byte, so candidates in the same slot ([`TagState::slot`]) at the
/// same byte read the rest alike: they are merged into one group ([`Groups`]), and at most
/// [`TagState::SLOTS`] groups are open at a time, however many tags are. The work is linear in
/// the segment's length.
fn tag_ends(bytes: &[u8], names: &[usize]) -> Vec<Option<usize>> {
    let mut groups = Groups::new(names.len());
    let mut open: [Option<usize>; TagState::SLOTS] = [None; TagState::SLOTS];
    let mut next = 0;
    let mut i = names.first().copied().unwrap_or(bytes.len());
    while i < bytes.len() {
        while names.get(next) == Some(&i) {
            let slot = &mut open[TagState::Name.slot(false)];
            *slot = Some(slot.map_or(next, |group| groups.union(group, next)));
            next += 1;
        }
        let mut after: [Option<usize>; TagState::SLOTS] = [None; TagState::SLOTS];
        for (slot, group) in open.iter().enumerate() {
            let Some(group) = *group else {
                continue;
            };
            let (state, quoted) = TagState::of_slot(slot);
            match state.after(bytes[i]) {
                None => groups.close(group, i + 1),
                Some(state) => {
                    let target = &mut after[state.slot(quoted || state.quoted_value())];
                    *target = Some(target.map_or(group, |other| groups.union(other, group)));
                }
            }
        }
        open = after;
        i += 1;
        if open.iter().all(Option::is_none) {
            // Nothing is open: go on at the next name.
            match names.get(next) {
                Some(&name) => i = name,
                None => break,
            }
        }
    }
    for (slot, group) in open.iter().enumerate() {
        if let Some(group) = *group
            && TagState::of_slot(slot).1
        {
            groups.close(group, bytes.len());
        }
    }
    groups.ends()
}

/// Just past the `>` that ends a tag whose name ends at `from`, read as [`tag_ends`] reads one,
/// or `None` when the text ends first.
fn tag_end_from(bytes: &[u8], from: usize) -> Option<usize> {
    let mut state = TagState::Name;
    for (i, &byte) in bytes.iter().enumerate().skip(from) {
        match state.after(byte) {
            None => return Some(i + 1),
            Some(next) => state = next,
        }
    }
    None
}

/// The tag candidates [`tag_ends`] reads together: a union-find over their indexes, with the
/// end each group reached.
struct Groups {
    parent: Vec<usize>,
    size: Vec<usize>,
    end: Vec<Option<usize>>,
}

impl Groups {
    fn new(count: usize) -> Self {
        Self {
            parent: (0..count).collect(),
            size: vec![1; count],
            end: vec![None; count],
        }
    }

    fn root(&mut self, mut member: usize) -> usize {
        while self.parent[member] != member {
            self.parent[member] = self.parent[self.parent[member]];
            member = self.parent[member];
        }
        member
    }

    /// Merges the groups of `a` and `b`, and returns the merged group.
    fn union(&mut self, a: usize, b: usize) -> usize {
        let (a, b) = (self.root(a), self.root(b));
        if a == b {
            return a;
        }
        let (big, small) = if self.size[a] >= self.size[b] {
            (a, b)
        } else {
            (b, a)
        };
        self.parent[small] = big;
        self.size[big] += self.size[small];
        big
    }

    fn close(&mut self, group: usize, end: usize) {
        let root = self.root(group);
        self.end[root] = Some(end);
    }

    fn ends(mut self) -> Vec<Option<usize>> {
        (0..self.parent.len())
            .map(|member| {
                let root = self.root(member);
                self.end[root]
            })
            .collect()
    }
}

/// Where the quote characters and the `>`s of a segment are, so a declaration's end is found
/// by looking them up ([`Quotes::declaration_end`]): each declaration reads only up to its end,
/// and what follows is read once.
struct Quotes {
    double: Vec<usize>,
    single: Vec<usize>,
    greater: Vec<usize>,
    len: usize,
}

impl Quotes {
    fn of(segment: &str) -> Self {
        let mut quotes = Self {
            double: Vec::new(),
            single: Vec::new(),
            greater: Vec::new(),
            len: segment.len(),
        };
        for (at, byte) in segment.bytes().enumerate() {
            match byte {
                b'"' => quotes.double.push(at),
                b'\'' => quotes.single.push(at),
                b'>' => quotes.greater.push(at),
                _ => {}
            }
        }
        quotes
    }

    /// The first of `positions` at or after `from`.
    fn first(positions: &[usize], from: usize) -> Option<usize> {
        positions
            .get(positions.partition_point(|&position| position < from))
            .copied()
    }

    /// Just past the first `>` at or after `from` outside quotes (`"…"` or `'…'`), or the
    /// segment's end when none follows or a quote does not close: where a declaration open at
    /// `from` ends. HTML's parser ends some at a `>` inside quotes (a DOCTYPE's quoted
    /// identifier ends there, abruptly); the guard reads on to the quote's end, failing closed,
    /// so no part of one is left.
    fn declaration_end(&self, from: usize) -> usize {
        let mut at = from;
        loop {
            let greater = Self::first(&self.greater, at);
            let double = Self::first(&self.double, at);
            let single = Self::first(&self.single, at);
            let (quote, positions) = match (double, single) {
                (Some(double), Some(single)) if single < double => (single, &self.single),
                (Some(double), _) => (double, &self.double),
                (None, Some(single)) => (single, &self.single),
                (None, None) => return greater.map_or(self.len, |end| end + 1),
            };
            if let Some(end) = greater.filter(|&end| end < quote) {
                return end + 1;
            }
            match Self::first(positions, quote + 1) {
                Some(close) => at = close + 1,
                None => return self.len,
            }
        }
    }
}

/// Just past the end tag that closes the `<script>` or `<style>` element whose content starts
/// at `from`, or the segment's end when none does: `</` and `name` in any case, then a blank,
/// `/` or `>`, as HTML's tokenizer ends such an element, read to its `>` like any tag.
fn closing_tag(segment: &str, from: usize, name: &str) -> usize {
    let bytes = segment.as_bytes();
    let mut search = from;
    while let Some(found) = substr(segment, search..).find("</") {
        let at = search + found;
        let name_end = at + 2 + name.len();
        let closes = starts_with_ignore_case(substr(segment, at + 2..), name)
            && bytes
                .get(name_end)
                .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'));
        if closes {
            return tag_end_from(bytes, name_end).unwrap_or(segment.len());
        }
        search = at + 2;
    }
    segment.len()
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

/// The character references of `<` and `>` a page's source may be written with.
const ESCAPED_LT: [&str; 3] = ["&lt;", "&#60;", "&#x3c;"];
const ESCAPED_GT: [&str; 3] = ["&gt;", "&#62;", "&#x3e;"];

/// The length of the reference among `references` that `text` starts with, in any case.
fn reference_at(text: &str, references: &[&str]) -> Option<usize> {
    references
        .iter()
        .find(|reference| starts_with_ignore_case(text, reference))
        .map(|reference| reference.len())
}

/// Whether an autolink that stays, written with references (`&lt;https://…&gt;`), starts at
/// `from`, just after the reference to `<`. Reads up to the next `&` at most.
fn is_escaped_autolink_at(segment: &str, from: usize) -> bool {
    let rest = substr(segment, from..);
    rest.bytes()
        .position(|byte| ends_autolink(byte, b"&"))
        .is_some_and(|len| {
            reference_at(substr(rest, len..), &ESCAPED_GT).is_some()
                && is_autolink(substr(rest, ..len))
        })
}

/// Tags written with character references, such as `&lt;div class="x"&gt;` or `&lt;/b&gt;`:
/// a rendered answer shows them as markup, so they are markup. A reference to `<` that starts
/// no tag stays (`a &lt; b`, `&lt;5%`), and so does an autolink that stays. Each candidate
/// reads up to the next `<`, reference to `<` or blank line at most, so the work is linear in
/// the text's length.
fn escaped_html_in(text: &str, gap: Range<usize>) -> Vec<Finding> {
    let segment = substr(text, gap.clone());
    let bytes = segment.as_bytes();
    let mut findings = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let Some(open) = (bytes[i] == b'&')
            .then(|| reference_at(substr(segment, i..), &ESCAPED_LT))
            .flatten()
        else {
            i += 1;
            continue;
        };
        let after = substr(segment, i + open..);
        let end_tag = after.starts_with('/');
        let name = tag_name(substr(after, usize::from(end_tag)..), b"&<");
        let body = i + open + usize::from(end_tag) + name.len();
        let markup = !name.is_empty() && (end_tag || !is_escaped_autolink_at(segment, i + open));
        let end = markup.then(|| escaped_tag_end(segment, body)).flatten();
        match end {
            Some(end) => {
                findings.push(Finding {
                    range: gap.start + i..gap.start + end,
                    kind: Removed::Html,
                });
                i = end;
            }
            None => i += open,
        }
    }
    findings
}

/// Just past the reference to `>` that ends an escaped tag whose name ends at `from`: the name
/// is followed by a blank, a `/` or the `>` itself, and no `<`, reference to `<` or blank line
/// comes first, however long the tag is. Each candidate reads at most up to the next of those,
/// where the next candidate can start at the earliest, so the reads do not overlap.
fn escaped_tag_end(segment: &str, from: usize) -> Option<usize> {
    let bytes = segment.as_bytes();
    let first = *bytes.get(from)?;
    if !(first.is_ascii_whitespace() || first == b'/' || first == b'&') {
        return None;
    }
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'<' => return None,
            b'\n' if bytes.get(i + 1) == Some(&b'\n') => return None,
            b'&' => {
                let rest = substr(segment, i..);
                if let Some(close) = reference_at(rest, &ESCAPED_GT) {
                    return Some(i + close);
                }
                if reference_at(rest, &ESCAPED_LT).is_some() {
                    return None;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------------------------
// SQL

/// A keyword that starts a statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Start {
    Select,
    With,
    Insert,
    Update,
    Delete,
    Create,
    Drop,
    Alter,
    Truncate,
    Grant,
    Revoke,
    Explain,
    Show,
}

impl Start {
    fn parse(word: &str) -> Option<Self> {
        const STARTS: [(&str, Start); 13] = [
            ("SELECT", Start::Select),
            ("WITH", Start::With),
            ("INSERT", Start::Insert),
            ("UPDATE", Start::Update),
            ("DELETE", Start::Delete),
            ("CREATE", Start::Create),
            ("DROP", Start::Drop),
            ("ALTER", Start::Alter),
            ("TRUNCATE", Start::Truncate),
            ("GRANT", Start::Grant),
            ("REVOKE", Start::Revoke),
            ("EXPLAIN", Start::Explain),
            ("SHOW", Start::Show),
        ];
        STARTS
            .iter()
            .find(|(name, _)| word.eq_ignore_ascii_case(name))
            .map(|(_, start)| *start)
    }
}

/// What `CREATE`, `DROP` and `ALTER` act on.
const OBJECTS: &[&str] = &[
    "DATABASE",
    "DICTIONARY",
    "FUNCTION",
    "INDEX",
    "POLICY",
    "ROLE",
    "SCHEMA",
    "SEQUENCE",
    "TABLE",
    "TRIGGER",
    "USER",
    "VIEW",
];

/// What `SHOW` shows.
const SHOWN: &[&str] = &[
    "COLUMNS",
    "CREATE",
    "DATABASES",
    "DICTIONARIES",
    "FUNCTIONS",
    "GRANTS",
    "INDEX",
    "INDEXES",
    "PROCESSLIST",
    "ROLES",
    "SETTINGS",
    "TABLES",
    "USERS",
];

/// Keywords that mark SQL in any case.
const STRONG_KEYWORDS: &[&str] = &[
    "HAVING", "INTERVAL", "JOIN", "LIMIT", "PREWHERE", "UNION", "WHERE",
];

/// Words that mark SQL only when written in upper case (in lower case they are English).
const WEAK_KEYWORDS: &[&str] = &[
    "AND", "AS", "ASC", "BETWEEN", "BY", "CASE", "DESC", "DISTINCT", "ELSE", "END", "IN", "INTO",
    "LIKE", "NOT", "NULL", "ON", "OR", "SET", "THEN", "VALUES", "WHEN",
];

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token<'a> {
    /// An identifier or keyword; `dotted` when it is `a.b`.
    Word {
        text: &'a str,
        dotted: bool,
    },
    Number,
    Str,
    Punct(&'a str),
}

impl Token<'_> {
    fn is_word(&self, keyword: &str) -> bool {
        matches!(self, Token::Word { text, dotted: false } if text.eq_ignore_ascii_case(keyword))
    }
}

/// The most bytes read after a statement keyword to decide whether a statement starts there:
/// twice telemetry-query's limit on the SQL the model may run. A statement that is longer is
/// still removed to its end.
const MAX_STATEMENT_SCAN: usize = 32 << 10;
/// The most tokens read after a statement keyword to decide (a token has 2 bytes at least).
const MAX_STATEMENT_TOKENS: usize = MAX_STATEMENT_SCAN / 2;
/// After this many statement keywords in a row that start no statement, each before the end of
/// the text the ones before it read, the scan goes on after that end: so a paragraph full of
/// such words is read a bounded number of times, not once per word.
const MAX_REJECTED_IN_A_ROW: usize = 64;

/// The SQL statements in `gap` of `text`. The work is linear in the text's length: a candidate
/// reuses the extent the one before it found when that held no string, every other read is
/// bounded by [`MAX_STATEMENT_SCAN`], and [`MAX_REJECTED_IN_A_ROW`] bounds how often one stretch
/// of text is read.
fn sql_in(text: &str, gap: Range<usize>) -> Vec<Finding> {
    let segment = substr(text, gap.clone());
    let bytes = segment.as_bytes();
    let mut findings = Vec::new();
    let mut known: Option<Extent> = None;
    // The farthest end the rejected candidates in a row read to, and how many there were.
    let mut read_to = 0;
    let mut rejected = 0;
    let mut i = 0;
    while i < bytes.len() {
        let starts_word = bytes[i].is_ascii_alphabetic() && (i == 0 || !is_word_byte(bytes[i - 1]));
        if !starts_word {
            i += 1;
            continue;
        }
        let end = i + bytes[i..]
            .iter()
            .take_while(|byte| is_word_byte(**byte))
            .count();
        let Some(start) = Start::parse(substr(segment, i..end)) else {
            i = end;
            continue;
        };
        if i >= read_to {
            rejected = 0;
        }
        let opening = code_opening(bytes, i);
        let code_run = opening.map_or(0, |opening| opening.run);
        let reusable = known.filter(|known| {
            !known.quoted && !known.capped && known.code_run == code_run && i < known.end
        });
        let found =
            reusable.unwrap_or_else(|| extent(segment, i, code_run, Some(MAX_STATEMENT_SCAN)));
        known = Some(found);
        match statement(segment, i, end, start, opening, found) {
            Some(range) => {
                findings.push(Finding {
                    range: gap.start + range.start..gap.start + range.end,
                    kind: Removed::Sql,
                });
                known = None;
                read_to = 0;
                rejected = 0;
                i = range.end;
            }
            None => {
                read_to = read_to.max(found.end);
                rejected += 1;
                if rejected >= MAX_REJECTED_IN_A_ROW {
                    known = None;
                    rejected = 0;
                    i = read_to.max(end);
                } else {
                    i = end;
                }
            }
        }
    }
    findings
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The inline code a statement keyword starts: the run of backticks that opens it, right before
/// the keyword or, for a run of two or more, one space before it (`` `` SELECT `a` FROM t `` ``,
/// CommonMark's padding).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Opening {
    /// Where the run starts.
    start: usize,
    /// How many backticks it has; the run of the same length closes the code.
    run: usize,
}

/// The inline code the keyword at `at` starts, if it starts one.
fn code_opening(bytes: &[u8], at: usize) -> Option<Opening> {
    let run_before = |end: usize| {
        bytes
            .get(..end)
            .unwrap_or_default()
            .iter()
            .rev()
            .take_while(|byte| **byte == b'`')
            .count()
    };
    let direct = run_before(at);
    if direct > 0 {
        return Some(Opening {
            start: at - direct,
            run: direct,
        });
    }
    let padded = at
        .checked_sub(1)
        .filter(|&space| bytes[space] == b' ')
        .map_or(0, run_before);
    (padded >= 2).then(|| Opening {
        start: at - 1 - padded,
        run: padded,
    })
}

/// The longest name quoted in backticks the guard reads as one (`` `device_id` ``).
const MAX_QUOTED_NAME_BYTES: usize = 128;

/// A name in backticks starts at `i` (as ClickHouse and MySQL quote names: a single backtick, the
/// name's characters, a single backtick): the offset after its closing backtick. A backtick
/// followed by anything else, a space included, is not one.
fn quoted_name_end(bytes: &[u8], i: usize) -> Option<usize> {
    let single = |at: usize| {
        bytes.get(at) == Some(&b'`')
            && (at == 0 || bytes[at - 1] != b'`')
            && bytes.get(at + 1) != Some(&b'`')
    };
    if !single(i) {
        return None;
    }
    let name = bytes
        .get(i + 1..)
        .unwrap_or_default()
        .iter()
        .take(MAX_QUOTED_NAME_BYTES)
        .take_while(|byte| is_word_byte(**byte) || matches!(**byte, b'.' | b'-' | b'$'))
        .count();
    let close = i + 1 + name;
    (name > 0 && single(close)).then_some(close + 1)
}

/// What a statement's text ends with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    /// A `;`, removed with it.
    Semicolon,
    /// A backtick that is not a quoted name's: in inline code, the run that closes it.
    Backtick,
    /// A `.` that ends a sentence, kept.
    Sentence,
    /// Anything else, kept.
    Other,
}

/// Where a candidate statement ends.
#[derive(Clone, Copy, Debug)]
struct Extent {
    /// The offset of its ending (a `;` or backtick is at it), or of the scan's limit.
    end: usize,
    ending: Ending,
    /// The text held a quote character or a name in backticks.
    quoted: bool,
    /// The scan stopped at its limit before the statement's end.
    capped: bool,
    /// The length of the backtick run that opened the inline code the statement is in (0: not in
    /// code), which the scan looked for.
    code_run: usize,
}

/// The range to remove for a statement whose keyword `start` is at `at..word_end` of
/// `segment`, in the inline code that `opening` opens if any, and ends at `extent`, if it is one.
fn statement(
    segment: &str,
    at: usize,
    word_end: usize,
    start: Start,
    opening: Option<Opening>,
    extent: Extent,
) -> Option<Range<usize>> {
    let in_code = opening.is_some();
    let keyword = substr(segment, at..word_end);
    let case = Case::of(keyword);
    let tokens = tokenize(substr(segment, at..extent.end), MAX_STATEMENT_TOKENS);
    let Some(companion) = companion(start, &tokens) else {
        // A `SELECT` without `FROM` (`SELECT now()`, `select 1`), written on its own or as
        // code: with only an expression after it up to the end of its line, a `;` or the end of
        // the code; in upper case also with an operator or a parenthesis. Anywhere in a sentence
        // in upper or lower case when only an expression with a call, an operator or a string
        // follows it up to the statement's end (`請執行 select max(kwh) 看尖峰`), which a number
        // alone is not (`SELECT 2 個設備`).
        let expression = || expression_only(&tokens) && ends_its_line(segment, at, extent);
        let strong = || evidence(&tokens, start).strong >= 1;
        let in_sentence = case != Case::Mixed && expression_only(&tokens) && strong();
        let bare_select = start == Start::Select
            && (in_sentence
                || ((in_code || statement_position(segment, at))
                    && match case {
                        Case::Upper => strong() || expression(),
                        Case::Lower => expression(),
                        Case::Mixed => in_code && expression(),
                    }));
        return bare_select.then(|| removal(segment, at, opening, extent));
    };
    // An upper-case companion after a plain select list (`select kwh FROM meters`) is SQL in any
    // case of the keyword.
    let upper_from =
        || start == Start::Select && Case::of(companion) == Case::Upper && plain_select(&tokens);
    // Only the keywords and plain names, written where a statement stands on its own
    // (`Query: select value from devices.`): SQL in any case, whatever ends it.
    let standalone = statement_position(segment, at) && bare_statement(start, &tokens);
    let accepted = in_code
        || standalone
        || match case {
            // The companion in any case (`SELECT kwh from devices`): a plain select list and
            // table anywhere, an upper-case companion or SQL-shaped names where a statement
            // stands on its own.
            Case::Upper => {
                let upper_companion = Case::of(companion) == Case::Upper;
                (statement_position(segment, at) && (upper_companion || sql_shaped(&tokens)))
                    || evidence(&tokens, start).total() >= 1
                    || (start == Start::Select && plain_select(&tokens))
            }
            // Anywhere in a sentence also a plain select list and table with a name with `_` or
            // a list with commas (`run select device_id, ts from tenant_telemetry`).
            Case::Lower => {
                let evidence = evidence(&tokens, start);
                evidence.strong >= 2
                    || upper_from()
                    || (start == Start::Select && plain_select(&tokens) && sql_shaped(&tokens))
                    || (statement_position(segment, at)
                        && (evidence.strong >= 1
                            || sql_shaped(&tokens)
                            || (extent.ending != Ending::Sentence && !has_article(&tokens))))
            }
            // Capitalized keywords (`Select device_id From devices`): only with SQL's
            // structure, a name with `_`, a list with commas or an operator, and no English
            // article, so a sentence that starts with "Select the meter from the list" stays.
            Case::Mixed => {
                upper_from()
                    || ((evidence(&tokens, start).strong >= 1 || sql_shaped(&tokens))
                        && !has_article(&tokens))
            }
        };
    accepted.then(|| removal(segment, at, opening, extent))
}

/// The range a statement at `at` that ends at `extent` takes out of `segment`: in inline code
/// that `opening` opens and the statement's end closes, the code with its backticks.
fn removal(segment: &str, at: usize, opening: Option<Opening>, extent: Extent) -> Range<usize> {
    let extent = if extent.capped {
        self::extent(segment, at, extent.code_run, None)
    } else {
        extent
    };
    let mut range = at..extent.end;
    match (extent.ending, opening) {
        (Ending::Semicolon, _) => range.end += 1,
        (Ending::Backtick, Some(opening)) => {
            range.start = opening.start;
            range.end += opening.run;
        }
        (Ending::Backtick | Ending::Sentence | Ending::Other, _) => {
            let kept = substr(segment, range.clone()).trim_end().len();
            range.end = range.start + kept;
        }
    }
    range
}

/// The keyword at `at` starts its line (after indentation and list or quote markers), follows
/// a colon, or starts a table cell: where a statement is written on its own.
fn statement_position(segment: &str, at: usize) -> bool {
    let line = substr(segment, ..at)
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    let prefix = substr(segment, line..at);
    container_content(prefix).is_empty() || prefix.trim_end().ends_with([':', '：', '|'])
}

/// The statement at `at` that ends at `extent` ends with a `;` or a backtick, or nothing but
/// blanks follows its last character on that line: text after it on the line (`SELECT 2 個設備`)
/// makes the words before it prose.
fn ends_its_line(segment: &str, at: usize, extent: Extent) -> bool {
    if matches!(extent.ending, Ending::Semicolon | Ending::Backtick) {
        return true;
    }
    let text = substr(segment, at..extent.end);
    let blank_tail = substr(text, text.trim_end().len()..);
    // Reads up to the first character that is not a blank, so each candidate reads little.
    blank_tail.contains('\n')
        || substr(segment, extent.end..)
            .bytes()
            .take_while(|byte| *byte != b'\n')
            .all(|byte| byte.is_ascii_whitespace())
}

/// After the keyword, the statement is only an expression list with a value in it (`SELECT 1`,
/// `SELECT 1 + 1 AS two`, `SELECT 'a', 2`): numbers, strings, ASCII operators and punctuation,
/// upper-case keywords, names with `_` or a dot, names in backticks, function names, any name
/// among a call's arguments (`max(kwh)`), and `AS` (in any case) with its alias. A word of
/// prose (`SELECT 3 devices`) or text in another script makes it prose.
fn expression_only(tokens: &[Token<'_>]) -> bool {
    let rest = tokens.get(1..).unwrap_or_default();
    let mut value = false;
    // For each open parenthesis, whether it is a call's.
    let mut parentheses: Vec<bool> = Vec::new();
    for (index, token) in rest.iter().enumerate() {
        let expression = match token {
            Token::Number | Token::Str => {
                value = true;
                true
            }
            Token::Punct("(") => {
                let call = index
                    .checked_sub(1)
                    .and_then(|before| rest.get(before))
                    .is_some_and(|before| matches!(before, Token::Word { .. }));
                parentheses.push(call);
                true
            }
            Token::Punct(")") => {
                parentheses.pop();
                true
            }
            Token::Punct(punct) => punct.is_ascii(),
            Token::Word { dotted: true, .. } => true,
            Token::Word {
                text,
                dotted: false,
            } => {
                let next = rest.get(index + 1);
                let call = next == Some(&Token::Punct("("));
                value |= call;
                let alias = text.eq_ignore_ascii_case("AS")
                    && matches!(next, Some(Token::Word { dotted: false, .. }));
                call || alias
                    || parentheses.last() == Some(&true)
                    || Case::of(text) == Case::Upper
                    || text.contains('_')
                    || text.starts_with('`')
                    || index
                        .checked_sub(1)
                        .and_then(|before| rest.get(before))
                        .is_some_and(|before| before.is_word("AS"))
            }
        };
        if !expression {
            return false;
        }
    }
    value
}

/// Words of English that a select list or a table name is not: in `I used SELECT and FROM
/// clauses` the words around the keywords are prose.
const FUNCTION_WORDS: [&str; 20] = [
    "and", "or", "but", "nor", "to", "of", "in", "on", "at", "for", "with", "by", "from", "into",
    "is", "are", "was", "were", "be", "then",
];

fn is_function_word(token: &Token<'_>) -> bool {
    matches!(token, Token::Word { text, dotted: false }
        if FUNCTION_WORDS.iter().chain(&ARTICLES).any(|word| text.eq_ignore_ascii_case(word)))
}

/// `SELECT` names columns and `FROM` a table, written as SQL anywhere in a sentence
/// (`Run SELECT kwh FROM t to see`): between them only names, `*` and commas, and a name after
/// `FROM`, none of them an English article or function word.
fn plain_select(tokens: &[Token<'_>]) -> bool {
    let Some(from) = (2..tokens.len()).find(|&index| tokens[index].is_word("FROM")) else {
        return false;
    };
    let list = tokens.get(1..from).unwrap_or_default();
    let names = list.iter().all(|token| {
        matches!(token, Token::Word { .. } | Token::Punct("," | "*")) && !is_function_word(token)
    });
    let table = tokens
        .get(from + 1)
        .is_some_and(|token| matches!(token, Token::Word { .. }) && !is_function_word(token));
    names && table
}

/// The statement is only its keywords and plain names, with nothing after them but
/// punctuation: `select value from devices.` (a [`plain_select`] that ends at its table) or
/// `delete from devices`. That is SQL's whole structure and no word of prose, where a sentence
/// such as "Select meters from groups to compare them" goes on after the names.
fn bare_statement(start: Start, tokens: &[Token<'_>]) -> bool {
    let only_punctuation = |from: usize| {
        tokens
            .get(from..)
            .unwrap_or_default()
            .iter()
            .all(|token| matches!(token, Token::Punct(_)))
    };
    match start {
        Start::Select => {
            plain_select(tokens)
                && (2..tokens.len())
                    .find(|&index| tokens[index].is_word("FROM"))
                    .is_some_and(|from| only_punctuation(from + 2))
        }
        Start::Delete => {
            tokens.get(1).is_some_and(|token| token.is_word("FROM"))
                && tokens.get(2).is_some_and(|token| {
                    matches!(token, Token::Word { .. }) && !is_function_word(token)
                })
                && only_punctuation(3)
        }
        _ => false,
    }
}

/// The statement names columns or tables as SQL does: a name with `_`, or a list with commas.
fn sql_shaped(tokens: &[Token<'_>]) -> bool {
    tokens.iter().any(|token| match token {
        Token::Word { text, .. } => text.contains('_'),
        Token::Punct(punct) => *punct == ",",
        _ => false,
    })
}

/// English articles and determiners, which prose has and a statement's names do not.
const ARTICLES: [&str; 14] = [
    "a", "an", "the", "this", "that", "these", "those", "my", "your", "our", "their", "its", "his",
    "her",
];

fn has_article(tokens: &[Token<'_>]) -> bool {
    tokens.iter().any(|token| {
        matches!(token, Token::Word { text, dotted: false }
            if ARTICLES.iter().any(|article| text.eq_ignore_ascii_case(article)))
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    Upper,
    Lower,
    Mixed,
}

impl Case {
    fn of(word: &str) -> Self {
        let letters = || word.bytes().filter(u8::is_ascii_alphabetic);
        if letters().all(|byte| byte.is_ascii_uppercase()) {
            Self::Upper
        } else if letters().all(|byte| byte.is_ascii_lowercase()) {
            Self::Lower
        } else {
            Self::Mixed
        }
    }
}

/// Where a statement starting at `at` ends, and with what: a `;` (at the returned offset), a
/// backtick (in inline code opened by a run of `code_run` backticks, the run of that length that
/// closes it), a table cell's `|`, a blank line, a line that starts a list item, a quote or a
/// heading, a `.` that ends a sentence, or a non-ASCII character outside a string. A name in
/// backticks (`` `device_id` ``) is part of the statement. Reads at most `limit` bytes when given.
fn extent(segment: &str, at: usize, code_run: usize, limit: Option<usize>) -> Extent {
    let bytes = segment.as_bytes();
    let stop = limit.map_or(bytes.len(), |limit| {
        bytes.len().min(at.saturating_add(limit))
    });
    let mut quote: Option<u8> = None;
    let mut quoted = false;
    let mut i = at;
    let found = |end, ending, quoted| Extent {
        end,
        ending,
        quoted,
        capped: false,
        code_run,
    };
    while i < stop {
        let byte = bytes[i];
        if let Some(open) = quote {
            if byte == open {
                quote = None;
            } else if byte == b'\n' {
                // An unclosed string ends at its line; the line's end is looked at again.
                quote = None;
                continue;
            }
            i += 1;
            continue;
        }
        match byte {
            b'\'' | b'"' if opens_string(bytes, i) => {
                quote = Some(byte);
                quoted = true;
            }
            b';' => return found(i, Ending::Semicolon, quoted),
            b'`' => {
                if let Some(end) = quoted_name_end(bytes, i).filter(|&end| end <= stop) {
                    quoted = true;
                    i = end;
                    continue;
                }
                let run = bytes
                    .get(i..stop)
                    .unwrap_or_default()
                    .iter()
                    .take_while(|byte| **byte == b'`')
                    .count();
                // Another run inside inline code (CommonMark closes it only with the same
                // length) ends the statement but not the code.
                let ending = if code_run == 0 || run == code_run {
                    Ending::Backtick
                } else {
                    Ending::Other
                };
                return found(i, ending, quoted);
            }
            // A table cell's end; `||` is SQL's concatenation.
            b'|' if bytes.get(i + 1) != Some(&b'|') && (i == at || bytes[i - 1] != b'|') => {
                return found(i, Ending::Other, quoted);
            }
            b'\n' if ends_paragraph(substr(segment, i + 1..)) => {
                return found(i, Ending::Other, quoted);
            }
            b'.' if bytes
                .get(i + 1)
                .is_none_or(|next| next.is_ascii_whitespace()) =>
            {
                return found(i, Ending::Sentence, quoted);
            }
            byte if !byte.is_ascii() => return found(i, Ending::Other, quoted),
            _ => {}
        }
        i += 1;
    }
    Extent {
        end: stop,
        ending: Ending::Other,
        quoted,
        capped: stop < bytes.len(),
        code_run,
    }
}

/// A quote at `i` opens a string: not an apostrophe inside a word, such as "it's".
fn opens_string(bytes: &[u8], i: usize) -> bool {
    i == 0 || !bytes[i - 1].is_ascii_alphanumeric()
}

/// The next line is blank, or starts a list item, a quote or a heading. Reads only the line's
/// start.
fn ends_paragraph(next: &str) -> bool {
    let content = next.trim_start_matches([' ', '\t']);
    content.is_empty()
        || content.starts_with(['\n', '\r', '>', '#'])
        || container_content(content).len() != content.len()
}

/// The first `max` tokens of `text`.
fn tokenize(text: &str, max: usize) -> Vec<Token<'_>> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() && tokens.len() < max {
        let byte = bytes[i];
        if byte.is_ascii_whitespace() {
            i += 1;
        } else if byte.is_ascii_alphabetic() || byte == b'_' {
            let start = i;
            let mut dotted = false;
            loop {
                while i < bytes.len() && is_word_byte(bytes[i]) {
                    i += 1;
                }
                let continues = bytes.get(i) == Some(&b'.')
                    && bytes
                        .get(i + 1)
                        .is_some_and(|next| next.is_ascii_alphabetic() || *next == b'_');
                if !continues {
                    break;
                }
                dotted = true;
                i += 1;
            }
            tokens.push(Token::Word {
                text: substr(text, start..i),
                dotted,
            });
        } else if byte.is_ascii_digit() {
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.') {
                i += 1;
            }
            tokens.push(Token::Number);
        } else if (byte == b'\'' || byte == b'"') && opens_string(bytes, i) {
            let close = substr(text, i + 1..).find(char::from(byte));
            i = close.map_or(bytes.len(), |close| i + 1 + close + 1);
            tokens.push(Token::Str);
        } else if let Some(end) = quoted_name_end(bytes, i) {
            // A name in backticks is a name, never a keyword: its text keeps the backticks.
            tokens.push(Token::Word {
                text: substr(text, i..end),
                dotted: false,
            });
            i = end;
        } else {
            let two = text.get(i..i + 2);
            let length = match two {
                Some("!=" | "<>" | "<=" | ">=" | "::" | "||") => 2,
                _ => substr(text, i..).chars().next().map_or(1, char::len_utf8),
            };
            tokens.push(Token::Punct(substr(text, i..i + length)));
            i += length;
        }
    }
    tokens
}

/// The word that confirms `start` begins a statement (`FROM` after `SELECT`, …), as written.
fn companion<'a>(start: Start, tokens: &[Token<'a>]) -> Option<&'a str> {
    let word = |index: usize| match tokens.get(index) {
        Some(Token::Word { text, .. }) => Some(*text),
        _ => None,
    };
    let is = |index: usize, keyword: &str| tokens.get(index).is_some_and(|t| t.is_word(keyword));
    let followed = |index: usize| {
        matches!(
            tokens.get(index + 1),
            Some(Token::Word { .. } | Token::Punct("("))
        )
    };
    let find_from = |from: usize, keyword: &str| {
        (from..tokens.len()).find(|&index| tokens[index].is_word(keyword))
    };
    match start {
        Start::Select => {
            let from = find_from(2, "FROM")?;
            followed(from).then(|| word(from)).flatten()
        }
        Start::With => {
            let as_at = find_from(1, "AS")?;
            let select = find_from(as_at + 1, "SELECT")?;
            let from = find_from(select + 2, "FROM")?;
            followed(from).then(|| word(select)).flatten()
        }
        Start::Insert => (is(1, "INTO") && word(2).is_some())
            .then(|| word(1))
            .flatten(),
        Start::Update => (word(1).is_some() && is(2, "SET"))
            .then(|| word(2))
            .flatten(),
        Start::Delete => (is(1, "FROM") && word(2).is_some())
            .then(|| word(1))
            .flatten(),
        Start::Create | Start::Drop | Start::Alter => {
            let skippable = [
                "OR",
                "REPLACE",
                "TEMPORARY",
                "TEMP",
                "MATERIALIZED",
                "UNIQUE",
            ];
            let object = (1..tokens.len().min(5))
                .find(|&index| !skippable.iter().any(|skip| tokens[index].is_word(skip)))?;
            let name = word(object)?;
            (OBJECTS
                .iter()
                .any(|object| name.eq_ignore_ascii_case(object))
                && word(object + 1).is_some())
            .then_some(name)
        }
        Start::Truncate => (is(1, "TABLE") && word(2).is_some())
            .then(|| word(1))
            .flatten(),
        Start::Grant | Start::Revoke => {
            let on = find_from(2, "ON")?;
            let target = if start == Start::Grant { "TO" } else { "FROM" };
            find_from(on + 2, target)?;
            word(on)
        }
        Start::Explain => {
            let next = word(1)?;
            [
                "SELECT", "WITH", "ANALYZE", "PLAN", "PIPELINE", "SYNTAX", "AST",
            ]
            .iter()
            .any(|keyword| next.eq_ignore_ascii_case(keyword))
            .then_some(next)
        }
        Start::Show => {
            let next = word(1)?;
            SHOWN
                .iter()
                .any(|keyword| next.eq_ignore_ascii_case(keyword))
                .then_some(next)
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Evidence {
    /// Operators, parentheses, `*`, strings, dotted names and keywords that are SQL in any case.
    strong: usize,
    /// Upper-case keywords that are English words in lower case.
    weak: usize,
}

impl Evidence {
    fn total(self) -> usize {
        self.strong + self.weak
    }
}

fn evidence(tokens: &[Token<'_>], start: Start) -> Evidence {
    let mut evidence = Evidence::default();
    // `SELECT *` is SQL even in lower case.
    if start == Start::Select && tokens.get(1) == Some(&Token::Punct("*")) {
        evidence.strong += 2;
    }
    for (index, token) in tokens.iter().enumerate().skip(1) {
        match token {
            Token::Punct("(" | "*" | "=" | "<" | ">" | "!=" | "<>" | "<=" | ">=" | "::")
            | Token::Str => {
                evidence.strong += 1;
            }
            Token::Word { dotted: true, .. } => evidence.strong += 1,
            Token::Word {
                text,
                dotted: false,
            } => {
                let pair_by = (text.eq_ignore_ascii_case("GROUP")
                    || text.eq_ignore_ascii_case("ORDER"))
                    && tokens.get(index + 1).is_some_and(|next| next.is_word("BY"));
                if pair_by
                    || STRONG_KEYWORDS
                        .iter()
                        .any(|keyword| text.eq_ignore_ascii_case(keyword))
                {
                    evidence.strong += 1;
                } else if Case::of(text) == Case::Upper
                    && WEAK_KEYWORDS.iter().any(|keyword| text == keyword)
                {
                    evidence.weak += 1;
                }
            }
            Token::Number | Token::Punct(_) => {}
        }
    }
    evidence
}

// ---------------------------------------------------------------------------------------------
// Inline code

/// The inline code spans of `gap` that hold code, backticks included. A span is CommonMark's: a
/// run of backticks up to the next run of the same length in the same paragraph, whatever its
/// length. Spans do not overlap, so each byte is read once.
fn inline_code_in(text: &str, gap: Range<usize>) -> Vec<Finding> {
    let segment = substr(text, gap.clone());
    let bytes = segment.as_bytes();
    // The runs of backticks (start, length), and where blank lines start.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut blank_lines: Vec<usize> = Vec::new();
    let mut i = 0;
    let mut line_blank = true;
    while i < bytes.len() {
        match bytes[i] {
            b'`' => {
                let start = i;
                while i < bytes.len() && bytes[i] == b'`' {
                    i += 1;
                }
                runs.push((start, i - start));
                line_blank = false;
                continue;
            }
            b'\n' => {
                if line_blank && i > 0 {
                    blank_lines.push(i);
                }
                line_blank = true;
            }
            b' ' | b'\t' | b'\r' => {}
            _ => line_blank = false,
        }
        i += 1;
    }
    // For each run, the next run of the same length.
    let mut next_same: Vec<Option<usize>> = vec![None; runs.len()];
    let mut later: HashMap<usize, usize> = HashMap::new();
    for (index, (_, length)) in runs.iter().enumerate().rev() {
        next_same[index] = later.insert(*length, index);
    }
    let mut findings = Vec::new();
    let mut index = 0;
    while index < runs.len() {
        let (start, length) = runs[index];
        let Some(close) = next_same[index] else {
            index += 1;
            continue;
        };
        let (close_start, close_length) = runs[close];
        let inner = start + length..close_start;
        let within_paragraph = || {
            let first = blank_lines.partition_point(|&blank| blank < inner.start);
            blank_lines
                .get(first)
                .is_none_or(|&blank| blank >= inner.end)
        };
        if !within_paragraph() {
            // Not a span: the run is literal text.
            index += 1;
            continue;
        }
        if holds_code(substr(segment, inner)) {
            findings.push(Finding {
                range: gap.start + start..gap.start + close_start + close_length,
                kind: Removed::Code,
            });
        }
        index = close + 1;
    }
    findings
}

/// The span's text is code: an assignment or comparison, a statement's end, a block, an arrow,
/// or a call (a name followed by `(`).
fn holds_code(inner: &str) -> bool {
    inner.contains(['=', ';', '{', '}'])
        || inner.contains("->")
        || inner
            .as_bytes()
            .windows(2)
            .any(|pair| is_word_byte(pair[0]) && pair[1] == b'(')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cleaned(text: &str) -> (String, Vec<Removed>) {
        let guarded = guard(text);
        (guarded.text, guarded.removed.kinds().collect())
    }

    fn kept(text: &str) {
        let guarded = guard(text);
        assert_eq!(guarded.text, text, "{text:?} must stay as it is");
        assert!(
            guarded.removed.is_empty(),
            "{text:?}: {:?}",
            guarded.removed
        );
    }

    #[test]
    fn fenced_code_is_removed_with_its_fences_whatever_the_language() {
        assert_eq!(
            cleaned(
                "本週用電 690 kWh。\n\n```sql\nSELECT sum(value) FROM tenant_telemetry_1h\n```\n\n比上週少 5%。"
            ),
            (
                "本週用電 690 kWh。\n\n比上週少 5%。".to_owned(),
                vec![Removed::Code]
            )
        );
        assert_eq!(
            cleaned("Before\n~~~~ js\nconst a = 1;\n~~~\nstill code\n~~~~\nAfter"),
            ("Before\nAfter".to_owned(), vec![Removed::Code])
        );
        // In a list item and a quote, indented.
        assert_eq!(
            cleaned("- The page:\n  ```html\n  <div>1</div>\n  ```\n- Done\n> ```\n> x\n> ```"),
            ("- The page:\n- Done".to_owned(), vec![Removed::Code])
        );
    }

    #[test]
    fn an_unclosed_fence_runs_to_the_end() {
        assert_eq!(
            cleaned("The query:\n```\nSELECT 1\nand everything after it"),
            ("The query:".to_owned(), vec![Removed::Code])
        );
    }

    #[test]
    fn code_written_with_a_fences_backticks_on_one_line_is_removed() {
        assert_eq!(
            cleaned("Total below.\n```x = 1```\nThat is all."),
            ("Total below.\nThat is all.".to_owned(), vec![Removed::Code])
        );
    }

    #[test]
    fn html_tags_comments_and_declarations_are_removed_and_their_text_stays() {
        assert_eq!(
            cleaned("<div class=\"total\">總用電 <b>690</b> kWh</div>"),
            ("總用電 690 kWh".to_owned(), vec![Removed::Html])
        );
        assert_eq!(
            cleaned("<!DOCTYPE html>\n<html><body><p>Hi</p></body></html>"),
            ("Hi".to_owned(), vec![Removed::Html])
        );
        assert_eq!(
            cleaned("A<br/>B<!-- hidden note -->C<hr>D"),
            ("ABCD".to_owned(), vec![Removed::Html])
        );
        assert_eq!(
            cleaned("<svg viewBox=\"0 0 10 10\"><rect width='4' height=5 /></svg>chart"),
            ("chart".to_owned(), vec![Removed::Html])
        );
        // Custom elements, and tags of names HTML does not know: its parser reads them as
        // elements too.
        assert_eq!(
            cleaned("<tq-chart>x</tq-chart> <widget data=\"1\">y"),
            ("x y".to_owned(), vec![Removed::Html])
        );
        for markup in [
            "<widget>content</widget>",
            "<Widget size=3 hidden>content</Widget >",
            "<device_id>content</device_id>",
            "<x-y.z>content</x-y.z>",
            "<widget/>content",
            "</widget class=\"x\">content",
            "<svg:path d=\"M0 0\"/>content",
            "<xlink:href>content",
            "<urn:x>content",
            "<a中文>content</a中文>",
            "<a<b>content",
            "<a b<c>content",
            "<b 與 c>content",
            "<input/output>content",
            "<a =\"x\">content",
            "<p title='a' class=\"b\"hidden>content",
        ] {
            assert_eq!(
                cleaned(markup),
                ("content".to_owned(), vec![Removed::Html]),
                "{markup}"
            );
        }
        // HTML's parser lowers tag names: a custom element in upper case is one too.
        assert_eq!(
            cleaned("<TQ-CHART>x</TQ-CHART> <Tq-Chart>y</Tq-Chart>"),
            ("x y".to_owned(), vec![Removed::Html])
        );
        // A quoted value may hold `<` and `>`.
        assert_eq!(
            cleaned("<div title=\"a < b\">text</div> <p data-x='<b>' class=\"y>z\">p</p>"),
            ("text p".to_owned(), vec![Removed::Html])
        );
        // Prose between `<` and a letter and a `>` is a tag to the parser: the guard fails
        // closed.
        assert_eq!(
            cleaned("電壓<V1時跳脫，電流>5A時告警"),
            ("電壓5A時告警".to_owned(), vec![Removed::Html])
        );
        // A tag cut short in a quoted value: the parser swallows the rest.
        for markup in [
            "Text <img src=\"x\" onerror=\"alert(1)\"",
            "Text <div title=\"a > b",
            "Text <p class='x' hidden",
        ] {
            assert_eq!(
                cleaned(markup),
                ("Text".to_owned(), vec![Removed::Html]),
                "{markup}"
            );
        }
    }

    #[test]
    fn declarations_mathml_and_tags_written_with_references_are_removed() {
        for markup in [
            "<?xml version=\"1.0\"?>",
            "<![CDATA[ x < y ]]>",
            "<!ENTITY x \"y\">",
            "&lt;br&gt;",
            "&lt;tq-chart data-x=\"1\"/&gt;",
            "&lt;svg:path d=\"M0\"/&gt;",
            // A `>` in quotes does not end a declaration, and one without its end takes the rest.
            "<!DOCTYPE html SYSTEM \"x>y\">",
            "<!DOCTYPE html PUBLIC 'a>b' \"c>d\">",
            "<!ENTITY x \"a>b\">",
            "<?xml version=\"1.0\" x='a>b'?>",
            "<!DOCTYPE html SYSTEM \"x>y",
            // `</` before anything but a letter, `<?` and `<!` before anything.
            "</ div>",
            "</3>",
            "</>",
            "<?>",
            "<!>",
        ] {
            assert_eq!(
                cleaned(markup),
                (String::new(), vec![Removed::Html]),
                "{markup}"
            );
        }
        assert_eq!(
            cleaned("<math><mi>x</mi><mo>+</mo><mn>1</mn></math>"),
            ("x+1".to_owned(), vec![Removed::Html])
        );
        assert_eq!(
            cleaned(
                "&lt;div class=\"x\"&gt;hi&lt;/div&gt; &#60;b&#62;bold&#x3C;/b&#x3E; &LT;I&GT;i"
            ),
            ("hi bold i".to_owned(), vec![Removed::Html])
        );
        assert_eq!(
            cleaned("&lt;divider&gt;x&lt;/divider&gt;"),
            ("x".to_owned(), vec![Removed::Html])
        );
        // A comment ends at `-->` or `--!>`, or right away when empty.
        for (markup, left) in [
            ("<!-->x", "x"),
            ("<!--->x", "x"),
            ("<!-- a --!>x", "x"),
            ("<!-- a -- b --->x", "x"),
        ] {
            assert_eq!(
                cleaned(markup),
                (left.to_owned(), vec![Removed::Html]),
                "{markup}"
            );
        }
        for text in [
            "a &lt; b &gt; c",
            "&lt;5% of the readings",
            "I &lt;3 dashboards",
            "AT&amp;T",
            "&lt;b\n\nbold&gt;",
            "&lt;https://example.com&gt;",
            "&lt;ops@example.com&gt;",
        ] {
            kept(text);
        }
    }

    #[test]
    fn tags_of_any_length_are_removed_to_their_end() {
        let long = |byte: &str| byte.repeat(40_000);
        for (markup, left) in [
            (format!("<{}>x</{}>", long("a"), long("a")), "x"),
            // A long quoted value with a `>` in it: the tag ends after the value.
            (format!("<div title=\"a>b{}\">x", long("y")), "x"),
            (format!("<div data-x=\"{}\" hidden>x", long("y")), "x"),
            (format!("<div {}>x", long("a ")), "x"),
            (format!("<script {}>alert(1)</script>x", long("a ")), "x"),
            // Cut short in a quoted value: the parser swallows the rest.
            (
                format!("Text <div {}title=\"{}", long("a "), long("y")),
                "Text",
            ),
            (format!("&lt;div {}&gt;x", long("a ")), "x"),
            (format!("<!DOCTYPE html SYSTEM \"{}>\">x", long("y")), "x"),
        ] {
            assert_eq!(cleaned(&markup), (left.to_owned(), vec![Removed::Html]));
        }
        // An autolink stays whatever its length, and so does prose after a `<` and a letter
        // that no `>` follows.
        kept(&format!("<https://example.com/{}>", long("a")));
        kept(&format!("電壓<V1{}", long("時")));
        kept(&format!("Text <div {}", long("a ")));
    }

    #[test]
    fn script_and_style_elements_are_removed_with_their_content() {
        assert_eq!(
            cleaned("Before<script>tq.on(\"data\", (d) => { if (a < b) draw(); });</script>After"),
            ("BeforeAfter".to_owned(), vec![Removed::Html])
        );
        assert_eq!(
            cleaned("A<STYLE type=\"text/css\">p { color: red }</Style >B<script>unclosed"),
            ("AB".to_owned(), vec![Removed::Html])
        );
        assert_eq!(
            cleaned("x<!-- unclosed"),
            ("x".to_owned(), vec![Removed::Html])
        );
        // A `/` before the `>` does not close them: HTML's parser ignores it there.
        assert_eq!(
            cleaned("A<script/>alert(1)</script>B<style />p { color: red }</style>C<SCRIPT/>x"),
            ("ABC".to_owned(), vec![Removed::Html])
        );
        // The end tag may carry attributes or a `/`; another name does not end the element.
        assert_eq!(
            cleaned(
                "A<script>x</script foo=\"a>b\">B<script>y</scripts>z</script/>C<style>p</style\n>D"
            ),
            ("ABCD".to_owned(), vec![Removed::Html])
        );
    }

    #[test]
    fn a_less_than_sign_that_starts_no_tag_stays() {
        for text in [
            "超過 <5% 的電表",
            "電壓<220V 時",
            "if a < b and c > d",
            "x<y",
            "1<2 且 3>2",
            "<電表 A>",
            "See <https://example.com/docs>",
            "Mail <ops@example.com>",
            "<= 5 kWh",
            "a <- b",
            "<3",
            "電壓<V1時跳脫",
            "if x<y and y<z, it's fine",
            "如果 a<b 且 c=d",
            "Use a <-> b",
            "See <HTTPS://EXAMPLE.COM/a?b=c>",
            "Write to <mailto:ops@example.com>",
            "</",
        ] {
            kept(text);
        }
    }

    #[test]
    fn sql_statements_are_removed_from_prose_and_inline_code() {
        assert_eq!(
            cleaned(
                "I computed it with SELECT sum(value_sum) FROM tenant_telemetry_1h WHERE bucket >= now() - INTERVAL 7 DAY; it is 690 kWh."
            ),
            (
                "I computed it with it is 690 kWh.".to_owned(),
                vec![Removed::Sql]
            )
        );
        assert_eq!(
            cleaned(
                "我用 SELECT max(value_max) AS peak FROM tenant_telemetry_1h 算出尖峰 35.5 kW。"
            ),
            ("我用 算出尖峰 35.5 kW。".to_owned(), vec![Removed::Sql])
        );
        assert_eq!(
            cleaned("查詢是 `select device_id from tenant_telemetry_1h`，結果有 3 台。"),
            ("查詢是，結果有 3 台。".to_owned(), vec![Removed::Sql])
        );
        assert_eq!(
            cleaned(
                "Query:\n\nSELECT device_id,\n  max(value_max) AS peak\nFROM tenant_telemetry_1h\nGROUP BY device_id\n\nThe peak was 35.5 kW."
            ),
            (
                "Query:\n\nThe peak was 35.5 kW.".to_owned(),
                vec![Removed::Sql]
            )
        );
        assert_eq!(
            cleaned(
                "with daily as (select toStartOfDay(bucket, 'UTC') as day from tenant_telemetry_1h) select day from daily"
            ),
            (String::new(), vec![Removed::Sql])
        );
        assert_eq!(
            cleaned("SQL：SELECT device_id FROM tenant_telemetry_1h"),
            ("SQL：".to_owned(), vec![Removed::Sql])
        );
        assert_eq!(
            cleaned("- SELECT name FROM devices\n- 另一項"),
            ("- 另一項".to_owned(), vec![Removed::Sql])
        );
        for statement in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1",
            "DELETE FROM t WHERE a = 1",
            // The statements that change a schema are in tests/answer_guard.rs: repo-check keeps
            // them out of services/*/src.
            "GRANT SELECT ON t TO r",
            "REVOKE SELECT ON t FROM r",
            "EXPLAIN SELECT 1",
            "SHOW TABLES",
            "select * from t",
        ] {
            assert_eq!(
                cleaned(statement),
                (String::new(), vec![Removed::Sql]),
                "{statement}"
            );
        }
    }

    #[test]
    fn inline_code_that_holds_code_is_removed_and_names_in_backticks_stay() {
        assert_eq!(
            cleaned("Set it with `const x = 1` first."),
            ("Set it with first.".to_owned(), vec![Removed::Code])
        );
        assert_eq!(
            cleaned("頁面以 `tq.text(el, ref)` 顯示數字。"),
            ("頁面以 顯示數字。".to_owned(), vec![Removed::Code])
        );
        assert_eq!(
            cleaned("The total is `sum(daily.kwh)`."),
            ("The total is.".to_owned(), vec![Removed::Code])
        );
        assert_eq!(
            cleaned("Use ``a = `b` `` here; `x -> y` too."),
            ("Use here; too.".to_owned(), vec![Removed::Code])
        );
        // A tag that is a whole span goes with its backticks.
        assert_eq!(
            cleaned("Wrap it in `<div>` here."),
            ("Wrap it in here.".to_owned(), vec![Removed::Html])
        );
        // However long the span is.
        let long = format!("Run `{}draw(rows)` once.", "x + ".repeat(4_000));
        assert_eq!(
            cleaned(&long),
            ("Run once.".to_owned(), vec![Removed::Code])
        );
        for text in [
            "資料集 `daily` 的 `kwh` 欄，時區 `Asia/Taipei`。",
            "The mark `c|daily|kwh|0` shows 12.5 `kWh` on `2026-10-26`.",
            "超過 `<5%` 的電表",
            "An unclosed `backtick stays = as it is",
            "A span across paragraphs `a = 1\n\nb` is no span.",
        ] {
            kept(text);
        }
    }

    #[test]
    fn a_select_without_from_is_removed_on_its_own_line_or_as_code() {
        assert_eq!(cleaned("SELECT now()"), (String::new(), vec![Removed::Sql]));
        assert_eq!(
            cleaned("請執行：SELECT max(kwh)\n結果是 42。"),
            ("請執行：\n結果是 42。".to_owned(), vec![Removed::Sql])
        );
        assert_eq!(
            cleaned("時間由 `SELECT now()` 取得。"),
            ("時間由 取得。".to_owned(), vec![Removed::Sql])
        );
        // Only an expression after it: a value, operators, keywords, function names, aliases.
        for statement in [
            "SELECT 1",
            "SELECT 1;",
            "SELECT 1 + 1 AS two",
            "SELECT 'a', 2",
            "SELECT -1, NULL",
            "SELECT toDate(1)",
        ] {
            assert_eq!(
                cleaned(statement),
                (String::new(), vec![Removed::Sql]),
                "{statement}"
            );
        }
        assert_eq!(
            cleaned("先確認連線：\nSELECT 1\n再查詢。"),
            ("先確認連線：\n再查詢。".to_owned(), vec![Removed::Sql])
        );
        assert_eq!(
            cleaned("用 `SELECT 1` 確認。"),
            ("用 確認。".to_owned(), vec![Removed::Sql])
        );
        // In lower case only with nothing but an expression on its line.
        for statement in ["select 1", "select now();", "select 1 + 1 as two"] {
            assert_eq!(
                cleaned(statement),
                (String::new(), vec![Removed::Sql]),
                "{statement}"
            );
        }
        for text in [
            "Press SELECT to continue.",
            "SELECT and OK are the two buttons.",
            "SELECT 3 devices on the left.",
            "SELECT ALL to pick every device.",
            "SELECT 2 個設備",
            "select 2 devices to compare",
            "select (or tap) the meter",
            "Select 1",
            "Step 2: SELECT 2 rows to compare.",
            "Press SELECT (the left button) twice.",
            "you can select now() or later",
            "Then select (or tap) it.",
        ] {
            kept(text);
        }
        // In a sentence: only an expression with a call, an operator or a string.
        assert_eq!(
            cleaned("請執行 select max(kwh) 看尖峰。"),
            ("請執行 看尖峰。".to_owned(), vec![Removed::Sql])
        );
        assert_eq!(
            cleaned("尖峰用 SELECT max(`kwh`) AS peak，平均用 select avg(kwh)。"),
            ("尖峰用，平均用。".to_owned(), vec![Removed::Sql])
        );
    }

    #[test]
    fn a_lower_case_statement_on_its_own_line_is_removed() {
        assert_eq!(
            cleaned("select device_id from t"),
            (String::new(), vec![Removed::Sql])
        );
        assert_eq!(
            cleaned("結果：\nselect name from devices\n共 3 台。"),
            ("結果：\n共 3 台。".to_owned(), vec![Removed::Sql])
        );
        assert_eq!(
            cleaned("- select a, b from t\n- 另一項"),
            ("- 另一項".to_owned(), vec![Removed::Sql])
        );
        for text in [
            "- select the meter from the list",
            "select one meter from your list.",
            "You can select device_id from the menu.",
            "select a meter from a site",
        ] {
            kept(text);
        }
    }

    #[test]
    fn upper_case_select_from_with_a_plain_list_and_table_cells_are_statements() {
        assert_eq!(
            cleaned("查詢是 SELECT kwh FROM tenant_telemetry_1h，結果如下。"),
            ("查詢是，結果如下。".to_owned(), vec![Removed::Sql])
        );
        // The statement runs to the sentence's end.
        assert_eq!(
            cleaned("Run SELECT kwh, peak FROM t to see."),
            ("Run.".to_owned(), vec![Removed::Sql])
        );
        // A table cell is a statement's own place, and `|` ends it; `||` does not.
        assert_eq!(
            cleaned("| query | rows |\n| SELECT a FROM t | 1 |\n| SELECT x || y FROM t | 2 |"),
            (
                "| query | rows |\n| | 1 |\n| | 2 |".to_owned(),
                vec![Removed::Sql]
            )
        );
        // The companion in any case.
        for statement in [
            "SELECT kwh from devices",
            "SELECT kwh, peak from devices",
            "DELETE from devices_old",
        ] {
            assert_eq!(
                cleaned(statement),
                (String::new(), vec![Removed::Sql]),
                "{statement}"
            );
        }
        assert_eq!(
            cleaned("查詢是 SELECT kwh from meters，結果如下。"),
            ("查詢是，結果如下。".to_owned(), vec![Removed::Sql])
        );
        // And an upper-case companion after a keyword in any case.
        for statement in [
            "請用 select kwh FROM meters 查詢。",
            "Select kwh FROM meters",
        ] {
            assert_eq!(cleaned(statement).1, vec![Removed::Sql], "{statement}");
        }
        for text in [
            "SELECT the meter from the list.",
            "Press SELECT and then from the menu choose one.",
            "DELETE from the draft what you do not need.",
            "I used SELECT and FROM clauses.",
            "Choose SELECT ALL FROM the menu.",
            "Press SELECT, then pick a FROM date.",
            "| 1 | SELECT a device |",
            "| SELECT | FROM |",
            "The ratio is 3 | 4.",
            "Use x || y to join.",
        ] {
            kept(text);
        }
    }

    #[test]
    fn capitalized_keywords_with_sqls_structure_are_a_statement() {
        for statement in [
            "Select device_id From devices",
            "Select max(kwh) From t",
            "Select kwh, peak From t",
            "Delete From devices Where id = 1",
        ] {
            assert_eq!(
                cleaned(statement),
                (String::new(), vec![Removed::Sql]),
                "{statement}"
            );
        }
        assert_eq!(
            cleaned("請用 Select device_id, ts From tenant_telemetry 查詢。"),
            ("請用 查詢。".to_owned(), vec![Removed::Sql])
        );
        for text in [
            "Select a device, then From the menu pick one.",
            "Update the draft Set to weekly totals.",
            "Select Devices From The Menu",
        ] {
            kept(text);
        }
    }

    #[test]
    fn names_in_backticks_are_part_of_a_statement_in_prose_and_in_code() {
        for statement in [
            "SELECT `device_id` FROM `devices`",
            "SELECT `device_id`, max(`kwh`) FROM `trustiot`.`tenant_telemetry_1h` GROUP BY `device_id`;",
            "select `device_id` from `devices`",
            "Select `device_id` From `devices`",
            // Inline code opened by two backticks holds single ones (CommonMark), padded or not.
            "``SELECT `device_id` FROM `devices` ``",
            "`` SELECT `device_id` FROM `devices` ``",
            "``select `kwh` from `meters` ``",
            // Single backticks around a name inside single-backtick code: still the statement.
            "`SELECT `device_id` FROM devices`",
        ] {
            assert_eq!(
                cleaned(statement),
                (String::new(), vec![Removed::Sql]),
                "{statement}"
            );
        }
        assert_eq!(
            cleaned("Use ``SELECT `kwh` FROM `meters` `` to check."),
            ("Use to check.".to_owned(), vec![Removed::Sql])
        );
        assert_eq!(
            cleaned("用電量：SELECT sum(`kwh`) FROM `meters` 之後再看。"),
            ("用電量： 之後再看。".to_owned(), vec![Removed::Sql])
        );
        assert_eq!(
            cleaned("Peak at 14:00.\n\nSELECT `kwh` FROM `meters` WHERE `kwh` > 5\n\nThat is all."),
            (
                "Peak at 14:00.\n\nThat is all.".to_owned(),
                vec![Removed::Sql]
            )
        );
        // The run that closes the code is the one of the same length; the text after it stays.
        assert_eq!(
            cleaned("Both ``SELECT `a` FROM `t` `` and `meters` are named."),
            (
                "Both and `meters` are named.".to_owned(),
                vec![Removed::Sql]
            )
        );
        for text in [
            "Pick `kwh` from `meters` to compare.",
            "Select `kwh` from the `meters` list.",
            "`SELECT` and `FROM` are the words SQL uses.",
            "Use `SELECT`, `FROM` and `WHERE`.",
            "The column is ``a`b`` in the export.",
            "請到 `devices` 頁面 select 一個電表。",
        ] {
            kept(text);
        }
    }

    #[test]
    fn lower_case_selects_with_sql_names_are_statements_anywhere() {
        assert_eq!(
            cleaned("請用 select device_id, ts from tenant_telemetry 查詢。"),
            ("請用 查詢。".to_owned(), vec![Removed::Sql])
        );
        assert_eq!(
            cleaned("I ran select device_id from devices to check."),
            ("I ran.".to_owned(), vec![Removed::Sql])
        );
        // In an indented code block, as in prose at the start of a line.
        assert_eq!(
            cleaned("The query:\n\n    select device_id from devices\n\nDone."),
            ("The query:\n\nDone.".to_owned(), vec![Removed::Sql])
        );
        for text in [
            "You can select meters from the list.",
            "Please select kwh or peak from the menu, then compare.",
            "We select the meter from building A.",
        ] {
            kept(text);
        }
    }

    #[test]
    fn prose_with_sql_words_stays() {
        for text in [
            "Select the meter from the list.",
            "select the meter from the list",
            "I used SELECT and FROM clauses.",
            "請在選單中 select 一個電表，再從 from 欄位開始。",
            "我們可以 SELECT 一段時間，再看 FROM 哪一天開始",
            "UPDATE: the draft now shows the peak.",
            "With the data from last week, the peak rose.",
            "Show tables of energy use by month.",
            "please update the draft set to weekly totals",
            "Drop the outliers from the table, then compare.",
            "You can create a table view later.",
            "Explain the peak.",
            "SELECT",
            "用電量 select 了 5 台電表",
        ] {
            kept(text);
        }
    }

    #[test]
    fn chinese_text_with_numbers_and_symbols_stays() {
        for text in [
            "本週總用電為 690 kWh，比上週少 5%（<5% 的變化視為持平）。",
            "尖峰 35.5 kW 出現在 2026-10-21 14:00；平均 28.7 kW。",
            "1. 總用電：690 kWh\n2. 尖峰：35.5 kW\n\n> 註：資料到 2026-10-26。",
            "儀表板「二號產線」顯示每日用電與目標 1,200 kWh 的比較。",
        ] {
            kept(text);
        }
    }

    #[test]
    fn mixed_content_reports_every_kind_and_tidies_what_is_left() {
        let text = "儀表板已更新：\n\n```html\n<div id=\"x\"></div>\n```\n\n- <b>總用電</b> 690 kWh\n- `SELECT sum(x) FROM t`\n\n\n\n結束。";
        let guarded = guard(text);
        assert_eq!(guarded.text, "儀表板已更新：\n\n- 總用電 690 kWh\n\n結束。");
        assert_eq!(
            guarded.removed.kinds().collect::<Vec<_>>(),
            vec![Removed::Code, Removed::Html, Removed::Sql]
        );
        assert_eq!(guarded.removed.names(), "code,html,sql");
        assert_eq!(
            serde_json::to_value(guarded.removed).expect("JSON"),
            serde_json::json!(["code", "html", "sql"])
        );
    }

    #[test]
    fn find_reports_ranges_for_streaming_and_an_open_fence_reaches_the_end() {
        let text = "a <b>x</b>\n```\ny\n```\nSELECT a FROM t\n```\nopen";
        let findings = find(text);
        let pieces: Vec<(&str, Removed)> = findings
            .iter()
            .map(|finding| (substr(text, finding.range.clone()), finding.kind))
            .collect();
        assert_eq!(
            pieces,
            vec![
                ("<b>", Removed::Html),
                ("</b>", Removed::Html),
                ("```\ny\n```\n", Removed::Code),
                ("SELECT a FROM t", Removed::Sql),
                ("```\nopen", Removed::Code),
            ]
        );
        assert_eq!(findings.last().map(|f| f.range.end), Some(text.len()));
    }

    #[test]
    fn pending_holds_back_what_more_text_may_still_change() {
        for (text, expected) in [
            ("", 0),
            ("Done.", 0),
            ("Done.\n\n", 7),
            ("Done.\n\nNext", 7),
            // The last paragraph may still grow into a statement.
            ("Done.\n\nSELECT kwh", 7),
            // A tag that may still close, any number of paragraphs later.
            ("Done.\n\nText <div", 7),
            ("Text <div\n\nmore\n\nand more", 0),
            // A finding that reaches the end: a comment or fence that may grow.
            ("Done.\n\n<b>x</b>\n\nText <!-- open", 17),
            ("Done.\n\n```\ncode\n\nmore", 7),
            // A tag closed in an earlier paragraph is final.
            ("<div\n\nclass=x> after\n\nnext", 22),
        ] {
            assert_eq!(pending(text), expected, "{text:?}");
        }
    }

    /// Whatever follows, what lies before `pending` stays: for every prefix of a few answers,
    /// `find` of the prefix and of the whole answer agree on the findings that start before the
    /// prefix's `pending`.
    #[test]
    fn pending_marks_what_stays_final_in_every_prefix() {
        let answers = [
            "Total: 690 kWh.\n\nThe <b>peak</b> was 35 kW at 14:00.\n\nQuery: select value from devices.\n\nDone.",
            "Before <div class=\"x\"\n\ntitle='a>b'>after\n\n`x = 1` and `kwh`\n\n```sql\nSELECT 1\n```\n\nEnd &lt;b&gt;bold&lt;/b&gt;.",
            "a<b and\n\nc>d\n\n<!-- note\n\nstill -->\n\n請執行 select max(kwh) 看尖峰。\n\nx<y",
            "<script>\nif (a < b) {}\n\n</script>\n\nSELECT kwh\nFROM meters\n\n<!DOCTYPE html SYSTEM \"x>y\">ok",
            "- item <img src=\"a.png\"\n\n  alt='x'>\n\n> quote: select device_id, ts from t\n\n```\nopen fence",
        ];
        for answer in answers {
            let whole = find(answer);
            for end in (0..=answer.len()).filter(|&end| answer.is_char_boundary(end)) {
                let prefix = substr(answer, ..end);
                let pending = pending(prefix);
                assert!(pending <= end, "{prefix:?}");
                let before = |findings: &[Finding]| {
                    findings
                        .iter()
                        .filter(|finding| finding.range.start < pending)
                        .cloned()
                        .collect::<Vec<_>>()
                };
                assert_eq!(
                    before(&find(prefix)),
                    before(&whole),
                    "{prefix:?} with pending {pending}"
                );
            }
        }
    }

    #[test]
    fn a_bare_statement_where_statements_stand_is_removed_whatever_ends_it() {
        for (text, left) in [
            ("Query: select value from devices.", "Query:."),
            ("select value from devices.", ""),
            ("Select value from devices.", ""),
            ("- select kwh from meters!", ""),
            ("查詢：select value from devices。", "查詢：。"),
            ("Query: delete from devices.", "Query:."),
            (
                "Run this:\n\nselect value from trustiot.devices;\n\nThen read it.",
                "Run this:\n\nThen read it.",
            ),
        ] {
            assert_eq!(
                cleaned(text),
                (left.to_owned(), vec![Removed::Sql]),
                "{text}"
            );
        }
        for text in [
            "Select the meter from the list.",
            "Select meters from groups to compare them.",
            "You can select value from devices.",
            "- select one from the list.",
            "Delete from the draft what you do not need.",
            "Query: select value from devices to compare.",
        ] {
            kept(text);
        }
    }

    /// The scan reads the text about once however many statement words it holds, and a
    /// statement longer than the decision's window is still removed whole.
    #[test]
    fn long_and_repetitive_text_is_still_guarded() {
        let words = "select ".repeat(20_000);
        let guarded = guard(&format!("{words}\n\nSELECT a FROM t;"));
        assert!(
            guarded.text == words.trim_end(),
            "the words stay, the statement goes"
        );
        assert_eq!(guarded.removed.names(), "sql");
        // Tags cut short in quoted values, one inside the other.
        let tags = "<a x=\"".repeat(20_000);
        assert_eq!(cleaned(&tags), (String::new(), vec![Removed::Html]));
        let quoted_closings = "<a b=\">\" ".repeat(20_000);
        assert_eq!(
            cleaned(&quoted_closings),
            (String::new(), vec![Removed::Html])
        );
        // Comparisons that no `>` follows.
        let comparisons = "x<y ".repeat(20_000);
        assert!(guard(&comparisons).removed.is_empty());
        let names = "<a<".repeat(20_000);
        assert!(guard(&names).removed.is_empty());
        let scripts = "<script></script>".repeat(20_000);
        assert_eq!(
            cleaned(&format!("{scripts}ok")),
            ("ok".to_owned(), vec![Removed::Html])
        );
        // Longer than the tokens and bytes a decision reads at first.
        let columns = (0..3_000)
            .map(|n| format!("c{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        // Joined, not formatted: tools/sql-check reads a `format!` of SQL-like text as SQL built
        // from strings, and these are only the guard's input.
        let long = [
            "SELECT ",
            columns.as_str(),
            " FROM tenant_telemetry_1h WHERE bucket > now()",
        ]
        .concat();
        assert!(long.len() > MAX_STATEMENT_SCAN / 2);
        assert_eq!(
            cleaned(&format!("前\n\n{long}\n\n後")),
            ("前\n\n後".to_owned(), vec![Removed::Sql])
        );
        // Recognized within the first bytes, removed to its end past them.
        let longest = [
            "SELECT a FROM t WHERE a IN (",
            "1, ".repeat(MAX_STATEMENT_SCAN).as_str(),
            "2)",
        ]
        .concat();
        assert_eq!(
            cleaned(&format!("{longest}\n\n後")),
            ("後".to_owned(), vec![Removed::Sql])
        );
        let quoted = "'select ".repeat(20_000);
        assert!(guard(&quoted).removed.is_empty());
        // Escaped tags and declarations that never close.
        let escaped = "&lt;b x ".repeat(20_000);
        assert!(guard(&escaped).removed.is_empty());
        // A declaration that never closes swallows the rest, as HTML's parser reads it.
        let declarations = "<?a <!b ".repeat(20_000);
        assert_eq!(cleaned(&declarations), (String::new(), vec![Removed::Html]));
        let closed = "<?a> <!b> x ".repeat(20_000);
        assert_eq!(guard(&closed).text, "x ".repeat(20_000).trim_end());
        // Tags whose attributes never close, one after another, and one `>` at the end.
        let unquoted = format!("<a {}", "b ".repeat(20_000)).repeat(4);
        assert!(guard(&unquoted).removed.is_empty());
        assert_eq!(
            cleaned(&format!("{unquoted}>x")),
            ("x".to_owned(), vec![Removed::Html])
        );
        let comments = "<!-- a -- ".repeat(20_000);
        assert_eq!(cleaned(&comments), (String::new(), vec![Removed::Html]));
        let selects = "SELECT 1 x\n".repeat(20_000);
        assert!(guard(&selects).removed.is_empty());
        // Names in backticks and runs of backticks before keywords.
        let names = "select `a` ".repeat(20_000);
        assert!(guard(&names).removed.is_empty());
        let runs = "``select x ".repeat(20_000);
        assert!(guard(&runs).removed.is_empty());
    }

    #[test]
    fn an_apostrophe_in_a_word_opens_no_string() {
        assert_eq!(
            cleaned("It's computed by SELECT sum(x) FROM t. That's all."),
            (
                "It's computed by. That's all.".to_owned(),
                vec![Removed::Sql]
            )
        );
    }

    #[test]
    fn nothing_found_keeps_the_text_byte_for_byte() {
        let text = "  Leading and trailing spaces stay when nothing is removed.\n\n\n";
        assert_eq!(guard(text).text, text);
    }
}
