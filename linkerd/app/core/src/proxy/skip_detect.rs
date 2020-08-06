use futures::prelude::*;
use linkerd2_error::Error;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tower::util::ServiceExt;

pub trait SkipTarget<T> {
    fn skip_target(&self, target: &T) -> bool;
}

pub struct SkipDetect<S, D, F> {
    skip: S,
    detect: D,
    tcp: F,
}

pub enum Accept<D, F> {
    Detect(D),
    Tcp(F),
}

impl<S, D, F> SkipDetect<S, D, F> {
    pub fn new(skip: S, detect: D, tcp: F) -> Self {
        Self { skip, detect, tcp }
    }
}

impl<T, S, D, F> tower::Service<T> for SkipDetect<S, D, F>
where
    T: Send + 'static,
    S: SkipTarget<T>,
    D: tower::Service<T> + Clone + Send + 'static,
    D::Error: Into<Error>,
    D::Future: Send,
    F: tower::Service<T> + Clone + Send + 'static,
    F::Error: Into<Error>,
    F::Future: Send,
{
    type Response = Accept<D::Response, F::Response>;
    type Error = Error;
    type Future = Pin<
        Box<dyn Future<Output = Result<Accept<D::Response, F::Response>, Error>> + Send + 'static>,
    >;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        if self.skip.skip_target(&target) {
            let tcp = self.tcp.clone();
            Box::pin(async move {
                let f = tcp.oneshot(target).err_into::<Error>().await?;
                Ok(Accept::Tcp(f))
            })
        } else {
            let detect = self.detect.clone();
            Box::pin(async move {
                let d = detect.oneshot(target).err_into::<Error>().await?;
                Ok(Accept::Detect(d))
            })
        }
    }
}

impl<D, F, T> tower::Service<T> for Accept<D, F>
where
    D: tower::Service<T, Response = ()>,
    D::Error: Into<Error>,
    D::Future: Send + 'static,
    F: tower::Service<T, Response = ()>,
    F::Error: Into<Error>,
    F::Future: Send + 'static,
{
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(match self {
            Self::Detect(d) => futures::ready!(d.poll_ready(cx)).map_err(Into::into),
            Self::Tcp(f) => futures::ready!(f.poll_ready(cx)).map_err(Into::into),
        })
    }

    fn call(&mut self, io: T) -> Self::Future {
        match self {
            Self::Detect(d) => Box::pin(d.call(io).err_into::<Error>()),
            Self::Tcp(f) => Box::pin(f.call(io).err_into::<Error>()),
        }
    }
}
