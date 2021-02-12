#![deny(warnings, rust_2018_idioms)]

mod connect;
pub mod listen;
pub mod metrics;

pub use self::{
    connect::{ConnectAddr, ConnectTcp},
    listen::{BindTcp, DefaultOrigDstAddr, GetOrigDstAddr, NoOrigDstAddr},
};
use linkerd_stack::Param;
use std::{
    fmt,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    time::Duration,
};
use tokio::{io, net::TcpStream};

/// A target types representing an accepted connections.
#[derive(Copy, Clone, Debug)]
pub struct AcceptAddrs {
    pub local: LocalAddr,
    pub client: ClientAddr,
    pub orig_dst: Option<OrigDstAddr>,
}

/// The address of a remote client.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ClientAddr(pub SocketAddr);

/// The address for a listener to bind on.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ListenAddr(pub SocketAddr);

/// The address of a local server.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct LocalAddr(pub SocketAddr);

/// An SO_ORIGINAL_DST address.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct OrigDstAddr(pub SocketAddr);

#[derive(Copy, Clone, Debug)]
pub struct Keepalive(pub Option<Duration>);

// === impl Addrs ===

impl AcceptAddrs {
    pub fn target_addr(&self) -> SocketAddr {
        if let Some(OrigDstAddr(a)) = self.orig_dst {
            return a;
        }

        let LocalAddr(a) = self.local;
        a
    }
}

impl Param<ClientAddr> for AcceptAddrs {
    fn param(&self) -> ClientAddr {
        self.client
    }
}

impl Param<LocalAddr> for AcceptAddrs {
    fn param(&self) -> LocalAddr {
        self.local
    }
}

impl Param<Option<OrigDstAddr>> for AcceptAddrs {
    fn param(&self) -> Option<OrigDstAddr> {
        self.orig_dst
    }
}

// === impl ClientAddr ===

impl Into<SocketAddr> for ClientAddr {
    fn into(self) -> SocketAddr {
        self.0
    }
}

impl ClientAddr {
    pub fn ip(&self) -> IpAddr {
        self.0.ip()
    }

    pub fn port(&self) -> u16 {
        self.0.port()
    }
}

impl fmt::Display for ClientAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// === impl ListenAddr ===

impl Into<SocketAddr> for ListenAddr {
    fn into(self) -> SocketAddr {
        self.0
    }
}

impl ToSocketAddrs for ListenAddr {
    type Iter = std::option::IntoIter<SocketAddr>;

    fn to_socket_addrs(&self) -> io::Result<Self::Iter> {
        Ok(Some(self.0).into_iter())
    }
}

impl ListenAddr {
    pub fn ip(&self) -> IpAddr {
        self.0.ip()
    }

    pub fn port(&self) -> u16 {
        self.0.port()
    }
}

impl fmt::Display for ListenAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// === impl LocalAddr ===

impl Into<SocketAddr> for LocalAddr {
    fn into(self) -> SocketAddr {
        self.0
    }
}

impl LocalAddr {
    pub fn ip(&self) -> IpAddr {
        self.0.ip()
    }

    pub fn port(&self) -> u16 {
        self.0.port()
    }
}

impl fmt::Display for LocalAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// === impl OrigDstAddr ===

impl Into<SocketAddr> for OrigDstAddr {
    fn into(self) -> SocketAddr {
        self.0
    }
}

impl OrigDstAddr {
    pub fn ip(&self) -> IpAddr {
        self.0.ip()
    }

    pub fn port(&self) -> u16 {
        self.0.port()
    }
}

impl fmt::Display for OrigDstAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// === impl Keepalive ===

impl Into<Option<Duration>> for Keepalive {
    fn into(self) -> Option<Duration> {
        self.0
    }
}

// Misc.

fn set_nodelay_or_warn(socket: &TcpStream) {
    if let Err(e) = socket.set_nodelay(true) {
        tracing::warn!("failed to set nodelay: {}", e);
    }
}

fn set_keepalive_or_warn(tcp: &TcpStream, ka: Option<Duration>) {
    // TODO(eliza): when https://github.com/tokio-rs/tokio/pull/3189 merges
    // upstream, we will be able to convert the Tokio `TcpStream` into a
    // `socket2::Socket` without unsafe, by converting it to a
    // `std::net::TcpStream` (as `socket2::Socket` has a
    // `From<std::net::TcpStream>`). What we're doing now is more or less
    // equivalent, but this would use a safe interface...
    #[cfg(unix)]
    let sock = unsafe {
        // Safety: `from_raw_fd` takes ownership of the underlying file
        // descriptor, and will close it when dropped. However, we obtain the
        // file descriptor via `as_raw_fd` rather than `into_raw_fd`, so the
        // Tokio `TcpStream` *also* retains ownership of the socket --- which is
        // what we want. Instead of letting the `socket2` socket returned by
        // `from_raw_fd` close the fd, we `mem::forget` the `Socket`, so that
        // its `Drop` impl will not run. This ensures the fd is not closed
        // prematurely.
        use std::os::unix::io::{AsRawFd, FromRawFd};
        socket2::Socket::from_raw_fd(tcp.as_raw_fd())
    };
    #[cfg(windows)]
    let sock = unsafe {
        // Safety: `from_raw_socket` takes ownership of the underlying Windows
        // SOCKET, and will close it when dropped. However, we obtain the
        // SOCKET via `as_raw_socket` rather than `into_raw_socket`, so the
        // Tokio `TcpStream` *also* retains ownership of the socket --- which is
        // what we want. Instead of letting the `socket2` socket returned by
        // `from_raw_socket` close the SOCKET, we `mem::forget` the `Socket`, so
        // that its `Drop` impl will not run. This ensures the socket is not
        // closed prematurely.
        use std::os::windows::io::{AsRawSocket, FromRawSocket};
        socket2::Socket::from_raw_socket(tcp.as_raw_socket())
    };

    if let Err(e) = sock.set_keepalive(ka) {
        tracing::warn!("failed to set keepalive: {}", e);
    }

    // Don't let the socket2 socket close the fd on drop!
    std::mem::forget(sock);
}
