use crate::{
    orig_dst::{GetOrigDstAddr, NoOrigDstAddr},
    AcceptAddrs, ClientAddr, ListenAddr, Local, Remote, ServerAddr,
};
use futures::prelude::*;
use std::{io, net::SocketAddr, time::Duration};
use tokio::net::TcpStream;
use tokio_stream::wrappers::TcpListenerStream;
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
        let keepalive = self.keepalive;
        let get_orig = self.orig_dst_addr.clone();

        let listen = {
            let l = std::net::TcpListener::bind(self.bind_addr)?;
            // Ensure that O_NONBLOCK is set on the socket before using it with Tokio.
            l.set_nonblocking(true)?;
            tokio::net::TcpListener::from_std(l).expect("Listener must be valid")
        };
        let addr = listen.local_addr()?;

        let accept = TcpListenerStream::new(listen)
            .and_then(move |tcp| future::ready(Self::accept(tcp, keepalive, get_orig.clone())));

        Ok((addr, accept))
    }

    fn accept(
        tcp: TcpStream,
        keepalive: Option<Duration>,
        get_orig: A,
    ) -> io::Result<(AcceptAddrs, TcpStream)> {
        super::set_nodelay_or_warn(&tcp);
        super::set_keepalive_or_warn(&tcp, keepalive);

        let local = Local(ServerAddr(tcp.local_addr()?));
        let client = Remote(ClientAddr(tcp.peer_addr()?));
        let orig_dst = get_orig.orig_dst_addr(&tcp);
        trace!(
            local.addr = %local,
            client.addr = %client,
            orig.addr = ?orig_dst,
            "Accepted",
        );
        let addrs = AcceptAddrs {
            local,
            client,
            orig_dst,
        };
        Ok((addrs, tcp))
    }
}
