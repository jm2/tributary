//! Stable, location-independent media identity.
//!
//! These values cross backend, registry, GTK, and playback boundaries.  The
//! UUID namespace and canonical input strings are persistent format state:
//! changing them requires an explicit migration.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};
use url::Url;
use uuid::Uuid;

/// Frozen namespace for deterministic application source identifiers.
pub const SOURCE_ID_NAMESPACE: Uuid = Uuid::from_u128(0xc931_938b_1524_4c8f_b63a_abfa_86ce_36f1);

/// Upper bound for an adapter-owned track identifier.
///
/// The large application-wide ceiling admits a losslessly encoded Windows
/// relative path. Network adapters apply the much smaller bound below before
/// publishing server-controlled catalogue values.
pub const MAX_TRACK_ID_BYTES: usize = 256 * 1024;

/// Upper bound for a server-controlled backend-native track identifier.
pub const MAX_REMOTE_TRACK_ID_BYTES: usize = 4 * 1024;

const MAX_VIEW_ORIGIN_BYTES: usize = 4 * 1024;

/// Opaque identity of one logical media source.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceId(Uuid);

impl SourceId {
    /// Adopt an already-persisted source UUID.
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    /// Create a new identity for a persisted source or ephemeral source
    /// session. Random assignment is always explicit at the caller.
    pub fn random() -> Self {
        Self(Uuid::new_v4())
    }

    /// Stable identity of the built-in local library.
    pub fn local() -> Self {
        Self::deterministic(b"builtin:local")
    }

    /// Stable identity of the built-in Radio-Browser adapter.
    pub fn radio_browser() -> Self {
        Self::deterministic(b"builtin:radio-browser")
    }

    /// Stable identity for an unsaved remote endpoint or legacy saved row.
    pub fn remote(backend: &str, base_url: &Url) -> Result<Self, IdentityError> {
        if backend.is_empty()
            || !backend
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
        {
            return Err(IdentityError::Source);
        }
        let canonical = canonical_remote_base_url(base_url)?;
        let input = format!("remote:{backend}:{canonical}");
        Ok(Self::deterministic(input.as_bytes()))
    }

    /// Stable identity for the best-available logical removable-filesystem
    /// key. The key remains opaque and is never interpreted as a path.
    pub fn removable(logical_key: &str) -> Result<Self, IdentityError> {
        if logical_key.is_empty() || logical_key.len() > MAX_TRACK_ID_BYTES {
            return Err(IdentityError::Source);
        }
        let input = format!("removable:{logical_key}");
        Ok(Self::deterministic(input.as_bytes()))
    }

    pub const fn as_uuid(self) -> Uuid {
        self.0
    }

    fn deterministic(input: &[u8]) -> Self {
        Self(Uuid::new_v5(&SOURCE_ID_NAMESPACE, input))
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for SourceId {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value)
            .map(Self)
            .map_err(|_| IdentityError::Source)
    }
}

/// Exact non-empty identifier assigned by one source adapter.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct TrackId(String);

impl TrackId {
    /// Validate an adapter-owned identifier against the application ceiling.
    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        Self::with_bound(value.into(), MAX_TRACK_ID_BYTES)
    }

    /// Validate a server-controlled identifier against the network ceiling.
    pub fn remote(value: impl Into<String>) -> Result<Self, IdentityError> {
        Self::with_bound(value.into(), MAX_REMOTE_TRACK_ID_BYTES)
    }

    fn with_bound(value: String, maximum: usize) -> Result<Self, IdentityError> {
        if value.is_empty() || value.len() > maximum {
            return Err(IdentityError::Track);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for TrackId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TrackId")
            .field("byte_len", &self.0.len())
            .finish_non_exhaustive()
    }
}

impl<'de> Deserialize<'de> for TrackId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Complete application identity of one playable media item.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct MediaKey {
    pub source_id: SourceId,
    pub track_id: TrackId,
}

impl MediaKey {
    pub fn new(source_id: SourceId, track_id: TrackId) -> Self {
        Self {
            source_id,
            track_id,
        }
    }
}

/// The view whose ordered projection supplied a queue item.
///
/// A view never changes `MediaKey`; it exists only for navigation,
/// re-selection, and duplicate occurrence ownership.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum ViewOrigin {
    Playlist(String),
    Radio(String),
}

impl ViewOrigin {
    pub fn playlist(id: impl Into<String>) -> Result<Self, IdentityError> {
        bounded_view(id.into()).map(Self::Playlist)
    }

    pub fn radio(query: impl Into<String>) -> Result<Self, IdentityError> {
        bounded_view(query.into()).map(Self::Radio)
    }
}

fn bounded_view(value: String) -> Result<String, IdentityError> {
    if value.is_empty() || value.len() > MAX_VIEW_ORIGIN_BYTES {
        return Err(IdentityError::View);
    }
    Ok(value)
}

/// Fixed identity-validation categories. Values are intentionally omitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum IdentityError {
    #[error("invalid source identity")]
    Source,
    #[error("invalid track identity")]
    Track,
    #[error("invalid view identity")]
    View,
    #[error("invalid remote base URL")]
    RemoteUrl,
}

/// Canonical spelling used only as deterministic remote identity input.
///
/// URL parsing already canonicalizes scheme/host case and default ports. A
/// trailing empty path segment is removed while every meaningful reverse-
/// proxy segment remains distinct.
pub fn canonical_remote_base_url(base_url: &Url) -> Result<String, IdentityError> {
    if base_url.cannot_be_a_base()
        || !matches!(base_url.scheme(), "http" | "https")
        || base_url.host_str().is_none()
        || !base_url.username().is_empty()
        || base_url.password().is_some()
        || base_url.query().is_some()
        || base_url.fragment().is_some()
    {
        return Err(IdentityError::RemoteUrl);
    }

    let mut canonical = base_url.clone();
    canonical
        .path_segments_mut()
        .map_err(|()| IdentityError::RemoteUrl)?
        .pop_if_empty();
    Ok(canonical.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_builtin_and_remote_source_ids_do_not_drift() {
        assert_eq!(
            SourceId::local().to_string(),
            "dbae1f16-7921-5209-939e-ce3177ec7b57"
        );
        assert_eq!(
            SourceId::radio_browser().to_string(),
            "39f5ad82-6349-5d36-b498-3b8904e9dcb4"
        );
        let url = Url::parse("https://music.example.test/").expect("URL");
        assert_eq!(
            SourceId::remote("subsonic", &url)
                .expect("remote ID")
                .to_string(),
            "86e16344-b0ec-5aeb-a798-2d5f401538d2"
        );
    }

    #[test]
    fn canonical_remote_spelling_normalizes_only_nonmeaningful_variants() {
        let spellings = [
            "HTTPS://MUSIC.EXAMPLE.TEST:443/base",
            "https://music.example.test/base/",
        ];
        let ids: Vec<_> = spellings
            .into_iter()
            .map(|value| {
                SourceId::remote("subsonic", &Url::parse(value).expect("URL")).expect("ID")
            })
            .collect();
        assert_eq!(ids[0], ids[1]);

        let other_port = SourceId::remote(
            "subsonic",
            &Url::parse("https://music.example.test:444/base").expect("URL"),
        )
        .expect("ID");
        let other_prefix = SourceId::remote(
            "subsonic",
            &Url::parse("https://music.example.test/other").expect("URL"),
        )
        .expect("ID");
        let other_backend = SourceId::remote(
            "jellyfin",
            &Url::parse("https://music.example.test/base").expect("URL"),
        )
        .expect("ID");
        assert_ne!(ids[0], other_port);
        assert_ne!(ids[0], other_prefix);
        assert_ne!(ids[0], other_backend);
    }

    #[test]
    fn track_ids_are_exact_bounded_and_redacted_from_debug() {
        let exact = TrackId::remote(" Case/Sensitive + Unicode ☃").expect("track ID");
        assert_eq!(exact.as_str(), " Case/Sensitive + Unicode ☃");
        let debug = format!("{exact:?}");
        assert!(!debug.contains("Sensitive"));
        assert!(TrackId::remote("").is_err());
        assert!(TrackId::remote("x".repeat(MAX_REMOTE_TRACK_ID_BYTES + 1)).is_err());
        assert!(TrackId::new("x".repeat(MAX_TRACK_ID_BYTES)).is_ok());
        assert!(TrackId::new("x".repeat(MAX_TRACK_ID_BYTES + 1)).is_err());
    }

    #[test]
    fn the_same_native_id_is_namespaced_by_source() {
        let track_id = TrackId::remote("same-native-id").expect("track ID");
        let first = MediaKey::new(SourceId::local(), track_id.clone());
        let second = MediaKey::new(SourceId::radio_browser(), track_id);
        assert_ne!(first, second);
    }
}
