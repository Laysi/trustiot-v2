//! Loopback addresses for tests.
//!
//! [`ClosedPort`] is the address of "nobody listens here". The tempting way to get one, binding
//! `127.0.0.1:0`, reading the port and dropping the listener, hands the port back to the
//! system: before the code under test connects, a listener of another test in the same binary
//! (or of the service under test) can be given that very port, and the connection then succeeds
//! when the test expects a refusal.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Mutex;

use socket2::{Domain, Socket, Type};

/// A loopback port that refuses every connection for as long as this value lives, and that no
/// other socket can bind meanwhile.
///
/// It is a socket that is bound but never put into the listening state: on Linux the kernel
/// answers a connection attempt to it with a reset (`ConnectionRefused`), and the port stays
/// taken. The socket does not set `SO_REUSEADDR`, so not even a listener that does (tokio's)
/// can take the port.
#[derive(Debug)]
pub struct ClosedPort(Socket);

impl ClosedPort {
    /// Takes a free loopback port and holds it, closed.
    pub fn new() -> Self {
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).expect("create a socket");
        socket
            .bind(&SocketAddr::from((Ipv4Addr::LOCALHOST, 0)).into())
            .expect("bind a loopback port");
        Self(socket)
    }

    /// The port, to put into a URL or a configuration.
    pub fn port(&self) -> u16 {
        self.address().port()
    }

    /// The address on the loopback interface.
    pub fn address(&self) -> SocketAddr {
        self.0
            .local_addr()
            .expect("the bound address")
            .as_socket()
            .expect("an IP address")
    }
}

impl Default for ClosedPort {
    fn default() -> Self {
        Self::new()
    }
}

/// Sockets of [`closed_port`], kept until the test process ends.
static HELD: Mutex<Vec<ClosedPort>> = Mutex::new(Vec::new());

/// A closed loopback port for the rest of the test process: for a test that writes the port into
/// a URL or a child process's environment and has no place to keep a [`ClosedPort`]. The
/// process holds a handful of idle sockets until it exits.
pub fn closed_port() -> u16 {
    let closed = ClosedPort::new();
    let port = closed.port();
    HELD.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(closed);
    port
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::net::TcpStream;

    use super::*;

    #[test]
    fn a_closed_port_refuses_connections_and_cannot_be_taken() {
        let closed = ClosedPort::new();
        let address = closed.address();
        assert_eq!(
            TcpStream::connect(address).unwrap_err().kind(),
            ErrorKind::ConnectionRefused
        );
        // Not even a listener with SO_REUSEADDR can take it.
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        socket.set_reuse_address(true).unwrap();
        assert_eq!(
            socket.bind(&address.into()).unwrap_err().kind(),
            ErrorKind::AddrInUse
        );
    }

    #[test]
    fn closed_ports_stay_held_and_are_distinct() {
        let first = closed_port();
        let second = closed_port();
        assert_ne!(first, second);
        for port in [first, second] {
            assert_eq!(
                TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                    .unwrap_err()
                    .kind(),
                ErrorKind::ConnectionRefused
            );
        }
    }
}
