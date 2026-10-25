//! Segment files: one reading per line (JSON lines), in arrival order.
//!
//! A line is the telemetry table's row without the IDs the loader assigns:
//!
//! ```json
//! {"stream":"live","batch_seq":1,"received_at":"2026-01-05T22:10:02.500Z","device":"meter-1","point":"energy_kwh","ts":"2026-01-05T22:00:00.120Z","value_type":"float","value":15230.4}
//! ```
//!
//! The file's line order is the order the batches arrived and the loader inserts them in. The
//! rows of a batch are contiguous and share one `received_at`; within a stream the batches
//! arrive with consecutive sequence numbers, and `received_at` never decreases from one batch
//! to the next.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::index::{Index, MAX_TEXT_BYTES, Segment, ValueType};
use crate::time;

/// One reading of a segment.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    pub stream: String,
    pub batch_seq: u64,
    /// When the batch arrived (the ingest service's receive time).
    pub received_at: DateTime<Utc>,
    pub device: String,
    pub point: String,
    /// The reading time (the gateway's clock).
    pub ts: DateTime<Utc>,
    pub value: Value,
}

/// A reading's value, in the telemetry table's value column for its type.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Float(f64),
    Int(i64),
    Bool(bool),
    String(String),
}

impl Value {
    #[must_use]
    pub const fn value_type(&self) -> ValueType {
        match self {
            Self::Float(_) => ValueType::Float,
            Self::Int(_) => ValueType::Int,
            Self::Bool(_) => ValueType::Bool,
            Self::String(_) => ValueType::String,
        }
    }
}

/// Why a segment file was refused: the 1-based line, when one line is at fault.
#[derive(Debug)]
pub struct SegmentError {
    pub line: Option<usize>,
    pub message: String,
}

impl SegmentError {
    fn file(message: impl Into<String>) -> Self {
        Self {
            line: None,
            message: message.into(),
        }
    }

    fn line(line: usize, message: impl Into<String>) -> Self {
        Self {
            line: Some(line),
            message: message.into(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Line {
    stream: String,
    batch_seq: u64,
    received_at: String,
    device: String,
    point: String,
    ts: String,
    value_type: ValueType,
    value: serde_json::Value,
}

/// The largest integer every `f64` between it and its negation represents exactly.
const EXACT_FLOAT_INTEGER: u64 = 1 << 53;

fn value(declared: ValueType, value: serde_json::Value) -> Result<Value, String> {
    match (declared, value) {
        (ValueType::Float, serde_json::Value::Number(number)) => {
            // An integer-looking number is a float reading too, if f64 holds it exactly.
            let exact = number
                .as_u64()
                .map_or_else(|| number.as_i64().map(i64::unsigned_abs), Some);
            if exact.is_some_and(|magnitude| magnitude > EXACT_FLOAT_INTEGER) {
                return Err(format!("{number} is not exactly representable as a float"));
            }
            number
                .as_f64()
                .filter(|float| float.is_finite())
                .map(Value::Float)
                .ok_or_else(|| format!("{number} is not a finite float"))
        }
        (ValueType::Int, serde_json::Value::Number(number)) => number
            .as_i64()
            .map(Value::Int)
            .ok_or_else(|| format!("{number} is not a 64-bit integer")),
        (ValueType::Bool, serde_json::Value::Bool(flag)) => Ok(Value::Bool(flag)),
        (ValueType::String, serde_json::Value::String(text)) => {
            if text.len() > MAX_TEXT_BYTES {
                Err(format!("the text is longer than {MAX_TEXT_BYTES} bytes"))
            } else {
                Ok(Value::String(text))
            }
        }
        (declared, other) => Err(format!(
            "value_type is {} but the value is {other}",
            declared.as_str()
        )),
    }
}

/// SHA-256 of `bytes`, lowercase hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Tracks the batch and stream order of the lines read so far.
#[derive(Default)]
struct Arrival<'a> {
    /// The batch the previous line belonged to, and its receive time.
    current: Option<(String, u64, DateTime<Utc>)>,
    /// The last batch of each stream in this segment.
    last_of_stream: BTreeMap<String, u64>,
    ranges: BTreeMap<&'a str, (u64, u64)>,
}

impl Arrival<'_> {
    fn next(&mut self, reading: &Reading) -> Result<(), String> {
        if let Some((stream, seq, received_at)) = &self.current
            && *stream == reading.stream
            && *seq == reading.batch_seq
        {
            if *received_at != reading.received_at {
                return Err(format!(
                    "received_at differs within batch {stream}/{seq}: every row of a batch \
                     arrived together"
                ));
            }
            return Ok(());
        }
        let (first, last) = *self.ranges.get(reading.stream.as_str()).ok_or_else(|| {
            format!(
                "stream `{}` is not among the segment's batches in the index",
                reading.stream
            )
        })?;
        let expected = match self.last_of_stream.get(&reading.stream) {
            Some(previous) => previous.saturating_add(1),
            None => first,
        };
        if reading.batch_seq != expected || reading.batch_seq > last {
            return Err(format!(
                "batch {}/{} arrives where {}/{expected} is due (a stream's batches are \
                 contiguous runs of rows, consecutive, within the index's range {first} to {last})",
                reading.stream, reading.batch_seq, reading.stream
            ));
        }
        if let Some((_, _, previous)) = &self.current
            && reading.received_at < *previous
        {
            return Err(format!(
                "batch {}/{} was received at {}, before the batch preceding it",
                reading.stream,
                reading.batch_seq,
                time::format(reading.received_at)
            ));
        }
        self.last_of_stream
            .insert(reading.stream.clone(), reading.batch_seq);
        self.current = Some((
            reading.stream.clone(),
            reading.batch_seq,
            reading.received_at,
        ));
        Ok(())
    }
}

/// What a segment holds, compared with what its index entry states.
#[derive(Default)]
struct Totals<'a> {
    rows: u64,
    ts: Option<(DateTime<Utc>, DateTime<Utc>)>,
    received: Option<(DateTime<Utc>, DateTime<Utc>)>,
    series: BTreeMap<(&'a str, &'a str), u64>,
}

fn widen(
    range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    time: DateTime<Utc>,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    Some(range.map_or((time, time), |(start, end)| {
        (start.min(time), end.max(time))
    }))
}

/// Parses a segment file and checks it against its index entry: size, hash, every line, the
/// arrival order, and the totals (rows, time ranges, readings per point, batches). Returns the
/// readings in file order.
///
/// # Errors
///
/// The first difference found.
pub fn parse(index: &Index, segment: &Segment, bytes: &[u8]) -> Result<Vec<Reading>, SegmentError> {
    if u64::try_from(bytes.len()).ok() != Some(segment.bytes) {
        return Err(SegmentError::file(format!(
            "the file has {} bytes, the index states {}",
            bytes.len(),
            segment.bytes
        )));
    }
    let digest = sha256_hex(bytes);
    if digest != segment.sha256 {
        return Err(SegmentError::file(format!(
            "the file's SHA-256 is {digest}, the index states {}",
            segment.sha256
        )));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|error| SegmentError::file(format!("the file is not UTF-8: {error}")))?;
    let Some(body) = text.strip_suffix('\n') else {
        return Err(SegmentError::file(
            "the last line does not end with a newline",
        ));
    };
    let mut arrival = Arrival {
        ranges: segment
            .batches
            .iter()
            .map(|range| (range.stream.as_str(), (range.first, range.last)))
            .collect(),
        ..Arrival::default()
    };
    let mut totals = Totals::default();
    let mut seen = HashSet::new();
    let mut readings = Vec::new();
    for (number, line) in body.split('\n').enumerate() {
        let number = number + 1;
        let fail = |message: String| SegmentError::line(number, message);
        if line.trim().is_empty() {
            return Err(fail("empty line".to_owned()));
        }
        let parsed: Line = serde_json::from_str(line).map_err(|error| fail(error.to_string()))?;
        let device = index
            .device(&parsed.device)
            .ok_or_else(|| fail(format!("device `{}` is not in the index", parsed.device)))?;
        let point = device
            .points
            .iter()
            .find(|point| point.key == parsed.point)
            .ok_or_else(|| {
                fail(format!(
                    "device `{}` has no point `{}` in the index",
                    parsed.device, parsed.point
                ))
            })?;
        if parsed.value_type != point.value_type {
            return Err(fail(format!(
                "value_type is {}, the index declares {}/{} as {}",
                parsed.value_type.as_str(),
                parsed.device,
                parsed.point,
                point.value_type.as_str()
            )));
        }
        let value = value(point.value_type, parsed.value).map_err(fail)?;
        if let (Some(wraps_at), Value::Int(int)) = (point.wraps_at, &value)
            && !u64::try_from(*int).is_ok_and(|int| int < wraps_at)
        {
            return Err(fail(format!(
                "{int} lies outside 0 to {} (wraps_at)",
                wraps_at - 1
            )));
        }
        let reading = Reading {
            ts: time::parse(&parsed.ts).map_err(|error| fail(format!("ts: {error}")))?,
            received_at: time::parse(&parsed.received_at)
                .map_err(|error| fail(format!("received_at: {error}")))?,
            stream: parsed.stream,
            batch_seq: parsed.batch_seq,
            device: parsed.device,
            point: parsed.point,
            value,
        };
        arrival.next(&reading).map_err(fail)?;
        if !seen.insert((device.key.as_str(), point.key.as_str(), reading.ts)) {
            return Err(fail(format!(
                "a second reading of {}/{} at {}",
                reading.device,
                reading.point,
                time::format(reading.ts)
            )));
        }
        totals.rows += 1;
        totals.ts = widen(totals.ts, reading.ts);
        totals.received = widen(totals.received, reading.received_at);
        *totals
            .series
            .entry((device.key.as_str(), point.key.as_str()))
            .or_default() += 1;
        readings.push(reading);
    }
    compare(segment, &arrival, &totals)?;
    Ok(readings)
}

fn compare(
    segment: &Segment,
    arrival: &Arrival<'_>,
    totals: &Totals<'_>,
) -> Result<(), SegmentError> {
    let mismatch = |what: &str, found: String, stated: String| {
        Err(SegmentError::file(format!(
            "{what}: the file has {found}, the index states {stated}"
        )))
    };
    if totals.rows != segment.rows {
        return mismatch("rows", totals.rows.to_string(), segment.rows.to_string());
    }
    let range = |range: Option<(DateTime<Utc>, DateTime<Utc>)>| {
        range.map_or_else(String::new, |(start, end)| {
            format!("{} to {}", time::format(start), time::format(end))
        })
    };
    for (what, found, stated) in [
        ("reading_time", totals.ts, segment.reading_time),
        ("receive_time", totals.received, segment.receive_time),
    ] {
        if found != Some((stated.start, stated.end)) {
            return mismatch(what, range(found), range(Some((stated.start, stated.end))));
        }
    }
    for batches in &segment.batches {
        let last = arrival.last_of_stream.get(&batches.stream).copied();
        if last != Some(batches.last) {
            return mismatch(
                &format!("batches of stream `{}`", batches.stream),
                last.map_or_else(|| "none".to_owned(), |last| format!("up to {last}")),
                format!("{} to {}", batches.first, batches.last),
            );
        }
    }
    let stated: BTreeSet<(&str, &str, u64)> = segment
        .series
        .iter()
        .map(|s| (s.device.as_str(), s.point.as_str(), s.rows))
        .collect();
    let found: BTreeSet<(&str, &str, u64)> = totals
        .series
        .iter()
        .map(|(&(device, point), &rows)| (device, point, rows))
        .collect();
    if let Some((device, point, _)) = found.symmetric_difference(&stated).next() {
        let in_file = totals.series.get(&(*device, *point)).copied().unwrap_or(0);
        let in_index = segment
            .series
            .iter()
            .find(|s| s.device == *device && s.point == *point)
            .map_or(0, |s| s.rows);
        return mismatch(
            &format!("readings of {device}/{point}"),
            in_file.to_string(),
            in_index.to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::VALID;

    const LINES: &str = concat!(
        r#"{"stream":"live","batch_seq":1,"received_at":"2026-01-05T00:00:01.000Z","device":"meter-1","point":"energy_kwh","ts":"2026-01-05T00:00:00.000Z","value_type":"float","value":12.5}"#,
        "\n",
        r#"{"stream":"live","batch_seq":2,"received_at":"2026-01-05T00:01:01.000Z","device":"meter-1","point":"energy_kwh","ts":"2026-01-05T00:01:00.000Z","value_type":"float","value":0}"#,
        "\n",
        r#"{"stream":"live","batch_seq":2,"received_at":"2026-01-05T00:01:01.000Z","device":"meter-2","point":"running","ts":"2026-01-05T00:00:30.000Z","value_type":"bool","value":true}"#,
        "\n",
    );

    /// The base index with its segment entry describing `lines` (size and hash).
    fn index_for(lines: &str) -> Index {
        let text = VALID
            .replace("bytes = 10", &format!("bytes = {}", lines.len()))
            .replace(
                "0000000000000000000000000000000000000000000000000000000000000000",
                &sha256_hex(lines.as_bytes()),
            );
        Index::parse(&text).unwrap()
    }

    fn check(lines: &str) -> Result<Vec<Reading>, String> {
        let index = index_for(lines);
        parse(&index, &index.segments[0], lines.as_bytes()).map_err(|error| {
            format!(
                "line {}: {}",
                error
                    .line
                    .map_or_else(|| "-".to_owned(), |line| line.to_string()),
                error.message
            )
        })
    }

    #[test]
    fn the_base_segment_parses() {
        let readings = check(LINES).unwrap();
        assert_eq!(readings.len(), 3);
        assert_eq!(readings[1].value, Value::Float(0.0));
        assert_eq!(readings[2].value, Value::Bool(true));
        assert_eq!(time::format(readings[2].ts), "2026-01-05T00:00:30.000Z");
    }

    #[test]
    fn a_changed_file_fails_its_hash() {
        let index = index_for(LINES);
        let changed = LINES.replace("12.5", "12.6");
        let error = parse(&index, &index.segments[0], changed.as_bytes()).unwrap_err();
        assert!(error.message.contains("SHA-256"), "{}", error.message);
        let longer = format!("{LINES}x");
        let error = parse(&index, &index.segments[0], longer.as_bytes()).unwrap_err();
        assert!(error.message.contains("bytes"), "{}", error.message);
    }

    #[test]
    fn every_rule_refuses_its_violation() {
        let cases = [
            ("12.5}", "12.5,\"extra\":1}", "unknown field"),
            (
                "\"meter-2\",\"point\"",
                "\"meter-3\",\"point\"",
                "not in the index",
            ),
            ("\"running\"", "\"mode\"", "has no point"),
            (
                "\"value_type\":\"bool\"",
                "\"value_type\":\"int\"",
                "declares",
            ),
            ("\"value\":true", "\"value\":1", "the value is 1"),
            ("\"value\":12.5", "\"value\":\"12.5\"", "the value is"),
            (
                "\"ts\":\"2026-01-05T00:00:30.000Z\"",
                "\"ts\":\"2026-01-05T00:00:30Z\"",
                "ts:",
            ),
            (
                "\"ts\":\"2026-01-05T00:00:30.000Z\"",
                "\"ts\":\"2026-01-05T00:00:00.000Z\",\"x\":0",
                "unknown field",
            ),
            (
                "\"stream\":\"live\",\"batch_seq\":1",
                "\"stream\":\"live\",\"batch_seq\":2",
                "arrives where",
            ),
            (
                "\"batch_seq\":2,\"received_at\":\"2026-01-05T00:01:01.000Z\",\"device\":\"meter-2\"",
                "\"batch_seq\":2,\"received_at\":\"2026-01-05T00:01:02.000Z\",\"device\":\"meter-2\"",
                "differs within batch",
            ),
            (
                "\"stream\":\"live\",\"batch_seq\":1",
                "\"stream\":\"other\",\"batch_seq\":1",
                "not among",
            ),
        ];
        for (from, to, expected) in cases {
            assert!(LINES.contains(from), "the base lines have no `{from}`");
            let error = check(&LINES.replacen(from, to, 1))
                .expect_err(&format!("`{from}` -> `{to}` was accepted"));
            assert!(
                error.contains(expected),
                "`{from}` -> `{to}`: `{error}` does not mention `{expected}`"
            );
        }
    }

    #[test]
    fn totals_must_match_the_index() {
        // Received earlier than the previous batch.
        let early = LINES.replace("00:01:01.000Z", "00:00:00.500Z");
        assert!(
            check(&early)
                .unwrap_err()
                .contains("before the batch preceding")
        );
        // The same reading twice.
        let twice = LINES.replace(
            "00:01:00.000Z\",\"value_type\":\"float\"",
            "00:00:00.000Z\",\"value_type\":\"float\"",
        );
        assert!(check(&twice).unwrap_err().contains("a second reading"));
        // A reading time that moves the stated range.
        let later = LINES.replace("00:00:30.000Z", "00:01:30.000Z");
        assert!(check(&later).unwrap_err().contains("reading_time"));
        // A batch missing from the end of the stated range.
        let one_batch: String = LINES.lines().take(1).map(|l| format!("{l}\n")).collect();
        let error = check(&one_batch).unwrap_err();
        assert!(error.contains("rows"), "{error}");
        // No final newline, and an empty line.
        assert!(check(LINES.trim_end()).unwrap_err().contains("newline"));
        assert!(
            check(&LINES.replacen('\n', "\n\n", 1))
                .unwrap_err()
                .contains("empty line")
        );
    }

    #[test]
    fn wrapping_counters_stay_below_wraps_at() {
        let index = index_for(LINES);
        let point = &index.devices[0].points[1];
        assert_eq!(point.wraps_at, Some(65536));
        assert_eq!(
            value(ValueType::Int, serde_json::json!(65535)).unwrap(),
            Value::Int(65535)
        );
        // The range check itself runs per line; a value at wraps_at is refused.
        let lines = LINES.replace(
            r#""point":"energy_kwh","ts":"2026-01-05T00:00:00.000Z","value_type":"float","value":12.5"#,
            r#""point":"pulses","ts":"2026-01-05T00:00:00.000Z","value_type":"int","value":65536"#,
        );
        assert!(check(&lines).unwrap_err().contains("wraps_at"));
    }

    #[test]
    fn values_follow_their_type() {
        assert!(
            value(
                ValueType::Float,
                serde_json::json!(9_007_199_254_740_993_u64)
            )
            .is_err()
        );
        assert_eq!(
            value(ValueType::Float, serde_json::json!(-3)).unwrap(),
            Value::Float(-3.0)
        );
        assert!(value(ValueType::Int, serde_json::json!(1.5)).is_err());
        assert!(value(ValueType::Int, serde_json::json!(u64::MAX)).is_err());
        assert!(value(ValueType::String, serde_json::json!("x".repeat(1025))).is_err());
        assert!(value(ValueType::Bool, serde_json::Value::Null).is_err());
    }
}
