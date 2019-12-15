use super::{WeightedAddr, WithAddr};
use futures::{future, try_ready, Async, Future, Poll};
use indexmap::IndexMap;
use linkerd2_addr::NameAddr;
use linkerd2_error::Error;
use linkerd2_stack::Make;
use rand::distributions::{Distribution, WeightedIndex};
use rand::rngs::SmallRng;
use tokio::sync::watch;
pub use tokio::sync::watch::error::SendError;

pub fn forward<T, M>(target: T, make: M, rng: SmallRng) -> (Service<M::Service>, Update<T, M>)
where
    T: Clone + WithAddr,
    M: Make<T>,
    M::Service: Clone,
{
    let routes = Inner::Forward {
        addr: None,
        service: make.make(target.clone()),
    };
    let (tx, rx) = watch::channel(routes.clone());
    let concrete = Service {
        routes: routes.clone(),
        updates: rx.clone(),
        next_split_index: None,
        rng,
    };
    let update = Update {
        target,
        make,
        tx,
        routes,
    };
    (concrete, update)
}

#[derive(Clone, Debug)]
pub struct Service<S> {
    routes: Inner<S>,
    updates: watch::Receiver<Inner<S>>,
    next_split_index: Option<usize>,
    rng: SmallRng,
}

#[derive(Debug)]
pub struct Update<T, M: Make<T>> {
    target: T,
    make: M,
    routes: Inner<M::Service>,
    tx: watch::Sender<Inner<M::Service>>,
}

#[derive(Clone, Debug)]
enum Inner<S> {
    Forward {
        addr: Option<NameAddr>,
        service: S,
    },
    Split {
        distribution: WeightedIndex<u32>,
        services: IndexMap<NameAddr, S>,
    },
}

impl<Req, S> tower::Service<Req> for Service<S>
where
    S: tower::Service<Req> + Clone,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::MapErr<S::Future, fn(S::Error) -> Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        loop {
            match self.updates.poll_ref().map_err(Error::from)? {
                Async::NotReady | Async::Ready(None) => break,
                Async::Ready(Some(routes)) => {
                    self.next_split_index = None;
                    self.routes = (*routes).clone();
                }
            }
        }

        match self.routes {
            Inner::Forward {
                ref mut service, ..
            } => {
                try_ready!(service.poll_ready().map_err(Into::into));
                Ok(Async::Ready(()))
            }

            Inner::Split {
                ref distribution,
                ref mut services,
            } => {
                // Note: this may not poll all inner services, but at least
                // polls _some_ inner services.
                debug_assert!(services.len() > 1);
                for _ in 0..services.len() {
                    let idx = distribution.sample(&mut self.rng);
                    let (_, svc) = services
                        .get_index_mut(idx)
                        .expect("split index out of range");
                    if svc.poll_ready().map_err(Into::into)?.is_ready() {
                        self.next_split_index = Some(idx);
                        return Ok(Async::Ready(()));
                    }
                }

                // We at least did some polling.
                Ok(Async::NotReady)
            }
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        match self.routes {
            Inner::Forward {
                ref mut service, ..
            } => service.call(req).map_err(Into::into),

            Inner::Split {
                ref mut services, ..
            } => {
                let idx = self
                    .next_split_index
                    .take()
                    .expect("concrete router is not ready");
                let (_, svc) = services
                    .get_index_mut(idx)
                    .expect("split index out of range");
                svc.call(req).map_err(Into::into)
            }
        }
    }
}

impl<T, M> Update<T, M>
where
    T: Clone,
    M: Make<T>,
    M::Service: Clone,
{
    pub fn set_forward(&mut self) -> Result<(), error::LostService> {
        if let Inner::Forward { addr: None, .. } = self.routes {
            // Already set.
            return Ok(());
        };

        self.routes = Inner::Forward {
            service: self.make.make(self.target.clone()),
            addr: None,
        };

        self.tx
            .broadcast(self.routes.clone())
            .map_err(|_| error::LostService(()))
    }

    pub fn set_split(&mut self, mut addrs: Vec<WeightedAddr>) -> Result<(), error::LostService>
    where
        T: WithAddr,
    {
        let routes = match self.routes {
            Inner::Forward { ref addr, .. } => {
                if addrs.len() == 1 {
                    let new_addr = addrs.pop().unwrap().addr;
                    if addr.as_ref().map(|a| a == &new_addr).unwrap_or(false) {
                        // Already set.
                        return Ok(());
                    }

                    let service = {
                        let t = self.target.clone().with_addr(new_addr.clone());
                        self.make.make(t)
                    };
                    Inner::Forward {
                        addr: Some(new_addr),
                        service,
                    }
                } else {
                    let distribution = WeightedIndex::new(addrs.iter().map(|w| w.weight))
                        .expect("invalid weight distribution");
                    let services = addrs
                        .into_iter()
                        .map(|wa| {
                            let t = self.target.clone().with_addr(wa.addr.clone());
                            let s = self.make.make(t);
                            (wa.addr, s)
                        })
                        .collect::<IndexMap<_, _>>();
                    Inner::Split {
                        distribution,
                        services,
                    }
                }
            }
            Inner::Split { ref services, .. } => {
                let distribution = WeightedIndex::new(addrs.iter().map(|w| w.weight))
                    .expect("invalid weight distribution");
                let prior = services;
                let mut services = IndexMap::with_capacity(addrs.len());
                for w in addrs.into_iter() {
                    match prior.get(&w.addr) {
                        None => {
                            let t = self.target.clone().with_addr(w.addr.clone());
                            let s = self.make.make(t);
                            services.insert(w.addr, s);
                        }
                        Some(s) => {
                            services.insert(w.addr, (*s).clone());
                        }
                    }
                }
                Inner::Split {
                    distribution,
                    services,
                }
            }
        };

        self.routes = routes.clone();
        self.tx
            .broadcast(routes)
            .map_err(|_| error::LostService(()))
    }
}

pub mod error {
    #[derive(Debug)]
    pub struct LostService(pub(super) ());

    impl std::fmt::Display for LostService {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "services lost")
        }
    }

    impl std::error::Error for LostService {}
}
