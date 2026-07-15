//! Credential-safe playback request types for remote media backends.
//!
//! Generic library models deliberately do not carry authenticated URLs.
//! A retained remote backend resolves its native identifiers into one of
//! these requests only when playback (or artwork loading) begins.  The
//! endpoint remains safe to copy and inspect; credentials stay in separate,
//! non-serializable storage until the app-owned proxy performs the request.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use url::Url;
use uuid::Uuid;

use super::backend::BackendResult;

/// Revocable ownership guard attached to an already-resolved media request.
///
/// Clones observe the same atomic state.  The source registry revokes the
/// lease when a connection is replaced or released, allowing every proxy
/// ticket derived from that session to fail closed immediately.
#[derive(Clone)]
pub struct MediaLease {
    active: Arc<AtomicBool>,
}

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
/// Sensitive headers and private query pairs must only be applied by the
/// app-owned media proxy immediately before its exact-origin upstream fetch.
#[derive(Clone)]
pub struct ResolvedHttpRequest {
    endpoint: Url,
    sensitive_headers: HeaderMap,
    private_query_pairs: Vec<(String, String)>,
    lease: Option<MediaLease>,
}

impl ResolvedHttpRequest {
    /// Start a resolved request from a credential-free HTTP(S) endpoint.
    pub(crate) fn new(endpoint: Url) -> BackendResult<Self> {
        validate_endpoint(&endpoint)?;
        Ok(Self {
            endpoint,
            sensitive_headers: HeaderMap::new(),
            private_query_pairs: Vec::new(),
            lease: None,
        })
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

    /// Add a private Subsonic authentication query pair.
    ///
    /// Keeping this API to the four protocol-defined credential keys prevents
    /// arbitrary request-shaping state from crossing the resolver/proxy trust
    /// boundary.
    pub(crate) fn with_private_query_pair(
        mut self,
        key: &str,
        value: impl Into<String>,
    ) -> BackendResult<Self> {
        if !matches!(key, "u" | "t" | "s" | "p") {
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

    pub(crate) fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    pub(crate) fn sensitive_headers(&self) -> &HeaderMap {
        &self.sensitive_headers
    }

    pub(crate) fn private_query_pairs(&self) -> &[(String, String)] {
        &self.private_query_pairs
    }

    /// Whether the source session that issued this request still owns it.
    pub(crate) fn is_active(&self) -> bool {
        self.lease.as_ref().is_none_or(MediaLease::is_active)
    }
}

/// Playback-time resolver retained by a live remote source session.
#[async_trait]
pub trait RemoteMediaResolver: Send + Sync {
    /// Resolve an application track UUID into a credential-isolated request.
    async fn resolve_stream(&self, track_id: &Uuid) -> BackendResult<ResolvedHttpRequest>;

    /// Resolve artwork for an application track UUID, when available.
    async fn resolve_artwork(&self, track_id: &Uuid) -> BackendResult<Option<ResolvedHttpRequest>>;
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
        )
    });
    if has_embedded_secret {
        return Err(
            anyhow::anyhow!("resolved media endpoint contains credential query state").into(),
        );
    }

    Ok(())
}

fn is_allowed_auth_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization" | "x-emby-authorization" | "x-plex-token"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_rejects_embedded_credentials_and_unsafe_schemes() {
        for endpoint in [
            "https://user:password@example.test/audio",
            "https://example.test/audio?api_key=secret",
            "file:///tmp/audio.flac",
        ] {
            let result = ResolvedHttpRequest::new(Url::parse(endpoint).unwrap());
            assert!(result.is_err());
        }
    }

    #[test]
    fn request_builders_allow_only_auth_material() {
        let request =
            ResolvedHttpRequest::new(Url::parse("https://example.test/audio?id=track-1").unwrap())
                .unwrap();

        for name in [
            reqwest::header::HOST,
            reqwest::header::REFERER,
            reqwest::header::COOKIE,
            reqwest::header::RANGE,
            reqwest::header::CONNECTION,
            reqwest::header::TRANSFER_ENCODING,
            reqwest::header::PROXY_AUTHORIZATION,
        ] {
            assert!(request
                .clone()
                .with_sensitive_header(name, HeaderValue::from_static("rejected"))
                .is_err());
        }
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
}
