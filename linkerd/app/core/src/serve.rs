use crate::{
    io,
    svc::{self, stack::Param},
    transport::{ClientAddr, Remote},
};
use futures::prelude::*;
use linkerd_error::Error;
use tower::util::ServiceExt;
use tracing::instrument::Instrument;
use tracing::{debug, debug_span, info, warn};

/// Spawns a task that binds an `L`-typed listener with an `A`-typed
/// connection-accepting service.
///
/// The task is driven until shutdown is signaled.
pub async fn serve<N, NSvc, T, I>(
    listen: impl Stream<Item = std::io::Result<(T, I)>>,
    mut new_accept: N,
    shutdown: impl Future,
) -> Result<(), Error>
where
    T: Param<Remote<ClientAddr>>,
    I: Send + 'static,
    N: svc::NewService<T, Service = NSvc>,
    NSvc: tower::Service<io::ScopedIo<I>, Response = ()> + Send + 'static,
    NSvc::Error: Into<Error>,
    NSvc::Future: Send + 'static,
{
    let accept = async move {
        futures::pin_mut!(listen);
        loop {
            match listen.next().await {
                None => return Ok(()),
                Some(conn) => {
                    // If the listener returned an error, complete the task.
                    let (target, io) = conn?;

                    // The local addr should be instrumented from the listener's context.
                    let span = debug_span!("accept", client.addr = %target.param());

                    let accept = new_accept.new_service(target);

                    // Dispatch all of the work for a given connection onto a connection-specific task.
                    tokio::spawn(
                        async move {
                            match accept.ready_oneshot().err_into::<Error>().await {
                                Ok(mut accept) => {
                                    match accept
                                        .call(io::ScopedIo::server(io))
                                        .err_into::<Error>()
                                        .await
                                    {
                                        Ok(()) => debug!("Connection closed"),
                                        Err(error) => info!(%error, "Connection closed"),
                                    }
                                    // Hold the service until the connection is
                                    // complete. This helps tie any inner cache
                                    // lifetimes to the services they return.
                                    drop(accept);
                                }
                                Err(error) => {
                                    warn!(%error, "Server failed to become ready");
                                }
                            }
                        }
                        .instrument(span),
                    );
                }
            }
        }
    };

    // Stop the accept loop when the shutdown signal fires.
    //
    // This ensures that the accept service's readiness can't block shutdown.
    tokio::select! {
        res = accept => { res }
        _ = shutdown => { Ok(()) }
    }
}
