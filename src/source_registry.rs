//! Ownership registry for standard remote media sources.
//!
//! Remote library rows carry only opaque, credential-free references. The
//! backend that can turn those references into authenticated requests stays in
//! this process-owned registry and is consulted only when media is consumed.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use url::Url;
use uuid::Uuid;

use crate::architecture::media::{MediaLease, RemoteMediaResolver, ResolvedHttpRequest};
use crate::architecture::{SourceId, TrackId};

const MEDIA_REFERENCE_SCHEME: &str = "tributary-remote";
const MEDIA_TRACK_SEGMENT_PREFIX: &str = "id-";

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum RegistryGate {
    #[default]
    Running,
    ShuttingDown,
}

struct ActiveSource {
    generation: u64,
    lease_key: Uuid,
    media_lease: MediaLease,
}

struct LeaseOwner {
    source_id: SourceId,
    generation: u64,
    media_lease: MediaLease,
    resolver: Arc<dyn RemoteMediaResolver>,
}

#[derive(Default)]
struct SourceRegistry {
    gate: RegistryGate,
    next_generation: u64,
    latest_generation: HashMap<SourceId, u64>,
    pending_attempts: HashSet<(SourceId, u64)>,
    by_source: HashMap<SourceId, ActiveSource>,
    by_lease: HashMap<Uuid, LeaseOwner>,
}

fn registry() -> &'static Mutex<SourceRegistry> {
    static REGISTRY: OnceLock<Mutex<SourceRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(SourceRegistry::default()))
}

fn lock_registry() -> MutexGuard<'static, SourceRegistry> {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A generation registered before remote authentication/network I/O starts.
/// Dropping an unfinished attempt removes it and restores any still-active
/// predecessor as the current owner.
pub struct ConnectionAttempt {
    source_id: SourceId,
    generation: u64,
    completed: bool,
}

impl ConnectionAttempt {
    /// Whether this attempt is still the newest allowed owner for its source.
    pub fn is_latest(&self) -> bool {
        let sources = lock_registry();
        sources.gate == RegistryGate::Running
            && sources.latest_generation.get(&self.source_id) == Some(&self.generation)
    }

    /// Retain a resolver if this connection attempt still owns the source.
    /// A replacement immediately revokes requests and references issued by the
    /// old lease, even when the source key is reused for a new login.
    pub fn retain(mut self, resolver: Arc<dyn RemoteMediaResolver>) -> Option<RetainedSource> {
        let mut sources = lock_registry();
        sources
            .pending_attempts
            .remove(&(self.source_id, self.generation));
        self.completed = true;

        let accepted = sources.gate == RegistryGate::Running
            && sources.latest_generation.get(&self.source_id) == Some(&self.generation);
        if !accepted {
            return None;
        }

        if let Some(previous) = sources.by_source.remove(&self.source_id) {
            previous.media_lease.revoke();
            sources.by_lease.remove(&previous.lease_key);
        }

        // UUID v4 collisions are vanishingly unlikely, but a registry key is
        // still selected under the lock and checked rather than overwriting a
        // live lease if one ever occurs.
        let lease_key = loop {
            let candidate = Uuid::new_v4();
            if !sources.by_lease.contains_key(&candidate) {
                break candidate;
            }
        };
        let media_lease = MediaLease::new();

        sources.by_source.insert(
            self.source_id,
            ActiveSource {
                generation: self.generation,
                lease_key,
                media_lease: media_lease.clone(),
            },
        );
        sources.by_lease.insert(
            lease_key,
            LeaseOwner {
                source_id: self.source_id,
                generation: self.generation,
                media_lease,
                resolver,
            },
        );

        Some(RetainedSource {
            source_id: self.source_id,
            generation: self.generation,
            lease_key,
        })
    }
}

impl Drop for ConnectionAttempt {
    fn drop(&mut self) {
        if self.completed {
            return;
        }

        let mut sources = lock_registry();
        sources
            .pending_attempts
            .remove(&(self.source_id, self.generation));
        if sources.gate == RegistryGate::Running
            && sources.latest_generation.get(&self.source_id) == Some(&self.generation)
        {
            if let Some(active_generation) = sources
                .by_source
                .get(&self.source_id)
                .map(|source| source.generation)
            {
                sources
                    .latest_generation
                    .insert(self.source_id, active_generation);
            } else {
                sources.latest_generation.remove(&self.source_id);
            }
        }
    }
}

/// Proof that one standard remote source owns a generation and opaque lease.
#[derive(Clone)]
pub struct RetainedSource {
    source_id: SourceId,
    generation: u64,
    lease_key: Uuid,
}

impl RetainedSource {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn lease_key(&self) -> Uuid {
        self.lease_key
    }

    pub fn is_current(&self) -> bool {
        is_current_source(self.source_id, self.generation, self.lease_key)
    }
}

/// Register source ownership before starting authentication or other network
/// I/O. Returns `None` after controlled shutdown closes the registry gate.
pub fn begin_connect(source_id: SourceId) -> Option<ConnectionAttempt> {
    let mut sources = lock_registry();
    if sources.gate != RegistryGate::Running {
        return None;
    }

    sources.next_generation = sources.next_generation.wrapping_add(1).max(1);
    let generation = sources.next_generation;
    sources.latest_generation.insert(source_id, generation);
    sources.pending_attempts.insert((source_id, generation));

    Some(ConnectionAttempt {
        source_id,
        generation,
        completed: false,
    })
}

/// Revoke the active lease and invalidate pending attempts for one source.
pub fn release_source(source_id: SourceId) -> bool {
    let mut sources = lock_registry();
    sources.latest_generation.remove(&source_id);
    let Some(source) = sources.by_source.remove(&source_id) else {
        return false;
    };
    source.media_lease.revoke();
    sources.by_lease.remove(&source.lease_key);
    true
}

/// Verify ownership carried by a queued remote-library publication.
pub fn is_current_source(source_id: SourceId, generation: u64, lease_key: Uuid) -> bool {
    let sources = lock_registry();
    sources.gate == RegistryGate::Running
        && sources
            .by_source
            .get(&source_id)
            .is_some_and(|source| source.generation == generation && source.lease_key == lease_key)
}

/// Close the registry and revoke every standard remote media lease.
pub fn begin_shutdown() {
    let mut sources = lock_registry();
    if sources.gate != RegistryGate::Running {
        return;
    }
    sources.gate = RegistryGate::ShuttingDown;
    sources.latest_generation.clear();
    sources.pending_attempts.clear();
    for source in sources.by_source.values() {
        source.media_lease.revoke();
    }
    sources.by_source.clear();
    sources.by_lease.clear();
}

/// Build an opaque playable reference. It contains no source address or
/// credentials and is useful only while its exact registry lease is active.
pub fn stream_reference(lease_key: Uuid, track_id: &TrackId) -> String {
    media_reference(lease_key, MediaKind::Stream, track_id)
}

/// Build an opaque artwork reference with the same lease isolation as streams.
pub fn artwork_reference(lease_key: Uuid, track_id: &TrackId) -> String {
    media_reference(lease_key, MediaKind::Artwork, track_id)
}

fn media_reference(lease_key: Uuid, kind: MediaKind, track_id: &TrackId) -> String {
    format!(
        "{MEDIA_REFERENCE_SCHEME}://{lease_key}/{}/{}",
        kind.path_component(),
        encode_track_segment(track_id)
    )
}

/// Encode an exact UTF-8 native identifier into one URL segment that cannot
/// be interpreted as `.` or `..` by an RFC 3986 parser.
///
/// The fixed non-dot prefix and lowercase hex alphabet make the representation
/// canonical, reversible, and independent of percent-decoder behavior.
fn encode_track_segment(track_id: &TrackId) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = track_id.as_str().as_bytes();
    let mut encoded = String::with_capacity(
        MEDIA_TRACK_SEGMENT_PREFIX
            .len()
            .saturating_add(bytes.len().saturating_mul(2)),
    );
    encoded.push_str(MEDIA_TRACK_SEGMENT_PREFIX);
    for &byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn decode_track_segment(segment: &str) -> Result<TrackId, MediaReferenceError> {
    let encoded = segment
        .strip_prefix(MEDIA_TRACK_SEGMENT_PREFIX)
        .ok_or(MediaReferenceError::Malformed)?;
    if encoded.is_empty() || !encoded.len().is_multiple_of(2) {
        return Err(MediaReferenceError::Malformed);
    }

    let mut decoded = Vec::with_capacity(encoded.len() / 2);
    for pair in encoded.as_bytes().chunks_exact(2) {
        let high = decode_hex_nibble(pair[0]).ok_or(MediaReferenceError::Malformed)?;
        let low = decode_hex_nibble(pair[1]).ok_or(MediaReferenceError::Malformed)?;
        decoded.push((high << 4) | low);
    }
    let value = String::from_utf8(decoded).map_err(|_| MediaReferenceError::Malformed)?;
    TrackId::remote(value).map_err(|_| MediaReferenceError::Malformed)
}

const fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Return true for references owned by this registry, including malformed
/// ones that must fail closed instead of being passed to a media backend.
pub fn is_media_reference(reference: &str) -> bool {
    crate::daap::is_media_reference(reference)
        || reference
            .split_once(':')
            .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case(MEDIA_REFERENCE_SCHEME))
}

/// Whether a playable reference is owned by one exact retained lease.
///
/// GTK uses this at remote-library publication time to distinguish a duplicate
/// snapshot from a same-source replacement. A malformed or wrong-kind value
/// fails closed and therefore cannot preserve a queue across replacement.
pub fn stream_reference_uses_lease(reference: &str, lease_key: Uuid) -> bool {
    parse_reference(reference, MediaKind::Stream).is_ok_and(|parsed| parsed.lease_key == lease_key)
}

/// Resolve one exact stream reference without exposing backend failures.
pub async fn resolve_stream_reference(
    reference: &str,
) -> Result<ResolvedHttpRequest, MediaReferenceError> {
    if crate::daap::is_media_reference(reference) {
        return crate::daap::resolve_stream_reference(reference)
            .map_err(|_| MediaReferenceError::Unavailable);
    }
    let parsed = parse_reference(reference, MediaKind::Stream)?;
    let (resolver, media_lease) = resolver_for(&parsed)?;
    let request = resolver
        .resolve_stream(&parsed.track_id)
        .await
        .map_err(|_| MediaReferenceError::Unavailable)?;
    ensure_current(&parsed)?;
    Ok(request.with_lease(media_lease))
}

/// Resolve one exact artwork reference without exposing backend failures.
pub async fn resolve_artwork_reference(
    reference: &str,
) -> Result<Option<ResolvedHttpRequest>, MediaReferenceError> {
    if crate::daap::is_media_reference(reference) {
        return crate::daap::resolve_artwork_reference(reference)
            .map(Some)
            .map_err(|_| MediaReferenceError::Unavailable);
    }
    let parsed = parse_reference(reference, MediaKind::Artwork)?;
    let (resolver, media_lease) = resolver_for(&parsed)?;
    let request = resolver
        .resolve_artwork(&parsed.track_id)
        .await
        .map_err(|_| MediaReferenceError::Unavailable)?;
    ensure_current(&parsed)?;
    Ok(request.map(|request| request.with_lease(media_lease)))
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum MediaKind {
    Stream,
    Artwork,
}

impl MediaKind {
    const fn path_component(self) -> &'static str {
        match self {
            Self::Stream => "stream",
            Self::Artwork => "artwork",
        }
    }
}

struct ParsedReference {
    lease_key: Uuid,
    track_id: TrackId,
}

fn parse_reference(
    reference: &str,
    expected_kind: MediaKind,
) -> Result<ParsedReference, MediaReferenceError> {
    let parsed = Url::parse(reference).map_err(|_| MediaReferenceError::Malformed)?;
    if parsed.scheme() != MEDIA_REFERENCE_SCHEME
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(MediaReferenceError::Malformed);
    }

    let lease_key = parsed
        .host_str()
        .ok_or(MediaReferenceError::Malformed)?
        .parse::<Uuid>()
        .map_err(|_| MediaReferenceError::Malformed)?;
    let mut segments = parsed
        .path_segments()
        .ok_or(MediaReferenceError::Malformed)?;
    let kind = match segments.next() {
        Some("stream") => MediaKind::Stream,
        Some("artwork") => MediaKind::Artwork,
        _ => return Err(MediaReferenceError::Malformed),
    };
    if kind != expected_kind {
        return Err(MediaReferenceError::WrongKind);
    }
    let encoded_track_id = segments.next().ok_or(MediaReferenceError::Malformed)?;
    let track_id = decode_track_segment(encoded_track_id)?;
    if segments.next().is_some() {
        return Err(MediaReferenceError::Malformed);
    }

    Ok(ParsedReference {
        lease_key,
        track_id,
    })
}

fn resolver_for(
    reference: &ParsedReference,
) -> Result<(Arc<dyn RemoteMediaResolver>, MediaLease), MediaReferenceError> {
    let sources = lock_registry();
    if sources.gate != RegistryGate::Running {
        return Err(MediaReferenceError::Unavailable);
    }
    let owner = sources
        .by_lease
        .get(&reference.lease_key)
        .ok_or(MediaReferenceError::Unavailable)?;
    let current = sources
        .by_source
        .get(&owner.source_id)
        .is_some_and(|source| {
            source.generation == owner.generation && source.lease_key == reference.lease_key
        });
    if !current {
        return Err(MediaReferenceError::Unavailable);
    }
    Ok((Arc::clone(&owner.resolver), owner.media_lease.clone()))
}

fn ensure_current(reference: &ParsedReference) -> Result<(), MediaReferenceError> {
    let sources = lock_registry();
    let Some(owner) = sources.by_lease.get(&reference.lease_key) else {
        return Err(MediaReferenceError::Unavailable);
    };
    let current = sources.gate == RegistryGate::Running
        && sources
            .by_source
            .get(&owner.source_id)
            .is_some_and(|source| {
                source.generation == owner.generation && source.lease_key == reference.lease_key
            });
    if current {
        Ok(())
    } else {
        Err(MediaReferenceError::Unavailable)
    }
}

/// Deliberately opaque: neither its display text nor its source chain contains
/// a backend URL, credential, response body, or request reference.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaReferenceError {
    Malformed,
    WrongKind,
    Unavailable,
}

impl fmt::Display for MediaReferenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Malformed => "invalid remote media reference",
            Self::WrongKind => "remote media reference has the wrong kind",
            Self::Unavailable => "remote media is unavailable",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for MediaReferenceError {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;

    use crate::architecture::backend::BackendResult;
    use crate::architecture::media::{RemoteMediaResolver, ResolvedHttpRequest};

    use super::*;

    static REGISTRY_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct MockResolver {
        endpoint: Url,
    }

    struct CapturingResolver {
        seen_streams: Arc<Mutex<Vec<TrackId>>>,
        seen_artwork: Arc<Mutex<Vec<TrackId>>>,
    }

    impl MockResolver {
        fn new(marker: &str) -> Self {
            Self {
                endpoint: Url::parse(&format!("https://media.invalid/{marker}"))
                    .expect("mock endpoint"),
            }
        }
    }

    #[async_trait]
    impl RemoteMediaResolver for MockResolver {
        async fn resolve_stream(&self, _track_id: &TrackId) -> BackendResult<ResolvedHttpRequest> {
            ResolvedHttpRequest::new(self.endpoint.clone())
        }

        async fn resolve_artwork(
            &self,
            _media_id: &TrackId,
        ) -> BackendResult<Option<ResolvedHttpRequest>> {
            ResolvedHttpRequest::new(self.endpoint.clone()).map(Some)
        }
    }

    #[async_trait]
    impl RemoteMediaResolver for CapturingResolver {
        async fn resolve_stream(&self, track_id: &TrackId) -> BackendResult<ResolvedHttpRequest> {
            self.seen_streams
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(track_id.clone());
            ResolvedHttpRequest::new(Url::parse("https://media.invalid/exact").expect("URL"))
        }

        async fn resolve_artwork(
            &self,
            track_id: &TrackId,
        ) -> BackendResult<Option<ResolvedHttpRequest>> {
            self.seen_artwork
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(track_id.clone());
            ResolvedHttpRequest::new(Url::parse("https://media.invalid/artwork").expect("URL"))
                .map(Some)
        }
    }

    fn reset_registry() {
        let mut sources = lock_registry();
        for source in sources.by_source.values() {
            source.media_lease.revoke();
        }
        *sources = SourceRegistry::default();
    }

    fn retain(source_id: SourceId, marker: &str) -> RetainedSource {
        begin_connect(source_id)
            .expect("connection attempt")
            .retain(Arc::new(MockResolver::new(marker)))
            .expect("retained source")
    }

    fn track_id() -> TrackId {
        TrackId::remote(Uuid::new_v4().to_string()).expect("test track ID")
    }

    #[tokio::test]
    async fn same_key_replacement_revokes_old_lease_and_reference() {
        let _guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry();
        let track_id = track_id();
        let source_id = SourceId::random();
        let first = retain(source_id, "first");
        let old_reference = stream_reference(first.lease_key(), &track_id);
        let old_request = resolve_stream_reference(&old_reference)
            .await
            .expect("first request");

        let second = retain(source_id, "second");
        assert!(!first.is_current());
        assert!(second.is_current());
        assert!(!stream_reference_uses_lease(
            &old_reference,
            second.lease_key()
        ));
        assert!(!old_request.is_active());
        assert!(matches!(
            resolve_stream_reference(&old_reference).await,
            Err(MediaReferenceError::Unavailable)
        ));
        let new_reference = stream_reference(second.lease_key(), &track_id);
        assert!(stream_reference_uses_lease(
            &new_reference,
            second.lease_key()
        ));
        assert!(resolve_stream_reference(&new_reference)
            .await
            .expect("replacement request")
            .endpoint()
            .path()
            .ends_with("/second"));
        reset_registry();
    }

    #[tokio::test]
    async fn stale_retain_cannot_replace_a_newer_attempt() {
        let _guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry();
        let source_id = SourceId::random();
        let stale = begin_connect(source_id).expect("stale attempt");
        let current = begin_connect(source_id).expect("current attempt");

        assert!(stale.retain(Arc::new(MockResolver::new("stale"))).is_none());
        let current = current
            .retain(Arc::new(MockResolver::new("current")))
            .expect("current retained");
        assert!(current.is_current());
        reset_registry();
    }

    #[tokio::test]
    async fn discovery_loss_invalidates_an_attempt_before_network_start() {
        let _guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry();
        let source_id = SourceId::random();
        let attempt = begin_connect(source_id).expect("queued attempt");

        // No backend is active yet, but release must still retire the minted
        // generation so a task first polled afterward cannot resurrect it.
        assert!(!release_source(source_id));
        assert!(!attempt.is_latest());
        assert!(attempt
            .retain(Arc::new(MockResolver::new("withdrawn")))
            .is_none());
        assert!(!lock_registry()
            .pending_attempts
            .iter()
            .any(|(key, _)| *key == source_id));
        reset_registry();
    }

    #[tokio::test]
    async fn release_invalidates_references_and_issued_requests() {
        let _guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry();
        let source_id = SourceId::random();
        let source = retain(source_id, "released");
        let reference = stream_reference(source.lease_key(), &track_id());
        let request = resolve_stream_reference(&reference)
            .await
            .expect("issued request");

        assert!(release_source(source_id));
        assert!(!source.is_current());
        assert!(!request.is_active());
        assert!(matches!(
            resolve_stream_reference(&reference).await,
            Err(MediaReferenceError::Unavailable)
        ));
        reset_registry();
    }

    #[tokio::test]
    async fn source_leases_are_collision_isolated() {
        let _guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry();
        let track_id = track_id();
        let first_source_id = SourceId::random();
        let second_source_id = SourceId::random();
        let first = retain(first_source_id, "first");
        let second = retain(second_source_id, "second");
        let first_reference = stream_reference(first.lease_key(), &track_id);
        let second_reference = stream_reference(second.lease_key(), &track_id);

        assert_ne!(first.lease_key(), second.lease_key());
        assert!(resolve_stream_reference(&first_reference)
            .await
            .expect("first request")
            .endpoint()
            .path()
            .ends_with("/first"));
        assert!(resolve_stream_reference(&second_reference)
            .await
            .expect("second request")
            .endpoint()
            .path()
            .ends_with("/second"));
        assert!(release_source(first_source_id));
        assert!(resolve_stream_reference(&second_reference).await.is_ok());
        reset_registry();
    }

    #[tokio::test]
    async fn references_expose_only_lease_kind_and_track_identity() {
        let _guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry();
        let source = retain(SourceId::random(), "artwork");
        let track_id = TrackId::remote(" Case/Sensitive + Unicode ☃").expect("test track ID");
        let stream = stream_reference(source.lease_key(), &track_id);
        let artwork = artwork_reference(source.lease_key(), &track_id);
        let encoded_track_id = encode_track_segment(&track_id);

        assert_eq!(
            stream,
            format!(
                "{MEDIA_REFERENCE_SCHEME}://{}/stream/{encoded_track_id}",
                source.lease_key(),
            )
        );
        assert_eq!(
            artwork,
            format!(
                "{MEDIA_REFERENCE_SCHEME}://{}/artwork/{encoded_track_id}",
                source.lease_key(),
            )
        );
        for reference in [&stream, &artwork] {
            assert!(!reference.contains("private.example"));
            assert!(!reference.contains("user"));
            assert!(!reference.contains("secret"));
        }
        assert!(resolve_stream_reference(&stream).await.is_ok());
        assert!(resolve_artwork_reference(&artwork)
            .await
            .expect("artwork resolution")
            .is_some());
        reset_registry();
    }

    #[tokio::test]
    async fn reference_round_trip_preserves_the_exact_backend_native_id() {
        let _guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry();
        let seen_streams = Arc::new(Mutex::new(Vec::new()));
        let seen_artwork = Arc::new(Mutex::new(Vec::new()));
        let source = begin_connect(SourceId::random())
            .expect("connection attempt")
            .retain(Arc::new(CapturingResolver {
                seen_streams: Arc::clone(&seen_streams),
                seen_artwork: Arc::clone(&seen_artwork),
            }))
            .expect("retained source");
        let track_ids = [
            TrackId::remote(".").expect("dot track ID"),
            TrackId::remote("..").expect("dot-dot track ID"),
        ];

        for track_id in &track_ids {
            let stream = stream_reference(source.lease_key(), track_id);
            let artwork = artwork_reference(source.lease_key(), track_id);
            assert!(!stream.contains("/./"));
            assert!(!stream.contains("/../"));
            assert!(!artwork.contains("/./"));
            assert!(!artwork.contains("/../"));

            resolve_stream_reference(&stream)
                .await
                .expect("resolve exact stream ID");
            assert!(resolve_artwork_reference(&artwork)
                .await
                .expect("resolve exact artwork ID")
                .is_some());
        }

        assert_eq!(
            seen_streams
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            track_ids.as_slice()
        );
        assert_eq!(
            seen_artwork
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            track_ids.as_slice()
        );
        reset_registry();
    }

    #[tokio::test]
    async fn malformed_and_wrong_kind_references_fail_closed() {
        let _guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry();
        let source = retain(SourceId::random(), "media");
        let track_id = track_id();
        let stream = stream_reference(source.lease_key(), &track_id);
        let artwork = artwork_reference(source.lease_key(), &track_id);

        assert!(is_media_reference(&stream));
        assert!(matches!(
            resolve_artwork_reference(&stream).await,
            Err(MediaReferenceError::WrongKind)
        ));
        assert!(matches!(
            resolve_stream_reference(&artwork).await,
            Err(MediaReferenceError::WrongKind)
        ));
        for malformed in [
            "not-a-reference",
            "tributary-remote://%",
            "tributary-remote:///stream/track",
            "tributary-remote://not-a-uuid/stream/track",
            "tributary-remote://00000000-0000-4000-8000-000000000001/stream/",
            "tributary-remote://00000000-0000-4000-8000-000000000001/stream/%FF",
            "tributary-remote://00000000-0000-4000-8000-000000000001/stream/id-",
            "tributary-remote://00000000-0000-4000-8000-000000000001/stream/id-0",
            "tributary-remote://00000000-0000-4000-8000-000000000001/stream/id-2E",
            "tributary-remote://00000000-0000-4000-8000-000000000001/stream/id-ff",
            "tributary-remote://00000000-0000-4000-8000-000000000001/stream/track/extra",
            "tributary-remote://00000000-0000-4000-8000-000000000001/stream/track?secret=no",
        ] {
            if malformed.starts_with("tributary-remote:") {
                assert!(is_media_reference(malformed));
            }
            assert!(resolve_stream_reference(malformed).await.is_err());
        }
        reset_registry();
    }

    #[tokio::test]
    async fn failed_new_attempt_restores_existing_owner() {
        let _guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry();
        let track_id = track_id();
        let source_id = SourceId::random();
        let existing = retain(source_id, "existing");
        let reference = stream_reference(existing.lease_key(), &track_id);
        let failed = begin_connect(source_id).expect("replacement attempt");
        assert!(existing.is_current());
        assert!(resolve_stream_reference(&reference).await.is_ok());
        drop(failed);

        assert!(lock_registry().pending_attempts.is_empty());

        assert!(existing.is_current());
        assert!(resolve_stream_reference(&reference).await.is_ok());
        reset_registry();
    }

    #[tokio::test]
    async fn shutdown_revokes_active_and_rejects_pending_ownership() {
        let _guard = REGISTRY_TEST_LOCK.lock().await;
        reset_registry();
        let source = retain(SourceId::random(), "active");
        let reference = stream_reference(source.lease_key(), &track_id());
        let request = resolve_stream_reference(&reference)
            .await
            .expect("active request");
        let pending = begin_connect(SourceId::random()).expect("pending attempt");

        begin_shutdown();

        assert!(!source.is_current());
        assert!(!request.is_active());
        assert!(!pending.is_latest());
        assert!(begin_connect(SourceId::random()).is_none());
        assert!(matches!(
            resolve_stream_reference(&reference).await,
            Err(MediaReferenceError::Unavailable)
        ));
        drop(pending);
        reset_registry();
    }
}
