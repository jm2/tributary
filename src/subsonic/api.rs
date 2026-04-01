//! Subsonic REST API JSON response types.
//!
//! Only the subset of fields Tributary actually uses are deserialized;
//! unknown fields are silently ignored (`#[serde(default)]`).

#![allow(dead_code)]

use serde::Deserialize;

// ── Top-level envelope ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SubsonicEnvelope {
    #[serde(rename = "subsonic-response")]
    pub response: SubsonicResponse,
}

#[derive(Debug, Deserialize)]
pub struct SubsonicResponse {
    pub status: String,
    #[serde(default)]
    pub error: Option<SubsonicError>,

    // ping.view
    #[serde(default)]
    pub version: Option<String>,

    // getArtists.view
    #[serde(default)]
    pub artists: Option<ArtistsWrapper>,

    // getArtist.view
    #[serde(default)]
    pub artist: Option<ArtistDetail>,

    // getAlbum.view
    #[serde(default)]
    pub album: Option<AlbumDetail>,

    // search3.view
    #[serde(default, rename = "searchResult3")]
    pub search_result3: Option<SearchResult3>,
}

#[derive(Debug, Deserialize)]
pub struct SubsonicError {
    pub code: i32,
    pub message: String,
}

// ── getArtists ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ArtistsWrapper {
    #[serde(default)]
    pub index: Vec<ArtistIndex>,
}

#[derive(Debug, Deserialize)]
pub struct ArtistIndex {
    #[serde(default)]
    pub artist: Vec<ArtistEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistEntry {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub album_count: Option<u32>,
    #[serde(default)]
    pub cover_art: Option<String>,
}

// ── getArtist (detail with albums) ──────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistDetail {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub album: Vec<AlbumEntry>,
}

// ── getAlbum (detail with songs) ────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumDetail {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub artist_id: Option<String>,
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub cover_art: Option<String>,
    #[serde(default)]
    pub song_count: Option<u32>,
    #[serde(default)]
    pub duration: Option<u64>,
    #[serde(default)]
    pub song: Vec<SongEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumEntry {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub artist_id: Option<String>,
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub cover_art: Option<String>,
    #[serde(default)]
    pub song_count: Option<u32>,
    #[serde(default)]
    pub duration: Option<u64>,
}

// ── Song / Child ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SongEntry {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub artist_id: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub album_id: Option<String>,
    #[serde(default)]
    pub track: Option<u32>,
    #[serde(default)]
    pub disc_number: Option<u32>,
    #[serde(default)]
    pub duration: Option<u64>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub bit_rate: Option<u32>,
    #[serde(default)]
    pub suffix: Option<String>,
    #[serde(default)]
    pub cover_art: Option<String>,
    #[serde(default)]
    pub play_count: Option<u32>,
}

// ── search3 ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult3 {
    #[serde(default)]
    pub artist: Vec<ArtistEntry>,
    #[serde(default)]
    pub album: Vec<AlbumEntry>,
    #[serde(default)]
    pub song: Vec<SongEntry>,
}
