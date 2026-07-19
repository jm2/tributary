//! Credential-safe playback request types for remote media backends.
//!
//! Generic library models deliberately do not carry authenticated URLs.
//! A retained remote backend resolves its native identifiers into one of
//! these requests only when playback (or artwork loading) begins.  The
//! endpoint remains safe to copy and inspect; credentials stay in separate,
//! non-serializable storage until the app-owned proxy performs the request.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Weak,
};

use super::identity::{SourceId, TrackId};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use url::Url;

use super::backend::BackendResult;

/// Maximum number of mDNS-advertised addresses retained for one HTTP origin.
///
/// Discovery input is unauthenticated and may contain duplicates or an
/// unreasonable number of records. Sixteen addresses is ample for a
/// multi-homed dual-stack server while keeping each route and reqwest DNS
/// override bounded.
const MAX_ADVERTISED_ROUTE_ADDRESSES: usize = 16;

const ADVERTISED_ROUTE_ORIGIN_MISMATCH: &str =
    "advertised HTTP route does not match the resolved media endpoint origin";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum HttpScheme {
    Http,
    Https,
}

/// A canonical set of discovered socket addresses for one HTTP(S) origin.
///
/// The hostname remains the request authority: this route changes only where
/// reqwest opens a direct connection. Consequently HTTP `Host`, TLS SNI, and
/// certificate identity continue to use the advertised hostname rather than
/// an IP literal.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AdvertisedHttpRoute {
    scheme: HttpScheme,
    hostname: String,
    port: u16,
    addresses: Arc<[SocketAddr]>,
}

impl AdvertisedHttpRoute {
    /// Build a bounded route for an already-validated discovery origin.
    ///
    /// Invalid origins and advertisements with no usable address return
    /// `None`, allowing callers to retain the hostname URL and fall back to
    /// ordinary DNS. Input ports are normalized to the origin's effective
    /// port, because an advertised address may never change HTTP origin.
    pub(crate) fn new(
        origin: &Url,
        addresses: impl IntoIterator<Item = SocketAddr>,
    ) -> Option<Self> {
        if origin.query().is_some() || origin.fragment().is_some() {
            return None;
        }
        let (scheme, hostname, port) = domain_origin(origin)?;
        let addresses = canonical_addresses(addresses, port);
        if addresses.is_empty() {
            return None;
        }

        Some(Self {
            scheme,
            hostname,
            port,
            addresses,
        })
    }

    /// Whether `url` has exactly the scheme, normalized hostname, and
    /// effective port owned by this route. Paths and query data are not part
    /// of an HTTP origin.
    pub(crate) fn matches_origin(&self, url: &Url) -> bool {
        domain_origin(url).is_some_and(|(scheme, hostname, port)| {
            self.scheme == scheme && self.hostname == hostname && self.port == port
        })
    }

    /// Union two advertisements only when they describe the same exact
    /// origin. The result is recanonicalized and remains bounded.
    pub(crate) fn merged_same_origin(&self, other: &Self) -> Option<Self> {
        if self.scheme != other.scheme || self.hostname != other.hostname || self.port != other.port
        {
            return None;
        }

        let addresses = canonical_addresses(
            self.addresses.iter().chain(other.addresses.iter()).copied(),
            self.port,
        );
        Some(Self {
            scheme: self.scheme,
            hostname: self.hostname.clone(),
            port: self.port,
            addresses,
        })
    }

    /// Domain key passed to reqwest's resolver override.
    pub(crate) fn hostname(&self) -> &str {
        &self.hostname
    }

    /// Canonical direct-connection candidates for this origin.
    pub(crate) fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }
}

fn domain_origin(url: &Url) -> Option<(HttpScheme, String, u16)> {
    if url.cannot_be_a_base() || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let scheme = match url.scheme() {
        "http" => HttpScheme::Http,
        "https" => HttpScheme::Https,
        _ => return None,
    };
    let hostname = match url.host()? {
        url::Host::Domain(hostname) => hostname.to_ascii_lowercase(),
        url::Host::Ipv4(_) | url::Host::Ipv6(_) => return None,
    };
    let port = url.port_or_known_default()?;
    Some((scheme, hostname, port))
}

fn canonical_addresses(
    addresses: impl IntoIterator<Item = SocketAddr>,
    port: u16,
) -> Arc<[SocketAddr]> {
    let mut addresses: Vec<_> = addresses
        .into_iter()
        .filter_map(|address| canonical_address(address, port))
        .collect();
    addresses.sort_unstable();
    addresses.dedup();
    addresses.truncate(MAX_ADVERTISED_ROUTE_ADDRESSES);
    addresses.into()
}

fn canonical_address(address: SocketAddr, port: u16) -> Option<SocketAddr> {
    match address {
        SocketAddr::V4(address) => {
            let ip = *address.ip();
            if ip.is_unspecified() || ip.is_multicast() || ip == Ipv4Addr::BROADCAST {
                return None;
            }
            Some(SocketAddrV4::new(ip, port).into())
        }
        SocketAddr::V6(address) => {
            let ip = *address.ip();
            if ip.is_unspecified() || ip.is_multicast() {
                return None;
            }
            let scope_id = if ip.is_unicast_link_local() {
                if address.scope_id() == 0 {
                    return None;
                }
                address.scope_id()
            } else {
                0
            };
            Some(SocketAddrV6::new(ip, port, 0, scope_id).into())
        }
    }
}

/// Revocable ownership guard attached to an already-resolved media request.
///
/// Clones observe the same atomic state.  The source registry revokes the
/// lease when a connection is replaced or released, allowing every proxy
/// ticket derived from that session to fail closed immediately.
#[derive(Clone)]
pub struct MediaLease {
    active: Arc<AtomicBool>,
}

/// Credential-free HTTP(S) stream locator retained only by a live source view.
///
/// The URL is intentionally crate-private: generic models, GTK rows, and
/// playback queues retain only typed source/media identity. Ordinary query
/// data is allowed because public radio streams commonly require request
/// shaping parameters; userinfo and fragments remain outside the locator
/// boundary.
#[derive(Clone, Eq, PartialEq)]
pub struct PublicHttpEndpoint {
    endpoint: Url,
}

impl PublicHttpEndpoint {
    pub(crate) fn new(endpoint: Url) -> BackendResult<Self> {
        validate_public_endpoint(&endpoint)?;
        Ok(Self { endpoint })
    }

    fn cloned_url(&self) -> Url {
        self.endpoint.clone()
    }
}

/// Weak final-consumption authority for a public request.
///
/// Implemented by the source registry without introducing an architecture-to-
/// lifecycle dependency. A pending request holds only `Weak` authority, so it
/// cannot keep the registry, source session, or accepted view alive.
pub trait PublicHttpAuthority: Send + Sync {
    fn is_current_public_stream(
        &self,
        source_id: SourceId,
        session_epoch: u64,
        winner_generation: u64,
        track_id: &TrackId,
    ) -> bool;
}

/// One-shot public stream request resolved from the newest accepted view.
///
/// Resolution alone is not authority to load the URL. [`Self::consume`]
/// rechecks the exact winning generation through a weak registry handle and
/// also checks the per-view lease. Replacing/removing the winning view,
/// disconnecting its source, or dropping the final registry handle therefore
/// fails closed even after resolution and before downstream consumption.
pub struct ResolvedPublicHttpRequest {
    endpoint: PublicHttpEndpoint,
    lease: MediaLease,
    authority: Weak<dyn PublicHttpAuthority>,
    source_id: SourceId,
    track_id: TrackId,
    session_epoch: u64,
    winner_generation: u64,
}

impl ResolvedPublicHttpRequest {
    pub(crate) fn new(
        endpoint: PublicHttpEndpoint,
        lease: MediaLease,
        authority: Weak<dyn PublicHttpAuthority>,
        source_id: SourceId,
        track_id: TrackId,
        session_epoch: u64,
        winner_generation: u64,
    ) -> Self {
        Self {
            endpoint,
            lease,
            authority,
            source_id,
            track_id,
            session_epoch,
            winner_generation,
        }
    }

    /// Consume this request immediately before passing its URL to an output.
    pub fn consume(self) -> BackendResult<Url> {
        if !self.lease.is_active() {
            return Err(crate::architecture::error::BackendError::Internal(
                anyhow::anyhow!("public media view is no longer active"),
            ));
        }
        let authority = self.authority.upgrade().ok_or_else(|| {
            crate::architecture::error::BackendError::Internal(anyhow::anyhow!(
                "source registry is no longer active"
            ))
        })?;
        if !authority.is_current_public_stream(
            self.source_id,
            self.session_epoch,
            self.winner_generation,
            &self.track_id,
        ) || !self.lease.is_active()
        {
            return Err(crate::architecture::error::BackendError::Internal(
                anyhow::anyhow!("public media view changed before consumption"),
            ));
        }
        Ok(self.endpoint.cloned_url())
    }

    fn is_active(&self) -> bool {
        self.lease.is_active()
            && self.authority.upgrade().is_some_and(|authority| {
                authority.is_current_public_stream(
                    self.source_id,
                    self.session_epoch,
                    self.winner_generation,
                    &self.track_id,
                )
            })
            && self.lease.is_active()
    }
}

/// At-use media request returned by a managed source adapter.
pub enum MediaRequest {
    /// Credential-isolated request consumed by the app-owned media proxy.
    ProtectedHttp(Box<ResolvedHttpRequest>),
    /// Credential-free public URL with exact accepted-view authority.
    PublicHttp(ResolvedPublicHttpRequest),
}

impl MediaRequest {
    pub(crate) fn is_active(&self) -> bool {
        match self {
            Self::ProtectedHttp(request) => request.is_active(),
            Self::PublicHttp(request) => request.is_active(),
        }
    }
}

/// Compatibility-neutral name used at the playback boundary.
pub type ResolvedStream = MediaRequest;

impl MediaLease {
    pub(crate) fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(true)),
        }
    }

    pub(crate) fn revoke(&self) {
        self.active.store(false, Ordering::Release);
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

/// A resolved HTTP request whose credential material is isolated from its URL.
///
/// This type is intentionally `Clone` but neither `Debug` nor serializable.
/// Required protocol headers, sensitive headers, and private query pairs must
/// only be applied by the app-owned media proxy immediately before its
/// exact-origin upstream fetch.
#[derive(Clone)]
pub struct ResolvedHttpRequest {
    endpoint: Url,
    required_headers: HeaderMap,
    sensitive_headers: HeaderMap,
    private_query_pairs: Vec<(String, String)>,
    advertised_route: Option<AdvertisedHttpRoute>,
    lease: Option<MediaLease>,
}

impl ResolvedHttpRequest {
    /// Start a resolved request from a credential-free HTTP(S) endpoint.
    pub(crate) fn new(endpoint: Url) -> BackendResult<Self> {
        validate_endpoint(&endpoint)?;
        Ok(Self {
            endpoint,
            required_headers: HeaderMap::new(),
            sensitive_headers: HeaderMap::new(),
            private_query_pairs: Vec::new(),
            advertised_route: None,
            lease: None,
        })
    }

    /// Add a fixed, non-secret header required by the remote media protocol.
    ///
    /// This deliberately narrow allowlist covers DAAP's content negotiation
    /// and client-identification contract without admitting receiver-owned,
    /// authentication, routing, proxy, framing, or hop-by-hop headers.
    pub(crate) fn with_required_header(
        mut self,
        name: HeaderName,
        value: HeaderValue,
    ) -> BackendResult<Self> {
        if !is_allowed_required_header(&name) {
            return Err(anyhow::anyhow!("media request required header is not allowlisted").into());
        }
        self.required_headers.insert(name, value);
        Ok(self)
    }

    /// Add an explicitly allowlisted authentication header.
    ///
    /// Values are marked sensitive before storage so even an accidental
    /// `HeaderMap` diagnostic redacts them.
    pub(crate) fn with_sensitive_header(
        mut self,
        name: HeaderName,
        mut value: HeaderValue,
    ) -> BackendResult<Self> {
        if !is_allowed_auth_header(&name) {
            return Err(anyhow::anyhow!("media request header is not allowlisted").into());
        }
        value.set_sensitive(true);
        self.sensitive_headers.insert(name, value);
        Ok(self)
    }

    /// Add a private Subsonic or DAAP authentication query pair.
    ///
    /// Keeping this API to the protocol-defined credential keys prevents
    /// arbitrary request-shaping state from crossing the resolver/proxy trust
    /// boundary. DAAP's `session-id` is a bearer credential just like the
    /// isolated Subsonic fields.
    pub(crate) fn with_private_query_pair(
        mut self,
        key: &str,
        value: impl Into<String>,
    ) -> BackendResult<Self> {
        if !matches!(key, "u" | "t" | "s" | "p" | "session-id") {
            return Err(anyhow::anyhow!("media request query key is not allowlisted").into());
        }
        self.private_query_pairs
            .push((key.to_string(), value.into()));
        Ok(self)
    }

    /// Attach the registry lease that owns this resolved request.
    pub(crate) fn with_lease(mut self, lease: MediaLease) -> Self {
        self.lease = Some(lease);
        self
    }

    /// Preserve a discovered direct route for this request's exact origin.
    pub(crate) fn with_advertised_route(
        mut self,
        route: AdvertisedHttpRoute,
    ) -> BackendResult<Self> {
        if !route.matches_origin(&self.endpoint) {
            return Err(anyhow::anyhow!(ADVERTISED_ROUTE_ORIGIN_MISMATCH).into());
        }
        self.advertised_route = Some(route);
        Ok(self)
    }

    pub(crate) fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    pub(crate) fn required_headers(&self) -> &HeaderMap {
        &self.required_headers
    }

    pub(crate) fn sensitive_headers(&self) -> &HeaderMap {
        &self.sensitive_headers
    }

    pub(crate) fn private_query_pairs(&self) -> &[(String, String)] {
        &self.private_query_pairs
    }

    pub(crate) fn advertised_route(&self) -> Option<&AdvertisedHttpRoute> {
        self.advertised_route.as_ref()
    }

    /// Whether the source session that issued this request still owns it.
    pub(crate) fn is_active(&self) -> bool {
        self.lease.as_ref().is_none_or(MediaLease::is_active)
    }
}

/// Playback-time resolver retained by a live remote source session.
#[async_trait]
pub trait RemoteMediaResolver: Send + Sync {
    /// Resolve an exact backend-native track ID into a credential-isolated request.
    async fn resolve_stream(&self, track_id: &TrackId) -> BackendResult<ResolvedHttpRequest>;

    /// Resolve artwork for an exact backend-native track ID, when available.
    async fn resolve_artwork(
        &self,
        track_id: &TrackId,
    ) -> BackendResult<Option<ResolvedHttpRequest>>;
}

fn validate_endpoint(endpoint: &Url) -> BackendResult<()> {
    let structurally_valid = !endpoint.cannot_be_a_base()
        && matches!(endpoint.scheme(), "http" | "https")
        && endpoint.host_str().is_some()
        && endpoint.username().is_empty()
        && endpoint.password().is_none()
        && endpoint.fragment().is_none();
    if !structurally_valid {
        return Err(anyhow::anyhow!(
            "resolved media endpoint must be an HTTP(S) URL without userinfo or a fragment"
        )
        .into());
    }

    // These credential shapes belong in the isolated fields above, never in
    // the inspectable endpoint.  The short keys are reserved here because the
    // only current producer that needs them is the Subsonic resolver.
    let has_embedded_secret = endpoint.query_pairs().any(|(key, _)| {
        matches!(
            key.to_ascii_lowercase().as_str(),
            "u" | "t"
                | "s"
                | "p"
                | "token"
                | "api_key"
                | "apikey"
                | "access_token"
                | "authorization"
                | "x-plex-token"
                | "session-id"
        )
    });
    if has_embedded_secret {
        return Err(
            anyhow::anyhow!("resolved media endpoint contains credential query state").into(),
        );
    }

    Ok(())
}

fn validate_public_endpoint(endpoint: &Url) -> BackendResult<()> {
    let structurally_valid = !endpoint.cannot_be_a_base()
        && matches!(endpoint.scheme(), "http" | "https")
        && endpoint.host_str().is_some()
        && endpoint.username().is_empty()
        && endpoint.password().is_none()
        && endpoint.fragment().is_none();
    if !structurally_valid {
        return Err(anyhow::anyhow!(
            "public media endpoint must be an HTTP(S) URL without userinfo or a fragment"
        )
        .into());
    }
    Ok(())
}

fn is_allowed_auth_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization" | "x-emby-authorization" | "x-plex-token"
    )
}

fn is_allowed_required_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "accept" | "user-agent" | "client-daap-version" | "client-daap-access-index"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn endpoint_rejects_embedded_credentials_and_unsafe_schemes() {
        for endpoint in [
            "https://user:password@example.test/audio",
            "https://example.test/audio?api_key=secret",
            "https://example.test/audio?session-id=secret",
            "file:///tmp/audio.flac",
        ] {
            let result = ResolvedHttpRequest::new(Url::parse(endpoint).unwrap());
            assert!(result.is_err());
        }
    }

    #[test]
    fn public_endpoint_accepts_queries_and_rejects_unsafe_url_shapes() {
        for endpoint in [
            "http://radio.example.test/live?codec=aac&mount=main",
            "https://radio.example.test:8443/stream?token=public-station-value",
        ] {
            assert!(PublicHttpEndpoint::new(Url::parse(endpoint).unwrap()).is_ok());
        }

        for endpoint in [
            "https://user:password@radio.example.test/live",
            "https://radio.example.test/live#fragment",
            "file:///tmp/station.m3u",
            "data:audio/aac,fixture",
        ] {
            assert!(PublicHttpEndpoint::new(Url::parse(endpoint).unwrap()).is_err());
        }
        assert!(
            Url::parse("http://").is_err(),
            "URL parsing itself rejects a hostless HTTP locator"
        );
    }

    #[test]
    fn request_builders_keep_required_and_sensitive_header_allowlists_disjoint() {
        let request =
            ResolvedHttpRequest::new(Url::parse("https://example.test/audio?id=track-1").unwrap())
                .unwrap();

        let required = [
            reqwest::header::ACCEPT,
            reqwest::header::USER_AGENT,
            HeaderName::from_static("client-daap-version"),
            HeaderName::from_static("client-daap-access-index"),
        ];
        for name in required {
            assert!(request
                .clone()
                .with_required_header(name.clone(), HeaderValue::from_static("accepted"))
                .is_ok());
            assert!(request
                .clone()
                .with_sensitive_header(name, HeaderValue::from_static("rejected"))
                .is_err());
        }

        let sensitive = [
            reqwest::header::AUTHORIZATION,
            HeaderName::from_static("x-emby-authorization"),
            HeaderName::from_static("x-plex-token"),
        ];
        for name in sensitive {
            assert!(request
                .clone()
                .with_sensitive_header(name.clone(), HeaderValue::from_static("accepted"))
                .is_ok());
            assert!(request
                .clone()
                .with_required_header(name, HeaderValue::from_static("rejected"))
                .is_err());
        }

        for name in [
            reqwest::header::HOST,
            reqwest::header::REFERER,
            reqwest::header::COOKIE,
            reqwest::header::RANGE,
            reqwest::header::CONTENT_LENGTH,
            reqwest::header::CONNECTION,
            HeaderName::from_static("keep-alive"),
            reqwest::header::TE,
            reqwest::header::TRAILER,
            reqwest::header::TRANSFER_ENCODING,
            reqwest::header::UPGRADE,
            reqwest::header::PROXY_AUTHORIZATION,
            HeaderName::from_static("proxy-connection"),
            HeaderName::from_static("x-arbitrary-request-header"),
        ] {
            assert!(request
                .clone()
                .with_required_header(name.clone(), HeaderValue::from_static("rejected"))
                .is_err());
            assert!(request
                .clone()
                .with_sensitive_header(name, HeaderValue::from_static("rejected"))
                .is_err());
        }
        assert!(request
            .clone()
            .with_private_query_pair("session-id", "daap-bearer")
            .is_ok());
        assert!(request
            .with_private_query_pair("redirect", "https://attacker.test")
            .is_err());
    }

    #[test]
    fn revoking_a_lease_invalidates_every_request_clone() {
        let lease = MediaLease::new();
        let request = ResolvedHttpRequest::new(Url::parse("https://example.test/audio").unwrap())
            .unwrap()
            .with_lease(lease.clone());
        let clone = request.clone();

        assert!(request.is_active());
        lease.revoke();
        assert!(!request.is_active());
        assert!(!clone.is_active());
    }

    #[test]
    fn advertised_route_requires_an_exact_http_domain_origin() {
        let address: SocketAddr = "127.0.0.1:9000".parse().unwrap();
        for invalid in [
            "ftp://music.local:9000",
            "https://user:secret@music.local:9000",
            "https://music.local:9000?token=secret",
            "https://music.local:9000#fragment",
            "https://127.0.0.1:9000",
            "file:///music",
        ] {
            assert!(
                AdvertisedHttpRoute::new(&Url::parse(invalid).unwrap(), [address]).is_none(),
                "{invalid} must not create a route"
            );
        }

        let route = AdvertisedHttpRoute::new(
            &Url::parse("https://MUSIC.local:443/base").unwrap(),
            [address],
        )
        .expect("valid route");
        assert_eq!(route.hostname(), "music.local");
        assert!(route.matches_origin(&Url::parse("https://music.local/stream?id=1").unwrap()));
        assert!(!route.matches_origin(&Url::parse("http://music.local:443/stream").unwrap()));
        assert!(!route.matches_origin(&Url::parse("https://other.local/stream").unwrap()));
        assert!(!route.matches_origin(&Url::parse("https://music.local:444/stream").unwrap()));
    }

    #[test]
    fn advertised_addresses_are_sorted_deduplicated_normalized_and_capped() {
        let origin = Url::parse("http://music.local:4533/base").unwrap();
        let mut descending: Vec<SocketAddr> = (1..=24)
            .rev()
            .map(|last| SocketAddr::from(([10, 0, 0, last], 9999)))
            .collect();
        let extra: [SocketAddr; 4] = [
            "10.0.0.3:1".parse().unwrap(),
            "0.0.0.0:1".parse().unwrap(),
            "224.0.0.1:1".parse().unwrap(),
            "255.255.255.255:1".parse().unwrap(),
        ];
        descending.extend(extra);
        let route = AdvertisedHttpRoute::new(&origin, descending).expect("usable addresses");

        let expected: Vec<SocketAddr> = (1..=16)
            .map(|last| SocketAddr::from(([10, 0, 0, last], 4533)))
            .collect();
        assert_eq!(route.addresses(), expected);

        let reordered = AdvertisedHttpRoute::new(
            &origin,
            (1..=24).map(|last| SocketAddr::from(([10, 0, 0, last], 0))),
        )
        .expect("same canonical route");
        assert_eq!(route, reordered, "canonical routes are stable cache keys");
    }

    #[test]
    fn advertised_ipv6_addresses_preserve_only_required_link_local_scopes() {
        use std::net::Ipv6Addr;

        let origin = Url::parse("http://music.local:4533").unwrap();
        let global = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        let link_local = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
        let route = AdvertisedHttpRoute::new(
            &origin,
            [
                SocketAddrV6::new(global, 1, 42, 7).into(),
                SocketAddrV6::new(global, 2, 99, 8).into(),
                SocketAddrV6::new(link_local, 3, 12, 0).into(),
                SocketAddrV6::new(link_local, 4, 34, 9).into(),
            ],
        )
        .expect("scoped route");

        assert_eq!(route.addresses().len(), 2);
        let global = route
            .addresses()
            .iter()
            .find_map(|address| match address {
                SocketAddr::V6(address) if address.ip() == &global => Some(address),
                _ => None,
            })
            .expect("global IPv6 address");
        assert_eq!(global.port(), 4533);
        assert_eq!(global.flowinfo(), 0);
        assert_eq!(global.scope_id(), 0);

        let link_local = route
            .addresses()
            .iter()
            .find_map(|address| match address {
                SocketAddr::V6(address) if address.ip() == &link_local => Some(address),
                _ => None,
            })
            .expect("scoped link-local IPv6 address");
        assert_eq!(link_local.port(), 4533);
        assert_eq!(link_local.flowinfo(), 0);
        assert_eq!(link_local.scope_id(), 9);

        assert!(AdvertisedHttpRoute::new(
            &origin,
            [SocketAddrV6::new(link_local.ip().to_owned(), 1, 0, 0).into()]
        )
        .is_none());
    }

    #[test]
    fn advertised_routes_merge_only_within_the_same_origin() {
        let implicit = Url::parse("https://music.local").unwrap();
        let explicit = Url::parse("https://music.local:443/base").unwrap();
        let first =
            AdvertisedHttpRoute::new(&implicit, [SocketAddr::from(([192, 0, 2, 2], 443))]).unwrap();
        let second =
            AdvertisedHttpRoute::new(&explicit, [SocketAddr::from(([192, 0, 2, 1], 1))]).unwrap();
        let merged = first.merged_same_origin(&second).expect("same origin");
        assert_eq!(
            merged.addresses(),
            [
                SocketAddr::from(([192, 0, 2, 1], 443)),
                SocketAddr::from(([192, 0, 2, 2], 443)),
            ]
        );

        let http = AdvertisedHttpRoute::new(
            &Url::parse("http://music.local:443").unwrap(),
            [SocketAddr::from(([192, 0, 2, 3], 443))],
        )
        .unwrap();
        assert!(first.merged_same_origin(&http).is_none());
    }

    #[test]
    fn resolved_request_accepts_only_a_matching_advertised_route() {
        let matching = AdvertisedHttpRoute::new(
            &Url::parse("https://music.local").unwrap(),
            [SocketAddr::from(([192, 0, 2, 1], 443))],
        )
        .unwrap();
        let request =
            ResolvedHttpRequest::new(Url::parse("https://music.local/stream?id=track-1").unwrap())
                .unwrap()
                .with_advertised_route(matching.clone())
                .expect("matching route");
        assert_eq!(request.advertised_route(), Some(&matching));

        let secret_host = format!("{}.invalid", Uuid::new_v4());
        let mismatched = AdvertisedHttpRoute::new(
            &Url::parse(&format!("https://{secret_host}")).unwrap(),
            [SocketAddr::from(([192, 0, 2, 2], 443))],
        )
        .unwrap();
        let Err(error) =
            ResolvedHttpRequest::new(Url::parse("https://music.local/stream?id=track-2").unwrap())
                .unwrap()
                .with_advertised_route(mismatched)
        else {
            panic!("mismatched route must be rejected");
        };
        let rendered = error.to_string();
        assert_eq!(
            rendered,
            format!("Internal error: {ADVERTISED_ROUTE_ORIGIN_MISMATCH}")
        );
        assert!(!rendered.contains(&secret_host));
    }
}
