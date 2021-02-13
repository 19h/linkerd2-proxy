use crate::{
    orig_dst::{GetOrigDstAddr, NoOrigDstAddr},
    AcceptAddrs, ClientAddr, Keepalive, ListenAddr, Local, Remote, ServerAddr,
};
use futures::prelude::*;
use linkerd_stack::Param;
use std::{io, net::SocketAddr, time::Duration};
use tokio::net::TcpStream;
use tokio_stream::wrappers::TcpListenerStream;
use tracing::trace;

#[derive(Clone, Debug)]
pub struct BindTcp<O> {
    orig_dst_addr: O,
}

pub type Connection = (AcceptAddrs, TcpStream);

impl Default for BindTcp<NoOrigDstAddr> {
    fn default() -> Self {
        Self::new(NoOrigDstAddr(()))
    }
}

impl<O: GetOrigDstAddr> BindTcp<O> {
    pub fn new(orig_dst_addr: O) -> Self {
        Self { orig_dst_addr }
    }

    pub fn bind<T>(
        &self,
        config: T,
    ) -> io::Result<(SocketAddr, impl Stream<Item = io::Result<Connection>>)>
    where
        T: Param<ListenAddr> + Param<Keepalive>,
    {
        let ListenAddr(bind_addr) = config.param();
        let Keepalive(keepalive) = config.param();
        let get_orig = self.orig_dst_addr.clone();

        let listen = {
            let l = std::net::TcpListener::bind(bind_addr)?;
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
        get_orig: O,
    ) -> io::Result<(AcceptAddrs, TcpStream)> {
        let addrs = {
            let local = Local(ServerAddr(tcp.local_addr()?));
            let client = Remote(ClientAddr(tcp.peer_addr()?));
            let orig_dst = get_orig.orig_dst_addr(&tcp);
            trace!(
                local.addr = %local,
                client.addr = %client,
                orig.addr = ?orig_dst,
                "Accepted",
            );
            AcceptAddrs {
                local,
                client,
                orig_dst,
            }
        };

        super::set_nodelay_or_warn(&tcp);
        super::set_keepalive_or_warn(&tcp, keepalive);

        Ok((addrs, tcp))
    }
}
