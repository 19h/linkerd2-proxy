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
    net::{SocketAddr, ToSocketAddrs},
    time::Duration,
};
use tokio::{io, net::TcpStream};

/// A target types representing an accepted connections.
#[derive(Copy, Clone, Debug)]
pub struct AcceptAddrs {
    pub local: Local<ServerAddr>,
    pub client: Remote<ClientAddr>,
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
pub struct ServerAddr(pub SocketAddr);

/// An SO_ORIGINAL_DST address.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct OrigDstAddr(pub SocketAddr);

#[derive(Copy, Clone, Debug)]
pub struct Keepalive(pub Option<Duration>);

/// Wraps an address type to indicate it describes an address describing this
/// process.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Local<T>(pub T);

/// Wraps an address type to indicate it describes another process.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Remote<T>(pub T);

// === impl Addrs ===

impl AcceptAddrs {
    pub fn target_addr(&self) -> SocketAddr {
        if let Some(OrigDstAddr(a)) = self.orig_dst {
            return a;
        }

        self.local.into()
    }
}

impl Param<Remote<ClientAddr>> for AcceptAddrs {
    fn param(&self) -> Remote<ClientAddr> {
        self.client
    }
}

impl Param<Local<ServerAddr>> for AcceptAddrs {
    fn param(&self) -> Local<ServerAddr> {
        self.local
    }
}

impl Param<Option<OrigDstAddr>> for AcceptAddrs {
    fn param(&self) -> Option<OrigDstAddr> {
        self.orig_dst
    }
}

// === impl ClientAddr ===

impl AsRef<SocketAddr> for ClientAddr {
    fn as_ref(&self) -> &SocketAddr {
        &self.0
    }
}

impl Into<SocketAddr> for ClientAddr {
    fn into(self) -> SocketAddr {
        self.0
    }
}

impl fmt::Display for ClientAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// === impl ListenAddr ===

impl AsRef<SocketAddr> for ListenAddr {
    fn as_ref(&self) -> &SocketAddr {
        &self.0
    }
}

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

impl fmt::Display for ListenAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// === impl ServerAddr ===

impl AsRef<SocketAddr> for ServerAddr {
    fn as_ref(&self) -> &SocketAddr {
        &self.0
    }
}

impl Into<SocketAddr> for ServerAddr {
    fn into(self) -> SocketAddr {
        self.0
    }
}

impl fmt::Display for ServerAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// === impl OrigDstAddr ===

impl AsRef<SocketAddr> for OrigDstAddr {
    fn as_ref(&self) -> &SocketAddr {
        &self.0
    }
}

impl Into<SocketAddr> for OrigDstAddr {
    fn into(self) -> SocketAddr {
        self.0
    }
}

impl fmt::Display for OrigDstAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// === impl Local ===

impl<T: AsRef<SocketAddr>> AsRef<SocketAddr> for Local<T> {
    fn as_ref(&self) -> &SocketAddr {
        self.0.as_ref()
    }
}

impl<T: Into<SocketAddr>> Into<SocketAddr> for Local<T> {
    fn into(self) -> SocketAddr {
        self.0.into()
    }
}

impl<T: fmt::Display> fmt::Display for Local<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// === impl Remote ===

impl<T: AsRef<SocketAddr>> AsRef<SocketAddr> for Remote<T> {
    fn as_ref(&self) -> &SocketAddr {
        self.0.as_ref()
    }
}

impl<T: Into<SocketAddr>> Into<SocketAddr> for Remote<T> {
    fn into(self) -> SocketAddr {
        self.0.into()
    }
}

impl<T: fmt::Display> fmt::Display for Remote<T> {
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
