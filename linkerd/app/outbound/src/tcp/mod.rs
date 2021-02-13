pub mod connect;
pub mod logical;
pub mod opaque_transport;
#[cfg(test)]
mod tests;

use crate::target;
pub use linkerd_app_core::proxy::tcp::Forward;
use linkerd_app_core::{
    svc::stack::Param, transport::ProxyAddrs, transport_header::SessionProtocol,
};

pub type Accept = target::Accept<()>;
pub type Logical = target::Logical<()>;
pub type Concrete = target::Concrete<()>;
pub type Endpoint = target::Endpoint<()>;

// FIXME this should actually use the OrigDstAddr type to prove its an original
// dst addr.
impl From<ProxyAddrs> for Accept {
    fn from(addrs: ProxyAddrs) -> Self {
        Self {
            orig_dst: addrs.orig_dst,
            protocol: (),
        }
    }
}

impl<P> From<(P, Accept)> for target::Accept<P> {
    fn from((protocol, Accept { orig_dst, .. }): (P, Accept)) -> Self {
        Self { orig_dst, protocol }
    }
}

impl Param<Option<SessionProtocol>> for Endpoint {
    fn param(&self) -> Option<SessionProtocol> {
        None
    }
}
