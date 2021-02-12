use crate::{AcceptAddrs, ClientAddr, ListenAddr, Local, OrigDstAddr, Remote, ServerAddr};
use async_stream::try_stream;
use futures::prelude::*;
use std::{io, net::SocketAddr, time::Duration};
use tokio::net::TcpStream;
use tracing::trace;

/// A mockable source for address info, i.e., for tests.
pub trait GetOrigDstAddr: Clone {
    fn orig_dst_addr(&self, socket: &TcpStream) -> Option<OrigDstAddr>;
}

#[derive(Clone, Debug)]
pub struct BindTcp<O: GetOrigDstAddr = NoOrigDstAddr> {
    bind_addr: ListenAddr,
    keepalive: Option<Duration>,
    orig_dst_addr: O,
}

pub type Connection = (AcceptAddrs, TcpStream);

#[derive(Copy, Clone, Debug)]
pub struct NoOrigDstAddr(());

// The mock-orig-dst feature disables use of the syscall-based GetOrigDstAddr implementation and
// replaces it with one that must be configured.

#[cfg(not(feature = "mock-orig-dst"))]
pub use self::sys::SysOrigDstAddr as DefaultOrigDstAddr;

#[cfg(feature = "mock-orig-dst")]
pub use self::mock::MockOrigDstAddr as DefaultOrigDstAddr;

impl BindTcp {
    pub fn new(addr: SocketAddr, keepalive: Option<Duration>) -> Self {
        Self {
            bind_addr: ListenAddr(addr),
            keepalive,
            orig_dst_addr: NoOrigDstAddr(()),
        }
    }
}

impl<A: GetOrigDstAddr> BindTcp<A> {
    pub fn with_orig_dst_addr<B: GetOrigDstAddr>(self, orig_dst_addr: B) -> BindTcp<B> {
        BindTcp {
            orig_dst_addr,
            bind_addr: self.bind_addr,
            keepalive: self.keepalive,
        }
    }

    pub fn bind_addr(&self) -> ListenAddr {
        self.bind_addr
    }

    pub fn keepalive(&self) -> Option<Duration> {
        self.keepalive
    }

    pub fn bind(&self) -> io::Result<(SocketAddr, impl Stream<Item = io::Result<Connection>>)> {
        let listen = std::net::TcpListener::bind(self.bind_addr)?;
        // Ensure that O_NONBLOCK is set on the socket before using it with Tokio.
        listen.set_nonblocking(true)?;
        let addr = listen.local_addr()?;
        let keepalive = self.keepalive;
        let get_orig = self.orig_dst_addr.clone();

        let accept = try_stream! {
            tokio::pin! {
                // The tokio listener is built lazily so that it is initialized on
                // the proper runtime.
                let listen = tokio::net::TcpListener::from_std(listen).expect("listener must be valid");
            };

            while let (tcp, client_addr) = listen.accept().await? {
                super::set_nodelay_or_warn(&tcp);
                super::set_keepalive_or_warn(&tcp, keepalive);

                let local_addr = tcp.local_addr()?;
                let orig_dst = get_orig.orig_dst_addr(&tcp);
                trace!(
                    local.addr = %local_addr,
                    client.addr = %client_addr,
                    orig.addr = ?orig_dst,
                    "Accepted",
                );
                let addrs = AcceptAddrs {
                    local: Local(ServerAddr(local_addr)),
                    client: Remote(ClientAddr(client_addr)),
                    orig_dst
                };
                yield (addrs, tcp);
            }
        };

        Ok((addr, accept))
    }
}

impl GetOrigDstAddr for NoOrigDstAddr {
    fn orig_dst_addr(&self, _: &TcpStream) -> Option<OrigDstAddr> {
        None
    }
}

#[cfg(not(feature = "mock-orig-dst"))]
mod sys {
    use super::{GetOrigDstAddr, SocketAddr, TcpStream};

    #[derive(Copy, Clone, Debug, Default)]
    pub struct SysOrigDstAddr(());

    impl GetOrigDstAddr for SysOrigDstAddr {
        #[cfg(target_os = "linux")]
        fn orig_dst_addr(&self, sock: &TcpStream) -> Option<OrigDstAddr> {
            use std::os::unix::io::AsRawFd;

            let fd = sock.as_raw_fd();
            let r = unsafe { linux::so_original_dst(fd) };
            r.ok().map(OrigDstAddr)
        }

        #[cfg(not(target_os = "linux"))]
        fn orig_dst_addr(&self, _sock: &TcpStream) -> Option<OrigDstAddr> {
            None
        }
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
        use std::os::unix::io::RawFd;
        use std::{io, mem};
        use tracing::warn;

        pub unsafe fn so_original_dst(fd: RawFd) -> io::Result<SocketAddr> {
            let mut sockaddr: libc::sockaddr_storage = mem::zeroed();
            let mut socklen: libc::socklen_t = mem::size_of::<libc::sockaddr_storage>() as u32;

            let ret = libc::getsockopt(
                fd,
                libc::SOL_IP,
                libc::SO_ORIGINAL_DST,
                &mut sockaddr as *mut _ as *mut _,
                &mut socklen as *mut _ as *mut _,
            );
            if ret != 0 {
                let e = io::Error::last_os_error();
                warn!("failed to read SO_ORIGINAL_DST: {:?}", e);
                return Err(e);
            }

            mk_addr(&sockaddr, socklen)
        }

        // Borrowed with love from net2-rs
        // https://github.com/rust-lang-nursery/net2-rs/blob/1b4cb4fb05fbad750b271f38221eab583b666e5e/src/socket.rs#L103
        fn mk_addr(
            storage: &libc::sockaddr_storage,
            len: libc::socklen_t,
        ) -> io::Result<SocketAddr> {
            match storage.ss_family as libc::c_int {
                libc::AF_INET => {
                    assert!(len as usize >= mem::size_of::<libc::sockaddr_in>());

                    let sa = {
                        let sa = storage as *const _ as *const libc::sockaddr_in;
                        unsafe { *sa }
                    };

                    let bits = ntoh32(sa.sin_addr.s_addr);
                    let ip = Ipv4Addr::new(
                        (bits >> 24) as u8,
                        (bits >> 16) as u8,
                        (bits >> 8) as u8,
                        bits as u8,
                    );
                    let port = sa.sin_port;
                    Ok(SocketAddr::V4(SocketAddrV4::new(ip, ntoh16(port))))
                }
                libc::AF_INET6 => {
                    assert!(len as usize >= mem::size_of::<libc::sockaddr_in6>());

                    let sa = {
                        let sa = storage as *const _ as *const libc::sockaddr_in6;
                        unsafe { *sa }
                    };

                    let arr = sa.sin6_addr.s6_addr;
                    let ip = Ipv6Addr::new(
                        (arr[0] as u16) << 8 | (arr[1] as u16),
                        (arr[2] as u16) << 8 | (arr[3] as u16),
                        (arr[4] as u16) << 8 | (arr[5] as u16),
                        (arr[6] as u16) << 8 | (arr[7] as u16),
                        (arr[8] as u16) << 8 | (arr[9] as u16),
                        (arr[10] as u16) << 8 | (arr[11] as u16),
                        (arr[12] as u16) << 8 | (arr[13] as u16),
                        (arr[14] as u16) << 8 | (arr[15] as u16),
                    );

                    let port = sa.sin6_port;
                    let flowinfo = sa.sin6_flowinfo;
                    let scope_id = sa.sin6_scope_id;
                    Ok(SocketAddr::V6(SocketAddrV6::new(
                        ip,
                        ntoh16(port),
                        flowinfo,
                        scope_id,
                    )))
                }
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid argument",
                )),
            }
        }

        fn ntoh16(i: u16) -> u16 {
            <u16>::from_be(i)
        }

        fn ntoh32(i: u32) -> u32 {
            <u32>::from_be(i)
        }
    }
}

#[cfg(feature = "mock-orig-dst")]
mod mock {
    use crate::OrigDstAddr;

    use super::{GetOrigDstAddr, SocketAddr, TcpStream};

    #[derive(Copy, Clone, Debug)]
    pub struct MockOrigDstAddr(SocketAddr);

    impl From<SocketAddr> for MockOrigDstAddr {
        fn from(addr: SocketAddr) -> Self {
            MockOrigDstAddr(addr)
        }
    }

    impl GetOrigDstAddr for MockOrigDstAddr {
        fn orig_dst_addr(&self, _: &TcpStream) -> Option<OrigDstAddr> {
            Some(OrigDstAddr(self.0))
        }
    }
}
