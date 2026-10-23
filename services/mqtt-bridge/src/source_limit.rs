//! The cap on connections per source (README.md, "連線上限"): without it, a few sources could take
//! every one of `max_connections` and keep the real gateways out (security review of
//! Laysi/trustiot-v2#85, Medium; Laysi/trustiot-v2#600).
//!
//! The source is the trusted client IP ([`crate::client_ip`]), so forged forwarding headers do
//! not change which source a connection counts against. IPv4 addresses count one by one; IPv6
//! addresses count by their /64 prefix, the block one subscriber usually gets, because a client
//! with a /64 can use any of its 2^64 addresses. An IPv4-mapped IPv6 address counts as the IPv4
//! address.

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex, PoisonError};

/// What connections are counted by: one IPv4 address or one IPv6 /64.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// One IPv4 address.
    V4(Ipv4Addr),
    /// The first 64 bits of an IPv6 address.
    V6Prefix(u64),
}

impl Source {
    /// The source a client IP counts against.
    pub fn of(ip: IpAddr) -> Self {
        match ip.to_canonical() {
            IpAddr::V4(address) => Self::V4(address),
            IpAddr::V6(address) => {
                // The upper half of the address: its /64 prefix. The shift leaves 64 bits.
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "the value is shifted right by 64 bits first, so it fits in u64"
                )]
                let prefix = (address.to_bits() >> 64) as u64;
                Self::V6Prefix(prefix)
            }
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::V4(address) => write!(f, "{address}"),
            Self::V6Prefix(prefix) => {
                let address = Ipv6Addr::from_bits(u128::from(*prefix) << 64);
                write!(f, "{address}/64")
            }
        }
    }
}

/// Counts open connections per [`Source`] and refuses one more than `per_source`.
#[derive(Clone, Debug)]
pub struct SourceLimit {
    per_source: usize,
    counts: Arc<Mutex<HashMap<Source, usize>>>,
}

impl SourceLimit {
    /// A limit of `per_source` connections for each source.
    pub fn new(per_source: usize) -> Self {
        Self {
            per_source,
            counts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Takes one of `ip`'s source's slots, or `None` when the source has `per_source` already.
    /// The slot is given back when the returned [`SourceSlot`] drops.
    pub fn try_acquire(&self, ip: IpAddr) -> Option<SourceSlot> {
        let source = Source::of(ip);
        // The map holds plain counters, so a panic elsewhere while it was locked cannot leave it
        // inconsistent: keep using it.
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        let count = counts.entry(source).or_insert(0);
        if *count >= self.per_source {
            return None;
        }
        *count += 1;
        Some(SourceSlot {
            source,
            counts: Arc::clone(&self.counts),
        })
    }

    /// Connections open from `ip`'s source now.
    pub fn open(&self, ip: IpAddr) -> usize {
        self.counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&Source::of(ip))
            .copied()
            .unwrap_or(0)
    }

    /// Sources with at least one open connection; the map never holds more entries.
    pub fn sources(&self) -> usize {
        self.counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// One connection's slot of its source; dropping it gives the slot back.
#[derive(Debug)]
pub struct SourceSlot {
    source: Source,
    counts: Arc<Mutex<HashMap<Source, usize>>>,
}

impl Drop for SourceSlot {
    fn drop(&mut self) {
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = counts.get_mut(&self.source) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                // No entry outlives its last connection, so the map is bounded by the open
                // connections.
                counts.remove(&self.source);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("an IP address")
    }

    #[test]
    fn each_source_gets_its_own_slots() {
        let limit = SourceLimit::new(2);
        let first = limit.try_acquire(ip("203.0.113.7")).expect("first");
        let second = limit.try_acquire(ip("203.0.113.7")).expect("second");
        assert!(limit.try_acquire(ip("203.0.113.7")).is_none(), "a third");
        // Another source is not affected.
        let other = limit
            .try_acquire(ip("203.0.113.8"))
            .expect("another source");
        assert_eq!(limit.open(ip("203.0.113.7")), 2);
        drop(first);
        let again = limit.try_acquire(ip("203.0.113.7")).expect("a freed slot");
        drop((second, again, other));
        assert_eq!(limit.open(ip("203.0.113.7")), 0);
        assert_eq!(limit.sources(), 0, "no entry outlives its connections");
    }

    #[test]
    fn ipv6_counts_by_its_64_prefix() {
        let limit = SourceLimit::new(1);
        let _slot = limit.try_acquire(ip("2001:db8:1:2::1")).expect("first");
        assert!(
            limit.try_acquire(ip("2001:db8:1:2:ffff::9")).is_none(),
            "the same /64"
        );
        assert!(
            limit.try_acquire(ip("2001:db8:1:3::1")).is_some(),
            "another /64"
        );
        assert_eq!(
            Source::of(ip("2001:db8:1:2:ffff::9")).to_string(),
            "2001:db8:1:2::/64"
        );
    }

    #[test]
    fn ipv4_mapped_addresses_count_as_ipv4() {
        let limit = SourceLimit::new(1);
        let _slot = limit.try_acquire(ip("203.0.113.7")).expect("first");
        assert!(limit.try_acquire(ip("::ffff:203.0.113.7")).is_none());
        assert_eq!(
            Source::of(ip("::ffff:203.0.113.7")),
            Source::V4(Ipv4Addr::new(203, 0, 113, 7))
        );
    }
}
