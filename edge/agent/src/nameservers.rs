//! The DNS allowance of the agent's default deny (Laysi/trustiot-v2#879): the nameservers glibc
//! uses, read from `/etc/resolv.conf` the way glibc reads it, and the nftables commands that put
//! them in the two sets the default deny lets the agent's account reach on port 53.
//!
//! The agent resolves names with glibc (`getaddrinfo`), which sends its queries to the
//! nameservers of `/etc/resolv.conf`; on the gateway NetworkManager writes the network's servers
//! there, and DHCP may change them at any time. The default deny
//! (`systemd/trustiot-agent-default-deny.nft`) declares the sets [`SET_V4`] and [`SET_V6`] in the
//! table of the fieldbus allowlist ([`crate::allowlist::TABLE`]); the separate executable
//! `trustiot-agent-nameservers` (`src/bin/trustiot-agent-nameservers.rs`) fills them, as root's
//! side, from the root-owned file: the agent never supplies them, so a compromised agent cannot
//! open UDP to an address of its choice. `README.md`, section "連線位址的限制", has who runs it
//! and when.
//!
//! [`Nameservers::parse`] follows glibc's `res_init` (glibc 2.41, Debian 13, the gateway's
//! Raspberry Pi OS 13): a line that starts with `nameserver` followed by a space or a tab; the
//! value is the text after the blanks up to the next blank or line end; an IPv4 address in any
//! form `inet_aton` takes (`127.0.53`, `0x7f.1`), else an IPv6 address with any `%zone` cut off;
//! anything else is skipped; the first [`MAX_NAMESERVERS`] that parse are used. An IPv4-mapped
//! IPv6 address goes out as IPv4, so it is kept as the IPv4 address it maps.

use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::allowlist::TABLE;

/// The file glibc reads its nameservers from.
pub const RESOLV_CONF: &str = "/etc/resolv.conf";

/// The largest `/etc/resolv.conf` read. A real one is a few hundred bytes; the executable treats a
/// larger file as unreadable rather than read without bound.
pub const MAX_FILE_BYTES: usize = 64 * 1024;

/// glibc's `MAXNS`: it uses at most this many nameservers, and ignores the others.
pub const MAX_NAMESERVERS: usize = 3;

/// The set of IPv4 nameservers, `ipv4_addr`. The default deny matches it with
/// `meta l4proto { tcp, udp } th dport 53 ip daddr @nameserver4`.
pub const SET_V4: &str = "nameserver4";

/// The set of IPv6 nameservers, `ipv6_addr`.
pub const SET_V6: &str = "nameserver6";

/// The nameservers glibc uses, in the order it asks them, each once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Nameservers {
    addresses: Vec<IpAddr>,
}

impl Nameservers {
    /// The nameservers of an `/etc/resolv.conf`, as glibc reads it (see the module
    /// documentation). Never fails: a line glibc would skip is skipped.
    pub fn parse(contents: &[u8]) -> Self {
        let mut addresses: Vec<IpAddr> = Vec::new();
        let mut used = 0;
        for line in contents.split(|&byte| byte == b'\n') {
            if used == MAX_NAMESERVERS {
                break;
            }
            // glibc works on C strings: a NUL byte ends the line.
            let line = line.split(|&byte| byte == 0).next().unwrap_or_default();
            let Some(rest) = line.strip_prefix(b"nameserver") else {
                continue;
            };
            if !matches!(rest.first(), Some(b' ' | b'\t')) {
                continue;
            }
            let rest = trim_blanks(rest);
            let value = rest
                .split(|&byte| matches!(byte, b' ' | b'\t'))
                .next()
                .unwrap_or_default();
            let Some(address) = nameserver(value) else {
                continue;
            };
            // glibc counts every nameserver it parsed, a repeated one too.
            used += 1;
            if !addresses.contains(&address) {
                addresses.push(address);
            }
        }
        Self { addresses }
    }

    /// The nameservers, in the order glibc asks them.
    pub fn addresses(&self) -> &[IpAddr] {
        &self.addresses
    }

    /// The nftables commands that make the two sets hold exactly these nameservers, as one file
    /// for `nft -f`, which applies a file as one transaction. Both sets are flushed and filled;
    /// nothing is created, so the commands fail, and nothing changes, when the default deny's
    /// table or sets are missing. Every byte not from this function's fixed text is an address
    /// written by `Display`.
    pub fn nft_script(&self) -> String {
        let mut script = String::new();
        for set in [SET_V4, SET_V6] {
            let _ = writeln!(script, "flush set inet {TABLE} {set}");
        }
        let (v4, v6): (Vec<&IpAddr>, Vec<&IpAddr>) =
            self.addresses.iter().partition(|address| address.is_ipv4());
        for (set, addresses) in [(SET_V4, v4), (SET_V6, v6)] {
            if addresses.is_empty() {
                continue;
            }
            let elements: Vec<String> = addresses.iter().map(ToString::to_string).collect();
            let _ = writeln!(
                script,
                "add element inet {TABLE} {set} {{ {} }}",
                elements.join(", ")
            );
        }
        script
    }

    /// The two sets as the default deny's file declares them (after the table and the fieldbus
    /// sets, [`crate::allowlist::Allowlist::declarations`]). Loading these again keeps existing
    /// sets and their elements.
    pub fn declarations() -> String {
        format!(
            "add set inet {TABLE} {SET_V4} {{ type ipv4_addr; size {MAX_NAMESERVERS}; }}\n\
             add set inet {TABLE} {SET_V6} {{ type ipv6_addr; size {MAX_NAMESERVERS}; }}\n"
        )
    }
}

impl std::fmt::Display for Nameservers {
    /// The addresses separated by spaces, or `none`.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.addresses.is_empty() {
            return formatter.write_str("none");
        }
        for (index, address) in self.addresses.iter().enumerate() {
            if index > 0 {
                formatter.write_str(" ")?;
            }
            write!(formatter, "{address}")?;
        }
        Ok(())
    }
}

fn trim_blanks(mut bytes: &[u8]) -> &[u8] {
    while let Some((b' ' | b'\t', rest)) = bytes.split_first() {
        bytes = rest;
    }
    bytes
}

/// One nameserver value as glibc reads it: `inet_aton` (exact), else IPv6 without its zone.
fn nameserver(value: &[u8]) -> Option<IpAddr> {
    if value.is_empty() {
        return None;
    }
    if let Some(address) = inet_aton(value) {
        return Some(IpAddr::V4(address));
    }
    let without_zone = value.split(|&byte| byte == b'%').next()?;
    let text = std::str::from_utf8(without_zone).ok()?;
    let address: Ipv6Addr = text.parse().ok()?;
    // An IPv4-mapped address goes out as an IPv4 packet to the address it maps.
    Some(IpAddr::V6(address).to_canonical())
}

/// glibc's `__inet_aton_exact`: one to four parts separated by dots, each decimal, octal (a
/// leading 0) or hexadecimal (`0x`), the last part filling the remaining bytes; nothing after it.
fn inet_aton(value: &[u8]) -> Option<Ipv4Addr> {
    let parts: Vec<&[u8]> = value.split(|&byte| byte == b'.').collect();
    if parts.len() > 4 {
        return None;
    }
    let mut numbers = Vec::with_capacity(parts.len());
    for part in &parts {
        numbers.push(inet_aton_part(part)?);
    }
    let (last, leading) = numbers.split_last()?;
    let mut address: u32 = 0;
    for (index, &byte) in leading.iter().enumerate() {
        let byte = u8::try_from(byte).ok()?;
        address |= u32::from(byte) << (24 - 8 * index);
    }
    let room = 32 - 8 * leading.len();
    if room < 32 && u64::from(*last) >= 1_u64 << room {
        return None;
    }
    Some(Ipv4Addr::from(address | last))
}

/// One part of an `inet_aton` address, as `strtoul` with base 0 reads it, up to `u32::MAX`.
fn inet_aton_part(part: &[u8]) -> Option<u32> {
    // inet_aton requires a digit where each part starts.
    if !part.first()?.is_ascii_digit() {
        return None;
    }
    let (digits, radix) = match part {
        [b'0', b'x' | b'X', rest @ ..] => (rest, 16),
        [b'0', rest @ ..] if !rest.is_empty() => (rest, 8),
        _ => (part, 10),
    };
    if digits.is_empty() || !digits.iter().all(|&byte| char::from(byte).is_digit(radix)) {
        return None;
    }
    let mut value: u64 = 0;
    for &byte in digits {
        let digit = char::from(byte).to_digit(radix)?;
        value = value
            .checked_mul(u64::from(radix))?
            .checked_add(u64::from(digit))?;
        if value > u64::from(u32::MAX) {
            return None;
        }
    }
    u32::try_from(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(contents: &str) -> Vec<String> {
        Nameservers::parse(contents.as_bytes())
            .addresses()
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn nameservers_are_read_as_glibc_reads_them() {
        // What NetworkManager writes.
        assert_eq!(
            parsed(
                "# Generated by NetworkManager\nsearch lan\nnameserver 192.168.1.1\n\
                 nameserver fd00::1\n"
            ),
            ["192.168.1.1", "fd00::1"]
        );
        // inet_aton's forms, and an IPv4-mapped address kept as IPv4.
        assert_eq!(
            parsed("nameserver 127.0.53\nnameserver 0x7f.1\nnameserver ::ffff:10.0.0.9\n"),
            ["127.0.0.53", "127.0.0.1", "10.0.0.9"]
        );
        assert_eq!(parsed("nameserver 012.0.0.1\n"), ["10.0.0.1"]);
        assert_eq!(parsed("nameserver 167772161\n"), ["10.0.0.1"]);
        // A zone is cut off; blanks before the value and text after it do not matter.
        assert_eq!(
            parsed("nameserver fe80::1%eth0\nnameserver \t 10.0.0.2 # router\n"),
            ["fe80::1", "10.0.0.2"]
        );
        // At most three, counting only the values that parse; a repeat counts but is listed once.
        assert_eq!(
            parsed(
                "nameserver 10.0.0.1\nnameserver dns.example.test\nnameserver 10.0.0.1\n\
                 nameserver 10.0.0.2\nnameserver 10.0.0.3\n"
            ),
            ["10.0.0.1", "10.0.0.2"]
        );
        // Lines glibc skips: another keyword, a leading blank, no blank after the keyword, a
        // comment, a name, a malformed address, trailing garbage, CRLF, nothing at all.
        for skipped in [
            "domain example.test\n",
            " nameserver 10.0.0.1\n",
            "nameserver\t\n",
            "nameserver10.0.0.1\n",
            "nameservers 10.0.0.1\n",
            "# nameserver 10.0.0.1\n",
            "nameserver dns.example.test\n",
            "nameserver 10.0.0.256\n",
            "nameserver 10.0.0.1.7\n",
            "nameserver 1.2.3.4.\n",
            "nameserver 08.0.0.1\n",
            "nameserver 0x\n",
            "nameserver 4294967296\n",
            "nameserver 1.16777216\n",
            "nameserver 10.0.0.1x\n",
            "nameserver 10.0.0.1\r\n",
            "nameserver fd00::1::2\n",
            "nameserver\n",
            "",
        ] {
            assert_eq!(parsed(skipped), Vec::<String>::new(), "{skipped:?}");
        }
        // glibc stops at a NUL byte.
        assert_eq!(
            Nameservers::parse(b"nameserver 10.0.0.1\0garbage\n").addresses(),
            [IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))]
        );
        // Bytes that are not UTF-8 are skipped, not a failure.
        assert_eq!(
            Nameservers::parse(b"nameserver \xff\xfe\nnameserver 10.0.0.1\n").addresses(),
            [IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))]
        );
        // The limits of the last part.
        assert_eq!(parsed("nameserver 1.2.65535\n"), ["1.2.255.255"]);
        assert_eq!(parsed("nameserver 1.2.65536\n"), Vec::<String>::new());
        assert_eq!(parsed("nameserver 4294967295\n"), ["255.255.255.255"]);
    }

    #[test]
    fn the_script_names_only_the_two_sets_and_writes_the_parsed_addresses() {
        let nameservers = Nameservers::parse(
            b"nameserver 10.81.3.1\nnameserver fd81:3::53\nnameserver 10.0.0.2\n",
        );
        assert_eq!(
            nameservers.nft_script(),
            "flush set inet trustiot_agent_allow nameserver4\n\
             flush set inet trustiot_agent_allow nameserver6\n\
             add element inet trustiot_agent_allow nameserver4 { 10.81.3.1, 10.0.0.2 }\n\
             add element inet trustiot_agent_allow nameserver6 { fd81:3::53 }\n"
        );
        assert_eq!(nameservers.to_string(), "10.81.3.1 fd81:3::53 10.0.0.2");
        // No nameserver: the sets are only flushed.
        assert_eq!(
            Nameservers::default().nft_script(),
            "flush set inet trustiot_agent_allow nameserver4\n\
             flush set inet trustiot_agent_allow nameserver6\n"
        );
        assert_eq!(Nameservers::default().to_string(), "none");
        assert_eq!(
            Nameservers::declarations(),
            "add set inet trustiot_agent_allow nameserver4 { type ipv4_addr; size 3; }\n\
             add set inet trustiot_agent_allow nameserver6 { type ipv6_addr; size 3; }\n"
        );
    }
}
