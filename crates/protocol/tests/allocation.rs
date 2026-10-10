//! Hostile payloads that declare huge collections must fail without the decoder allocating
//! memory for them: a gateway (or a cloud replica) receiving one must not run out of memory.
//! This binary installs a counting allocator, so it holds exactly one test and nothing else
//! allocates while it measures.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use cbor4ii::core::Value as Cbor;
use protocol::health::Status;
use protocol::telemetry::TelemetryBatch;
use protocol::{DecodeError, encode};

/// The system allocator, counting the bytes currently allocated and the peak since reset.
struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

#[allow(
    unsafe_code,
    reason = "a global allocator is an unsafe trait; this one only forwards to System"
)]
// SAFETY: every method forwards to `System` with the caller's arguments unchanged, so
// `Counting` upholds the `GlobalAlloc` contract exactly as `System` does; the counters are
// plain atomics and never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged; the caller guarantees `layout` has a non-zero size.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(now, Ordering::SeqCst);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged; the caller guarantees `pointer` came from `alloc` with
        // this `layout`.
        unsafe { System.dealloc(pointer, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::SeqCst);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The peak number of bytes allocated while `run` runs, above what was allocated before.
fn peak_during(run: impl FnOnce()) -> usize {
    let base = CURRENT.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    run();
    PEAK.load(Ordering::SeqCst) - base
}

fn text(value: &str) -> Cbor {
    Cbor::Text(value.to_owned())
}

/// `payload` with the only encoding of the text `marker` replaced by `raw`.
fn splice(payload: &[u8], marker: &str, raw: &[u8]) -> Vec<u8> {
    let needle = cbor4ii::serde::to_vec(Vec::new(), &text(marker)).unwrap();
    let at = payload
        .windows(needle.len())
        .position(|window| window == needle)
        .unwrap();
    [&payload[..at], raw, &payload[at + needle.len()..]].concat()
}

#[test]
fn declared_lengths_never_drive_allocation() {
    // Telemetry whose `series` array claims u32::MAX entries and then ends.
    let batch = encode(&common::telemetry_batch()).unwrap();
    let mut value: Cbor = cbor4ii::serde::from_slice(&batch).unwrap();
    let Cbor::Map(entries) = &mut value else {
        panic!("a batch is a map");
    };
    for (key, field) in entries.iter_mut() {
        if *key == text("series") {
            *field = text("SERIES");
        }
    }
    let template = cbor4ii::serde::to_vec(Vec::new(), &value).unwrap();
    let mut hostile = vec![
        splice(&template, "SERIES", &[0x9a, 0xff, 0xff, 0xff, 0xff]),
        // Every level of the series claims more than it holds.
        splice(
            &template,
            "SERIES",
            &[0x9a, 0xff, 0xff, 0xff, 0xff, 0xba, 0xff, 0xff, 0xff, 0xff],
        ),
    ];
    // Nested arrays, each claiming u32::MAX items, as deep as the nesting limit allows.
    let mut nested = Vec::new();
    for _ in 0..protocol::codec::MAX_NESTING - 2 {
        nested.extend_from_slice(&[0x9a, 0xff, 0xff, 0xff, 0xff]);
    }
    hostile.push(splice(&template, "SERIES", &nested));

    for payload in &hostile {
        let mut result = None;
        let peak = peak_during(|| result = Some(TelemetryBatch::decode(payload)));
        assert!(
            matches!(result, Some(Err(DecodeError::Malformed(_)))),
            "{result:?}"
        );
        assert!(peak < 4 * 1024, "decoding allocated {peak} bytes");
    }

    // The status is an internally tagged enum, which buffers fields before decoding them.
    let status = encode(&common::status_online()).unwrap();
    let mut value: Cbor = cbor4ii::serde::from_slice(&status).unwrap();
    let Cbor::Map(entries) = &mut value else {
        panic!("a status is a map");
    };
    entries.push((text("future"), text("FUTURE")));
    let template = cbor4ii::serde::to_vec(Vec::new(), &value).unwrap();
    let payload = splice(&template, "FUTURE", &[0x9a, 0xff, 0xff, 0xff, 0xff]);
    let mut result = None;
    let peak = peak_during(|| result = Some(Status::decode(&payload)));
    assert!(
        matches!(result, Some(Err(DecodeError::Malformed(_)))),
        "{result:?}"
    );
    assert!(peak < 4 * 1024, "decoding allocated {peak} bytes");
}
