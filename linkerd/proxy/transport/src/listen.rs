use crate::{
    orig_dst::{GetOrigDstAddr, NoOrigDstAddr},
    AcceptAddrs, ClientAddr, ListenAddr, Local, Remote, ServerAddr,
};
use async_stream::try_stream;
use futures::prelude::*;
use std::{io, net::SocketAddr, time::Duration};
use tokio::net::TcpStream;
use tracing::trace;

#[derive(Clone, Debug)]
pub struct BindTcp<O: GetOrigDstAddr = NoOrigDstAddr> {
    bind_addr: ListenAddr,
    keepalive: Option<Duration>,
    orig_dst_addr: O,
}

pub type Connection = (AcceptAddrs, TcpStream);

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
