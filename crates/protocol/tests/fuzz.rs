//! A seeded, stable-Rust fuzz loop over every decode entry point.
//!
//! Corpus: the golden payloads (current and frozen v1). Inputs: byte-level mutations, structure
//! aware mutations of the parsed CBOR tree (edge integers and floats, long strings, replicated
//! array elements, duplicated and renamed keys, tags, deep nesting), a custom CBOR writer that
//! emits non-canonical forms (indefinite lengths, wide arguments, f16 and f32 floats, chunked
//! strings) and random CBOR keyed by the field names the messages really use.
//!
//! Every input must satisfy these invariants for every message type:
//!
//! - decoding never panics;
//! - decoding allocates at most [`ALLOC_FACTOR`] times the payload plus [`ALLOC_SLACK`] bytes;
//! - an accepted payload is within its channel's limit, nests at most `MAX_NESTING` levels deep,
//!   contains no chunked string, and a duplicated map key never changes which message it is;
//! - an accepted message re-encodes (unless it is an `Unknown` placeholder), the re-encoding
//!   decodes to the same message, and encoding that message again gives the same bytes.
//!
//! By default the test runs a few seconds. `FUZZ_ITERS` (per message type) and `FUZZ_SEED`
//! scale and move it: `FUZZ_ITERS=200000 cargo test --release -p protocol --test fuzz`.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeSet;
use std::fmt::Debug;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use cbor4ii::core::Value as Cbor;
use common::{cases, golden_dir, read_hex};
use protocol::acl::{filter_matches, gateway_acl_topic};
use protocol::codec::{MAX_NESTING, SEQ_PREALLOC_BYTES};
use protocol::config::{ConfigReport, DesiredConfig};
use protocol::health::{Heartbeat, Status};
use protocol::live::{LiveControl, LiveSamples};
use protocol::logs::{LogBatch, LogDump, LogRequest};
use protocol::telemetry::{TelemetryAck, TelemetryBatch};
use protocol::topic::{Channel, Direction, parse_topic};
use protocol::{DecodeError, EncodeError, GatewayId, Message, decode, encode};

/// The decoder may allocate this many bytes per payload byte, plus [`ALLOC_SLACK`].
const ALLOC_FACTOR: usize = 200;
const ALLOC_SLACK: usize = 64 * 1024;
/// No single decode of an input within the channel limit takes longer than this.
const SLOW: Duration = Duration::from_secs(2);

struct Counting;

thread_local! {
    static CURRENT: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

#[allow(
    unsafe_code,
    reason = "a global allocator is an unsafe trait; this one only forwards to System"
)]
// SAFETY: every method forwards to `System` with the caller's arguments unchanged, so
// `Counting` upholds the `GlobalAlloc` contract exactly as `System` does; the counters are
// const-initialised thread locals without destructors and never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged; the caller guarantees `layout` has a non-zero size.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let _ = CURRENT.try_with(|current| {
                let now = current.get() + layout.size().cast_signed();
                current.set(now);
                let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
            });
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged; the caller guarantees `pointer` came from `alloc` with
        // this `layout`.
        unsafe { System.dealloc(pointer, layout) };
        let _ =
            CURRENT.try_with(|current| current.set(current.get() - layout.size().cast_signed()));
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The peak number of bytes allocated while `run` runs, above what was allocated before.
fn peak_during<T>(run: impl FnOnce() -> T) -> (T, usize) {
    let base = CURRENT.with(Cell::get);
    PEAK.with(|peak| peak.set(base));
    let result = run();
    let peak = PEAK.with(Cell::get) - base;
    (result, usize::try_from(peak).unwrap_or(0))
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound.max(1)).unwrap()).unwrap()
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

// ---------------------------------------------------------------------------------------------
// Targets

#[derive(Default)]
struct Outcome {
    accepted: bool,
    peak: usize,
    /// A broken invariant.
    violation: Option<String>,
}

fn violation(text: impl Into<String>) -> Outcome {
    Outcome {
        violation: Some(text.into()),
        ..Outcome::default()
    }
}

/// Decodes `payload` as `M` and checks the invariants that need only `M`.
fn run<M: Message + PartialEq + Debug>(payload: &[u8]) -> Outcome {
    let (result, peak) = peak_during(|| decode::<M>(payload));
    let limit = M::CHANNEL.max_payload_bytes();
    let mut outcome = Outcome {
        peak,
        ..Outcome::default()
    };
    if payload.len() > limit && !matches!(result, Err(DecodeError::TooLarge { .. })) {
        return violation(format!(
            "{} bytes (limit {limit}) gave {result:?}",
            payload.len()
        ));
    }
    let message = match result {
        Ok(message) => message,
        Err(_) => return outcome,
    };
    outcome.accepted = true;
    if payload.is_empty() || payload.len() > limit {
        return violation("accepted an empty or oversized payload");
    }
    match structure_facts(payload) {
        Ok(facts) => {
            if facts.depth > MAX_NESTING {
                return violation(format!("accepted nesting of {}", facts.depth));
            }
            if facts.chunked_string {
                return violation("accepted a chunked string");
            }
        }
        Err(error) => return violation(format!("accepted a payload my walker refuses: {error}")),
    }
    if let Some(problem) = duplicate_key_problem::<M>(payload, &message) {
        return violation(problem);
    }
    match encode(&message) {
        Ok(bytes) => match decode::<M>(&bytes) {
            Ok(again) if again == message => match encode(&again) {
                Ok(second) if second == bytes => {}
                other => return violation(format!("encode is not canonical: {other:?}")),
            },
            other => return violation(format!("re-encoding decodes to {other:?}")),
        },
        // The placeholder for content from a newer peer is receive-only.
        Err(EncodeError::Invalid(_)) => {}
        Err(EncodeError::TooLarge { .. }) => {
            // An accepted payload whose canonical form no longer fits its channel (for
            // example half floats that widen). Reported separately by the caller.
            outcome.violation = Some("accepted message does not fit after re-encoding".into());
        }
        Err(error) => return violation(format!("accepted message does not encode: {error}")),
    }
    outcome
}

type Runner = fn(&[u8]) -> Outcome;

fn runner(name: &str) -> Runner {
    match name {
        "telemetry" => run::<TelemetryBatch>,
        n if n.starts_with("telemetry-ack") => run::<TelemetryAck>,
        "config" => run::<DesiredConfig>,
        n if n.starts_with("config-report") => run::<ConfigReport>,
        n if n.starts_with("status") => run::<Status>,
        "heartbeat" => run::<Heartbeat>,
        "live" => run::<LiveSamples>,
        n if n.starts_with("live-control") => run::<LiveControl>,
        "log" => run::<LogBatch>,
        "log-request" => run::<LogRequest>,
        "log-dump" => run::<LogDump>,
        other => panic!("no runner for {other}"),
    }
}

/// `peek_header` must never panic, and must agree with a successful decode.
fn peek_agrees(name: &str, payload: &[u8]) -> Option<String> {
    match name {
        "telemetry" => {
            let peek = TelemetryBatch::peek_header(payload);
            if let Ok(batch) = TelemetryBatch::decode(payload) {
                match peek {
                    Ok(header) if header.stream == batch.stream && header.seq == batch.seq => {}
                    other => return Some(format!("peek {other:?} disagrees with decode")),
                }
            }
        }
        "config" => {
            let peek = DesiredConfig::peek_header(payload);
            if let Ok(config) = DesiredConfig::decode(payload) {
                match peek {
                    Ok(header) if header.version == config.version => {}
                    other => return Some(format!("peek {other:?} disagrees with decode")),
                }
            }
        }
        _ => {}
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Independent structure walker, used to judge accepted payloads

struct Facts {
    depth: usize,
    chunked_string: bool,
}

fn structure_facts(payload: &[u8]) -> Result<Facts, String> {
    fn arg(rest: &mut &[u8], info: u8) -> Result<Option<u64>, String> {
        let width = match info {
            0..=23 => return Ok(Some(u64::from(info))),
            24 => 1,
            25 => 2,
            26 => 4,
            27 => 8,
            31 => return Ok(None),
            _ => return Err("reserved".into()),
        };
        if rest.len() < width {
            return Err("short".into());
        }
        let (head, tail) = rest.split_at(width);
        *rest = tail;
        Ok(Some(
            head.iter()
                .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte)),
        ))
    }
    fn walk(rest: &mut &[u8], depth: usize, facts: &mut Facts) -> Result<(), String> {
        facts.depth = facts.depth.max(depth);
        let Some((&initial, tail)) = rest.split_first() else {
            return Err("short".into());
        };
        *rest = tail;
        let (major, info) = (initial >> 5, initial & 0x1f);
        let argument = arg(rest, info)?;
        match (major, argument) {
            (0 | 1 | 7, Some(_)) => Ok(()),
            (2 | 3, Some(len)) => {
                let len = usize::try_from(len).map_err(|_| "len")?;
                if rest.len() < len {
                    return Err("short".into());
                }
                *rest = &rest[len..];
                Ok(())
            }
            (2 | 3, None) => {
                facts.chunked_string = true;
                Err("chunked".into())
            }
            (4 | 5, Some(count)) => {
                let items = count * if major == 5 { 2 } else { 1 };
                for _ in 0..items {
                    walk(rest, depth + 1, facts)?;
                }
                Ok(())
            }
            (4 | 5, None) => {
                while rest.first() != Some(&0xff) {
                    walk(rest, depth + 1, facts)?;
                }
                *rest = &rest[1..];
                Ok(())
            }
            (6, Some(_)) => walk(rest, depth + 1, facts),
            _ => Err("bad head".into()),
        }
    }
    let mut facts = Facts {
        depth: 0,
        chunked_string: false,
    };
    let mut rest = payload;
    match walk(&mut rest, 0, &mut facts) {
        Ok(()) if rest.is_empty() => Ok(facts),
        Ok(()) => Err("trailing".into()),
        // A chunked string is a finding by itself, whatever else is wrong with the walk.
        Err(_) if facts.chunked_string => Ok(facts),
        Err(error) => Err(error),
    }
}

/// For an accepted payload with a repeated text key in some map: removing either occurrence must
/// not change the decoded message, or the key's value that wins would be ambiguous.
fn duplicate_key_problem<M: Message + PartialEq + Debug>(
    payload: &[u8],
    message: &M,
) -> Option<String> {
    let value: Cbor = cbor4ii::serde::from_slice(payload).ok()?;
    let mut found = Vec::new();
    collect_duplicates(&value, &mut Vec::new(), &mut found);
    for path in found.into_iter().take(4) {
        for keep_first in [true, false] {
            let mut variant = value.clone();
            dedupe_at(&mut variant, &path, keep_first);
            let bytes = encode_value(&variant)?;
            match decode::<M>(&bytes) {
                Ok(other) if &other == message => {}
                other => {
                    return Some(format!(
                        "a repeated key at {path:?} is accepted but keeping the {} occurrence gives {other:?}",
                        if keep_first { "first" } else { "last" }
                    ));
                }
            }
        }
    }
    None
}

/// Canonical CBOR of a tree. (`cbor4ii::serde` cannot serialize tags, the core encoder can.)
fn encode_value(value: &Cbor) -> Option<Vec<u8>> {
    use cbor4ii::core::enc::Encode;
    let mut writer = cbor4ii::core::utils::BufWriter::new(Vec::new());
    value.encode(&mut writer).ok()?;
    Some(writer.into_inner())
}

/// Paths (child indexes) of maps that contain a repeated key. Every map counts: message structs
/// and the data maps (`LogEntry::fields`, `DeviceStats::exception_codes`) all refuse a repeated
/// key since Laysi/trustiot-v2#269 (finding F1 of the verification report), so only a repeated
/// unknown key, which is skipped, may appear in an accepted payload.
fn collect_duplicates(value: &Cbor, path: &mut Vec<usize>, found: &mut Vec<Vec<usize>>) {
    match value {
        Cbor::Map(entries) => {
            let mut keys = BTreeSet::new();
            let mut repeated = false;
            for (key, _) in entries {
                repeated |= !keys.insert(format!("{key:?}"));
            }
            if repeated {
                found.push(path.clone());
            }
            for (index, (_, child)) in entries.iter().enumerate() {
                path.push(index);
                collect_duplicates(child, path, found);
                path.pop();
            }
        }
        Cbor::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                path.push(index);
                collect_duplicates(child, path, found);
                path.pop();
            }
        }
        Cbor::Tag(_, inner) => {
            path.push(0);
            collect_duplicates(inner, path, found);
            path.pop();
        }
        _ => {}
    }
}

fn dedupe_at(value: &mut Cbor, path: &[usize], keep_first: bool) {
    match (path.split_first(), value) {
        (None, Cbor::Map(entries)) => {
            let mut seen = BTreeSet::new();
            if keep_first {
                entries.retain(|(key, _)| seen.insert(format!("{key:?}")));
            } else {
                let mut kept = Vec::new();
                for (key, child) in entries.drain(..).rev() {
                    if seen.insert(format!("{key:?}")) {
                        kept.push((key, child));
                    }
                }
                kept.reverse();
                *entries = kept;
            }
        }
        (Some((&index, rest)), Cbor::Map(entries)) => {
            dedupe_at(&mut entries[index].1, rest, keep_first)
        }
        (Some((&index, rest)), Cbor::Array(items)) => {
            dedupe_at(&mut items[index], rest, keep_first)
        }
        (Some((_, rest)), Cbor::Tag(_, inner)) => dedupe_at(inner, rest, keep_first),
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------------
// Input generation

/// Writes `value` as CBOR in a randomly chosen, still valid, non-canonical form.
fn write_wild(out: &mut Vec<u8>, value: &Cbor, rng: &mut Rng) {
    fn head(out: &mut Vec<u8>, major: u8, argument: u64, rng: &mut Rng) {
        let wide = rng.chance(10);
        let shortest = match argument {
            0..=23 => 0,
            24..=0xff => 1,
            0x100..=0xffff => 2,
            0x1_0000..=0xffff_ffff => 4,
            _ => 8,
        };
        let width = if wide {
            [1usize, 2, 4, 8]
                .into_iter()
                .filter(|width| *width >= shortest.max(1))
                .nth(rng.below(4).min(3))
                .unwrap_or(8)
        } else {
            shortest
        };
        let m = major << 5;
        match width {
            0 => out.push(m | u8::try_from(argument).unwrap()),
            1 => out.extend([m | 24, u8::try_from(argument).unwrap()]),
            2 => {
                out.push(m | 25);
                out.extend(u16::try_from(argument).unwrap().to_be_bytes());
            }
            4 => {
                out.push(m | 26);
                out.extend(u32::try_from(argument).unwrap().to_be_bytes());
            }
            _ => {
                out.push(m | 27);
                out.extend(argument.to_be_bytes());
            }
        }
    }
    match value {
        Cbor::Null => out.push(0xf6),
        Cbor::Bool(flag) => out.push(if *flag { 0xf5 } else { 0xf4 }),
        Cbor::Integer(number) => {
            if *number >= 0 {
                head(out, 0, u64::try_from(*number).unwrap_or(u64::MAX), rng);
            } else {
                head(out, 1, u64::try_from(-1 - *number).unwrap_or(u64::MAX), rng);
            }
        }
        Cbor::Float(number) => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "the narrow form is written only when it converts back to the same bits"
            )]
            let narrow = *number as f32;
            if rng.chance(15) {
                let bits: u16 = *rng.pick(&[
                    0x0000, 0x8000, 0x3c00, 0x7c00, 0xfc00, 0x7e00, 0x7bff, 0x0001, 0x5640, 0xffff,
                ]);
                out.push(0xf9);
                out.extend(bits.to_be_bytes());
            } else if rng.chance(30) && f64::from(narrow).to_bits() == number.to_bits() {
                out.push(0xfa);
                out.extend(narrow.to_be_bytes());
            } else {
                out.push(0xfb);
                out.extend(number.to_be_bytes());
            }
        }
        Cbor::Bytes(bytes) => {
            head(out, 2, bytes.len() as u64, rng);
            out.extend(bytes);
        }
        Cbor::Text(text) => {
            if rng.chance(4) && text.len() >= 2 && text.is_char_boundary(1) {
                // A chunked string.
                out.push(0x7f);
                let (a, b) = text.split_at(1);
                for chunk in [a, b] {
                    head(out, 3, chunk.len() as u64, rng);
                    out.extend(chunk.as_bytes());
                }
                out.push(0xff);
            } else {
                head(out, 3, text.len() as u64, rng);
                out.extend(text.as_bytes());
            }
        }
        Cbor::Array(items) => {
            if rng.chance(15) {
                out.push(0x9f);
                for item in items {
                    write_wild(out, item, rng);
                }
                out.push(0xff);
            } else {
                head(out, 4, items.len() as u64, rng);
                for item in items {
                    write_wild(out, item, rng);
                }
            }
        }
        Cbor::Map(entries) => {
            if rng.chance(15) {
                out.push(0xbf);
                for (key, child) in entries {
                    write_wild(out, key, rng);
                    write_wild(out, child, rng);
                }
                out.push(0xff);
            } else {
                head(out, 5, entries.len() as u64, rng);
                for (key, child) in entries {
                    write_wild(out, key, rng);
                    write_wild(out, child, rng);
                }
            }
        }
        Cbor::Tag(tag, inner) => {
            head(out, 6, *tag, rng);
            write_wild(out, inner, rng);
        }
        _ => out.push(0xf6),
    }
}

fn count_nodes(value: &Cbor) -> usize {
    1 + match value {
        Cbor::Array(items) => items.iter().map(count_nodes).sum(),
        Cbor::Map(entries) => entries
            .iter()
            .map(|(k, v)| count_nodes(k) + count_nodes(v))
            .sum(),
        Cbor::Tag(_, inner) => count_nodes(inner),
        _ => 0,
    }
}

/// The `index`th node in pre-order.
fn nth_mut<'a>(value: &'a mut Cbor, index: &mut usize) -> Option<&'a mut Cbor> {
    if *index == 0 {
        return Some(value);
    }
    *index -= 1;
    match value {
        Cbor::Array(items) => {
            for item in items {
                if let Some(found) = nth_mut(item, index) {
                    return Some(found);
                }
            }
            None
        }
        Cbor::Map(entries) => {
            for (key, child) in entries {
                if let Some(found) = nth_mut(key, index) {
                    return Some(found);
                }
                if let Some(found) = nth_mut(child, index) {
                    return Some(found);
                }
            }
            None
        }
        Cbor::Tag(_, inner) => nth_mut(inner, index),
        _ => None,
    }
}

fn edge_scalar(rng: &mut Rng, names: &[String]) -> Cbor {
    let long = |rng: &mut Rng, fill: &str, near: usize| -> String {
        let count = near + rng.below(3) - 1;
        fill.chars().cycle().take(count).collect()
    };
    match rng.below(14) {
        0 => Cbor::Integer(*rng.pick(&[
            0i128,
            1,
            -1,
            23,
            24,
            255,
            256,
            65_535,
            65_536,
            i128::from(u32::MAX),
            i128::from(u32::MAX) + 1,
            i128::from(i32::MAX),
            i128::from(i32::MIN),
            i128::from(i64::MAX),
            i128::from(i64::MAX) + 1,
            i128::from(i64::MIN),
            i128::from(i64::MIN) - 1,
            i128::from(u64::MAX),
            -i128::from(u64::MAX) - 1,
        ])),
        1 => Cbor::Float(*rng.pick(&[
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -0.0,
            0.0,
            1e308,
            -1e308,
            5e-324,
            f64::from(f32::MAX),
            f64::from(f32::MIN_POSITIVE),
            1.5,
            60.0,
            1e-5,
        ])),
        2 => Cbor::Text(long(rng, "a", 64)),
        3 => Cbor::Text(long(rng, "x", 1024)),
        4 => Cbor::Text(long(rng, "é", 1024)),
        5 => Cbor::Text(
            (*rng.pick(&[
                "",
                "+",
                "#",
                "a/b",
                "$SYS",
                "a\0b",
                "é",
                "電壓",
                "０",
                "a b",
                "-a",
                ".a",
                "_a",
                "..",
                "a.b_c-d",
                "trustiot/v1/gw/x",
                "%u",
                "Ａ",
                "a\u{200b}b",
                "a\u{202e}b",
            ]))
            .to_owned(),
        ),
        6 => Cbor::Bool(rng.chance(50)),
        7 => Cbor::Null,
        8 => Cbor::Bytes(
            (0..rng.below(5))
                .map(|_| u8::try_from(rng.below(256)).unwrap())
                .collect(),
        ),
        9 => Cbor::Text(long(rng, "b", 256)),
        10 => Cbor::Text(long(rng, "c", 40 + 1)),
        11 | 12 => Cbor::Text(rng.pick(names).clone()),
        _ => Cbor::Integer(i128::from(rng.next() >> rng.below(64))),
    }
}

fn mutate_tree(value: &mut Cbor, rng: &mut Rng, names: &[String], grafts: &[Cbor], limit: usize) {
    let total = count_nodes(value);
    let mut index = rng.below(total);
    let Some(node) = nth_mut(value, &mut index) else {
        return;
    };
    match rng.below(14) {
        0..=2 => *node = edge_scalar(rng, names),
        3 => {
            // Replicate one array element, possibly past the limit.
            if let Cbor::Array(items) = node
                && !items.is_empty()
            {
                let picked = items[rng.below(items.len())].clone();
                let size = encode_value(&picked).map_or(1, |bytes| bytes.len());
                let want = rng.below(limit / size.max(1) + 3).max(2);
                let want = want.min(limit / size.max(1) + 3);
                items.extend(std::iter::repeat_n(picked, want));
            }
        }
        4 => match node {
            Cbor::Array(items) if !items.is_empty() => {
                items.remove(rng.below(items.len()));
            }
            Cbor::Map(entries) if !entries.is_empty() => {
                entries.remove(rng.below(entries.len()));
            }
            _ => {}
        },
        5 => {
            // Duplicate a key, with the same or another value.
            if let Cbor::Map(entries) = node
                && !entries.is_empty()
            {
                let (key, child) = entries[rng.below(entries.len())].clone();
                let other = if rng.chance(50) {
                    child
                } else {
                    edge_scalar(rng, names)
                };
                let at = rng.below(entries.len() + 1);
                entries.insert(at, (key, other));
            }
        }
        6 => {
            // Rename a key to another known field name, or add one.
            if let Cbor::Map(entries) = node {
                let name = Cbor::Text(rng.pick(names).clone());
                if !entries.is_empty() && rng.chance(50) {
                    let at = rng.below(entries.len());
                    entries[at].0 = name;
                } else {
                    entries.push((name, edge_scalar(rng, names)));
                }
            }
        }
        7 => {
            let tag = *rng.pick(&[0u64, 1, 2, 3, 4, 5, 24, 32, 36, 55799, u64::MAX, 258]);
            let inner = std::mem::replace(node, Cbor::Null);
            *node = Cbor::Tag(tag, Box::new(inner));
        }
        8 => {
            // Nest the node, possibly beyond the limit, as arrays, maps or tags.
            let depth = 1 + rng.below(MAX_NESTING + 8);
            let mut inner = std::mem::replace(node, Cbor::Null);
            let kind = rng.below(3);
            for _ in 0..depth {
                inner = match kind {
                    0 => Cbor::Array(vec![inner]),
                    1 => Cbor::Map(vec![(Cbor::Text("future".into()), inner)]),
                    _ => Cbor::Tag(55799, Box::new(inner)),
                };
            }
            *node = inner;
        }
        9 => {
            if !grafts.is_empty() {
                *node = rng.pick(grafts).clone();
            }
        }
        10 => {
            // Change the node's type.
            *node = match node {
                Cbor::Array(items) => Cbor::Map(
                    items
                        .drain(..)
                        .map(|item| (Cbor::Integer(0), item))
                        .collect(),
                ),
                Cbor::Map(entries) => {
                    Cbor::Array(entries.drain(..).map(|(_, child)| child).collect())
                }
                Cbor::Text(text) => Cbor::Bytes(text.clone().into_bytes()),
                Cbor::Integer(_) | Cbor::Float(_) => Cbor::Text("1".into()),
                _ => Cbor::Array(Vec::new()),
            };
        }
        11 => {
            // A map or array grows with many unknown entries.
            if let Cbor::Map(entries) = node {
                for n in 0..rng.below(200) {
                    entries.push((Cbor::Text(format!("k{n}")), edge_scalar(rng, names)));
                }
            }
        }
        12 => {
            // Swap with another subtree's scalar.
            *node = edge_scalar(rng, names);
        }
        _ => {
            // Truncate a long list or map.
            match node {
                Cbor::Array(items) => items.truncate(rng.below(items.len() + 1)),
                Cbor::Map(entries) => entries.truncate(rng.below(entries.len() + 1)),
                _ => {}
            }
        }
    }
}

fn random_cbor(rng: &mut Rng, names: &[String], depth: usize) -> Cbor {
    match rng.below(if depth > 5 { 6 } else { 9 }) {
        0..=3 => edge_scalar(rng, names),
        4 => Cbor::Integer(i128::try_from(rng.below(300)).unwrap()),
        5 => Cbor::Text(rng.pick(names).clone()),
        6 => Cbor::Array(
            (0..rng.below(5))
                .map(|_| random_cbor(rng, names, depth + 1))
                .collect(),
        ),
        7 => Cbor::Map(
            (0..rng.below(6))
                .map(|_| {
                    (
                        Cbor::Text(rng.pick(names).clone()),
                        random_cbor(rng, names, depth + 1),
                    )
                })
                .collect(),
        ),
        _ => Cbor::Tag(
            u64::try_from(rng.below(40)).unwrap(),
            Box::new(random_cbor(rng, names, depth + 1)),
        ),
    }
}

fn mutate_bytes(bytes: &mut Vec<u8>, other: &[u8], rng: &mut Rng) {
    const HEADS: &[&[u8]] = &[
        &[0x1b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        &[0x3b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        &[0x9b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        &[0xbb, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        &[0x7b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        &[0x5f, 0x41, 0x00, 0xff],
        &[0x7f, 0x61, b'a', 0xff],
        &[0x9f],
        &[0xbf],
        &[0xff],
        &[0xf9, 0x7e, 0x00],
        &[0xf9, 0x7c, 0x00],
        &[0xfa, 0x7f, 0xc0, 0, 0],
        &[0xfb, 0x7f, 0xf8, 0, 0, 0, 0, 0, 0],
        &[0xc2, 0x41, 0x01],
        &[0xd9, 0xd9, 0xf7],
        &[0xc0],
        &[0xf8, 0xff],
        &[0x1c],
        &[0x3c],
        &[0xfc],
        &[0x1f],
        &[0xdf],
    ];
    if bytes.is_empty() {
        return;
    }
    match rng.below(10) {
        0 => {
            let at = rng.below(bytes.len());
            bytes[at] ^= 1 << rng.below(8);
        }
        1 => {
            let at = rng.below(bytes.len());
            bytes[at] = u8::try_from(rng.below(256)).unwrap();
        }
        2 => {
            let at = rng.below(bytes.len() + 1);
            bytes.insert(at, u8::try_from(rng.below(256)).unwrap());
        }
        3 => {
            let at = rng.below(bytes.len());
            let len = 1 + rng.below(8.min(bytes.len() - at));
            bytes.drain(at..at + len);
        }
        4 => {
            let at = rng.below(bytes.len());
            let len = 1 + rng.below(16.min(bytes.len() - at));
            let chunk = bytes[at..at + len].to_vec();
            let to = rng.below(bytes.len() + 1);
            bytes.splice(to..to, chunk);
        }
        5 => bytes.truncate(rng.below(bytes.len())),
        6 => {
            let head = rng.pick(HEADS);
            let at = rng.below(bytes.len());
            let end = (at + head.len()).min(bytes.len());
            bytes.splice(at..end, head.iter().copied());
        }
        7 => {
            if !other.is_empty() {
                let from = rng.below(other.len());
                let len = 1 + rng.below(32.min(other.len() - from));
                let at = rng.below(bytes.len());
                bytes.splice(at..at, other[from..from + len].iter().copied());
            }
        }
        8 => bytes.push(u8::try_from(rng.below(256)).unwrap()),
        _ => {
            let head = rng.pick(HEADS);
            let at = rng.below(bytes.len() + 1);
            bytes.splice(at..at, head.iter().copied());
        }
    }
}

struct Corpus {
    name: String,
    payloads: Vec<Vec<u8>>,
}

fn load_corpus() -> Vec<Corpus> {
    cases()
        .iter()
        .map(|case| {
            let mut payloads = vec![case.encode()];
            let frozen = golden_dir("v1-frozen").join(format!("{}.cbor.hex", case.name()));
            if frozen.exists() {
                payloads.push(read_hex(&frozen));
            }
            Corpus {
                name: case.name().to_owned(),
                payloads,
            }
        })
        .collect()
}

fn field_names(corpus: &[Corpus]) -> Vec<String> {
    let mut names = BTreeSet::new();
    fn walk(value: &Cbor, names: &mut BTreeSet<String>) {
        match value {
            Cbor::Text(text) if text.len() <= 40 => {
                names.insert(text.clone());
            }
            Cbor::Array(items) => items.iter().for_each(|item| walk(item, names)),
            Cbor::Map(entries) => entries.iter().for_each(|(k, v)| {
                walk(k, names);
                walk(v, names);
            }),
            Cbor::Tag(_, inner) => walk(inner, names),
            _ => {}
        }
    }
    for entry in corpus {
        for payload in &entry.payloads {
            walk(&cbor4ii::serde::from_slice(payload).unwrap(), &mut names);
        }
    }
    for extra in [
        "v", "rev", "seq", "stream", "series", "samples", "status", "state", "op", "outcome",
        "future",
    ] {
        names.insert(extra.to_owned());
    }
    names.into_iter().collect()
}

/// One hostile input for `entry`.
fn generate(
    entry: &Corpus,
    corpus: &[Corpus],
    names: &[String],
    grafts: &[Cbor],
    rng: &mut Rng,
) -> Vec<u8> {
    let limit = protocol_limit(&entry.name);
    let seed = rng.pick(&entry.payloads).clone();
    let donor = rng.pick(corpus);
    let other = rng.pick(&donor.payloads).clone();
    let mut bytes = match rng.below(20) {
        0 => {
            // Pure random bytes.
            (0..rng.below(64))
                .map(|_| u8::try_from(rng.below(256)).unwrap())
                .collect()
        }
        1 | 2 => {
            let mut out = Vec::new();
            write_wild(&mut out, &random_cbor(rng, names, 0), rng);
            out
        }
        3..=13 => {
            let mut tree: Cbor = cbor4ii::serde::from_slice(&seed).unwrap();
            for _ in 0..=rng.below(3) {
                mutate_tree(&mut tree, rng, names, grafts, limit);
            }
            let mut out = Vec::new();
            if rng.chance(70) {
                write_wild(&mut out, &tree, rng);
            } else {
                out = encode_value(&tree).unwrap_or_default();
            }
            out
        }
        _ => seed,
    };
    for _ in 0..rng.below(3) {
        if rng.chance(40) {
            mutate_bytes(&mut bytes, &other, rng);
        }
    }
    bytes
}

fn protocol_limit(name: &str) -> usize {
    let channel = match name {
        "telemetry" => Channel::Telemetry,
        n if n.starts_with("telemetry-ack") => Channel::TelemetryAck,
        "config" => Channel::Config,
        n if n.starts_with("config-report") => Channel::ConfigReport,
        n if n.starts_with("status") => Channel::Status,
        "heartbeat" => Channel::Heartbeat,
        "live" => Channel::Live,
        n if n.starts_with("live-control") => Channel::LiveControl,
        "log" => Channel::Log,
        "log-request" => Channel::LogRequest,
        _ => Channel::LogDump,
    };
    channel.max_payload_bytes()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(default)
}

#[derive(Default)]
struct Totals {
    runs: u64,
    accepted: u64,
    max_peak_ratio: f64,
    max_peak: usize,
    slowest: Duration,
    slowest_input: String,
    worst_ratio_input: String,
    notes: BTreeSet<String>,
    widened: u64,
}

#[test]
fn decoders_survive_hostile_payloads() {
    let iterations = env_number("FUZZ_ITERS", 150);
    let seed = env_number("FUZZ_SEED", 0x5eed_f00d_dead_beef);
    let corpus = load_corpus();
    let names = field_names(&corpus);
    let grafts: Vec<Cbor> = corpus
        .iter()
        .flat_map(|entry| entry.payloads.iter())
        .flat_map(|payload| {
            let tree: Cbor = cbor4ii::serde::from_slice(payload).unwrap();
            let mut subtrees = Vec::new();
            fn collect(value: &Cbor, out: &mut Vec<Cbor>) {
                out.push(value.clone());
                match value {
                    Cbor::Array(items) => items.iter().for_each(|item| collect(item, out)),
                    Cbor::Map(entries) => entries.iter().for_each(|(_, v)| collect(v, out)),
                    _ => {}
                }
            }
            collect(&tree, &mut subtrees);
            subtrees
        })
        .filter(|tree| count_nodes(tree) < 64)
        .collect();

    let mut failures: Vec<String> = Vec::new();
    let mut totals = Totals::default();
    for (index, entry) in corpus.iter().enumerate() {
        let run_one = runner(&entry.name);
        let mut rng =
            Rng(seed ^ (u64::try_from(index).unwrap() + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        for _ in 0..iterations {
            let payload = generate(entry, &corpus, &names, &grafts, &mut rng);
            let started = Instant::now();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let outcome = run_one(&payload);
                let peek = peek_agrees(&entry.name, &payload);
                (outcome, peek)
            }));
            let elapsed = started.elapsed();
            totals.runs += 1;
            if elapsed > totals.slowest {
                totals.slowest = elapsed;
                totals.slowest_input =
                    format!("{} {} bytes {}", entry.name, payload.len(), hex(&payload));
            }
            let problem = match result {
                Err(panic) => Some(format!(
                    "PANIC {}",
                    panic
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
                        .unwrap_or_default()
                )),
                Ok((outcome, peek)) => {
                    totals.accepted += u64::from(outcome.accepted);
                    let ratio = outcome.peak as f64 / payload.len().max(1) as f64;
                    if ratio > totals.max_peak_ratio && payload.len() > 64 {
                        totals.max_peak_ratio = ratio;
                        totals.worst_ratio_input =
                            format!("{} {} bytes {}", entry.name, payload.len(), hex(&payload));
                    }
                    totals.max_peak = totals.max_peak.max(outcome.peak);
                    if outcome.peak > ALLOC_FACTOR * payload.len() + ALLOC_SLACK {
                        Some(format!(
                            "allocated {} bytes for a {}-byte payload",
                            outcome.peak,
                            payload.len()
                        ))
                    } else if elapsed > SLOW && payload.len() <= protocol_limit(&entry.name) {
                        Some(format!("took {elapsed:?}"))
                    } else if let Some(text) = outcome.violation {
                        // The re-encoding of an accepted message may exceed the limit only for
                        // widened floats; keep it as a note.
                        if text.starts_with("accepted message does not fit") {
                            totals.widened += 1;
                            if totals.notes.len() < 3 {
                                totals.notes.insert(format!(
                                    "{}: {text}: {} bytes {}",
                                    entry.name,
                                    payload.len(),
                                    hex(&payload).chars().take(300).collect::<String>()
                                ));
                            }
                            None
                        } else {
                            Some(text)
                        }
                    } else {
                        peek
                    }
                }
            };
            if let Some(problem) = problem {
                failures.push(format!("{} {problem}: {}", entry.name, hex(&payload)));
                if failures.len() >= 20 {
                    break;
                }
            }
        }
    }
    println!(
        "fuzz: {} inputs, {} accepted, peak allocation {} bytes (max {:.1}x payload), slowest {:?}, seed {seed:#x}",
        totals.runs, totals.accepted, totals.max_peak, totals.max_peak_ratio, totals.slowest
    );
    let clip = |text: &str| text.chars().take(600).collect::<String>();
    println!("slowest input: {}", clip(&totals.slowest_input));
    println!(
        "worst allocation input: {}",
        clip(&totals.worst_ratio_input)
    );
    println!(
        "accepted messages that no longer fit after re-encoding: {}",
        totals.widened
    );
    for note in &totals.notes {
        println!("note: {note}");
    }
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        totals.accepted > 0,
        "the mutator never produces a valid message"
    );
}

// ---------------------------------------------------------------------------------------------
// Topics and the ACL

fn topic_alphabet() -> Vec<&'static str> {
    vec![
        "trustiot",
        "v1",
        "v2",
        "gw",
        "up",
        "down",
        "telemetry",
        "telemetry-ack",
        "config",
        "status",
        "heartbeat",
        "live",
        "live-control",
        "log",
        "log-request",
        "log-dump",
        "config-report",
        "gw-1",
        "GW-1",
        "gw-2",
        "a",
        "+",
        "#",
        "$SYS",
        "$share",
        "",
        "%u",
        "gw/1",
        "gw\0",
        "gw\n",
        "gw ",
        " gw",
        "gw\u{ff0f}1",
        "gw\u{2215}1",
        "ｇｗ",
        "gw１",
        "gw\u{0430}",
        "gw\u{200b}",
        "é",
        "\u{202e}gw",
        "A",
        "a.b",
        "a_b",
        "a-b",
        ".",
        "..",
        "-",
        "_",
        "0",
    ]
}

#[test]
fn topics_parse_only_when_canonical_and_acl_topics_never_widen() {
    let iterations = env_number("FUZZ_TOPIC_ITERS", 20_000);
    let mut rng = Rng(env_number("FUZZ_SEED", 7) ^ 0x70b1c);
    let alphabet = topic_alphabet();
    let mut accepted = 0u64;
    for _ in 0..iterations {
        // Mostly near-valid topics: the canonical shape with one or two levels swapped for
        // hostile ones, sometimes with extra, missing or empty levels.
        let mut levels: Vec<String> = vec![
            "trustiot".into(),
            "v1".into(),
            "gw".into(),
            "gw-1".into(),
            "up".into(),
            "telemetry".into(),
        ];
        for _ in 0..rng.below(3) {
            let at = rng.below(levels.len());
            levels[at] = (*rng.pick(&alphabet)).to_owned();
        }
        match rng.below(8) {
            0 => levels.push((*rng.pick(&alphabet)).to_owned()),
            1 => {
                levels.pop();
            }
            2 => levels.insert(rng.below(levels.len() + 1), String::new()),
            _ => {}
        }
        let mut topic = levels.join("/");
        match rng.below(10) {
            0 => topic.push('/'),
            1 => topic.insert(0, '/'),
            2 => topic = topic.to_uppercase(),
            3 => topic.push_str(&"x".repeat(rng.below(100))),
            _ => {}
        }
        let parsed = catch_unwind(AssertUnwindSafe(|| parse_topic(&topic)));
        let Ok(parsed) = parsed else {
            panic!("parse_topic panicked on {topic:?}");
        };
        let Ok(parsed) = parsed else { continue };
        accepted += 1;
        // Accepted means exactly canonical: the topic is what `Channel::topic` builds.
        assert_eq!(parsed.channel.topic(&parsed.gateway), topic, "{topic:?}");
        assert!(
            !topic.contains(['+', '#', '\0', '$', '%']) && topic.is_ascii(),
            "{topic:?}"
        );
        // The gateway's ACL covers exactly its own tree of this direction.
        let acl =
            gateway_acl_topic(parsed.channel.direction()).replace("%u", parsed.gateway.as_str());
        assert!(filter_matches(&acl, &topic), "{acl} must match {topic}");
        assert_eq!(acl.matches('#').count(), 1);
        assert!(!acl.contains('+'));
        let other_direction = match parsed.channel.direction() {
            Direction::Up => Direction::Down,
            Direction::Down => Direction::Up,
        };
        let other_acl = gateway_acl_topic(other_direction).replace("%u", parsed.gateway.as_str());
        assert!(!filter_matches(&other_acl, &topic));
    }
    assert!(
        accepted > 50,
        "the topic generator reached only {accepted} valid topics"
    );
}

#[test]
fn a_gateways_acl_never_matches_another_gateways_topics() {
    let iterations = env_number("FUZZ_TOPIC_ITERS", 20_000);
    let mut rng = Rng(0xac1);
    let valid_chars: Vec<char> = "abAB019-_.".chars().collect();
    let any_chars: Vec<char> = "abAB019-_.+#/$ é電０\0\n%\u{200b}\u{ff0f}"
        .chars()
        .collect();
    let mut equal = 0u64;
    let mut different = 0u64;
    for _ in 0..iterations {
        let make = |rng: &mut Rng| -> String {
            let len = 1 + rng.below(4);
            let alphabet = if rng.chance(80) {
                &valid_chars
            } else {
                &any_chars
            };
            (0..len).map(|_| *rng.pick(alphabet)).collect()
        };
        let first = make(&mut rng);
        let second = if rng.chance(30) {
            first.clone()
        } else {
            make(&mut rng)
        };
        let (Ok(a), Ok(b)) = (GatewayId::new(first), GatewayId::new(second)) else {
            continue;
        };
        if a == b {
            equal += 1;
        } else {
            different += 1;
        }
        for direction in [Direction::Up, Direction::Down] {
            let acl = gateway_acl_topic(direction).replace("%u", a.as_str());
            for channel in Channel::ALL {
                let topic = channel.topic(&b);
                let expected = a == b && channel.direction() == direction;
                assert_eq!(filter_matches(&acl, &topic), expected, "{acl} vs {topic}");
            }
        }
    }
    assert!(
        equal > 100 && different > 100,
        "{equal} equal, {different} different"
    );
}

// ---------------------------------------------------------------------------------------------
// Allocation bound

/// `{"v": 1, <prefix>, <field>: [item; count]}` with the array length declared as `count`.
fn array_payload(prefix: &[(&str, Vec<u8>)], field: &str, count: usize, item: u8) -> Vec<u8> {
    let key = |name: &str| {
        let mut bytes = vec![0x60 | u8::try_from(name.len()).unwrap()];
        bytes.extend(name.as_bytes());
        bytes
    };
    let mut bytes = vec![0xa0 | u8::try_from(2 + prefix.len()).unwrap()];
    bytes.extend(key("v"));
    bytes.push(1);
    for (name, value) in prefix {
        bytes.extend(key(name));
        bytes.extend(value);
    }
    bytes.extend(key(field));
    bytes.push(0x9a);
    bytes.extend(u32::try_from(count).unwrap().to_be_bytes());
    bytes.extend(std::iter::repeat_n(item, count));
    bytes
}

/// A payload that declares `count` array items (each a one-byte `true`) used to make serde
/// reserve room for all of them before the first item failed to decode: up to 1 MiB per
/// collection, ~180 times the payload for a few kilobytes (finding F2 of the verification
/// report). Arrays now reserve at most `SEQ_PREALLOC_BYTES` before their items decode
/// (Laysi/trustiot-v2#269), so the peak no longer depends on the declared count. This test pins
/// the bound so that it cannot grow unnoticed.
#[test]
fn a_hostile_array_count_reserves_at_most_a_bounded_amount() {
    const BOUND: usize = 2 * SEQ_PREALLOC_BYTES;
    let key = |name: &str| {
        let mut bytes = vec![0x60 | u8::try_from(name.len()).unwrap()];
        bytes.extend(name.as_bytes());
        bytes
    };
    let mut worst = 0;
    for count in [1_000usize, 4_200, 20_000, 50_000, 60_000] {
        let payloads: Vec<(&str, Vec<u8>, Runner)> = vec![
            (
                "heartbeat",
                array_payload(&[], "devices", count, 0xf5),
                run::<Heartbeat>,
            ),
            (
                "telemetry",
                array_payload(
                    &[
                        ("stream", key("s")),
                        ("seq", vec![1]),
                        ("clock_synced", vec![0xf5]),
                    ],
                    "series",
                    count,
                    0xf5,
                ),
                run::<TelemetryBatch>,
            ),
            (
                "live",
                array_payload(&[], "series", count, 0xf5),
                run::<LiveSamples>,
            ),
            (
                "log",
                array_payload(&[], "entries", count, 0xf5),
                run::<LogBatch>,
            ),
            (
                "config",
                array_payload(
                    &[("rev", vec![1]), ("version", vec![1])],
                    "models",
                    count,
                    0xf5,
                ),
                run::<DesiredConfig>,
            ),
        ];
        for (name, payload, run_one) in payloads {
            let limit = protocol_limit(name);
            if payload.len() > limit {
                continue;
            }
            let outcome = run_one(&payload);
            assert!(!outcome.accepted, "{name} x{count}");
            assert!(
                outcome.peak <= BOUND,
                "{name} x{count}: {} bytes",
                outcome.peak
            );
            worst = worst.max(outcome.peak);
        }
    }
    // The probe still reaches the reservation (the bound is not met by failing earlier).
    assert!(
        worst > SEQ_PREALLOC_BYTES / 2,
        "the probe no longer reserves anything: {worst}"
    );
}
