use crate::{BindAddr, ClientAddr, Keepalive, Local, Remote, ServerAddr};
use futures::prelude::*;
use linkerd_stack::Param;
use std::{io, pin::Pin, time::Duration};
use tokio::net::TcpStream;
use tokio_stream::wrappers::TcpListenerStream;
use tracing::trace;

pub trait Bind<T> {
    type Addrs;
    type Io;
    type Accept: Stream<Item = io::Result<(Self::Addrs, Self::Io)>>;

    fn bind(&self, config: T) -> io::Result<(Local<ServerAddr>, Self::Accept)>;
}

impl<F, C, T, I, A> Bind<C> for F
where
    F: Fn(C) -> io::Result<(Local<ServerAddr>, A)>,
    A: Stream<Item = io::Result<(T, I)>>,
{
    type Addrs = T;
    type Io = I;
    type Accept = A;

    fn bind(&self, config: C) -> io::Result<(Local<ServerAddr>, A)> {
        (self)(config)
    }
}

pub struct BindTcp;

#[derive(Clone, Debug)]
pub struct AcceptAddrs {
    pub client: Remote<ClientAddr>,
    pub server: Local<ServerAddr>,
}

impl<T> Bind<T> for BindTcp
where
    T: Param<BindAddr> + Param<Keepalive> + 'static,
{
    type Addrs = AcceptAddrs;
    type Io = TcpStream;
    type Accept = Pin<Box<dyn Stream<Item = io::Result<(AcceptAddrs, Self::Io)>> + Send + 'static>>;

    fn bind(&self, config: T) -> io::Result<(Local<ServerAddr>, Self::Accept)> {
        let (addr, accept) = bind_tcp(config)?;
        Ok((addr, Box::pin(accept)))
    }
}

pub fn bind_tcp<T>(
    config: T,
) -> io::Result<(
    Local<ServerAddr>,
    impl Stream<Item = io::Result<(AcceptAddrs, TcpStream)>> + Send,
)>
where
    T: Param<BindAddr> + Param<Keepalive>,
{
    let BindAddr(bind_addr) = config.param();
    let Keepalive(keepalive) = config.param();

    let listen = {
        let l = std::net::TcpListener::bind(bind_addr)?;
        // Ensure that O_NONBLOCK is set on the socket before using it with Tokio.
        l.set_nonblocking(true)?;
        tokio::net::TcpListener::from_std(l).expect("Listener must be valid")
    };
    let addr = Local(ServerAddr(listen.local_addr()?));

    let accept =
        TcpListenerStream::new(listen).and_then(move |tcp| future::ready(accept(tcp, keepalive)));

    Ok((addr, accept))
}

fn accept(tcp: TcpStream, keepalive: Option<Duration>) -> io::Result<(AcceptAddrs, TcpStream)> {
    let addrs = {
        let client = Remote(ClientAddr(tcp.peer_addr()?));
        let server = Local(ServerAddr(tcp.local_addr()?));
        trace!(client.addr = %client, server.addr = %server, "Accepted");
        AcceptAddrs { client, server }
    };

    super::set_nodelay_or_warn(&tcp);
    super::set_keepalive_or_warn(&tcp, keepalive);

    Ok((addrs, tcp))
}

impl Param<Remote<ClientAddr>> for AcceptAddrs {
    fn param(&self) -> Remote<ClientAddr> {
        self.client
    }
}

impl Param<Local<ServerAddr>> for AcceptAddrs {
    fn param(&self) -> Local<ServerAddr> {
        self.server
    }
}
