//! Configures and runs the inbound proxy.
//!
//! The inbound proxy is responsible for terminating traffic from other network
//! endpoints inbound to the local application.

#![deny(warnings, rust_2018_idioms)]

use self::allow_discovery::AllowProfile;
pub use self::endpoint::{
    HttpEndpoint, ProfileTarget, RequestTarget, Target, TcpAccept, TcpEndpoint,
};
use self::prevent_loop::PreventLoop;
use self::require_identity_for_ports::RequireIdentityForPorts;
use linkerd2_app_core::{
    classify,
    config::{ConnectConfig, ProxyConfig},
    drain, dst, errors, metrics, opaque_transport,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::{
        http::{self, orig_proto, strip_header},
        identity, tap, tcp,
    },
    reconnect,
    spans::SpanConverter,
    svc,
    transport::{self, io, listen, metrics::SensorIo, tls, NewDetectService},
    Error, NameAddr, NameMatch, TraceContext, DST_OVERRIDE_HEADER,
};
use std::{collections::HashMap, fmt::Debug, net::SocketAddr, time::Duration};
use tokio::sync::mpsc;
use tracing::debug_span;

mod allow_discovery;
pub mod endpoint;
mod prevent_loop;
mod require_identity_for_ports;

#[derive(Clone, Debug)]
pub struct Config {
    pub allow_discovery: NameMatch,
    pub proxy: ProxyConfig,
    pub require_identity_for_inbound_ports: RequireIdentityForPorts,
    pub disable_protocol_detection_for_ports: SkipByPort,
    pub profile_idle_timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct SkipByPort(std::sync::Arc<indexmap::IndexSet<u16>>);

#[derive(Default)]
struct RefuseNonOpaque(());

// === impl Config ===

pub fn tcp_connect<T: Into<u16>>(
    config: &ConnectConfig,
) -> impl svc::Service<
    T,
    Response = impl io::AsyncRead + io::AsyncWrite + Send,
    Error = Error,
    Future = impl Send,
> + Clone {
    // Establishes connections to remote peers (for both TCP
    // forwarding and HTTP proxying).
    svc::stack(transport::ConnectTcp::new(config.keepalive))
        .push_map_target(|t: T| ([127, 0, 0, 1], t.into()))
        // Limits the time we wait for a connection to be established.
        .push_timeout(config.timeout)
        .into_inner()
}

#[allow(clippy::too_many_arguments)]
impl Config {
    pub fn build<I, P, C, H, HSvc, T, TSvc>(
        self,
        listen_addr: SocketAddr,
        local_identity: tls::Conditional<identity::Local>,
        connect: C,
        http_gateway: H,
        tcp_gateway: Option<T>,
        profiles_client: P,
        tap: tap::Registry,
        metrics: metrics::Proxy,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> impl svc::NewService<
        listen::Addrs,
        Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
    > + Clone
    where
        I: tls::accept::Detectable
            + io::AsyncRead
            + io::AsyncWrite
            + io::PeerAddr
            + Debug
            + Send
            + Unpin
            + 'static,
        C: svc::Service<TcpEndpoint> + Clone + Send + Sync + Unpin + 'static,
        C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
        C::Error: Into<Error>,
        C::Future: Send + Unpin,
        H: svc::NewService<Target, Service = HSvc> + Clone + Send + Sync + Unpin + 'static,
        HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Send
            + Unpin
            + 'static,
        HSvc::Error: Into<Error>,
        HSvc::Future: Send,
        T: svc::NewService<(opaque_transport::Header, TcpAccept), Service = TSvc>
            + Clone
            + Send
            + Sync
            + Unpin
            + 'static,
        TSvc: svc::Service<io::PrefixedIo<SensorIo<tls::accept::Io<I>>>, Response = ()>
            + Send
            + Unpin
            + 'static,
        TSvc::Error: Into<Error>,
        TSvc::Future: Send,
        P: profiles::GetProfile<NameAddr> + Clone + Send + Sync + 'static,
        P::Error: Send,
        P::Future: Send,
    {
        let prevent_loop = PreventLoop::from(listen_addr.port());

        // Forwards TCP streams that cannot be decoded as HTTP.
        let tcp_forward = svc::stack(connect.clone())
            .push(metrics.transport.layer_connect())
            .push_make_thunk()
            .push_on_response(
                svc::layers()
                    .push(tcp::Forward::layer())
                    .push(drain::Retain::layer(drain.clone())),
            )
            .instrument(|_: &_| debug_span!("tcp"))
            .into_inner();

        // Handle connections that target the inbound server port by (1)
        // detecting an opaque transort header or (2) routing HTTP requests
        // through the provided `http_loopback` service.
        let direct = {
            // Route non-opaque HTTP requests through the provided service.
            let http = svc::stack(http_gateway)
                .push(svc::NewRouter::layer(RequestTarget::from))
                .push_on_response(
                    svc::layers()
                        .push(svc::FailFast::layer(
                            "HTTP Direct",
                            self.proxy.dispatch_timeout,
                        ))
                        .push_spawn_buffer(self.proxy.buffer_capacity)
                        .push(metrics.stack.layer(stack_labels("http", "direct"))),
                )
                .push_cache(self.proxy.cache_max_idle_age)
                .push_on_response(
                    svc::layers()
                        .push(http::Retain::layer())
                        .push(http::BoxResponse::layer()),
                )
                .into_inner();

            // If there was an opaque transport header, use it to determine the
            // local endpoint and forward the connection.
            svc::stack(tcp_forward.clone())
                .push_map_target(|(h, _): (opaque_transport::Header, _)| TcpEndpoint::from(h))
                .push(svc::stack::NewOptional::layer(tcp_gateway))
                .push(svc::NewUnwrapOr::layer(
                    // If there's no opaque transport header, try to detect
                    // HTTP. If that can't be done, fail the connection as if it
                    // were refused.
                    svc::stack(self.http_server(http, &metrics, span_sink.clone(), drain.clone()))
                        .push(svc::NewUnwrapOr::layer(
                            svc::Fail::<_, RefuseNonOpaque>::default(),
                        ))
                        .push(NewDetectService::layer(
                            self.proxy.detect_protocol_timeout,
                            http::DetectHttp::default(),
                        ))
                        .into_inner(),
                ))
                .push(NewDetectService::layer(
                    self.proxy.detect_protocol_timeout,
                    opaque_transport::DetectHeader::default(),
                ))
                .into_inner()
        };

        let http = {
            let router =
                self.http_router(connect, profiles_client, tap, &metrics, span_sink.clone());
            self.http_server(router, &metrics, span_sink, drain)
        };

        svc::stack(http)
            .check_new::<(http::Version, TcpAccept)>()
            .push_cache(self.proxy.cache_max_idle_age)
            .push(svc::NewUnwrapOr::layer(
                svc::stack(tcp_forward.clone())
                    .push_map_target(TcpEndpoint::from)
                    .check_new::<TcpAccept>()
                    .into_inner(),
            ))
            .push(NewDetectService::layer(
                self.proxy.detect_protocol_timeout,
                http::DetectHttp::default(),
            ))
            // If the connection targets the inbound port, use the direct stack.
            .push_switch(prevent_loop, direct)
            .push_request_filter(self.require_identity_for_inbound_ports)
            .push(metrics.transport.layer_accept())
            .check_new::<TcpAccept>()
            .push_map_target(TcpAccept::from)
            .push(tls::NewDetectTls::layer(
                local_identity,
                self.proxy.detect_protocol_timeout,
            ))
            .push_switch(
                self.disable_protocol_detection_for_ports,
                svc::stack(tcp_forward)
                    .push_map_target(TcpEndpoint::from)
                    .push(metrics.transport.layer_accept())
                    .push_map_target(TcpAccept::from)
                    .into_inner(),
            )
            .into_inner()
    }

    fn http_router<T, C, P>(
        &self,
        connect: C,
        profiles_client: P,
        tap: tap::Registry,
        metrics: &metrics::Proxy,
        span_sink: Option<mpsc::Sender<oc::Span>>,
    ) -> impl svc::NewService<
        T,
        Service = impl svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
            Future = impl Send,
        > + Clone,
    > + Clone
    where
        RequestTarget: From<T>,
        C: svc::Service<TcpEndpoint> + Clone + Send + Sync + Unpin + 'static,
        C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
        C::Error: Into<Error>,
        C::Future: Send + Unpin,
        P: profiles::GetProfile<NameAddr> + Clone + Send + Sync + 'static,
        P::Future: Send,
        P::Error: Send,
    {
        // Creates HTTP clients for each inbound port & HTTP settings.
        let endpoint = svc::stack(connect)
            .push(metrics.transport.layer_connect())
            .push_map_target(TcpEndpoint::from)
            .push(http::client::layer(
                self.proxy.connect.h1_settings,
                self.proxy.connect.h2_settings,
            ))
            .push(reconnect::layer({
                let backoff = self.proxy.connect.backoff;
                move |_| Ok(backoff.stream())
            }))
            .check_new_service::<HttpEndpoint, http::Request<_>>();

        let target = endpoint
            .push_map_target(HttpEndpoint::from)
            // Registers the stack to be tapped.
            .push(tap::NewTapHttp::layer(tap))
            // Records metrics for each `Target`.
            .push(metrics.http_endpoint.to_layer::<classify::Response, _>())
            .push_on_response(TraceContext::layer(
                span_sink.map(|span_sink| SpanConverter::client(span_sink, trace_labels())),
            ))
            .push_on_response(http::BoxResponse::layer())
            .check_new_service::<Target, http::Request<_>>();

        // Attempts to discover a service profile for each logical target (as
        // informed by the request's headers). The stack is cached until a
        // request has not been received for `cache_max_idle_age`.
        let profile = target
            .clone()
            .check_new_service::<Target, http::Request<http::BoxBody>>()
            .push_on_response(http::BoxRequest::layer())
            // The target stack doesn't use the profile resolution, so drop it.
            .push_map_target(endpoint::Target::from)
            .push(profiles::http::route_request::layer(
                svc::proxies()
                    // Sets the route as a request extension so that it can be used
                    // by tap.
                    .push_http_insert_target()
                    // Records per-route metrics.
                    .push(metrics.http_route.to_layer::<classify::Response, _>())
                    // Sets the per-route response classifier as a request
                    // extension.
                    .push(classify::NewClassify::layer())
                    .check_new_clone::<dst::Route>()
                    .push_map_target(endpoint::route)
                    .into_inner(),
            ))
            .push_map_target(endpoint::Logical::from)
            .push(profiles::discover::layer(
                profiles_client,
                AllowProfile(self.allow_discovery.clone()),
            ))
            .push_on_response(http::BoxResponse::layer())
            .instrument(|_: &Target| debug_span!("profile"))
            // Skip the profile stack if it takes too long to become ready.
            .push_when_unready(target.clone(), self.profile_idle_timeout)
            .check_new_service::<Target, http::Request<http::BoxBody>>();

        // If the traffic is targeted at the inbound port, send it through
        // the loopback service (i.e. as a gateway).
        svc::stack(profile)
            .check_new_service::<Target, http::Request<http::BoxBody>>()
            .push_on_response(
                svc::layers()
                    .push(svc::FailFast::layer("Logical", self.proxy.dispatch_timeout))
                    .push_spawn_buffer(self.proxy.buffer_capacity)
                    .push(metrics.stack.layer(stack_labels("http", "logical"))),
            )
            .push_cache(self.proxy.cache_max_idle_age)
            .push_on_response(
                svc::layers()
                    .push(http::Retain::layer())
                    .push(http::BoxResponse::layer()),
            )
            // Boxing is necessary purely to limit the link-time overhead of
            // having enormous types.
            .push(svc::BoxNewService::layer())
            .check_new_service::<Target, http::Request<http::BoxBody>>()
            // Removes the override header after it has been used to
            // determine a reuquest target.
            .push_on_response(strip_header::request::layer(DST_OVERRIDE_HEADER))
            // Routes each request to a target, obtains a service for that
            // target, and dispatches the request.
            .instrument_from_target()
            .push(svc::NewRouter::layer(RequestTarget::from))
            .into_inner()
    }

    fn http_server<T, I, H, HSvc>(
        &self,
        http: H,
        metrics: &metrics::Proxy,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> impl svc::NewService<
        (http::Version, T),
        Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
    > + Clone
    where
        T: Clone + Send + Sync + Unpin + 'static,
        for<'t> &'t T: Into<SocketAddr>,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Unpin + 'static,
        H: svc::NewService<T, Service = HSvc> + Clone + Send + 'static,
        HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Clone
            + Send
            + Unpin
            + 'static,
        HSvc::Error: Into<Error>,
        HSvc::Future: Send,
    {
        svc::stack(http)
            .push_http_insert_target() // Used by tap.
            .push_on_response(
                svc::layers()
                    // Downgrades the protocol if upgraded by an outbound proxy.
                    .push(orig_proto::Downgrade::layer())
                    // Limits the number of in-flight requests.
                    .push(svc::ConcurrencyLimit::layer(
                        self.proxy.max_in_flight_requests,
                    ))
                    // Eagerly fail requests when the proxy is out of capacity for a
                    // dispatch_timeout.
                    .push(svc::FailFast::layer(
                        "HTTP Server",
                        self.proxy.dispatch_timeout,
                    ))
                    .push(metrics.http_errors.clone())
                    // Synthesizes responses for proxy errors.
                    .push(errors::layer())
                    .push(TraceContext::layer(span_sink.map(|span_sink| {
                        SpanConverter::server(span_sink, trace_labels())
                    })))
                    .push(metrics.stack.layer(stack_labels("http", "server")))
                    .push(http::BoxResponse::layer())
                    .push(http::BoxRequest::layer()),
            )
            .push(http::NewNormalizeUri::layer())
            .push_map_target(|(_, t): (_, T)| t)
            .instrument(|(v, _): &(http::Version, _)| debug_span!("http", %v))
            .push(http::NewServeHttp::layer(
                self.proxy.server.h2_settings,
                drain,
            ))
            .check_new_service::<(http::Version, T), I>()
            .into_inner()
    }
}

pub fn trace_labels() -> HashMap<String, String> {
    let mut l = HashMap::new();
    l.insert("direction".to_string(), "inbound".to_string());
    l
}

fn stack_labels(proto: &'static str, name: &'static str) -> metrics::StackLabels {
    metrics::StackLabels::inbound(proto, name)
}

// === impl SkipByPort ===

impl From<indexmap::IndexSet<u16>> for SkipByPort {
    fn from(ports: indexmap::IndexSet<u16>) -> Self {
        SkipByPort(ports.into())
    }
}

impl svc::stack::Switch<listen::Addrs> for SkipByPort {
    fn use_primary(&self, t: &listen::Addrs) -> bool {
        !self.0.contains(&t.target_addr().port())
    }
}

// === impl RefuseNonOpaque ===

impl Into<Error> for RefuseNonOpaque {
    fn into(self) -> Error {
        Error::from(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "Non-opaque connection refused",
        ))
    }
}
