use crate::core::{
    admin, classify,
    config::ServerConfig,
    detect, drain, errors, io,
    metrics::{self, FmtMetrics},
    serve, tls, trace,
    transport::{bind::AcceptAddrs, Bind, Local, OrigDstAddr, ProxyAddrs, ServerAddr},
    Error,
};
use crate::{
    http,
    identity::LocalCrtKey,
    inbound::target::{HttpAccept, Target, TcpAccept},
    svc,
};
use std::{fmt, pin::Pin, time::Duration};
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub struct Config {
    pub server: ServerConfig,
    pub metrics_retain_idle: Duration,
}

pub struct Admin {
    pub listen_addr: Local<ServerAddr>,
    pub latch: admin::Latch,
    pub serve: Pin<Box<dyn std::future::Future<Output = Result<(), Error>> + Send + 'static>>,
}

#[derive(Debug, Default)]
pub struct AdminHttpOnly(());

impl Config {
    #[allow(clippy::clippy::too_many_arguments)]
    pub fn build<R, B>(
        self,
        report: R,
        bind: B,
        identity: Option<LocalCrtKey>,
        metrics: metrics::Proxy,
        trace: trace::Handle,
        drain: drain::Watch,
        shutdown: mpsc::UnboundedSender<()>,
    ) -> Result<Admin, Error>
    where
        R: FmtMetrics + Clone + Send + 'static + Unpin,
        B: Bind<ServerConfig, Addrs = AcceptAddrs>,
        B::Accept: Send + 'static,
        B::Io: io::AsyncRead
            + io::AsyncWrite
            + io::Peek
            + io::PeerAddr
            + Send
            + Sync
            + Unpin
            + 'static,
    {
        const DETECT_TIMEOUT: Duration = Duration::from_secs(1);

        let (listen_addr, listen) = bind.bind(self.server)?;

        let (ready, latch) = admin::Readiness::new();

        // TODO this should use different stack types.
        let admin = svc::stack(admin::Admin::new(report, ready, shutdown, trace))
            .push(metrics.http_endpoint.to_layer::<classify::Response, _>())
            .push_on_response(
                svc::layers()
                    .push(metrics.http_errors.clone())
                    .push(errors::layer())
                    .push(http::BoxResponse::layer()),
            )
            .push_map_target(Target::from)
            .push(http::NewServeHttp::layer(Default::default(), drain.clone()))
            .push_map_target(HttpAccept::from)
            .push(svc::UnwrapOr::layer(
                svc::Fail::<_, AdminHttpOnly>::default(),
            ))
            .push(detect::NewDetectService::layer(
                DETECT_TIMEOUT,
                http::DetectHttp::default(),
            ))
            .push(metrics.transport.layer_accept())
            .push_map_target(TcpAccept::from)
            .push(tls::NewDetectTls::layer(identity, DETECT_TIMEOUT))
            .push_map_target(|AcceptAddrs { client, server }| ProxyAddrs {
                client,
                server,
                // XXX We shouldn't do this, but it's where we're at for now.
                orig_dst: OrigDstAddr(server.into()),
            })
            .into_inner();

        let serve = Box::pin(serve::serve(listen, admin, drain.signaled()));
        Ok(Admin {
            listen_addr,
            latch,
            serve,
        })
    }
}

impl fmt::Display for AdminHttpOnly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("proxy admin server is HTTP-only")
    }
}

impl std::error::Error for AdminHttpOnly {}
