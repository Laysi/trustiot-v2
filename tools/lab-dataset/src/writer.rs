//! Writing a dataset in the index format: segment files (JSON lines) and the index that lists
//! them. Shared by `lab-dataset record` ([`crate::record`]) and `lab-dataset export`
//! ([`crate::export`]), so both write the same format and `Dataset::open` checks both.
//!
//! The caller decides what a batch is (its stream, its sequence number, its receive time and
//! whether it has a completion record); the writer numbers nothing. Batches must arrive in the
//! order the index requires: receive times never decrease, and a stream's sequence numbers are
//! consecutive. Segments are cut between batches when the next one would pass the size limit,
//! and the index is rewritten after every closed segment, so an interrupted run keeps a valid
//! index of the segments it finished.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::index::{self, MAX_SEGMENT_BYTES, PointKind};
use crate::{Error, INDEX_FILE, time};

/// What the index says besides the devices and segments.
pub(crate) struct Header {
    pub name: String,
    pub description: String,
    /// The first comment line of the index, after `# `: who wrote it.
    pub origin: String,
    /// Every stream the batches use, with its description.
    pub streams: Vec<(String, String)>,
    /// The largest segment file, in bytes.
    pub segment_bytes: u64,
}

/// One reading of a batch, as a line of a segment file.
pub(crate) struct Row<'a> {
    pub device: &'a str,
    pub point: &'a str,
    pub ts: DateTime<Utc>,
    pub value_type: &'static str,
    pub value: serde_json::Value,
}

/// One line of a segment file, in the field order of `datasets/lab/README.md`.
#[derive(Serialize)]
struct Line<'a> {
    stream: &'a str,
    batch_seq: u64,
    received_at: &'a str,
    device: &'a str,
    point: &'a str,
    ts: String,
    value_type: &'static str,
    value: &'a serde_json::Value,
}

/// A segment file being written.
struct OpenSegment {
    key: String,
    location: String,
    file: BufWriter<File>,
    path: PathBuf,
    hash: Sha256,
    bytes: u64,
    rows: u64,
    reading_time: Option<(DateTime<Utc>, DateTime<Utc>)>,
    receive_time: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// The first and last batch of each stream in this segment.
    batches: BTreeMap<String, (u64, u64)>,
    uncommitted: Vec<(String, u64)>,
    series: BTreeMap<(String, String), u64>,
}

/// A closed segment's index entry.
struct SegmentEntry {
    key: String,
    location: String,
    bytes: u64,
    sha256: String,
    rows: u64,
    reading_time: (DateTime<Utc>, DateTime<Utc>),
    receive_time: (DateTime<Utc>, DateTime<Utc>),
    batches: BTreeMap<String, (u64, u64)>,
    uncommitted: Vec<(String, u64)>,
    series: BTreeMap<(String, String), u64>,
}

fn widen(
    range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    time: DateTime<Utc>,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    Some(range.map_or((time, time), |(start, end)| {
        (start.min(time), end.max(time))
    }))
}

/// What a finished write holds.
#[derive(Debug)]
pub(crate) struct Finished {
    /// The index file; written only when `segments` is not 0.
    pub index: PathBuf,
    pub segments: usize,
    pub rows: u64,
    pub batches: u64,
    /// The earliest reading time and the latest receive time.
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
}

/// Writes segment files and the index into a dataset directory that already exists.
pub(crate) struct SegmentWriter {
    dir: PathBuf,
    header: Header,
    devices: Vec<index::Device>,
    segments: Vec<SegmentEntry>,
    open: Option<OpenSegment>,
    batches: u64,
}

impl SegmentWriter {
    pub(crate) fn new(dir: &Path, header: Header, devices: Vec<index::Device>) -> Self {
        Self {
            dir: dir.to_path_buf(),
            header,
            devices,
            segments: Vec::new(),
            open: None,
            batches: 0,
        }
    }

    /// Writes one batch: its rows in the given order, all with `received_at`. A batch with no
    /// rows writes nothing and is not counted. `committed` is false for a batch without a
    /// completion record (the index's `uncommitted`).
    pub(crate) fn write_batch(
        &mut self,
        stream: &str,
        batch_seq: u64,
        received_at: DateTime<Utc>,
        rows: &[Row<'_>],
        committed: bool,
    ) -> Result<(), Error> {
        if rows.is_empty() {
            return Ok(());
        }
        let received_text = time::format(received_at);
        let mut text = String::new();
        for row in rows {
            let line = Line {
                stream,
                batch_seq,
                received_at: &received_text,
                device: row.device,
                point: row.point,
                ts: time::format(row.ts),
                value_type: row.value_type,
                value: &row.value,
            };
            let json = serde_json::to_string(&line)
                .map_err(|error| Error::Record(format!("cannot encode a line: {error}")))?;
            text.push_str(&json);
            text.push('\n');
        }
        let length = u64::try_from(text.len()).unwrap_or(u64::MAX);
        if length > MAX_SEGMENT_BYTES {
            return Err(Error::Record(format!(
                "batch {batch_seq} is {length} bytes, more than a segment holds"
            )));
        }
        if self
            .open
            .as_ref()
            .is_some_and(|open| open.bytes + length > self.header.segment_bytes)
        {
            self.close_segment()?;
        }
        if self.open.is_none() {
            self.open_segment()?;
        }
        let open = self
            .open
            .as_mut()
            .ok_or_else(|| Error::Record("internal: segment".to_owned()))?;
        open.file
            .write_all(text.as_bytes())
            .map_err(|error| Error::Io {
                path: open.path.display().to_string(),
                message: error.to_string(),
            })?;
        open.hash.update(text.as_bytes());
        open.bytes += length;
        open.batches
            .entry(stream.to_owned())
            .and_modify(|(_, last)| *last = batch_seq)
            .or_insert((batch_seq, batch_seq));
        if !committed {
            open.uncommitted.push((stream.to_owned(), batch_seq));
        }
        for row in rows {
            open.rows += 1;
            open.reading_time = widen(open.reading_time, row.ts);
            *open
                .series
                .entry((row.device.to_owned(), row.point.to_owned()))
                .or_default() += 1;
        }
        open.receive_time = widen(open.receive_time, received_at);
        self.batches += 1;
        Ok(())
    }

    fn open_segment(&mut self) -> Result<(), Error> {
        let key = format!("part-{:04}", self.segments.len() + 1);
        let location = format!("segments/{key}.jsonl");
        let path = self.dir.join(&location);
        let file = File::create_new(&path).map_err(|error| Error::Io {
            path: path.display().to_string(),
            message: error.to_string(),
        })?;
        self.open = Some(OpenSegment {
            key,
            location,
            file: BufWriter::new(file),
            path,
            hash: Sha256::new(),
            bytes: 0,
            rows: 0,
            reading_time: None,
            receive_time: None,
            batches: BTreeMap::new(),
            uncommitted: Vec::new(),
            series: BTreeMap::new(),
        });
        Ok(())
    }

    /// Closes the open segment and rewrites the index with every closed one, so that a run
    /// that stops abruptly keeps a valid index of what it finished.
    fn close_segment(&mut self) -> Result<(), Error> {
        let Some(open) = self.open.take() else {
            return Ok(());
        };
        let io = |error: std::io::Error| Error::Io {
            path: open.path.display().to_string(),
            message: error.to_string(),
        };
        let file = open
            .file
            .into_inner()
            .map_err(|error| io(error.into_error()))?;
        file.sync_all().map_err(io)?;
        let (Some(reading_time), Some(receive_time)) = (open.reading_time, open.receive_time)
        else {
            return Err(Error::Record("internal: an empty segment".to_owned()));
        };
        self.segments.push(SegmentEntry {
            key: open.key,
            location: open.location,
            bytes: open.bytes,
            sha256: hex(&open.hash.finalize()),
            rows: open.rows,
            reading_time,
            receive_time,
            batches: open.batches,
            uncommitted: open.uncommitted,
            series: open.series,
        });
        self.write_index()
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join(INDEX_FILE)
    }

    fn write_index(&self) -> Result<(), Error> {
        let text = self.index_text();
        let path = self.index_path();
        let temporary = self.dir.join(format!("{INDEX_FILE}.partial"));
        let io = |error: std::io::Error| Error::Io {
            path: path.display().to_string(),
            message: error.to_string(),
        };
        let mut file = File::create(&temporary).map_err(io)?;
        file.write_all(text.as_bytes()).map_err(io)?;
        file.sync_all().map_err(io)?;
        fs::rename(&temporary, &path).map_err(io)
    }

    /// Closes the last segment. With no segment (nothing was written) there is no index either.
    pub(crate) fn finish(mut self) -> Result<Finished, Error> {
        self.close_segment()?;
        Ok(Finished {
            index: self.index_path(),
            segments: self.segments.len(),
            rows: self.segments.iter().map(|s| s.rows).sum(),
            batches: self.batches,
            start: self.segments.iter().map(|s| s.reading_time.0).min(),
            end: self.segments.iter().map(|s| s.receive_time.1).max(),
        })
    }

    fn index_text(&self) -> String {
        let mut text = String::new();
        let mut line = |line: String| {
            text.push_str(&line);
            text.push('\n');
        };
        line(format!("# {}; the format is in", self.header.origin));
        line("# datasets/lab/README.md. Converters appear only by their logical keys.".to_owned());
        line(String::new());
        line(format!("format = {}", index::FORMAT));
        line(format!("name = {}", quote(&self.header.name)));
        line("version = 1".to_owned());
        line(format!("description = {}", quote(&self.header.description)));
        for (key, description) in &self.header.streams {
            line(String::new());
            line("[[streams]]".to_owned());
            line(format!("key = {}", quote(key)));
            line(format!("description = {}", quote(description)));
        }
        for device in &self.devices {
            line(String::new());
            line("[[devices]]".to_owned());
            line(format!("key = {}", quote(&device.key)));
            line(format!("model = {}", quote(&device.model)));
            if let Some(converter) = &device.converter {
                line(format!("converter = {}", quote(converter)));
            }
            line(format!("unit = {}", device.unit));
            line(format!("interval_s = {}", device.interval_s));
            line("points = [".to_owned());
            for point in &device.points {
                let mut fields = vec![
                    format!("key = {}", quote(&point.key)),
                    format!("type = {}", quote(point.value_type.as_str())),
                    format!("kind = {}", quote(point_kind(point.kind))),
                ];
                if let Some(unit) = &point.unit {
                    fields.push(format!("unit = {}", quote(unit)));
                }
                if let Some(wraps_at) = point.wraps_at {
                    fields.push(format!("wraps_at = {wraps_at}"));
                }
                line(format!("  {{ {} }},", fields.join(", ")));
            }
            line("]".to_owned());
        }
        for segment in &self.segments {
            let range = |(start, end): (DateTime<Utc>, DateTime<Utc>)| {
                format!(
                    "{{ start = {}, end = {} }}",
                    quote(&time::format(start)),
                    quote(&time::format(end))
                )
            };
            line(String::new());
            line("[[segments]]".to_owned());
            line(format!("key = {}", quote(&segment.key)));
            line(format!("location = {}", quote(&segment.location)));
            line(format!("bytes = {}", segment.bytes));
            line(format!("sha256 = {}", quote(&segment.sha256)));
            line(format!("rows = {}", segment.rows));
            line(format!("reading_time = {}", range(segment.reading_time)));
            line(format!("receive_time = {}", range(segment.receive_time)));
            line("batches = [".to_owned());
            for (stream, (first, last)) in &segment.batches {
                line(format!(
                    "  {{ stream = {}, first = {first}, last = {last} }},",
                    quote(stream)
                ));
            }
            line("]".to_owned());
            if !segment.uncommitted.is_empty() {
                line("uncommitted = [".to_owned());
                for (stream, batch_seq) in &segment.uncommitted {
                    line(format!(
                        "  {{ stream = {}, batch_seq = {batch_seq} }},",
                        quote(stream)
                    ));
                }
                line("]".to_owned());
            }
            line("series = [".to_owned());
            for ((device, point), rows) in &segment.series {
                line(format!(
                    "  {{ device = {}, point = {}, rows = {rows} }},",
                    quote(device),
                    quote(point)
                ));
            }
            line("]".to_owned());
        }
        text
    }
}

const fn point_kind(kind: PointKind) -> &'static str {
    match kind {
        PointKind::Gauge => "gauge",
        PointKind::Cumulative => "cumulative",
        PointKind::State => "state",
    }
}

/// A one-line TOML basic string: quotation marks, backslashes and control characters escaped.
pub(crate) fn quote(text: &str) -> String {
    use std::fmt::Write as _;
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            '\r' => quoted.push_str("\\r"),
            control if control.is_control() => {
                let _ = write!(quoted, "\\u{:04X}", u32::from(control));
            }
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_escapes() {
        let text = "a \"b\"\n\\ \u{7f}\u{1} \u{3a3}";
        let quoted = quote(text);
        assert_eq!(quoted, "\"a \\\"b\\\"\\n\\\\ \\u007F\\u0001 \u{3a3}\"");
        let parsed: toml::Table = toml::from_str(&format!("x = {quoted}")).unwrap();
        assert_eq!(parsed["x"].as_str(), Some(text));
    }
}
