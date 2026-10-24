//! Which addresses a Modbus connection may go to (Laysi/trustiot-v2#653).
//!
//! A tenant's editor sets each device's host in the cloud. The cloud's device API refuses hosts
//! that name the gateway itself literally, but it cannot know what a DNS name resolves to on the
//! site's network, or what it will resolve to later (DNS rebinding). Polling such a host would
//! send Modbus requests to the gateway's own ports and return what answers as telemetry: a way
//! to probe the gateway's local services. So the gateway checks at the moment it connects:
//! [`AddressGuard::connect`] resolves the host once, refuses the addresses [`refusal`] lists,
//! and connects to a checked [`SocketAddr`], never to the name again (a second resolution could
//! return another address). An IP address in the host goes through the same check.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::net::TcpStream;

use super::ReadError;

/// The cloud metadata service's IPv4 address (AWS, Google Cloud, Azure and others).
const METADATA_V4: Ipv4Addr = Ipv4Addr::new(169, 254, 169, 254);
/// The cloud metadata service's IPv6 address (AWS EC2).
const METADATA_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254);
/// Most resolved addresses a refusal lists.
const MAX_LISTED: usize = 8;

/// Why an address is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// 127.0.0.0/8 or `::1`.
    Loopback,
    /// 0.0.0.0/8 (`0.0.0.0` and the rest of "this network", RFC 1122) or `::`: a connection to
    /// it reaches the gateway itself.
    Unspecified,
    /// An address on one of the gateway's own interfaces.
    Gateway,
    /// 224.0.0.0/4 or `ff00::/8`.
    Multicast,
    /// `255.255.255.255` or the broadcast address of one of the gateway's IPv4 subnets.
    Broadcast,
    /// A cloud metadata service: `169.254.169.254` or `fd00:ec2::254`.
    Metadata,
    /// `::/96` other than `::` and `::1`: IPv4-compatible IPv6 addresses, deprecated (RFC 4291
    /// section 2.5.5.1), which no field device uses.
    Ipv4Compatible,
}

/// Why `address` must not be connected to, or `None` when it may be.
///
/// An IPv4-mapped IPv6 address (`::ffff:0:0/96`) is checked as the IPv4 address it maps. The
/// IPv6 forms that carry an IPv4 address a translator or relay may forward to are refused
/// whenever that IPv4 address is: NAT64 `64:ff9b::/96` and local-use NAT64 `64:ff9b:1::/48`
/// (RFC 8215) by their last 32 bits, the /96 layout inside the /48 (other prefix lengths in it
/// are not decoded), and 6to4 `2002::/16` by bits 16–47. `::/96` (IPv4-compatible) is refused
/// whole.
///
/// Everything else is allowed: the private ranges (10/8, 172.16/12, 192.168/16, IPv6 ULA) are
/// where field devices are, and 169.254/16 (link-local) other than the metadata address is
/// what an RS485 converter without DHCP often takes.
pub(crate) fn refusal(address: IpAddr, interfaces: &Interfaces) -> Option<Refusal> {
    let v6 = match address.to_canonical() {
        IpAddr::V4(v4) => return refusal_v4(v4, interfaces),
        IpAddr::V6(v6) => v6,
    };
    let segments = v6.segments();
    let refusal = if v6 == METADATA_V6 {
        Refusal::Metadata
    } else if v6.is_loopback() {
        Refusal::Loopback
    } else if v6.is_unspecified() {
        Refusal::Unspecified
    } else if segments[..6] == [0; 6] {
        Refusal::Ipv4Compatible
    } else if v6.is_multicast() {
        Refusal::Multicast
    } else if let Some(refusal) = embedded_v4(&segments).and_then(|v4| refusal_v4(v4, interfaces)) {
        refusal
    } else if interfaces.addresses.contains(&IpAddr::V6(v6)) {
        Refusal::Gateway
    } else {
        return None;
    };
    Some(refusal)
}

/// Why the IPv4 address `v4` must not be connected to, or `None` when it may be.
fn refusal_v4(v4: Ipv4Addr, interfaces: &Interfaces) -> Option<Refusal> {
    let refusal = if v4.is_loopback() {
        Refusal::Loopback
    } else if v4.octets()[0] == 0 {
        Refusal::Unspecified
    } else if v4.is_multicast() {
        Refusal::Multicast
    } else if v4.is_broadcast() || interfaces.broadcasts.contains(&v4) {
        Refusal::Broadcast
    } else if v4 == METADATA_V4 {
        Refusal::Metadata
    } else if interfaces.addresses.contains(&IpAddr::V4(v4)) {
        Refusal::Gateway
    } else {
        return None;
    };
    Some(refusal)
}

/// The IPv4 address an IPv6 address of a translation or relay prefix carries: NAT64
/// `64:ff9b::/96` and the /96 layout in local-use NAT64 `64:ff9b:1::/48` in their last 32
/// bits, 6to4 `2002::/16` in bits 16–47.
fn embedded_v4(segments: &[u16; 8]) -> Option<Ipv4Addr> {
    let from = |high: u16, low: u16| Ipv4Addr::from((u32::from(high) << 16) | u32::from(low));
    match segments {
        [0x0064, 0xff9b, 0, 0, 0, 0, high, low] | [0x0064, 0xff9b, 0x0001, _, _, _, high, low] => {
            Some(from(*high, *low))
        }
        [0x2002, high, low, ..] => Some(from(*high, *low)),
        _ => None,
    }
}

/// The gateway's own addresses, read when a connection is opened: DHCP may change them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Interfaces {
    /// Every address on one of the gateway's interfaces, in canonical form.
    addresses: Vec<IpAddr>,
    /// The broadcast address of every IPv4 subnet on one of them; none for a /31 (RFC 3021,
    /// its two addresses are both hosts) or a /32.
    broadcasts: Vec<Ipv4Addr>,
}

impl Interfaces {
    /// The addresses on the gateway's interfaces now, up or down.
    pub(crate) fn read() -> io::Result<Self> {
        let mut interfaces = Self::default();
        for (address, netmask) in sys::interface_addresses()? {
            interfaces.add(address, netmask);
        }
        Ok(interfaces)
    }

    /// Adds an interface address and its netmask.
    pub(crate) fn add(&mut self, address: IpAddr, netmask: Option<IpAddr>) {
        let address = address.to_canonical();
        if let (IpAddr::V4(v4), Some(IpAddr::V4(mask))) = (address, netmask) {
            let mask = u32::from(mask);
            if mask.leading_ones() < 31 {
                let broadcast = Ipv4Addr::from(u32::from(v4) | !mask);
                if !self.broadcasts.contains(&broadcast) {
                    self.broadcasts.push(broadcast);
                }
            }
        }
        if !self.addresses.contains(&address) {
            self.addresses.push(address);
        }
    }

    /// Every address on one of the gateway's interfaces.
    #[cfg(test)]
    pub(crate) fn addresses(&self) -> &[IpAddr] {
        &self.addresses
    }

    /// The broadcast address of every IPv4 subnet on one of them.
    #[cfg(test)]
    pub(crate) fn broadcasts(&self) -> &[Ipv4Addr] {
        &self.broadcasts
    }
}

/// Resolves a host and connects only to addresses that [`refusal`] allows.
///
/// The agent's own configuration has no way to change what it refuses. Tests that run the fake
/// Modbus endpoint on 127.0.0.1 exempt that address through `exempt_for_tests` on
/// [`super::tcp::TcpConnection`] or [`super::endpoint::Endpoints`], which exist only in this
/// crate's unit tests and with the `test-exempt-addresses` feature that only this crate's
/// dev-dependency on itself enables (`Cargo.toml`).
#[derive(Clone, Debug, Default)]
pub(crate) struct AddressGuard {
    /// Addresses a test may reach although they are refused, in canonical form.
    #[cfg(any(test, feature = "test-exempt-addresses"))]
    exempt: Vec<IpAddr>,
    /// A unit test's resolver and record of the addresses dialed.
    #[cfg(test)]
    hooks: Option<std::sync::Arc<hooks::Hooks>>,
}

impl AddressGuard {
    /// Lets a test reach `addresses` although they are refused.
    #[cfg(any(test, feature = "test-exempt-addresses"))]
    pub(crate) fn exempt(&mut self, addresses: impl IntoIterator<Item = IpAddr>) {
        self.exempt
            .extend(addresses.into_iter().map(|address| address.to_canonical()));
    }

    /// Whether a test exempted `address`.
    #[cfg(any(test, feature = "test-exempt-addresses"))]
    fn exempts(&self, address: IpAddr) -> bool {
        self.exempt.contains(&address.to_canonical())
    }

    /// Whether a test exempted `address`: never, outside tests.
    #[cfg(not(any(test, feature = "test-exempt-addresses")))]
    fn exempts(&self, _address: IpAddr) -> bool {
        false
    }

    /// Resolves `host` (an IP address or a DNS name) once and connects to the first of its
    /// addresses that is allowed and accepts, in the resolver's order. Fails with
    /// [`ReadError::Connect`] without opening a socket when every address is refused, or when
    /// the gateway's interface addresses cannot be read (then nothing is allowed). The caller
    /// bounds the time.
    pub(crate) async fn connect(&self, host: &str, port: u16) -> Result<TcpStream, ReadError> {
        let resolved = self
            .resolve(host, port)
            .await
            .map_err(|error| ReadError::Connect(format!("resolving the host failed: {error}")))?;
        if resolved.is_empty() {
            return Err(ReadError::Connect(
                "the host resolved to no address".to_owned(),
            ));
        }
        let interfaces = Interfaces::read().map_err(|error| {
            ReadError::Connect(format!(
                "reading the gateway's interface addresses failed, so no address is allowed: \
                 {error}"
            ))
        })?;
        let allowed: Vec<SocketAddr> = resolved
            .iter()
            .map(|address| canonical(*address))
            .filter(|address| {
                self.exempts(address.ip()) || refusal(address.ip(), &interfaces).is_none()
            })
            .collect();
        if allowed.is_empty() {
            return Err(ReadError::Connect(refused_detail(&resolved)));
        }
        let mut last = None;
        for address in allowed {
            match self.dial(address).await {
                Ok(stream) => return Ok(stream),
                Err(error) => last = Some(error),
            }
        }
        Err(ReadError::Connect(last.map_or_else(
            || "no address accepted".to_owned(),
            |error| error.to_string(),
        )))
    }

    /// Every address `host` resolves to: one lookup, which an IP address answers itself.
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        #[cfg(test)]
        if let Some(hooks) = &self.hooks {
            return hooks.resolve(host, port);
        }
        Ok(tokio::net::lookup_host((host, port)).await?.collect())
    }

    /// Opens a TCP connection to `address`, an address that was checked.
    async fn dial(&self, address: SocketAddr) -> io::Result<TcpStream> {
        #[cfg(test)]
        if let Some(hooks) = &self.hooks {
            return hooks.dial(address).await;
        }
        TcpStream::connect(address).await
    }

    /// Resolves and dials through `hooks` instead of the system.
    #[cfg(test)]
    pub(crate) fn with_hooks(mut self, hooks: std::sync::Arc<hooks::Hooks>) -> Self {
        self.hooks = Some(hooks);
        self
    }
}

/// `address` with an IPv4-mapped IPv6 address as the IPv4 address it maps, so the address
/// connected to is the one checked.
fn canonical(address: SocketAddr) -> SocketAddr {
    match address {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), v6.port()),
            None => address,
        },
        SocketAddr::V4(_) => address,
    }
}

/// What a refused connection reports: the rule and the resolved addresses, nothing else (the
/// detail reaches the cloud in the gateway's health report).
fn refused_detail(resolved: &[SocketAddr]) -> String {
    let mut listed: Vec<String> = resolved
        .iter()
        .take(MAX_LISTED)
        .map(|address| address.ip().to_string())
        .collect();
    if resolved.len() > MAX_LISTED {
        listed.push(format!("and {} more", resolved.len() - MAX_LISTED));
    }
    format!(
        "refused: every address the host resolved to points at the gateway itself or is not \
         allowed ({})",
        listed.join(", ")
    )
}

/// Reading the interface addresses from the operating system.
mod sys {
    use std::io;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    /// Every IPv4 and IPv6 address on an interface, with its netmask when there is one.
    #[allow(
        unsafe_code,
        reason = "getifaddrs has no safe wrapper in std or in the crates the workspace locks"
    )]
    pub(super) fn interface_addresses() -> io::Result<Vec<(IpAddr, Option<IpAddr>)>> {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: `list` is a valid place for the pointer to the list getifaddrs allocates.
        if unsafe { libc::getifaddrs(&raw mut list) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut addresses = Vec::new();
        let mut entry = list;
        while !entry.is_null() {
            // SAFETY: `entry` is a non-null node of the list getifaddrs returned, which stays
            // allocated until freeifaddrs below.
            let node = unsafe { &*entry };
            // SAFETY: getifaddrs sets each of these to null or to a sockaddr of the family it
            // names, valid until freeifaddrs.
            if let Some(address) = unsafe { ip(node.ifa_addr) } {
                // SAFETY: as above.
                let netmask = unsafe { ip(node.ifa_netmask) };
                addresses.push((address, netmask));
            }
            entry = node.ifa_next;
        }
        // SAFETY: `list` came from a successful getifaddrs and is freed once; nothing read from
        // it is used afterwards (the addresses were copied).
        unsafe { libc::freeifaddrs(list) };
        Ok(addresses)
    }

    /// The IP address in `address`, or `None` for a null pointer or another family (such as
    /// AF_PACKET).
    ///
    /// # Safety
    ///
    /// `address` is null or points to a sockaddr whose `sa_family` says what follows it.
    #[allow(
        unsafe_code,
        reason = "reads a sockaddr that getifaddrs filled in, by its family"
    )]
    unsafe fn ip(address: *const libc::sockaddr) -> Option<IpAddr> {
        if address.is_null() {
            return None;
        }
        // SAFETY: the caller guarantees a valid sockaddr; read unaligned to assume nothing.
        let family = i32::from(unsafe { std::ptr::read_unaligned(address) }.sa_family);
        match family {
            libc::AF_INET => {
                // SAFETY: an AF_INET sockaddr is a sockaddr_in.
                let v4 = unsafe { std::ptr::read_unaligned(address.cast::<libc::sockaddr_in>()) };
                // `s_addr` is in network byte order, so its bytes in memory are the octets.
                Some(IpAddr::V4(Ipv4Addr::from(v4.sin_addr.s_addr.to_ne_bytes())))
            }
            libc::AF_INET6 => {
                // SAFETY: an AF_INET6 sockaddr is a sockaddr_in6.
                let v6 = unsafe { std::ptr::read_unaligned(address.cast::<libc::sockaddr_in6>()) };
                Some(IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.s6_addr)))
            }
            _ => None,
        }
    }
}

/// A unit test's stand-ins for the system resolver and for dialing.
#[cfg(test)]
pub(crate) mod hooks {
    use std::io;
    use std::net::SocketAddr;
    use std::sync::Mutex;

    use tokio::net::TcpStream;

    type Resolver = dyn Fn(&str, u16) -> io::Result<Vec<SocketAddr>> + Send + Sync;

    /// Answers every lookup with `resolver` and records every address dialed. Only loopback
    /// addresses are really dialed; any other fails without a packet, so tests never reach the
    /// network of the machine they run on.
    pub(crate) struct Hooks {
        resolver: Box<Resolver>,
        lookups: Mutex<Vec<String>>,
        dialed: Mutex<Vec<SocketAddr>>,
    }

    impl std::fmt::Debug for Hooks {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("Hooks")
                .field("lookups", &self.lookups)
                .field("dialed", &self.dialed)
                .finish_non_exhaustive()
        }
    }

    impl Hooks {
        pub(crate) fn new(
            resolver: impl Fn(&str, u16) -> io::Result<Vec<SocketAddr>> + Send + Sync + 'static,
        ) -> Self {
            Self {
                resolver: Box::new(resolver),
                lookups: Mutex::new(Vec::new()),
                dialed: Mutex::new(Vec::new()),
            }
        }

        /// The hosts looked up, in order.
        pub(crate) fn lookups(&self) -> Vec<String> {
            self.lookups.lock().unwrap().clone()
        }

        /// The addresses dialed, in order.
        pub(crate) fn dialed(&self) -> Vec<SocketAddr> {
            self.dialed.lock().unwrap().clone()
        }

        pub(super) fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
            self.lookups.lock().unwrap().push(host.to_owned());
            (self.resolver)(host, port)
        }

        pub(super) async fn dial(&self, address: SocketAddr) -> io::Result<TcpStream> {
            self.dialed.lock().unwrap().push(address);
            if address.ip().is_loopback() {
                TcpStream::connect(address).await
            } else {
                Err(io::Error::new(
                    io::ErrorKind::HostUnreachable,
                    format!("{address} is not dialed in tests"),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    /// A gateway with 192.168.10.5/24 on its LAN, 10.0.0.1/31 on a point-to-point link,
    /// 172.20.0.9/32, and fd12:3456::5 and a link-local address on IPv6.
    fn site() -> Interfaces {
        let mut interfaces = Interfaces::default();
        interfaces.add(ip("127.0.0.1"), Some(ip("255.0.0.0")));
        interfaces.add(ip("192.168.10.5"), Some(ip("255.255.255.0")));
        interfaces.add(ip("10.0.0.1"), Some(ip("255.255.255.254")));
        interfaces.add(ip("172.20.0.9"), Some(ip("255.255.255.255")));
        interfaces.add(
            ip("::1"),
            Some(ip("ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff")),
        );
        interfaces.add(ip("fd12:3456::5"), Some(ip("ffff:ffff:ffff:ffff::")));
        interfaces.add(ip("fe80::5"), Some(ip("ffff:ffff:ffff:ffff::")));
        interfaces.add(ip("192.168.10.5"), Some(ip("255.255.255.0")));
        interfaces
    }

    #[test]
    fn interfaces_hold_their_addresses_and_subnet_broadcasts() {
        let interfaces = site();
        assert_eq!(
            interfaces.addresses(),
            [
                ip("127.0.0.1"),
                ip("192.168.10.5"),
                ip("10.0.0.1"),
                ip("172.20.0.9"),
                ip("::1"),
                ip("fd12:3456::5"),
                ip("fe80::5"),
            ]
        );
        // None for the /31, whose other address is the peer, nor for the /32.
        assert_eq!(
            interfaces.broadcasts(),
            [
                Ipv4Addr::new(127, 255, 255, 255),
                Ipv4Addr::new(192, 168, 10, 255)
            ]
        );
    }

    #[test]
    fn refused_addresses() {
        let interfaces = site();
        let cases = [
            ("127.0.0.1", Refusal::Loopback),
            ("127.1.2.3", Refusal::Loopback),
            ("::1", Refusal::Loopback),
            ("::ffff:127.0.0.1", Refusal::Loopback),
            ("0.0.0.0", Refusal::Unspecified),
            ("0.1.2.3", Refusal::Unspecified),
            ("::", Refusal::Unspecified),
            ("::ffff:0.0.0.0", Refusal::Unspecified),
            ("224.0.0.1", Refusal::Multicast),
            ("239.255.255.250", Refusal::Multicast),
            ("ff02::1", Refusal::Multicast),
            ("ff05::1:3", Refusal::Multicast),
            ("255.255.255.255", Refusal::Broadcast),
            ("192.168.10.255", Refusal::Broadcast),
            ("::ffff:192.168.10.255", Refusal::Broadcast),
            ("169.254.169.254", Refusal::Metadata),
            ("::ffff:169.254.169.254", Refusal::Metadata),
            ("fd00:ec2::254", Refusal::Metadata),
            ("192.168.10.5", Refusal::Gateway),
            ("::ffff:192.168.10.5", Refusal::Gateway),
            ("10.0.0.1", Refusal::Gateway),
            ("172.20.0.9", Refusal::Gateway),
            ("fd12:3456::5", Refusal::Gateway),
            ("fe80::5", Refusal::Gateway),
            // IPv6 forms that carry an IPv4 address.
            ("::127.0.0.1", Refusal::Ipv4Compatible),
            ("::192.168.10.6", Refusal::Ipv4Compatible),
            ("64:ff9b::127.0.0.1", Refusal::Loopback),
            ("64:ff9b::169.254.169.254", Refusal::Metadata),
            ("64:ff9b::192.168.10.5", Refusal::Gateway),
            ("64:ff9b::192.168.10.255", Refusal::Broadcast),
            ("64:ff9b:1:abcd::127.0.0.1", Refusal::Loopback),
            ("2002:7f00:1::", Refusal::Loopback),
            ("2002:a9fe:a9fe::1", Refusal::Metadata),
            ("2002:e000:1::", Refusal::Multicast),
            ("2002::1", Refusal::Unspecified),
        ];
        for (address, expected) in cases {
            assert_eq!(
                refusal(ip(address), &interfaces),
                Some(expected),
                "{address}"
            );
        }
    }

    #[test]
    fn field_networks_are_allowed() {
        let interfaces = site();
        for address in [
            // Link-local without DHCP (APIPA), other than the metadata address.
            "169.254.1.20",
            "169.254.169.253",
            "169.254.169.255",
            "::ffff:169.254.1.20",
            // The private ranges, the gateway's own subnet included.
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.10.6",
            "192.168.10.0",
            "192.168.1.255",
            "fd12:3456::6",
            "fd00:ec2::253",
            "fe80::6",
            // The peer of the /31.
            "10.0.0.0",
            // Public addresses (a device behind a router or VPN).
            "192.0.2.10",
            "2001:db8::10",
            // IPv6 forms that carry an allowed IPv4 address.
            "64:ff9b::192.168.10.6",
            "64:ff9b:1::10.1.2.3",
            "2002:c000:20a::1",
        ] {
            assert_eq!(refusal(ip(address), &interfaces), None, "{address}");
        }
    }

    #[test]
    fn the_gateways_interfaces_can_be_read() {
        let interfaces = Interfaces::read().unwrap();
        // Every Linux network namespace has a loopback interface.
        assert!(
            interfaces.addresses().contains(&ip("127.0.0.1")),
            "{interfaces:?}"
        );
    }

    #[test]
    fn a_refusal_lists_only_the_resolved_addresses() {
        let resolved: Vec<SocketAddr> = (1..=10)
            .map(|last| SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, last)), 502))
            .collect();
        assert_eq!(
            refused_detail(&resolved[..2]),
            "refused: every address the host resolved to points at the gateway itself or is \
             not allowed (127.0.0.1, 127.0.0.2)"
        );
        assert!(
            refused_detail(&resolved).ends_with("127.0.0.8, and 2 more)"),
            "{}",
            refused_detail(&resolved)
        );
    }

    #[test]
    fn an_exempt_address_is_canonical() {
        let mut guard = AddressGuard::default();
        guard.exempt([ip("::ffff:127.0.0.1")]);
        assert!(guard.exempts(ip("127.0.0.1")));
        assert!(guard.exempts(ip("::ffff:127.0.0.1")));
        assert!(!guard.exempts(ip("127.0.0.2")));
        assert!(!AddressGuard::default().exempts(ip("127.0.0.1")));
    }

    #[test]
    fn a_mapped_address_is_connected_to_as_ipv4() {
        let mapped: SocketAddr = "[::ffff:192.168.10.6]:502".parse().unwrap();
        assert_eq!(canonical(mapped), "192.168.10.6:502".parse().unwrap());
        let v6: SocketAddr = "[fd12:3456::6]:502".parse().unwrap();
        assert_eq!(canonical(v6), v6);
    }
}
