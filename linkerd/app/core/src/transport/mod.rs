use crate::svc::stack::Param;
use futures::{prelude::*, stream};
pub use linkerd_proxy_transport::*;
use std::io;

pub mod labels;

pub type Metrics = metrics::Registry<labels::Key>;

#[derive(Clone, Debug)]
pub struct ProxyAddrs {
    pub orig_dst: OrigDstAddr,
    pub client: Remote<ClientAddr>,
    pub server: Local<ServerAddr>,
}

pub struct BindProxy<B, G> {
    inner: B,
    get_orig_dst: G,
}

// === impl BindProxy ===

impl<B, G> BindProxy<B, G> {
    pub fn new(inner: B, get_orig_dst: G) -> Self {
        Self {
            inner,
            get_orig_dst,
        }
    }
}

impl<T, B, G> Bind<T> for BindProxy<B, G>
where
    B: Bind<T>,
    B::Addrs: Param<Remote<ClientAddr>> + Param<Local<ServerAddr>>,
    B::Io: Send + 'static,
    B::Accept: Send + 'static,
    G: GetOrigDstAddr<B::Io> + Send + 'static,
{
    type Addrs = ProxyAddrs;
    type Io = B::Io;
    type Accept = stream::BoxStream<'static, io::Result<(ProxyAddrs, B::Io)>>;

    fn bind(&self, config: T) -> io::Result<(Local<ServerAddr>, Self::Accept)> {
        let (addr, accept) = self.inner.bind(config)?;
        let get_orig_dst = self.get_orig_dst.clone();
        let accept = accept.and_then(move |(addrs, io)| {
            let orig_dst = match get_orig_dst.orig_dst_addr(&io) {
                Ok(addr) => addr,
                Err(e) => return future::err(e),
            };
            let addrs = ProxyAddrs {
                orig_dst,
                client: addrs.param(),
                server: addrs.param(),
            };
            future::ok((addrs, io))
        });
        Ok((addr, Box::pin(accept)))
    }
}

// === impl ProxyAddrs ===

impl Param<OrigDstAddr> for ProxyAddrs {
    fn param(&self) -> OrigDstAddr {
        self.orig_dst
    }
}

impl Param<Remote<ClientAddr>> for ProxyAddrs {
    fn param(&self) -> Remote<ClientAddr> {
        self.client
    }
}

impl Param<Local<ServerAddr>> for ProxyAddrs {
    fn param(&self) -> Local<ServerAddr> {
        self.server
    }
}
