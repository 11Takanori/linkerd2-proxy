use indexmap::IndexMap;
use linkerd2_app_core::{
    dst, metric_labels,
    metric_labels::{prefix_labels, EndpointLabels},
    proxy::{
        api_resolve::{Metadata, ProtocolHint},
        http::{self, identity_from_header, profiles},
        identity,
        resolve::map_endpoint::MapEndpoint,
        tap,
    },
    router,
    transport::{connect, tls},
    Addr, Conditional, NameAddr, L5D_REQUIRE_ID,
};
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Copy, Clone, Debug)]
pub struct FromMetadata;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Logical {
    pub dst: Addr,
    pub settings: http::Settings,
}

#[derive(Clone, Debug)]
pub struct LogicalOrFallbackTarget(tls::accept::Meta);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Profile(Addr);

#[derive(Clone, Debug)]
pub struct ProfileTarget;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Concrete {
    pub dst: Addr,
    pub settings: http::Settings,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Endpoint {
    pub addr: SocketAddr,
    pub identity: tls::PeerIdentity,
    pub concrete: Concrete,
    pub metadata: Metadata,
}

impl Endpoint {
    pub fn can_use_orig_proto(&self) -> bool {
        if let ProtocolHint::Unknown = self.metadata.protocol_hint() {
            return false;
        }

        match self.concrete.settings {
            http::Settings::Http2 => false,
            http::Settings::Http1 {
                keep_alive: _,
                wants_h1_upgrade,
                was_absolute_form: _,
            } => !wants_h1_upgrade,
            http::Settings::NotHttp => {
                unreachable!(
                    "Endpoint::can_use_orig_proto called when NotHttp: {:?}",
                    self,
                );
            }
        }
    }
}

impl From<SocketAddr> for Endpoint {
    fn from(addr: SocketAddr) -> Self {
        Self {
            addr,
            metadata: Metadata::empty(),
            identity: Conditional::None(tls::ReasonForNoPeerName::NotHttp.into()),
            concrete: Concrete {
                dst: addr.into(),
                settings: http::Settings::NotHttp,
            },
        }
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.addr.fmt(f)
    }
}

impl std::hash::Hash for Endpoint {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.addr.hash(state);
        self.identity.hash(state);
        self.concrete.hash(state);
        // Ignore metadata.
    }
}

impl tls::HasPeerIdentity for Endpoint {
    fn peer_identity(&self) -> tls::PeerIdentity {
        self.identity.clone()
    }
}

impl connect::HasPeerAddr for Endpoint {
    fn peer_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl http::settings::HasSettings for Endpoint {
    fn http_settings(&self) -> &http::Settings {
        &self.concrete.settings
    }
}

impl http::normalize_uri::ShouldNormalizeUri for Endpoint {
    fn should_normalize_uri(&self) -> Option<http::uri::Authority> {
        if let http::Settings::Http1 {
            was_absolute_form: false,
            ..
        } = self.concrete.settings
        {
            return Some(self.concrete.dst.to_authority());
        }
        None
    }
}

impl tap::Inspect for Endpoint {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<SocketAddr> {
        req.extensions()
            .get::<tls::accept::Meta>()
            .map(|s| s.addrs.peer())
    }

    fn src_tls<'a, B>(
        &self,
        _: &'a http::Request<B>,
    ) -> Conditional<&'a identity::Name, tls::ReasonForNoIdentity> {
        Conditional::None(tls::ReasonForNoPeerName::Loopback.into())
    }

    fn dst_addr<B>(&self, _: &http::Request<B>) -> Option<SocketAddr> {
        Some(self.addr)
    }

    fn dst_labels<B>(&self, _: &http::Request<B>) -> Option<&IndexMap<String, String>> {
        Some(self.metadata.labels())
    }

    fn dst_tls<B>(
        &self,
        _: &http::Request<B>,
    ) -> Conditional<&identity::Name, tls::ReasonForNoIdentity> {
        self.identity.as_ref()
    }

    fn route_labels<B>(&self, req: &http::Request<B>) -> Option<Arc<IndexMap<String, String>>> {
        req.extensions()
            .get::<dst::Route>()
            .map(|r| r.route.labels().clone())
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        true
    }
}

impl MapEndpoint<Concrete, Metadata> for FromMetadata {
    type Out = Endpoint;

    fn map_endpoint(&self, concrete: &Concrete, addr: SocketAddr, metadata: Metadata) -> Endpoint {
        tracing::trace!(%concrete, %addr, ?metadata, "endpoint");
        let identity = metadata
            .identity()
            .cloned()
            .map(Conditional::Some)
            .unwrap_or_else(|| {
                Conditional::None(tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into())
            });
        Endpoint {
            addr,
            identity,
            metadata,
            concrete: concrete.clone(),
        }
    }
}

impl Into<EndpointLabels> for Endpoint {
    fn into(self) -> EndpointLabels {
        use linkerd2_app_core::metric_labels::{Direction, TlsId};
        EndpointLabels {
            dst_concrete: self.concrete.dst.name_addr().cloned(),
            direction: Direction::Out,
            tls_id: self.identity.as_ref().map(|id| TlsId::ServerId(id.clone())),
            labels: prefix_labels("dst", self.metadata.labels().into_iter()),
        }
    }
}

impl std::fmt::Display for Concrete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.dst.fmt(f)
    }
}

// === impl LogicalOrFallbackTarget ===

impl From<tls::accept::Meta> for LogicalOrFallbackTarget {
    fn from(accept: tls::accept::Meta) -> Self {
        LogicalOrFallbackTarget(accept)
    }
}

impl<B> router::Key<http::Request<B>> for LogicalOrFallbackTarget {
    type Key = (Logical, Endpoint);

    fn key(&self, req: &http::Request<B>) -> Self::Key {
        use linkerd2_app_core::{
            http_request_authority_addr, http_request_host_addr, http_request_l5d_override_dst_addr,
        };
        tracing::debug!(headers = ?req.headers());
        let dst = http_request_l5d_override_dst_addr(req)
            .map(|addr| {
                tracing::debug!(%addr, "using dst-override");
                addr
            })
            .or_else(|_| {
                http_request_authority_addr(req).map(|addr| {
                    tracing::debug!(%addr, "using authority");
                    addr
                })
            })
            .or_else(|_| {
                http_request_host_addr(req).map(|addr| {
                    tracing::debug!(%addr, "using host");
                    addr
                })
            })
            .unwrap_or_else(|_| {
                let addr = self.0.addrs.target_addr();
                tracing::debug!(%addr, "using socket target");
                addr.into()
            });

        let settings = http::Settings::from_request(req);

        let logical = Logical {
            dst: dst.clone(),
            settings,
        };

        let fallback = Endpoint {
            addr: self.0.addrs.target_addr(),
            metadata: Metadata::empty(),
            identity: match identity_from_header(req, L5D_REQUIRE_ID) {
                Some(require_id) => Conditional::Some(require_id),
                None => {
                    Conditional::None(tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery.into())
                }
            },
            concrete: logical.clone().into(),
        };

        (logical, fallback)
    }
}

impl http::canonicalize::Target for Logical {
    fn addr(&self) -> &Addr {
        &self.dst
    }

    fn addr_mut(&mut self) -> &mut Addr {
        &mut self.dst
    }
}

impl<'t> From<&'t Logical> for ::http::header::HeaderValue {
    fn from(logical: &'t Logical) -> Self {
        ::http::header::HeaderValue::from_str(&logical.dst.to_string())
            .expect("addr must be a valid header")
    }
}

impl http::normalize_uri::ShouldNormalizeUri for Logical {
    fn should_normalize_uri(&self) -> Option<http::uri::Authority> {
        if let http::Settings::Http1 {
            was_absolute_form: false,
            ..
        } = self.settings
        {
            return Some(self.dst.to_authority());
        }
        None
    }
}

impl profiles::OverrideDestination for Concrete {
    fn dst_mut(&mut self) -> &mut Addr {
        &mut self.dst
    }
}

impl From<Logical> for Concrete {
    fn from(logical: Logical) -> Self {
        Concrete {
            dst: logical.dst.clone(),
            settings: logical.settings,
        }
    }
}

// === impl ProfileTarget ===

impl router::Key<Logical> for ProfileTarget {
    type Key = Profile;

    fn key(&self, t: &Logical) -> Self::Key {
        Profile(t.dst.clone())
    }
}

impl profiles::HasDestination for Profile {
    fn destination(&self) -> Addr {
        self.0.clone()
    }
}

impl profiles::WithRoute for Profile {
    type Route = dst::Route;

    fn with_route(self, route: profiles::Route) -> Self::Route {
        dst::Route {
            route,
            target: self.0.clone(),
            direction: metric_labels::Direction::Out,
        }
    }
}
