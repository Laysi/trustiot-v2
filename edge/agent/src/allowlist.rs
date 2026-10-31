//! The fieldbus allowlist (Laysi/trustiot-v2#864): the TCP destinations, address and port, that
//! the agent's account may reach once the default deny of Laysi/trustiot-v2#865 is in force, and
//! the interface through which the agent hands them to the component that keeps them in the
//! kernel.
//!
//! The agent cannot change nftables: it runs without capabilities. The component that can is a
//! separate executable, `trustiot-agent-allowlist` (`src/bin/trustiot-agent-allowlist.rs`),
//! started by systemd for each connection to [`SOCKET`] with `CAP_NET_ADMIN` and nothing else
//! (`systemd/trustiot-agent-allowlist.socket`, `systemd/trustiot-agent-allowlist@.service`). It
//! shares this module with the agent, so both sides read and write the same format. It treats a
//! request as untrusted: it takes nothing but a list in the format below ([`Allowlist::parse`]),
//! refuses the whole list when any part of it is anything else, and replaces the contents of the
//! two sets in one nftables transaction ([`Allowlist::nft_script`]); the table and the sets belong
//! to the default deny's file, which declares them ([`Allowlist::declarations`]), and the
//! component fails rather than create them. The commands it hands to nft
//! are fixed text around the parsed addresses and ports, written by `Display`: no byte of the
//! request reaches nft. `README.md`, section "fieldbus 放行集合", has the interface, the permission
//! boundary and the threat model.
//!
//! A request is printable ASCII, and every line, the last one included, ends in `\n`:
//!
//! ```text
//! trustiot-agent-allowlist v1
//! 10.0.0.5 502
//! fd00::7 8886
//! end
//! ```
//!
//! - The first line is [`HEADER`].
//! - Each entry is an IP address in its canonical text form (as [`IpAddr`]'s `Display` writes
//!   it; an IPv4-mapped address as the IPv4 address it maps), one space, and a TCP port from 1
//!   to 65535 in decimal without leading zeros. Every entry is a TCP destination: the format has
//!   no protocol, no other field and no comments. No entry may appear twice, and no address the
//!   agent's address guard refuses wherever the gateway is (loopback, unspecified, multicast,
//!   the limited broadcast address, the cloud metadata addresses, IPv4-compatible IPv6 and the
//!   IPv6 forms that embed one of these; `src/modbus/address.rs`) may appear at all.
//! - The last line is [`END`], so a list cut short is refused rather than applied in part.
//! - At most [`MAX_ENTRIES`] entries and [`MAX_REQUEST_BYTES`] bytes.
//!
//! The list is the complete set, not a change: each request replaces what the previous one put
//! there. The component answers with one line ([`Reply`]).

use std::collections::BTreeSet;
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use crate::modbus::address::{Interfaces, Refusal, refusal};

/// The unix socket the component listens on (`ListenStream=` of
/// `systemd/trustiot-agent-allowlist.socket`): owned by root and the agent's group, mode 0660, in
/// a directory only root can write.
pub const SOCKET: &str = "/run/trustiot-agent-allowlist/socket";

/// The first line of a request: the format and its version.
pub const HEADER: &str = "trustiot-agent-allowlist v1";

/// The last line of a request.
pub const END: &str = "end";

/// The most entries a list may hold. A configuration has at most
/// [`protocol::config::MAX_DEVICES`] devices, so at most that many endpoints; a host name may
/// resolve to several addresses, and this leaves room for about four per endpoint.
pub const MAX_ENTRIES: usize = 4096;

/// The longest entry line: the longest canonical IPv6 text (eight groups of four hex digits and
/// seven colons), a space, a five-digit port and the newline.
const MAX_ENTRY_LINE: usize = 39 + 1 + 5 + 1;

/// The largest request: [`MAX_ENTRIES`] of the longest entries between the two fixed lines. A
/// larger request is refused without reading the rest of it.
pub const MAX_REQUEST_BYTES: usize =
    HEADER.len() + 1 + MAX_ENTRIES * MAX_ENTRY_LINE + END.len() + 1;

/// The nftables table (family `inet`) that holds the two sets. Laysi/trustiot-v2#865's default
/// deny for the agent's account goes in this table too, because a rule can only use a set of its
/// own table. It is not `inet trustiot_agent` (Laysi/trustiot-v2#813): that file replaces its
/// whole table on every load, which would empty the sets.
pub const TABLE: &str = "trustiot_agent_allow";

/// The set of IPv4 destinations, `ipv4_addr . inet_service`. A rule matches it with
/// `meta l4proto tcp ip daddr . tcp dport @fieldbus4`.
pub const SET_V4: &str = "fieldbus4";

/// The set of IPv6 destinations, `ipv6_addr . inet_service`. A rule matches it with
/// `meta l4proto tcp ip6 daddr . tcp dport @fieldbus6`.
pub const SET_V6: &str = "fieldbus6";

/// The most elements one `add element` command carries; a list is spread over several commands
/// of the same transaction.
const ELEMENTS_PER_COMMAND: usize = 256;

/// One TCP destination of the list: an address the agent's address guard does not refuse
/// wherever the gateway is, in canonical form, and a port from 1 to 65535.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Destination {
    address: IpAddr,
    port: u16,
}

impl Destination {
    /// The destination `address`, `port`. An IPv4-mapped IPv6 address is kept as the IPv4
    /// address it maps, as the address guard connects to it.
    ///
    /// # Errors
    ///
    /// When the port is 0, or the address guard refuses the address wherever the gateway is
    /// ([`EntryError::Refused`]); the gateway's own addresses and its subnets' broadcast
    /// addresses depend on its interfaces, and the kernel refuses those
    /// (`systemd/trustiot-agent-egress.nft`).
    pub fn new(address: IpAddr, port: u16) -> Result<Self, EntryError> {
        let address = address.to_canonical();
        if port == 0 {
            return Err(EntryError::Port);
        }
        if let Some(refused) = refusal(address, &Interfaces::default()) {
            return Err(EntryError::Refused(describe(refused)));
        }
        Ok(Self { address, port })
    }

    /// The address, in canonical form.
    pub fn address(&self) -> IpAddr {
        self.address
    }

    /// The TCP port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Reads one entry line (without its newline).
    fn parse(line: &str) -> Result<Self, EntryError> {
        let (address_text, port_text) = line.split_once(' ').ok_or(EntryError::Shape)?;
        if port_text.contains(' ') {
            return Err(EntryError::Shape);
        }
        let address = IpAddr::from_str(address_text).map_err(|_| EntryError::Address)?;
        if address.to_canonical().to_string() != address_text {
            return Err(EntryError::Address);
        }
        let port = u16::from_str(port_text).map_err(|_| EntryError::Port)?;
        if port.to_string() != port_text {
            return Err(EntryError::Port);
        }
        Self::new(address, port)
    }
}

/// The entry line, without its newline: `<address> <port>`.
impl fmt::Display for Destination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.address, self.port)
    }
}

fn describe(refused: Refusal) -> &'static str {
    match refused {
        Refusal::Loopback => "loopback",
        Refusal::Unspecified => "unspecified",
        Refusal::Gateway => "gateway's own",
        Refusal::Multicast => "multicast",
        Refusal::Broadcast => "broadcast",
        Refusal::Metadata => "cloud metadata",
        Refusal::Ipv4Compatible => "IPv4-compatible IPv6",
    }
}

/// Why one entry line is not a destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EntryError {
    /// The line holds bytes other than printable ASCII.
    #[error("not printable ASCII")]
    NotText,
    /// The line is not two fields separated by one space.
    #[error("not \"<address> <port>\"")]
    Shape,
    /// The first field is not an IP address in its canonical text form.
    #[error("not an IP address in canonical form")]
    Address,
    /// The second field is not a port from 1 to 65535 in decimal without leading zeros.
    #[error("not a TCP port from 1 to 65535 in decimal")]
    Port,
    /// The address is one the agent's address guard always refuses.
    #[error("a {0} address, which the agent never connects to")]
    Refused(&'static str),
    /// The destination appeared on an earlier line too.
    #[error("listed twice")]
    Duplicate,
}

/// Why a request is refused as a whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ListError {
    /// More than [`MAX_REQUEST_BYTES`] bytes.
    #[error("larger than {MAX_REQUEST_BYTES} bytes")]
    TooLarge,
    /// More than [`MAX_ENTRIES`] entries.
    #[error("more than {MAX_ENTRIES} entries")]
    TooMany,
    /// The first line is not [`HEADER`].
    #[error("the first line is not \"{HEADER}\"")]
    Header,
    /// No [`END`] line: the list is incomplete.
    #[error("no \"{END}\" line, so the list may be incomplete")]
    Unterminated,
    /// Something follows the [`END`] line.
    #[error("data after the \"{END}\" line")]
    AfterEnd,
    /// A line is not an entry.
    #[error("line {line}: {problem}")]
    Line {
        /// The line's number, from 1 (the header).
        line: usize,
        /// What is wrong with it.
        problem: EntryError,
    },
}

/// The complete list of destinations, each once, ordered (IPv4 first).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Allowlist {
    destinations: BTreeSet<Destination>,
}

impl Allowlist {
    /// The list of `destinations`; one that appears more than once is listed once.
    ///
    /// # Errors
    ///
    /// [`ListError::TooMany`] when more than [`MAX_ENTRIES`] different destinations remain.
    pub fn new(destinations: impl IntoIterator<Item = Destination>) -> Result<Self, ListError> {
        let destinations: BTreeSet<Destination> = destinations.into_iter().collect();
        if destinations.len() > MAX_ENTRIES {
            return Err(ListError::TooMany);
        }
        Ok(Self { destinations })
    }

    /// How many destinations the list holds.
    pub fn len(&self) -> usize {
        self.destinations.len()
    }

    /// Whether the list is empty (no device to reach).
    pub fn is_empty(&self) -> bool {
        self.destinations.is_empty()
    }

    /// The destinations, IPv4 first, each family in address and port order.
    pub fn iter(&self) -> impl Iterator<Item = &Destination> {
        self.destinations.iter()
    }

    /// The number of IPv4 and of IPv6 destinations.
    pub fn families(&self) -> (usize, usize) {
        let v4 = self
            .destinations
            .iter()
            .filter(|destination| destination.address.is_ipv4())
            .count();
        (v4, self.destinations.len() - v4)
    }

    /// The SHA-256 of the list's request ([`Self::encode`]) in lowercase hex: one list has one
    /// request, so the component's log names the list it applied and the agent can log the same
    /// value for the list it sent.
    pub fn digest(&self) -> String {
        ring::digest::digest(&ring::digest::SHA256, self.encode().as_bytes())
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// The request that carries this list.
    pub fn encode(&self) -> String {
        let mut request = format!("{HEADER}\n");
        for destination in &self.destinations {
            request.push_str(&format!("{destination}\n"));
        }
        request.push_str(END);
        request.push('\n');
        request
    }

    /// Reads a request.
    ///
    /// # Errors
    ///
    /// The first thing that makes it anything other than a complete list in the format of this
    /// module, which refuses the whole request.
    pub fn parse(request: &[u8]) -> Result<Self, ListError> {
        if request.len() > MAX_REQUEST_BYTES {
            return Err(ListError::TooLarge);
        }
        if let Some(at) = request
            .iter()
            .position(|&byte| byte != b'\n' && !(b' '..=b'~').contains(&byte))
        {
            let line = 1 + request[..at].iter().filter(|&&byte| byte == b'\n').count();
            return Err(ListError::Line {
                line,
                problem: EntryError::NotText,
            });
        }
        // Printable ASCII and newlines only, so this cannot fail.
        let text = std::str::from_utf8(request).map_err(|_| ListError::Line {
            line: 1,
            problem: EntryError::NotText,
        })?;
        let mut rest = text
            .strip_prefix(HEADER)
            .and_then(|rest| rest.strip_prefix('\n'))
            .ok_or(ListError::Header)?;
        let mut destinations = BTreeSet::new();
        let mut line = 1;
        loop {
            line += 1;
            let (current, after) = rest.split_once('\n').ok_or(ListError::Unterminated)?;
            if current == END {
                if !after.is_empty() {
                    return Err(ListError::AfterEnd);
                }
                return Ok(Self { destinations });
            }
            let destination =
                Destination::parse(current).map_err(|problem| ListError::Line { line, problem })?;
            if !destinations.insert(destination) {
                return Err(ListError::Line {
                    line,
                    problem: EntryError::Duplicate,
                });
            }
            if destinations.len() > MAX_ENTRIES {
                return Err(ListError::TooMany);
            }
            rest = after;
        }
    }

    /// The nftables commands that make the two sets hold exactly this list, as one file for
    /// `nft -f`, which applies a file as one transaction: all of it or none of it.
    ///
    /// Both sets are flushed and filled. The script neither creates nor declares anything: the
    /// table and the sets come from the file that loads the default deny (Laysi/trustiot-v2#865,
    /// with [`Self::declarations`]). When they are missing (no default deny loaded, or
    /// `nft flush ruleset` removed it), or a set cannot take the elements, the transaction fails
    /// and nothing changes; an applied list therefore always lands in the sets of a loaded
    /// default deny. Nothing else is named: no other table, set, chain or rule. Every byte that
    /// does not come from this function's fixed text comes from an [`IpAddr`] or a `u16`
    /// written by `Display`.
    pub fn nft_script(&self) -> String {
        let mut script = String::new();
        for set in [SET_V4, SET_V6] {
            script.push_str(&format!("flush set inet {TABLE} {set}\n"));
        }
        let (v4, v6): (Vec<&Destination>, Vec<&Destination>) = self
            .destinations
            .iter()
            .partition(|destination| destination.address.is_ipv4());
        for (set, destinations) in [(SET_V4, v4), (SET_V6, v6)] {
            for chunk in destinations.chunks(ELEMENTS_PER_COMMAND) {
                let elements: Vec<String> = chunk
                    .iter()
                    .map(|destination| format!("{} . {}", destination.address, destination.port))
                    .collect();
                script.push_str(&format!(
                    "add element inet {TABLE} {set} {{ {} }}\n",
                    elements.join(", ")
                ));
            }
        }
        script
    }

    /// The table and the two sets, as the file that loads the default deny
    /// (Laysi/trustiot-v2#865) declares them; the component itself never creates them. Loading
    /// these again keeps existing sets and their elements, and fails the whole load when a set
    /// exists with another type.
    pub fn declarations() -> String {
        format!(
            "add table inet {TABLE}\n\
             add set inet {TABLE} {SET_V4} {{ type ipv4_addr . inet_service; size {MAX_ENTRIES}; }}\n\
             add set inet {TABLE} {SET_V6} {{ type ipv6_addr . inet_service; size {MAX_ENTRIES}; }}\n"
        )
    }
}

/// The component's answer to a request: one line, then it closes the connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// The sets now hold exactly the list, which had this many destinations: `ok <n>`.
    Applied(usize),
    /// The request is not a list in the format of this module; the sets are unchanged:
    /// `refused: <reason>`.
    Refused(String),
    /// The list was valid but nft did not apply it; the sets are unchanged: `failed: <reason>`.
    Failed(String),
}

impl Reply {
    /// Reads the answer line (without its newline).
    pub fn parse(line: &str) -> Option<Self> {
        if let Some(text) = line.strip_prefix("ok ") {
            let count = usize::from_str(text).ok()?;
            (count.to_string() == text).then_some(Self::Applied(count))
        } else if let Some(reason) = line.strip_prefix("refused: ") {
            Some(Self::Refused(reason.to_owned()))
        } else {
            line.strip_prefix("failed: ")
                .map(|reason| Self::Failed(reason.to_owned()))
        }
    }
}

/// The answer line, without its newline. A reason's control characters are written as spaces, so
/// the answer stays one line.
impl fmt::Display for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let one_line = |reason: &str| -> String {
            reason
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect()
        };
        match self {
            Self::Applied(count) => write!(f, "ok {count}"),
            Self::Refused(reason) => write!(f, "refused: {}", one_line(reason)),
            Self::Failed(reason) => write!(f, "failed: {}", one_line(reason)),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "test code; clippy.toml allows it in tests but not in a test module's helpers"
    )]

    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    fn destination(address: &str, port: u16) -> Destination {
        Destination::new(address.parse().unwrap(), port).unwrap()
    }

    fn request(entries: &[&str]) -> String {
        let mut request = format!("{HEADER}\n");
        for entry in entries {
            request.push_str(entry);
            request.push('\n');
        }
        request.push_str("end\n");
        request
    }

    #[test]
    fn a_list_round_trips_in_canonical_order() {
        let list = Allowlist::new([
            destination("fd00::7", 8886),
            destination("10.0.0.5", 502),
            destination("10.0.0.5", 502),
            destination("192.168.1.20", 4196),
            destination("::ffff:10.0.0.9", 8899),
        ])
        .unwrap();
        assert_eq!(list.len(), 4);
        assert_eq!(list.families(), (3, 1));
        let encoded = list.encode();
        assert_eq!(
            encoded,
            "trustiot-agent-allowlist v1\n10.0.0.5 502\n10.0.0.9 8899\n192.168.1.20 4196\n\
             fd00::7 8886\nend\n"
        );
        assert_eq!(Allowlist::parse(encoded.as_bytes()).unwrap(), list);
        let empty = Allowlist::default();
        assert_eq!(empty.encode(), "trustiot-agent-allowlist v1\nend\n");
        assert_eq!(Allowlist::parse(empty.encode().as_bytes()).unwrap(), empty);
    }

    #[test]
    fn anything_but_a_complete_list_is_refused_whole() {
        let line = |line, problem| Err(ListError::Line { line, problem });
        let cases: Vec<(String, Result<Allowlist, ListError>)> = vec![
            (String::new(), Err(ListError::Header)),
            (
                "trustiot-agent-allowlist v2\nend\n".into(),
                Err(ListError::Header),
            ),
            (
                "trustiot-agent-allowlist v1 \nend\n".into(),
                Err(ListError::Header),
            ),
            ("trustiot-agent-allowlist v1".into(), Err(ListError::Header)),
            (
                "trustiot-agent-allowlist v1\n".into(),
                Err(ListError::Unterminated),
            ),
            (
                "trustiot-agent-allowlist v1\n10.0.0.5 502\n".into(),
                Err(ListError::Unterminated),
            ),
            (
                "trustiot-agent-allowlist v1\n10.0.0.5 502\nend".into(),
                Err(ListError::Unterminated),
            ),
            (format!("{}x", request(&[])), Err(ListError::AfterEnd)),
            (format!("{}\n", request(&[])), Err(ListError::AfterEnd)),
            (format!("{}end\n", request(&[])), Err(ListError::AfterEnd)),
            // Windows line ends, tabs, NUL and anything not ASCII.
            (
                "trustiot-agent-allowlist v1\r\nend\r\n".into(),
                line(1, EntryError::NotText),
            ),
            (request(&["10.0.0.5\t502"]), line(2, EntryError::NotText)),
            (request(&["10.0.0.5 502\0"]), line(2, EntryError::NotText)),
            (request(&["10.0.0.5 50２"]), line(2, EntryError::NotText)),
            // Other rules, other fields, nft syntax: never an entry.
            (request(&["flush ruleset"]), line(2, EntryError::Address)),
            (request(&["flush ruleset now"]), line(2, EntryError::Shape)),
            (
                request(&["10.0.0.5 502 accept"]),
                line(2, EntryError::Shape),
            ),
            (
                request(&["10.0.0.5 502; flush ruleset"]),
                line(2, EntryError::Shape),
            ),
            (request(&["10.0.0.5 . 502"]), line(2, EntryError::Shape)),
            (request(&["10.0.0.5  502"]), line(2, EntryError::Shape)),
            (request(&[" 10.0.0.5 502"]), line(2, EntryError::Shape)),
            (request(&["10.0.0.5 502 "]), line(2, EntryError::Shape)),
            (request(&["10.0.0.5"]), line(2, EntryError::Shape)),
            (request(&[""]), line(2, EntryError::Shape)),
            (request(&["# a comment"]), line(2, EntryError::Shape)),
            (request(&["tcp 10.0.0.5:502"]), line(2, EntryError::Address)),
            (request(&["udp 10.0.0.5"]), line(2, EntryError::Address)),
            (request(&["10.0.0.5 } 502"]), line(2, EntryError::Shape)),
            (request(&["10.0.0.5} 502"]), line(2, EntryError::Address)),
            (request(&["10.0.0.0/8 502"]), line(2, EntryError::Address)),
            (
                request(&["10.0.0.5-10.0.0.9 502"]),
                line(2, EntryError::Address),
            ),
            (request(&["* 502"]), line(2, EntryError::Address)),
            (
                request(&["plc.example.test 502"]),
                line(2, EntryError::Address),
            ),
            // Only the canonical spelling of an address.
            (request(&["010.0.0.5 502"]), line(2, EntryError::Address)),
            (request(&["10.0.5 502"]), line(2, EntryError::Address)),
            (request(&["FD00::7 502"]), line(2, EntryError::Address)),
            (request(&["fd00:0::7 502"]), line(2, EntryError::Address)),
            (
                request(&["::ffff:10.0.0.5 502"]),
                line(2, EntryError::Address),
            ),
            (request(&["[fd00::7] 502"]), line(2, EntryError::Address)),
            (request(&["fe80::1%eth0 502"]), line(2, EntryError::Address)),
            // Only a port 1-65535 in plain decimal.
            (request(&["10.0.0.5 0"]), line(2, EntryError::Port)),
            (request(&["10.0.0.5 65536"]), line(2, EntryError::Port)),
            (request(&["10.0.0.5 0502"]), line(2, EntryError::Port)),
            (request(&["10.0.0.5 +502"]), line(2, EntryError::Port)),
            (request(&["10.0.0.5 502-503"]), line(2, EntryError::Port)),
            (request(&["10.0.0.5 modbus"]), line(2, EntryError::Port)),
            // Addresses no field device has.
            (
                request(&["127.0.0.1 502"]),
                line(2, EntryError::Refused("loopback")),
            ),
            (
                request(&["::1 502"]),
                line(2, EntryError::Refused("loopback")),
            ),
            (
                request(&["0.0.0.0 502"]),
                line(2, EntryError::Refused("unspecified")),
            ),
            (
                request(&[":: 502"]),
                line(2, EntryError::Refused("unspecified")),
            ),
            (
                request(&["224.0.0.1 502"]),
                line(2, EntryError::Refused("multicast")),
            ),
            (
                request(&["ff02::1 502"]),
                line(2, EntryError::Refused("multicast")),
            ),
            (
                request(&["255.255.255.255 502"]),
                line(2, EntryError::Refused("broadcast")),
            ),
            (
                request(&["169.254.169.254 80"]),
                line(2, EntryError::Refused("cloud metadata")),
            ),
            (
                request(&["fd00:ec2::254 80"]),
                line(2, EntryError::Refused("cloud metadata")),
            ),
            (
                request(&["64:ff9b::7f00:1 502"]),
                line(2, EntryError::Refused("loopback")),
            ),
            (
                // ::10.0.0.5, written as Display writes it.
                request(&[&format!(
                    "{} 502",
                    Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0x0a00, 5)
                )]),
                line(2, EntryError::Refused("IPv4-compatible IPv6")),
            ),
            (
                request(&["10.0.0.5 502", "10.0.0.6 502", "10.0.0.5 502"]),
                line(4, EntryError::Duplicate),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(Allowlist::parse(input.as_bytes()), expected, "{input:?}");
        }
        // A refusal names the line and never repeats the request's bytes.
        let refused = Allowlist::parse(request(&["10.0.0.5 502; flush ruleset"]).as_bytes())
            .unwrap_err()
            .to_string();
        assert_eq!(refused, "line 2: not \"<address> <port>\"");
    }

    #[test]
    fn the_limits_hold_at_the_edge() {
        let full: Vec<Destination> = (0..MAX_ENTRIES)
            .map(|index| {
                let index = u32::try_from(index).unwrap();
                Destination::new(IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + index)), 502).unwrap()
            })
            .collect();
        let list = Allowlist::new(full.clone()).unwrap();
        assert_eq!(Allowlist::parse(list.encode().as_bytes()).unwrap(), list);
        let one_more = Destination::new(IpAddr::V4(Ipv4Addr::new(10, 1, 0, 0)), 502).unwrap();
        assert_eq!(
            Allowlist::new(full.iter().copied().chain([one_more])),
            Err(ListError::TooMany)
        );
        let mut over = list.encode();
        over.truncate(over.len() - "end\n".len());
        over.push_str(&format!("{one_more}\nend\n"));
        assert_eq!(Allowlist::parse(over.as_bytes()), Err(ListError::TooMany));

        // The longest list of the longest entries fits the byte limit exactly.
        let longest: Vec<Destination> = (0..MAX_ENTRIES)
            .map(|index| {
                let index = u16::try_from(index).unwrap();
                let address = Ipv6Addr::new(
                    0xfd00,
                    0xffff,
                    0xffff,
                    0xffff,
                    0xffff,
                    0xffff,
                    0xffff,
                    0xf000 + index,
                );
                Destination::new(IpAddr::V6(address), 65535).unwrap()
            })
            .collect();
        let encoded = Allowlist::new(longest).unwrap().encode();
        assert_eq!(encoded.len(), MAX_REQUEST_BYTES);
        assert!(Allowlist::parse(encoded.as_bytes()).is_ok());
        let mut larger = encoded.into_bytes();
        larger.push(b'\n');
        assert_eq!(Allowlist::parse(&larger), Err(ListError::TooLarge));
    }

    #[test]
    fn the_script_names_only_the_two_sets_and_writes_the_parsed_values() {
        let list = Allowlist::parse(request(&["10.0.0.5 502", "fd00:: 8886"]).as_bytes()).unwrap();
        assert_eq!(
            list.nft_script(),
            "flush set inet trustiot_agent_allow fieldbus4\n\
             flush set inet trustiot_agent_allow fieldbus6\n\
             add element inet trustiot_agent_allow fieldbus4 { 10.0.0.5 . 502 }\n\
             add element inet trustiot_agent_allow fieldbus6 { fd00:: . 8886 }\n"
        );
        // An empty list only flushes.
        assert_eq!(
            Allowlist::default().nft_script(),
            "flush set inet trustiot_agent_allow fieldbus4\n\
             flush set inet trustiot_agent_allow fieldbus6\n"
        );
        // The declarations, which the default deny's file uses and the script never does.
        assert_eq!(
            Allowlist::declarations(),
            "add table inet trustiot_agent_allow\n\
             add set inet trustiot_agent_allow fieldbus4 { type ipv4_addr . inet_service; size 4096; }\n\
             add set inet trustiot_agent_allow fieldbus6 { type ipv6_addr . inet_service; size 4096; }\n"
        );
        // One digest per list, whatever order the destinations came in.
        let mut destinations: Vec<Destination> = list.iter().copied().collect();
        destinations.reverse();
        let reversed = Allowlist::new(destinations).unwrap();
        assert_eq!(reversed.digest(), list.digest());
        assert_eq!(list.digest().len(), 64);
        assert_ne!(Allowlist::default().digest(), list.digest());
        // A long list is spread over several commands of the one file.
        let many: Vec<Destination> = (0..600_u32)
            .map(|index| {
                Destination::new(IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + index)), 502).unwrap()
            })
            .collect();
        let script = Allowlist::new(many).unwrap().nft_script();
        assert_eq!(script.matches("add element").count(), 3);
        assert_eq!(script.matches(" . 502").count(), 600);
        // Every line of any script is one of these shapes.
        for line in script.lines() {
            assert!(
                line.starts_with("flush set inet trustiot_agent_allow fieldbus")
                    || line.starts_with("add element inet trustiot_agent_allow fieldbus4 { 10."),
                "{line}"
            );
        }
    }

    #[test]
    fn the_reply_is_one_line_both_ways() {
        for (reply, line) in [
            (Reply::Applied(3), "ok 3"),
            (Reply::Applied(0), "ok 0"),
            (
                Reply::Refused("line 2: listed twice".into()),
                "refused: line 2: listed twice",
            ),
            (
                Reply::Failed("nft exited with status 1".into()),
                "failed: nft exited with status 1",
            ),
        ] {
            assert_eq!(reply.to_string(), line);
            assert_eq!(Reply::parse(line), Some(reply));
        }
        assert_eq!(
            Reply::Failed("two\nlines".into()).to_string(),
            "failed: two lines"
        );
        for line in [
            "",
            "ok",
            "ok -1",
            "ok 03",
            "ok 3 more",
            "OK 3",
            "refused",
            "done",
        ] {
            assert_eq!(Reply::parse(line), None, "{line:?}");
        }
    }
}
