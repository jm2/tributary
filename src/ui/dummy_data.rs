//! The sidebar's static source list.

use super::objects::{HeaderKind, SourceObject};

/// Build the sidebar source list.
///
/// Only the "Local" section is created statically. Remote category
/// headers (Subsonic, Jellyfin / Plex, DAAP) are added dynamically
/// when servers are discovered or configured — see `window.rs`.
pub fn build_sources() -> Vec<SourceObject> {
    vec![
        SourceObject::header(rust_i18n::t!("sidebar.local").as_ref(), HeaderKind::Local),
        SourceObject::source(
            rust_i18n::t!("sidebar.local_filesystem").as_ref(),
            "local",
            "drive-harddisk-symbolic",
        ),
        // Internet Radio sources (always present).
        SourceObject::header(
            rust_i18n::t!("sidebar.internet_radio").as_ref(),
            HeaderKind::InternetRadio,
        ),
        SourceObject::source(
            rust_i18n::t!("sidebar.top_clicked").as_ref(),
            "radio-topclick",
            "network-wireless-symbolic",
        ),
        SourceObject::source(
            rust_i18n::t!("sidebar.top_voted").as_ref(),
            "radio-topvote",
            "network-wireless-symbolic",
        ),
        SourceObject::source(
            rust_i18n::t!("sidebar.stations_near_me").as_ref(),
            "radio-nearme",
            "network-wireless-symbolic",
        ),
        // Playlists section (entries populated dynamically after DB load).
        SourceObject::header(
            rust_i18n::t!("sidebar.playlists").as_ref(),
            HeaderKind::Playlists,
        ),
    ]
}
