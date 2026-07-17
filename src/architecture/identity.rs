//! Stable, location-independent media identity.
//!
//! These values cross backend, registry, GTK, and playback boundaries.  The
//! UUID namespace and canonical input strings are persistent format state:
//! changing them requires an explicit migration.

use std::fmt;
use std::path::{Component, Path, PathBuf};
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

    /// Fresh identity for one external-file playback session.
    pub fn external() -> Self {
        Self::random()
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

    /// Whether this value is reserved and therefore cannot identify a
    /// persisted remote source.
    ///
    /// Nil is kept unavailable as a sentinel, while the two built-in IDs
    /// have application-wide owners that a saved-server file must never be
    /// able to impersonate.
    pub fn is_reserved_remote(self) -> bool {
        self.0.is_nil() || self == Self::local() || self == Self::radio_browser()
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

    /// Fresh identity for one external-file playback session.
    pub fn external() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// Lossless, mount-independent identity for one removable-source path.
    ///
    /// Native path code units are hex encoded after stripping the current
    /// mount root. A remount may therefore change the playable locator while
    /// preserving the same source-scoped track identity.
    pub fn removable_relative(root: &Path, path: &Path) -> Result<Self, IdentityError> {
        let relative = path.strip_prefix(root).map_err(|_| IdentityError::Track)?;
        let mut normalized = PathBuf::new();
        for component in relative.components() {
            match component {
                Component::Normal(value) => normalized.push(value),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(IdentityError::Track);
                }
            }
        }
        if normalized.as_os_str().is_empty() {
            return Err(IdentityError::Track);
        }

        #[cfg(unix)]
        let value = {
            use std::os::unix::ffi::OsStrExt;
            let bytes = normalized.as_os_str().as_bytes();
            validate_encoded_length("unix:", bytes.len())?;
            format!("unix:{}", encode_hex(bytes))
        };
        #[cfg(windows)]
        let value = {
            use std::os::windows::ffi::OsStrExt;
            let bytes: Vec<u8> = normalized
                .as_os_str()
                .encode_wide()
                .flat_map(u16::to_le_bytes)
                .collect();
            validate_encoded_length("windows-utf16le:", bytes.len())?;
            format!("windows-utf16le:{}", encode_hex(&bytes))
        };
        #[cfg(not(any(unix, windows)))]
        let value = {
            let bytes = normalized.to_str().ok_or(IdentityError::Track)?.as_bytes();
            validate_encoded_length("portable-utf8:", bytes.len())?;
            format!("portable-utf8:{}", encode_hex(bytes))
        };

        Self::new(value)
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

fn validate_encoded_length(prefix: &str, input_bytes: usize) -> Result<(), IdentityError> {
    let encoded_bytes = input_bytes
        .checked_mul(2)
        .and_then(|length| length.checked_add(prefix.len()))
        .ok_or(IdentityError::Track)?;
    if encoded_bytes > MAX_TRACK_ID_BYTES {
        return Err(IdentityError::Track);
    }
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for &byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
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
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub enum ViewOrigin {
    Playlist(String),
    Radio(String),
}

impl<'de> Deserialize<'de> for ViewOrigin {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        enum SerializedViewOrigin {
            Playlist(String),
            Radio(String),
        }

        match SerializedViewOrigin::deserialize(deserializer)? {
            SerializedViewOrigin::Playlist(id) => Self::playlist(id),
            SerializedViewOrigin::Radio(query) => Self::radio(query),
        }
        .map_err(serde::de::Error::custom)
    }
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
    fn persisted_remote_identity_reserves_nil_and_builtin_owners() {
        assert!(SourceId::from_uuid(Uuid::nil()).is_reserved_remote());
        assert!(SourceId::local().is_reserved_remote());
        assert!(SourceId::radio_browser().is_reserved_remote());
        assert!(!SourceId::random().is_reserved_remote());
        assert!(!SourceId::remote(
            "subsonic",
            &Url::parse("https://music.example.test").expect("URL")
        )
        .expect("remote ID")
        .is_reserved_remote());
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

    #[test]
    fn removable_track_identity_ignores_the_current_mount_location() {
        let relative = Path::new("Artist").join("Album").join("Track.flac");
        let first = TrackId::removable_relative(
            Path::new("/media/one"),
            &Path::new("/media/one").join(&relative),
        )
        .expect("first identity");
        let second = TrackId::removable_relative(
            Path::new("/run/media/two"),
            &Path::new("/run/media/two").join(&relative),
        )
        .expect("second identity");
        assert_eq!(first, second);
        assert!(TrackId::removable_relative(
            Path::new("/media/one"),
            Path::new("/outside/Track.flac")
        )
        .is_err());
        assert_eq!(
            TrackId::removable_relative(
                Path::new("/media/one"),
                Path::new("/media/one/./Artist/Album/Track.flac")
            )
            .expect("normalized identity"),
            first
        );
    }

    #[cfg(unix)]
    #[test]
    fn removable_track_identity_preserves_non_utf8_native_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let root = Path::new("/media/device");
        let path = root.join(std::ffi::OsString::from_vec(vec![b'a', 0xff, b'.', b'f']));
        let identity = TrackId::removable_relative(root, &path).expect("native identity");
        assert_eq!(identity.as_str(), "unix:61ff2e66");
    }

    #[test]
    fn view_origin_deserialization_preserves_the_bound() {
        let valid: ViewOrigin =
            serde_json::from_str(r#"{"Playlist":"playlist-id"}"#).expect("valid view");
        assert_eq!(valid, ViewOrigin::Playlist("playlist-id".to_string()));
        assert!(serde_json::from_str::<ViewOrigin>(r#"{"Radio":""}"#).is_err());
        let oversized = serde_json::json!({ "Playlist": "x".repeat(MAX_VIEW_ORIGIN_BYTES + 1) });
        assert!(serde_json::from_value::<ViewOrigin>(oversized).is_err());
    }
}
