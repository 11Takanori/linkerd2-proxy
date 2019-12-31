use crate::svc::{self, ServiceExt};
use futures::{try_ready, Future, Poll};
use linkerd2_error::Error;
use linkerd2_stack::Make;

#[derive(Copy, Clone, Debug)]
pub struct Layer(());

#[derive(Clone, Debug)]
pub struct MakePending<M> {
    inner: M,
}

/// Creates a `Service` immediately, even while the future making the service
/// is still pending.
pub enum Pending<F, S> {
    Making(F),
    Made(S),
}

pub fn layer() -> Layer {
    Layer(())
}

// === impl Layer ===

impl<M> svc::Layer<M> for Layer {
    type Service = MakePending<M>;

    fn layer(&self, inner: M) -> Self::Service {
        MakePending { inner }
    }
}

// === impl MakePending ===

impl<T, M> Make<T> for MakePending<M>
where
    M: svc::Service<T> + Clone,
{
    type Service = Pending<svc::Oneshot<M, T>, <M as svc::Service<T>>::Response>;

    fn make(&self, target: T) -> Self::Service {
        let fut = self.inner.clone().oneshot(target);
        Pending::Making(fut)
    }
}

// === impl Pending ===

impl<F, S, Req> svc::Service<Req> for Pending<F, S>
where
    F: Future<Item = S>,
    F::Error: Into<Error>,
    S: svc::Service<Req>,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = futures::future::MapErr<S::Future, fn(S::Error) -> Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        loop {
            *self = match self {
                Pending::Making(fut) => {
                    let svc = try_ready!(fut.poll().map_err(Into::into));
                    Pending::Made(svc)
                }
                Pending::Made(svc) => return svc.poll_ready().map_err(Into::into),
            };
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        if let Pending::Made(ref mut s) = self {
             return s.call(req).map_err(Into::into);
        }

        panic!("pending not ready yet"),
    }
}
