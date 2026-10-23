//! The cap on connections per source (README.md, "連線上限"): without it, a few sources could take
//! every one of `max_connections` and keep the real gateways out (security review of
//! Laysi/trustiot-v2#85, Medium; Laysi/trustiot-v2#600).
//!
//! The source is the trusted client IP ([`crate::client_ip`]), so forged forwarding headers do
//! not change which source a connection counts against. A connection counts against every
//! [`Bucket`] its address falls in, and is refused when any of them is full:
//!
//! - An IPv4 address is one bucket of `per_source`. So is an IPv4-mapped IPv6 address and a 6to4
//!   address (`2002::/16`), which carries an IPv4 address in its bits 16 to 47: whoever holds that
//!   IPv4 address holds the whole 6to4 /48.
//! - Any other IPv6 address counts against its /64 (`per_source`, the block one subscriber
//!   usually gets: a client with a /64 can use any of its 2^64 addresses), its /56 (twice that)
//!   and its /48 (four times). Residential subscribers get a /56 or a /48 and a free tunnel
//!   broker hands out a /48, so a /64 cap alone would let one subscriber open thousands of
//!   connections. Twice and four times leave room for a site whose gateways sit in a few /64s of
//!   its prefix, while one /48 can take at most `4 * per_source` slots.
//!
//! [`RefusalLog`] keeps the WARN lines of refused upgrades to one per bucket per minute.

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How much larger than `per_source` the cap of each wider IPv6 prefix is.
const V6_PREFIXES: [(u8, usize); 3] = [(64, 1), (56, 2), (48, 4)];

/// One group of addresses whose connections are counted together.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Bucket {
    /// One IPv4 address (also reached through an IPv4-mapped or a 6to4 address).
    V4(Ipv4Addr),
    /// An IPv6 prefix: its first 64 bits with the bits after `length` cleared.
    V6 { network: u64, length: u8 },
}

impl Bucket {
    /// The buckets a client IP counts against, each with its cap as a multiple of `per_source`,
    /// narrowest first.
    pub fn of(ip: IpAddr) -> Vec<(Self, usize)> {
        match ip.to_canonical() {
            IpAddr::V4(address) => vec![(Self::V4(address), 1)],
            IpAddr::V6(address) => {
                let bits = address.to_bits();
                if bits >> 112 == 0x2002 {
                    // 6to4: 2002:AABB:CCDD::/48 belongs to the holder of AA.BB.CC.DD.
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "the value is shifted right by 80 bits and masked to 32 bits"
                    )]
                    let embedded = ((bits >> 80) & 0xffff_ffff) as u32;
                    return vec![(Self::V4(Ipv4Addr::from_bits(embedded)), 1)];
                }
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "the value is shifted right by 64 bits first, so it fits in u64"
                )]
                let upper = (bits >> 64) as u64;
                V6_PREFIXES
                    .iter()
                    .map(|&(length, factor)| {
                        let mask = u64::MAX << (64 - u32::from(length));
                        (
                            Self::V6 {
                                network: upper & mask,
                                length,
                            },
                            factor,
                        )
                    })
                    .collect()
            }
        }
    }
}

impl fmt::Display for Bucket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::V4(address) => write!(f, "{address}"),
            Self::V6 { network, length } => {
                let address = Ipv6Addr::from_bits(u128::from(*network) << 64);
                write!(f, "{address}/{length}")
            }
        }
    }
}

type Counts = Arc<Mutex<HashMap<Bucket, usize>>>;

/// Counts open connections per [`Bucket`] and refuses one that would put a bucket over its cap.
#[derive(Clone, Debug)]
pub struct SourceLimit {
    per_source: usize,
    counts: Counts,
}

impl SourceLimit {
    /// A limit of `per_source` connections for each IPv4 address or IPv6 /64, and the wider IPv6
    /// prefixes' multiples of it.
    pub fn new(per_source: usize) -> Self {
        Self {
            per_source,
            counts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Takes a slot in each of `ip`'s buckets, or none and `Err` with the first full bucket. The
    /// slots are given back, once, when the returned [`SourceSlot`] drops.
    pub fn try_acquire(&self, ip: IpAddr) -> Result<SourceSlot, Bucket> {
        let buckets = Bucket::of(ip);
        // The map holds plain counters, so a panic elsewhere while it was locked cannot leave it
        // inconsistent: keep using it.
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        for &(bucket, factor) in &buckets {
            let open = counts.get(&bucket).copied().unwrap_or(0);
            if open >= self.per_source.saturating_mul(factor) {
                return Err(bucket);
            }
        }
        for &(bucket, _) in &buckets {
            *counts.entry(bucket).or_insert(0) += 1;
        }
        Ok(SourceSlot {
            buckets: buckets.into_iter().map(|(bucket, _)| bucket).collect(),
            counts: Arc::clone(&self.counts),
        })
    }

    /// Connections open in `bucket` now.
    pub fn open(&self, bucket: Bucket) -> usize {
        self.counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&bucket)
            .copied()
            .unwrap_or(0)
    }

    /// Buckets with at least one open connection; the map never holds more entries.
    pub fn buckets(&self) -> usize {
        self.counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// One connection's slots in its buckets; dropping it gives them back.
#[derive(Debug)]
pub struct SourceSlot {
    buckets: Vec<Bucket>,
    counts: Counts,
}

impl Drop for SourceSlot {
    fn drop(&mut self) {
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        for bucket in &self.buckets {
            if let Some(count) = counts.get_mut(bucket) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    // No entry outlives its last connection, so the map is bounded by the open
                    // connections.
                    counts.remove(bucket);
                }
            }
        }
    }
}

/// How often a full bucket's refusals are logged: the first refusal in a window gets a WARN line
/// that also counts the refusals suppressed since the bucket's previous line. The
/// `mqtt_bridge.connections.refused` metric counts every refusal.
#[derive(Debug)]
pub struct RefusalLog {
    window: Duration,
    capacity: usize,
    state: Mutex<RefusalState>,
}

#[derive(Debug, Default)]
struct RefusalState {
    windows: HashMap<Bucket, RefusalWindow>,
    /// Suppressed refusals of buckets no longer tracked (their window ended and the map was
    /// full, or the map was full when they were refused); reported with the next line.
    elsewhere: u64,
}

#[derive(Debug)]
struct RefusalWindow {
    since: Instant,
    suppressed: u64,
}

/// A refusal to log, with the refusals not logged before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogRefusal {
    /// Refusals of this bucket suppressed since its previous line.
    pub suppressed: u64,
    /// Refusals of other buckets that no later line of their own will report.
    pub suppressed_elsewhere: u64,
}

impl RefusalLog {
    /// One line per bucket per minute.
    pub const WINDOW: Duration = Duration::from_secs(60);
    /// Buckets tracked at once; beyond it, refusals of new buckets are only counted.
    pub const CAPACITY: usize = 1024;

    pub fn new(window: Duration, capacity: usize) -> Self {
        Self {
            window,
            capacity,
            state: Mutex::new(RefusalState::default()),
        }
    }

    /// A refusal of `bucket` at `now`: whether to log it, and what to report with it.
    pub fn note(&self, bucket: Bucket, now: Instant) -> Option<LogRefusal> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let window = self.window;
        if let Some(entry) = state.windows.get_mut(&bucket) {
            if now.saturating_duration_since(entry.since) < window {
                entry.suppressed += 1;
                return None;
            }
            let suppressed = std::mem::take(&mut entry.suppressed);
            entry.since = now;
            return Some(LogRefusal {
                suppressed,
                suppressed_elsewhere: std::mem::take(&mut state.elsewhere),
            });
        }
        if state.windows.len() >= self.capacity {
            // Forget the buckets whose window ended; what they suppressed goes with the next line.
            let mut moved = 0;
            state.windows.retain(|_, entry| {
                let current = now.saturating_duration_since(entry.since) < window;
                if !current {
                    moved += entry.suppressed;
                }
                current
            });
            state.elsewhere += moved;
            if state.windows.len() >= self.capacity {
                state.elsewhere += 1;
                return None;
            }
        }
        state.windows.insert(
            bucket,
            RefusalWindow {
                since: now,
                suppressed: 0,
            },
        );
        Some(LogRefusal {
            suppressed: 0,
            suppressed_elsewhere: std::mem::take(&mut state.elsewhere),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("an IP address")
    }

    fn v6(text: &str, length: u8) -> Bucket {
        Bucket::of(ip(text))
            .into_iter()
            .map(|(bucket, _)| bucket)
            .find(|bucket| matches!(bucket, Bucket::V6 { length: l, .. } if *l == length))
            .expect("an IPv6 bucket of that length")
    }

    #[test]
    fn each_source_gets_its_own_slots() {
        let limit = SourceLimit::new(2);
        let source = Bucket::V4(Ipv4Addr::new(203, 0, 113, 7));
        let first = limit.try_acquire(ip("203.0.113.7")).expect("first");
        let second = limit.try_acquire(ip("203.0.113.7")).expect("second");
        assert_eq!(limit.try_acquire(ip("203.0.113.7")).err(), Some(source));
        // Another source is not affected.
        let other = limit
            .try_acquire(ip("203.0.113.8"))
            .expect("another source");
        assert_eq!(limit.open(source), 2);
        drop(first);
        let again = limit.try_acquire(ip("203.0.113.7")).expect("a freed slot");
        drop((second, again, other));
        assert_eq!(limit.open(source), 0);
        assert_eq!(limit.buckets(), 0, "no entry outlives its connections");
    }

    #[test]
    fn ipv6_counts_by_its_64_prefix() {
        let limit = SourceLimit::new(1);
        let _slot = limit.try_acquire(ip("2001:db8:1:2::1")).expect("first");
        assert_eq!(
            limit.try_acquire(ip("2001:db8:1:2:ffff::9")).err(),
            Some(v6("2001:db8:1:2::", 64)),
            "the same /64"
        );
        assert!(
            limit.try_acquire(ip("2001:db8:1:3::1")).is_ok(),
            "another /64"
        );
        assert_eq!(
            v6("2001:db8:1:2:ffff::9", 64).to_string(),
            "2001:db8:1:2::/64"
        );
        assert_eq!(
            v6("2001:db8:1:2:ffff::9", 56).to_string(),
            "2001:db8:1::/56"
        );
        assert_eq!(
            v6("2001:db8:1:2:ffff::9", 48).to_string(),
            "2001:db8:1::/48"
        );
    }

    #[test]
    fn one_48_cannot_take_more_than_four_times_the_cap_across_its_64s() {
        let limit = SourceLimit::new(2);
        // Each /64 of 2001:db8:1::/48 is new, but every /56 holds at most 4 and the /48 at most 8.
        let mut held = Vec::new();
        for subnet in 0..4u16 {
            // Two /56s (subnets 0x00.. and 0x100..), each with two full /64s.
            let prefix = if subnet < 2 { subnet } else { 0x100 + subnet };
            for host in 1..=2 {
                held.push(
                    limit
                        .try_acquire(ip(&format!("2001:db8:1:{prefix:x}::{host}")))
                        .expect("within every cap"),
                );
            }
        }
        // The first /56 is full although this /64 is new.
        assert_eq!(
            limit.try_acquire(ip("2001:db8:1:9::1")).err(),
            Some(v6("2001:db8:1::", 56))
        );
        // A third /56: its /64 and /56 are empty, the /48 is full.
        assert_eq!(
            limit.try_acquire(ip("2001:db8:1:200::1")).err(),
            Some(v6("2001:db8:1::", 48))
        );
        assert_eq!(limit.open(v6("2001:db8:1::", 48)), 8);
        // Another /48 is not affected.
        let other = limit.try_acquire(ip("2001:db8:2::1")).expect("another /48");
        // A refusal took nothing; every slot comes back exactly once.
        drop(held);
        drop(other);
        assert_eq!(limit.buckets(), 0, "no entry outlives its connections");
    }

    #[test]
    fn ipv4_mapped_and_6to4_addresses_count_as_their_ipv4_address() {
        let limit = SourceLimit::new(1);
        let source = Bucket::V4(Ipv4Addr::new(203, 0, 113, 7));
        let _slot = limit.try_acquire(ip("203.0.113.7")).expect("first");
        assert_eq!(
            limit.try_acquire(ip("::ffff:203.0.113.7")).err(),
            Some(source)
        );
        // 2002:cb00:7107::/48 is 6to4 for 203.0.113.7, whatever the rest of the address.
        assert_eq!(
            limit.try_acquire(ip("2002:cb00:7107::1")).err(),
            Some(source)
        );
        assert_eq!(
            limit.try_acquire(ip("2002:cb00:7107:ffff::9")).err(),
            Some(source)
        );
        assert_eq!(Bucket::of(ip("2002:cb00:7107:1::1")), vec![(source, 1)]);
        // Another IPv4 address's 6to4 prefix is another bucket.
        assert!(limit.try_acquire(ip("2002:cb00:7108::1")).is_ok());
    }

    #[test]
    fn refusals_are_logged_once_per_bucket_per_window() {
        let log = RefusalLog::new(Duration::from_secs(60), 1024);
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let a = Bucket::V4(Ipv4Addr::new(203, 0, 113, 7));
        let b = Bucket::V4(Ipv4Addr::new(203, 0, 113, 8));
        let first = LogRefusal {
            suppressed: 0,
            suppressed_elsewhere: 0,
        };
        assert_eq!(log.note(a, at(0)), Some(first));
        assert_eq!(log.note(a, at(1)), None);
        assert_eq!(log.note(a, at(59)), None);
        // Another bucket has its own window.
        assert_eq!(log.note(b, at(2)), Some(first));
        // The next window's first line counts what was suppressed.
        assert_eq!(
            log.note(a, at(60)),
            Some(LogRefusal {
                suppressed: 2,
                suppressed_elsewhere: 0
            })
        );
        assert_eq!(log.note(a, at(61)), None);
    }

    #[test]
    fn refusals_beyond_the_tracked_buckets_are_counted_not_lost() {
        let log = RefusalLog::new(Duration::from_secs(60), 2);
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let bucket = |last| Bucket::V4(Ipv4Addr::new(203, 0, 113, last));
        assert!(log.note(bucket(1), at(0)).is_some());
        assert!(log.note(bucket(1), at(1)).is_none());
        assert!(log.note(bucket(2), at(0)).is_some());
        // Full, and both windows are current: the third bucket is only counted.
        assert_eq!(log.note(bucket(3), at(10)), None);
        // Once bucket 1's window ended, it is forgotten; its suppressed refusal and bucket 3's
        // are reported by the next line.
        assert_eq!(
            log.note(bucket(4), at(60)),
            Some(LogRefusal {
                suppressed: 0,
                suppressed_elsewhere: 2
            })
        );
    }
}
