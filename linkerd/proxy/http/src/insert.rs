use futures::{try_ready, Future, Poll};
use http;
use linkerd2_stack::{layer, Make, Proxy};
use std::marker::PhantomData;

pub trait Lazy<V>: Clone {
    fn value(&self) -> V;
}

/// Wraps an HTTP `Service` so that the `T -typed value` is cloned into
/// each request's extensions.
#[derive(Clone, Debug)]
pub struct Layer<L, V> {
    lazy: L,
    _marker: PhantomData<fn() -> V>,
}

#[derive(Clone)]
pub struct MakeInsert<M, L, V> {
    inner: M,
    lazy: L,
    _marker: PhantomData<fn() -> V>,
}

pub struct MakeFuture<F, L, V> {
    inner: F,
    lazy: L,
    _marker: PhantomData<fn() -> V>,
}

pub struct Insert<S, L, V> {
    inner: S,
    lazy: L,
    _marker: PhantomData<fn() -> V>,
}

#[derive(Clone, Debug)]
pub struct FnLazy<F>(F);

#[derive(Clone, Debug)]
pub struct ValLazy<V>(V);

pub fn layer<F, V>(f: F) -> Layer<FnLazy<F>, V>
where
    F: Fn() -> V + Clone,
    V: Send + Sync + 'static,
{
    Layer::new(FnLazy(f))
}

// === impl Layer ===

impl<L, V> Layer<L, V>
where
    L: Lazy<V>,
    V: Send + Sync + 'static,
{
    pub fn new(lazy: L) -> Self {
        Self {
            lazy,
            _marker: PhantomData,
        }
    }
}

impl<M, L, V> layer::Layer<M> for Layer<L, V>
where
    L: Lazy<V>,
    V: Send + Sync + 'static,
{
    type Service = MakeInsert<M, L, V>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service {
            inner,
            lazy: self.lazy.clone(),
            _marker: PhantomData,
        }
    }
}

// === impl Make ===

impl<T, M, L, V> Make<T> for MakeInsert<M, L, V>
where
    M: Make<T>,
    L: Lazy<V>,
    V: Send + Sync + 'static,
{
    type Service = Insert<M::Service, L, V>;

    fn make(&self, t: T) -> Self::Service {
        Insert::new(self.inner.make(t), self.lazy.clone())
    }
}

impl<T, M, L, V> tower::Service<T> for MakeInsert<M, L, V>
where
    M: tower::Service<T>,
    L: Lazy<V>,
    V: Send + Sync + 'static,
{
    type Response = Insert<M::Response, L, V>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future, L, V>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, t: T) -> Self::Future {
        Self::Future {
            inner: self.inner.call(t),
            lazy: self.lazy.clone(),
            _marker: PhantomData,
        }
    }
}

// === impl MakeFuture ===

impl<F, L, V> Future for MakeFuture<F, L, V>
where
    F: Future,
    L: Lazy<V>,
    V: Send + Sync + 'static,
{
    type Item = Insert<F::Item, L, V>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let svc = Insert::new(inner, self.lazy.clone());
        Ok(svc.into())
    }
}

// === impl Insert ===

impl<S, L, V> Insert<S, L, V> {
    fn new(inner: S, lazy: L) -> Self {
        Self {
            inner,
            lazy,
            _marker: PhantomData,
        }
    }
}

impl<P, S, L, V, B> Proxy<http::Request<B>, S> for Insert<P, L, V>
where
    P: Proxy<http::Request<B>, S>,
    S: tower::Service<P::Request>,
    L: Lazy<V>,
    V: Clone + Send + Sync + 'static,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = P::Error;
    type Future = P::Future;

    fn proxy(&self, svc: &mut S, mut req: http::Request<B>) -> Self::Future {
        req.extensions_mut().insert(self.lazy.value());
        self.inner.proxy(svc, req)
    }
}

impl<S, L, V, B> tower::Service<http::Request<B>> for Insert<S, L, V>
where
    S: tower::Service<http::Request<B>>,
    L: Lazy<V>,
    V: Clone + Send + Sync + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        req.extensions_mut().insert(self.lazy.value());
        self.inner.call(req)
    }
}

impl<S: Clone, L: Clone, V> Clone for Insert<S, L, V> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            lazy: self.lazy.clone(),
            _marker: self._marker,
        }
    }
}

impl<V> Lazy<V> for ValLazy<V>
where
    V: Clone + Send + Sync + 'static,
{
    fn value(&self) -> V {
        self.0.clone()
    }
}

impl<F, V> Lazy<V> for FnLazy<F>
where
    F: Fn() -> V,
    F: Clone,
    V: Send + Sync + 'static,
{
    fn value(&self) -> V {
        (self.0)()
    }
}

pub mod target {
    use super::*;
    use linkerd2_stack as stack;

    /// Wraps an HTTP `Service` so that the Stack's `T -typed target` is cloned into
    /// each request's extensions.
    #[derive(Clone, Debug)]
    pub struct Make<M>(M);

    pub struct MakeFuture<F, T> {
        inner: F,
        target: T,
    }

    // === impl Layer ===

    pub fn layer<M>() -> impl layer::Layer<M, Service = Make<M>> + Copy {
        layer::mk(Make)
    }

    // === impl Stack ===

    impl<T, M> stack::Make<T> for Make<M>
    where
        T: Clone + Send + Sync + 'static,
        M: stack::Make<T>,
    {
        type Service = Insert<M::Service, super::ValLazy<T>, T>;

        fn make(&self, target: T) -> Self::Service {
            let inner = self.0.make(target.clone());
            super::Insert::new(inner, super::ValLazy(target))
        }
    }

    impl<T, M> tower::Service<T> for Make<M>
    where
        T: Clone + Send + Sync + 'static,
        M: tower::Service<T>,
    {
        type Response = Insert<M::Response, super::ValLazy<T>, T>;
        type Error = M::Error;
        type Future = MakeFuture<M::Future, T>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.0.poll_ready()
        }

        fn call(&mut self, target: T) -> Self::Future {
            let inner = self.0.call(target.clone());
            MakeFuture { inner, target }
        }
    }

    // === impl MakeFuture ===

    impl<F, T> Future for MakeFuture<F, T>
    where
        F: Future,
        T: Clone,
    {
        type Item = Insert<F::Item, ValLazy<T>, T>;
        type Error = F::Error;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            let inner = try_ready!(self.inner.poll());
            let svc = Insert::new(inner, super::ValLazy(self.target.clone()));
            Ok(svc.into())
        }
    }
}
