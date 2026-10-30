//! One connection per endpoint, shared by the devices behind it (rebuild checklist F2).
//!
//! An RS485 converter sends every device's response to every TCP connection
//! (Laysi/trustiot#183): with a connection per device, responses landed on the wrong device and
//! requests collided on the bus. So every device at the same endpoint (transport, host and
//! port) reads through one [`SharedConnection`], one request at a time, and a response arrives
//! while only the request it answers waits; the framing discards what other units send
//! ([`super::tcp`]). [`Endpoints`] hands out these connections.
//!
//! Devices take turns request by request, not poll by poll. Each read waits for the
//! connection, sends its request, and gives the connection up when its answer, its failure or
//! its deadline comes. The wait is first come, first served (a fair [`tokio::sync::Mutex`]), so
//! devices polled at the same time read in the order A1 B1 C1 A2 B2 C2, and a device that stops
//! answering holds the others up by one request timeout per request, not by its whole poll. A
//! request's timeout starts when it is sent, not while it waits for its turn. Dropping a read
//! that waits for its turn gives the turn up.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use protocol::config::{Device, Transport};
use tokio::sync::Mutex;

use super::address::AddressGuard;
use super::pdu::Read;
use super::tcp::TcpConnection;
use super::{Connection, Exchange};
use crate::polling::Disconnect;

/// A connection several devices' readers use in turn. A clone is another handle to the same
/// connection; each device's poll reads through its own handle.
#[derive(Debug)]
pub struct SharedConnection<C = TcpConnection> {
    connection: Arc<Mutex<C>>,
}

impl<C> Clone for SharedConnection<C> {
    fn clone(&self) -> Self {
        Self {
            connection: Arc::clone(&self.connection),
        }
    }
}

impl<C> SharedConnection<C> {
    /// Shares `connection`.
    pub fn new(connection: C) -> Self {
        Self {
            connection: Arc::new(Mutex::new(connection)),
        }
    }

    /// Whether `self` and `other` are handles to the same connection.
    pub fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.connection, &other.connection)
    }
}

impl<C: Connection + Send> Connection for SharedConnection<C> {
    /// Waits for the turn, then reads as the shared connection does; the turn ends with the
    /// read. `timeout` starts once the turn has come.
    async fn read(&mut self, unit: u8, read: Read, timeout: Duration) -> Exchange {
        self.connection.lock().await.read(unit, read, timeout).await
    }
}

impl<C: Disconnect + Send> Disconnect for SharedConnection<C> {
    /// Waits for the turn, then closes the shared connection, for every device at the endpoint:
    /// their next reads open a new one. The turn comes within the read of every device ahead,
    /// each bounded by its timeouts and its own watchdog.
    async fn disconnect(&mut self) {
        self.connection.lock().await.disconnect().await;
    }
}

/// The shared connection of every endpoint in use: one per transport, host and port.
///
/// Modbus TCP devices at the same host and port share one too: a Modbus TCP gateway in front of
/// a serial bus has the same single bus, and may accept only a few connections. The transport
/// is part of the key, so that a device is never read with another device's framing, even in a
/// configuration that did not pass validation (which refuses two transports at one endpoint).
#[derive(Debug)]
pub struct Endpoints {
    connect_timeout: Duration,
    /// The address guard of every connection it creates (`address.rs`).
    guard: AddressGuard,
    connections: HashMap<(Transport, String, u16), SharedConnection>,
}

impl Endpoints {
    /// No endpoints yet; connections it creates wait at most `connect_timeout` to connect, and
    /// connect only to addresses the address guard allows ([`TcpConnection`]).
    pub fn new(connect_timeout: Duration) -> Self {
        Self {
            connect_timeout,
            guard: AddressGuard::default(),
            connections: HashMap::new(),
        }
    }

    /// Test code only: lets the connections it creates from now on reach `addresses` although
    /// the address guard refuses them, as [`TcpConnection::exempt_for_tests`] does. It exists
    /// only in this crate's unit tests and with the `test-exempt-addresses` feature, which only
    /// dev-dependencies enable (`Cargo.toml`).
    #[cfg(any(test, feature = "test-exempt-addresses"))]
    #[must_use]
    pub fn exempt_for_tests(
        mut self,
        addresses: impl IntoIterator<Item = std::net::IpAddr>,
    ) -> Self {
        self.guard.exempt(addresses);
        self
    }

    /// The connection `device` reads through: the same one for every device with the same
    /// transport, host and port, created on first use. Hosts are compared and connected to in
    /// their canonical form ([`protocol::config::Host::canonical`]), so `::ffff:192.0.2.10`
    /// and `192.0.2.10`, or DNS names that differ only in case, are one endpoint. Nothing is
    /// opened until the first read.
    pub fn connection(&mut self, device: &Device) -> SharedConnection {
        let host = device.host.canonical();
        self.connections
            .entry((device.transport, host.clone(), device.port))
            .or_insert_with(|| {
                SharedConnection::new(TcpConnection::guarded(
                    device.transport,
                    host,
                    device.port,
                    self.connect_timeout,
                    self.guard.clone(),
                ))
            })
            .clone()
    }

    /// Forgets the connection of every endpoint that no device reads through any more (no
    /// handle from [`Endpoints::connection`] is left), which closes it, and returns how many it
    /// forgot. A device added later at such an endpoint gets a new
    /// connection. Endpoints still in use keep theirs, so their devices are not reconnected.
    pub fn prune(&mut self) -> usize {
        let before = self.connections.len();
        self.connections
            .retain(|_, connection| Arc::strong_count(&connection.connection) > 1);
        before - self.connections.len()
    }

    /// How many endpoints have a connection.
    pub fn len(&self) -> usize {
        self.connections.len()
    }

    /// Whether no endpoint has a connection yet.
    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;

    use protocol::config::{FunctionCode, Host};
    use protocol::{DeviceId, ModelName};
    use testkit::clock::bounded;
    use tokio::sync::oneshot;

    use super::*;
    use crate::modbus::ReadError;

    const TIMEOUT: Duration = Duration::from_secs(5);

    /// Answers every read with a timeout after yielding a few times, so that reads on other
    /// handles get to run in between, and records each read when it ends. A read waits for
    /// `gate` first, while one is set.
    #[derive(Default)]
    struct Recorder {
        gate: Option<oneshot::Receiver<()>>,
        done: Vec<(u8, u16)>,
    }

    impl Connection for Recorder {
        async fn read(&mut self, unit: u8, read: Read, _timeout: Duration) -> Exchange {
            if let Some(gate) = self.gate.take() {
                gate.await.unwrap();
            }
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            self.done.push((unit, read.start()));
            Exchange {
                result: Err(ReadError::Timeout),
                unit_mismatches: 0,
            }
        }
    }

    fn holding(start: u16) -> Read {
        Read::new(FunctionCode::ReadHoldingRegisters, start, 1).unwrap()
    }

    /// Reads holding registers 0 and 1 of `unit`, one after the other.
    async fn two_reads(mut connection: SharedConnection<Recorder>, unit: u8) {
        connection.read(unit, holding(0), TIMEOUT).await;
        connection.read(unit, holding(1), TIMEOUT).await;
    }

    #[tokio::test]
    async fn devices_take_turns_request_by_request() {
        let shared = SharedConnection::new(Recorder::default());
        // `biased` polls the three in order, so unit 1 takes the connection first and units 2
        // and 3 queue behind it in that order.
        bounded("three devices' reads", async {
            tokio::join!(
                biased;
                two_reads(shared.clone(), 1),
                two_reads(shared.clone(), 2),
                two_reads(shared.clone(), 3),
            )
        })
        .await;
        assert_eq!(
            shared.connection.lock().await.done,
            [(1, 0), (2, 0), (3, 0), (1, 1), (2, 1), (3, 1)]
        );
    }

    #[tokio::test]
    async fn a_read_dropped_while_waiting_for_its_turn_gives_it_up() {
        let (open, gate) = oneshot::channel();
        let shared = SharedConnection::new(Recorder {
            gate: Some(gate),
            done: Vec::new(),
        });
        let (mut a, mut b, mut c) = (shared.clone(), shared.clone(), shared.clone());
        let mut first = pin!(a.read(1, holding(0), TIMEOUT));
        let mut second = Box::pin(b.read(2, holding(0), TIMEOUT));
        // The first read takes the connection and waits at the gate; the second queues for its
        // turn and is dropped while it waits.
        tokio::select! {
            biased;
            _ = &mut first => panic!("the first read ended before the gate opened"),
            _ = &mut second => panic!("the second read ended before its turn"),
            () = std::future::ready(()) => {}
        }
        drop(second);
        open.send(()).unwrap();
        bounded("the first read and the one after the dropped read", async {
            tokio::join!(first, c.read(3, holding(0), TIMEOUT))
        })
        .await;
        assert_eq!(shared.connection.lock().await.done, [(1, 0), (3, 0)]);
    }

    fn device(transport: Transport, host: &str, port: u16) -> Device {
        Device {
            id: DeviceId::new("meter-1").unwrap(),
            model: ModelName::new("meter").unwrap(),
            transport,
            host: Host::new(host).unwrap(),
            port,
            unit_id: 1,
            poll_interval_ms: 1000,
        }
    }

    #[test]
    fn devices_at_one_endpoint_share_its_connection() {
        let mut endpoints = Endpoints::new(Duration::from_secs(10));
        assert!(endpoints.is_empty());
        let rtu = Transport::RtuOverTcp;
        let converter = endpoints.connection(&device(rtu, "192.0.2.10", 502));
        let mapped = endpoints.connection(&device(rtu, "::ffff:192.0.2.10", 502));
        assert!(
            converter.same(&mapped),
            "an IPv4-mapped address is the same host"
        );
        assert!(converter.same(&converter.clone()));
        let named = endpoints.connection(&device(rtu, "Converter.example", 4196));
        let lower = endpoints.connection(&device(rtu, "converter.example", 4196));
        assert!(named.same(&lower), "DNS names compare without case");
        assert_eq!(endpoints.len(), 2);

        let other_port = endpoints.connection(&device(rtu, "192.0.2.10", 503));
        assert!(!converter.same(&other_port));
        let other_transport =
            endpoints.connection(&device(Transport::ModbusTcp, "192.0.2.10", 502));
        assert!(!converter.same(&other_transport));
        assert!(!named.same(&converter));
        assert_eq!(endpoints.len(), 4);

        let transport = |shared: &SharedConnection| {
            shared
                .connection
                .try_lock()
                .expect("nobody reads")
                .transport()
        };
        assert_eq!(transport(&converter), rtu);
        assert_eq!(transport(&other_transport), Transport::ModbusTcp);
    }

    #[test]
    fn prune_forgets_only_endpoints_nobody_reads_through() {
        let mut endpoints = Endpoints::new(Duration::from_secs(10));
        let rtu = Transport::RtuOverTcp;
        let kept = endpoints.connection(&device(rtu, "192.0.2.10", 502));
        let sibling = endpoints.connection(&device(rtu, "192.0.2.10", 502));
        drop(endpoints.connection(&device(rtu, "192.0.2.11", 502)));
        assert_eq!(endpoints.len(), 2);
        assert_eq!(endpoints.prune(), 1);
        assert_eq!(endpoints.len(), 1);
        drop(sibling);
        assert_eq!(endpoints.prune(), 0, "one handle is still in use");
        assert!(
            endpoints
                .connection(&device(rtu, "192.0.2.10", 502))
                .same(&kept),
            "the endpoint in use keeps its connection"
        );
        drop(kept);
        assert_eq!(endpoints.prune(), 1);
        assert!(endpoints.is_empty());
    }
}
