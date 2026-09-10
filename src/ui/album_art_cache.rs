//! Bounded, display-side texture cache for the browser album pane.
//!
//! Split out of `album_pane_art.rs` so the widget/controller code and the
//! cache live in separately sized modules. The cache is intentionally
//! **display-side** (a `gdk::Texture` plus `gtk::Image` swap), not a
//! transport cache: the album-art worker in `album_art.rs` already
//! provides the byte-level cache + byte-cap enforcement; this module only
//! ensures the UI doesn't multiply fetches for visible rows.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use gtk::gdk;
use gtk::gdk::prelude::TextureExt;

use crate::architecture::SourceId;

/// Maximum number of cached album-art entries. The cache is keyed by
/// `(source, album_key, pixel_size)`. A library of 10 000 albums × one
/// size variant × ~32 KiB decoded surface is well under the working-set
/// budget; the bound is here so an attacker-controlled catalog (e.g., a
/// misbehaving Subsonic peer) cannot inflate memory through the UI path.
pub const MAX_CACHED_ALBUM_ARTS: usize = 512;

/// Upper bound on the cache's decoded texture memory budget.
///
/// GTK4 `gdk::Texture` does not expose a byte count, so we conservatively
/// approximate each entry as `width × height × 4` bytes (RGBA8888 at the
/// texture's ACTUAL decoded dimensions) and reject any insert that would
/// push the total past this cap. 32 MiB covers ~250 48-px thumbnails or
/// ~24 128-px thumbnails; the count cap still enforces the upper bound on
/// huge libraries with many pixel-size variants.
pub const MAX_CACHE_BYTES: u64 = 32 * 1024 * 1024;

/// Per-pane cache shared across every row in the album pane.
///
/// The cache is bounded by [`MAX_CACHED_ALBUM_ARTS`] and
/// [`MAX_CACHE_BYTES`]; an insert past either bound evicts the
/// least-recently-inserted entry until both bounds hold. A `gdk::Texture`
/// retains the decoded pixbuf only as long as GTK holds it; if the system
/// drops the underlying surface, re-decoding happens through the worker.
#[derive(Clone)]
pub struct AlbumArtCache {
    pub(crate) inner: Rc<RefCell<AlbumArtCacheInner>>,
}

pub struct AlbumArtCacheInner {
    /// Map from the source-qualified cache key to its decoded texture.
    /// The key bundles
    /// `(source, source_epoch, content_generation, album_key, pixel_size)`
    /// so a track that resolves through two different remote sources is
    /// not aliased to a single texture, a source that is reactivated
    /// under a new session epoch never serves artwork decoded under a
    /// previous epoch's identity, and artwork decoded before a library
    /// content change is never served afterwards.
    pub(crate) entries: HashMap<String, CacheEntry>,
    /// Insertion order for FIFO eviction; hits bump entries to the tail
    /// so a hot row doesn't get evicted under memory pressure.
    pub(crate) order: VecDeque<String>,
    /// Running total of approximated decoded bytes across all entries.
    /// Used to enforce [`MAX_CACHE_BYTES`] even when the entry count is
    /// well under [`MAX_CACHED_ALBUM_ARTS`].
    pub(crate) total_bytes: u64,
    /// Library content generation. Bumped whenever the browser's track
    /// set is rebuilt (FullSync, source switch): entries keyed under an
    /// older generation become unqueryable, so a changed cover is
    /// re-resolved within the same source session instead of serving the
    /// pre-sync pixels (2026-09-10 review finding — the key previously
    /// carried the source epoch but no artwork/content generation, and a
    /// same-session FullSync left changed covers stale).
    pub(crate) content_generation: u64,
}

/// One entry in [`AlbumArtCache`]. Stores the texture alongside the
/// approximated byte cost so eviction can drop the right amount from the
/// running total without re-measuring.
pub struct CacheEntry {
    pub(crate) texture: gdk::Texture,
    pub(crate) bytes: u64,
}

impl Default for AlbumArtCache {
    fn default() -> Self {
        Self::new()
    }
}

impl AlbumArtCache {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(AlbumArtCacheInner {
                entries: HashMap::new(),
                order: VecDeque::new(),
                total_bytes: 0,
                content_generation: 0,
            })),
        }
    }

    /// Invalidate every cached entry by advancing the content
    /// generation. Old-generation entries stay resident only until
    /// bounded eviction drops them; no future lookup can ever query
    /// them. Called by the browser rebuild path on FullSync / source
    /// switches.
    pub fn bump_content_generation(&self) {
        self.inner.borrow_mut().content_generation += 1;
    }

    /// Current library content generation. Test seam for the rebuild
    /// path's invalidation contract.
    #[allow(dead_code)] // exercised by the widget-test build and cache tests
    pub fn content_generation(&self) -> u64 {
        self.inner.borrow().content_generation
    }

    /// Look up a cached texture for
    /// `(source, source_epoch, content_generation, album_key, pixel_size)`
    /// at the CURRENT content generation. Returns `None` on miss; a hit
    /// also bumps the entry to the most-recent position so a hot row
    /// doesn't get evicted under memory pressure.
    pub fn get(
        &self,
        source: Option<&SourceId>,
        source_epoch: Option<u64>,
        album_key: &str,
        pixel_size: i32,
    ) -> Option<gdk::Texture> {
        let mut inner = self.inner.borrow_mut();
        let key = cache_key(
            source,
            source_epoch,
            inner.content_generation,
            album_key,
            pixel_size,
        );
        let entry = inner.entries.get(&key)?;
        let texture = entry.texture.clone();
        if let Some(position) = inner.order.iter().position(|existing| existing == &key) {
            inner.order.remove(position);
        }
        inner.order.push_back(key);
        Some(texture)
    }

    /// Insert a new texture, evicting the oldest entries if either bound
    /// would be exceeded. The eviction is FIFO with a recency-bump on
    /// read so a long-running scroll session never displaces hot
    /// entries. Both the count cap and the byte cap are enforced on
    /// every insert. A single texture whose decoded surface exceeds the
    /// ENTIRE budget is refused outright: no eviction order can make
    /// room for it, so admitting it would leave the cache permanently
    /// over budget (2026-09-10 review finding — the former loop stopped
    /// at one entry precisely to avoid that state, which let one oversized
    /// decoded texture pin the cache over its cap). The row keeps
    /// displaying the refused texture; only its cache retention is
    /// declined, and the next bind simply re-resolves it.
    pub fn insert(
        &self,
        source: Option<&SourceId>,
        source_epoch: Option<u64>,
        content_generation: u64,
        album_key: &str,
        pixel_size: i32,
        texture: gdk::Texture,
    ) {
        let bytes = approximate_texture_bytes(&texture);
        if bytes > MAX_CACHE_BYTES {
            return;
        }
        let mut inner = self.inner.borrow_mut();
        let key = cache_key(
            source,
            source_epoch,
            content_generation,
            album_key,
            pixel_size,
        );
        if let Some(existing) = inner.entries.remove(&key) {
            inner.total_bytes = inner.total_bytes.saturating_sub(existing.bytes);
            if let Some(position) = inner.order.iter().position(|existing| existing == &key) {
                inner.order.remove(position);
            }
        }
        inner
            .entries
            .insert(key.clone(), CacheEntry { texture, bytes });
        inner.order.push_back(key.clone());
        inner.total_bytes = inner.total_bytes.saturating_add(bytes);
        // Evict until both bounds hold. No admitted entry can exceed the
        // byte cap on its own (oversized inserts are refused above), so
        // this loop always reaches a compliant state before it could
        // touch the just-inserted key.
        while inner.entries.len() > MAX_CACHED_ALBUM_ARTS || inner.total_bytes > MAX_CACHE_BYTES {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            if let Some(evicted) = inner.entries.remove(&oldest) {
                inner.total_bytes = inner.total_bytes.saturating_sub(evicted.bytes);
            }
        }
    }

    /// Drop every cached entry. Used by the rebuild path so a freshly
    /// compiled bind factory never serves a stale texture from a previous
    /// layout state.
    pub fn clear(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.entries.clear();
        inner.order.clear();
        inner.total_bytes = 0;
    }

    /// Total number of entries currently cached.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.inner.borrow().entries.len()
    }

    /// True if the cache holds zero entries.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.inner.borrow().entries.is_empty()
    }

    /// Approximate decoded byte cost across every cached entry. Used by
    /// tests to assert that the byte budget is enforced independently of
    /// the count cap.
    #[allow(dead_code)]
    pub fn approximate_byte_total(&self) -> u64 {
        self.inner.borrow().total_bytes
    }
}

fn cache_key(
    source: Option<&SourceId>,
    source_epoch: Option<u64>,
    content_generation: u64,
    album_key: &str,
    pixel_size: i32,
) -> String {
    // Use a unit-separator byte (\x1f) between fields so an album_key
    // that contains the typical cache separator characters cannot
    // collide with a different key tuple. `SourceId` displays as its
    // underlying UUID, so the source-qualified key is opaque without
    // exposing the Uuid type at the call site. The source session epoch
    // sits between the source identity and the content generation: a
    // source that is reactivated under a new epoch gets fresh keys, so
    // stale artwork from the previous session can never be served
    // (2026-09-07 review finding), and a library content change inside
    // one session gets fresh keys through the generation field
    // (2026-09-10 review finding). Local rows have no epoch; their keys
    // omit that field but still carry the generation.
    let epoch = match source_epoch {
        Some(epoch) => epoch.to_string(),
        None => String::from("-"),
    };
    match source {
        Some(id) => format!(
            "src:{}\x1f{}\x1f{}\x1f{}\x1f{}",
            id, epoch, content_generation, album_key, pixel_size
        ),
        None => format!(
            "src:local\x1f{}\x1f{}\x1f{}",
            content_generation, album_key, pixel_size
        ),
    }
}

/// Approximate the decoded byte cost of a `gdk::Texture` for the cache's
/// memory budget. GTK4 does not expose the texture's GPU-backed byte
/// count, so we conservatively model an RGBA8888 surface at the
/// texture's ACTUAL decoded dimensions: `width × height × 4` bytes.
/// Charging the requested pixel size instead let a full-resolution cover
/// (~16 MiB retained for a 2000×2000 RGBA image) count as a few KiB, so
/// the advertised 32 MiB budget could hold hundreds of large decoded
/// textures (2026-09-08 review finding).
fn approximate_texture_bytes(texture: &gdk::Texture) -> u64 {
    (texture.width() as u64)
        .saturating_mul(texture.height() as u64)
        .saturating_mul(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::glib;

    fn fake_texture() -> gdk::Texture {
        // 1×1 RGBA PNG with full filter byte per row. PNG's raw stream is
        // a per-row filter byte (0 = none) plus RGBA pixels; the IDAT
        // zlib-stream deflates those bytes. CRC table from PNG spec §B.
        let png: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0B, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x60, 0x00, 0x02, 0x00, 0x00, 0x05, 0x00, 0x01, 0x7A, 0x5E, 0xAB, 0x3F,
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        gdk::Texture::from_bytes(&glib::Bytes::from_static(png)).expect("valid 1×1 PNG decodes")
    }

    #[test]
    fn cache_bounded_eviction_replaces_oldest_entry() {
        let cache = AlbumArtCache::new();
        let texture = fake_texture();
        for i in 0..(MAX_CACHED_ALBUM_ARTS + 4) {
            cache.insert(
                None,
                None,
                cache.content_generation(),
                &format!("album-{i}"),
                48,
                texture.clone(),
            );
        }
        assert_eq!(cache.len(), MAX_CACHED_ALBUM_ARTS);
        // Earliest entries must have been evicted.
        assert!(cache.get(None, None, "album-0", 48).is_none());
        assert!(cache.get(None, None, "album-1", 48).is_none());
        // Most recent insertions survive.
        let last = MAX_CACHED_ALBUM_ARTS + 4 - 1;
        assert!(cache
            .get(None, None, &format!("album-{last}"), 48)
            .is_some());
    }

    #[test]
    fn cache_hit_promotes_to_most_recent() {
        let cache = AlbumArtCache::new();
        let texture = fake_texture();
        for i in 0..MAX_CACHED_ALBUM_ARTS {
            cache.insert(
                None,
                None,
                cache.content_generation(),
                &format!("album-{i}"),
                48,
                texture.clone(),
            );
        }
        // Touch the earliest entry — it should survive a follow-up
        // insert that would otherwise evict it.
        assert!(cache.get(None, None, "album-0", 48).is_some());
        cache.insert(
            None,
            None,
            cache.content_generation(),
            "newcomer",
            48,
            texture.clone(),
        );
        assert_eq!(cache.len(), MAX_CACHED_ALBUM_ARTS);
        assert!(cache.get(None, None, "album-0", 48).is_some());
        assert!(cache.get(None, None, "album-1", 48).is_none());
    }

    #[test]
    fn cache_pixel_size_distinguishes_entries() {
        let cache = AlbumArtCache::new();
        let texture = fake_texture();
        cache.insert(
            None,
            None,
            cache.content_generation(),
            "album-a",
            32,
            texture.clone(),
        );
        assert!(cache.get(None, None, "album-a", 32).is_some());
        assert!(cache.get(None, None, "album-a", 48).is_none());
        cache.insert(
            None,
            None,
            cache.content_generation(),
            "album-a",
            48,
            texture,
        );
        assert!(cache.get(None, None, "album-a", 32).is_some());
        assert!(cache.get(None, None, "album-a", 48).is_some());
    }

    /// Two remote sources that happen to expose the same upstream track
    /// id (different subsonic peers, two distinct Plex libraries, etc.)
    /// must not share a cached texture. The key includes the source id
    /// so a hit on one source never serves a texture decoded from the
    /// other source's bytes.
    #[test]
    fn cache_source_qualified_keys_do_not_collide() {
        use crate::architecture::SourceId;
        let cache = AlbumArtCache::new();
        let texture_a = fake_texture();
        let texture_b = fake_texture();
        let source_a = SourceId::local();
        let source_b = SourceId::radio_browser();
        // Same track id, same size, different sources.
        cache.insert(
            Some(&source_a),
            None,
            cache.content_generation(),
            "shared-id",
            48,
            texture_a.clone(),
        );
        cache.insert(
            Some(&source_b),
            None,
            cache.content_generation(),
            "shared-id",
            48,
            texture_b.clone(),
        );
        // Both must be independently retrievable. We can only assert
        // hit/miss from the public surface — `gdk::Texture` doesn't
        // expose identity — but the count must reflect both.
        assert_eq!(cache.len(), 2);
        assert!(cache.get(Some(&source_a), None, "shared-id", 48).is_some());
        assert!(cache.get(Some(&source_b), None, "shared-id", 48).is_some());
        // And a local-only row never aliases to a remote-source row.
        cache.insert(
            None,
            None,
            cache.content_generation(),
            "shared-id",
            48,
            texture_a.clone(),
        );
        assert_eq!(cache.len(), 3);
        assert!(cache.get(None, None, "shared-id", 48).is_some());
    }

    /// Adversarial key input: a key made entirely of unit-separator
    /// bytes (`\x1f`) must not collide with another key whose internal
    /// field happens to look like a separator. The cache-key builder
    /// uses `\x1f` as the field separator, so a track id of `"\x1f"`
    /// alone is a worst-case input.
    #[test]
    fn cache_adversarial_separator_only_track_id_is_isolated() {
        let cache = AlbumArtCache::new();
        let texture = fake_texture();
        let key = "\x1f";
        cache.insert(
            None,
            None,
            cache.content_generation(),
            key,
            48,
            texture.clone(),
        );
        assert!(cache.get(None, None, key, 48).is_some());
        // A different pixel size must not match this key.
        assert!(cache.get(None, None, key, 49).is_none());
        // A control key that contains the same byte must not collide.
        let almost = "\x1f\x1f";
        assert!(cache.get(None, None, almost, 48).is_none());
    }

    /// A track id far in excess of any sane remote identifier must not
    /// crash the cache. The previous review asked for "adversarial
    /// production-path" tests; this is the cheapest one to write
    /// because the cache path is pure (no GTK, no async).
    #[test]
    fn cache_adversarial_long_track_id_does_not_panic() {
        let cache = AlbumArtCache::new();
        let texture = fake_texture();
        let huge = "a".repeat(8 * 1024);
        cache.insert(
            None,
            None,
            cache.content_generation(),
            &huge,
            48,
            texture.clone(),
        );
        assert!(cache.get(None, None, &huge, 48).is_some());
        // Bumping the same key with a different size stays distinct.
        cache.insert(None, None, cache.content_generation(), &huge, 64, texture);
        assert_eq!(cache.len(), 2);
    }

    /// A texture with exact decoded dimensions for budget tests.
    /// `gdk::MemoryTexture` wraps in-memory RGBA8888 rows, so its
    /// `width × height × 4` surface cost is known without decoding a
    /// crafted PNG. Zero-filled rows are fine — the budget models
    /// memory, not content.
    fn solid_texture(width: i32, height: i32) -> gdk::Texture {
        let stride = width as usize * 4;
        let data = vec![0x80_u8; stride * height as usize];
        gdk::MemoryTexture::new(
            width,
            height,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from_owned(data),
            stride,
        )
        .into()
    }

    /// The byte budget must charge the texture's ACTUAL decoded
    /// dimensions, not the requested thumbnail size. A full-resolution
    /// cover shown as a 48-px thumbnail still retains
    /// `width × height × 4` bytes; under the previous requested-size
    /// approximation a 2000×2000 cover counted as ~9 KiB, so the
    /// nominal 32 MiB budget could retain hundreds of large decoded
    /// textures (2026-09-08 review finding).
    #[test]
    fn cache_budget_charges_decoded_texture_dimensions_not_requested_pixel_size() {
        let cache = AlbumArtCache::new();
        // A 2048×2048 RGBA cover retains 16 MiB no matter that the row
        // asked for a 48-px thumbnail; the budget must see all of it.
        cache.insert(
            None,
            None,
            cache.content_generation(),
            "big-album",
            48,
            solid_texture(2048, 2048),
        );
        assert_eq!(
            cache.approximate_byte_total(),
            2048_u64 * 2048 * 4,
            "a full-resolution texture must be charged its decoded surface, not pixel_size²"
        );

        // Nine 1024×1024 covers (4 MiB each) push total demand to
        // 52 MiB against the 32 MiB budget: the byte budget — not the
        // count cap — must evict the oldest entries.
        for i in 0..9 {
            cache.insert(
                None,
                None,
                cache.content_generation(),
                &format!("album-{i}"),
                48,
                solid_texture(1024, 1024),
            );
        }
        assert!(
            cache.approximate_byte_total() <= MAX_CACHE_BYTES,
            "byte budget must be enforced: got {}",
            cache.approximate_byte_total()
        );
        assert!(
            cache.get(None, None, "big-album", 48).is_none()
                && cache.get(None, None, "album-0", 48).is_none(),
            "the oversized 16-MiB entry and the oldest 4-MiB entry must be evicted"
        );
        assert!(cache.get(None, None, "album-8", 48).is_some());
        assert_eq!(
            cache.len(),
            8,
            "32 MiB budget holds exactly eight 4-MiB entries"
        );
    }

    /// `clear` drops every entry and resets the byte counter. The
    /// rebuild path relies on this between bind-factory swaps.
    #[test]
    fn cache_clear_resets_count_and_bytes() {
        let cache = AlbumArtCache::new();
        let texture = fake_texture();
        cache.insert(
            None,
            None,
            cache.content_generation(),
            "album-a",
            48,
            texture.clone(),
        );
        cache.insert(
            None,
            None,
            cache.content_generation(),
            "album-b",
            48,
            texture.clone(),
        );
        assert_eq!(cache.len(), 2);
        assert!(cache.approximate_byte_total() > 0);
        cache.clear();
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
        assert_eq!(cache.approximate_byte_total(), 0);
        // The cleared cache is reusable: a fresh insert hits and grows
        // the byte counter back up.
        cache.insert(
            None,
            None,
            cache.content_generation(),
            "album-c",
            48,
            texture.clone(),
        );
        assert!(cache.get(None, None, "album-c", 48).is_some());
    }

    /// The cache key builder must not be exposed verbatim. The
    /// field-separator byte (`\x1f`) is a non-printing control
    /// character; if it leaks into a logged cache key, downstream
    /// debugging becomes impossible. The cache itself hides the key
    /// shape behind `get`/`insert`, but the constant is exercised here
    /// to lock in the choice.
    #[test]
    fn cache_field_separator_is_unit_separator_control_byte() {
        let cache = AlbumArtCache::new();
        let texture = fake_texture();
        // Insert two keys that differ only by the separator byte and
        // confirm both round-trip independently.
        cache.insert(
            None,
            None,
            cache.content_generation(),
            "abc",
            48,
            texture.clone(),
        );
        cache.insert(
            None,
            None,
            cache.content_generation(),
            "a\x1fbc",
            48,
            texture.clone(),
        );
        assert_eq!(cache.len(), 2);
        assert!(cache.get(None, None, "abc", 48).is_some());
        assert!(cache.get(None, None, "a\x1fbc", 48).is_some());
        assert!(
            cache.get(None, None, "a|bc", 48).is_none(),
            "pipe must not alias"
        );
    }

    /// The cache key must bind the source session epoch: a remote source
    /// that is reactivated (reconnect, credential refresh, backend
    /// restart) mints a NEW epoch, and the same album key under the new
    /// epoch must never serve the texture decoded under the previous
    /// epoch's identity (2026-09-07 review finding). Old-epoch entries
    /// simply age out through the bounded eviction; they can never be
    /// *queried* by a newer epoch.
    #[test]
    fn cache_keys_distinguish_source_session_epochs() {
        use crate::architecture::SourceId;
        let cache = AlbumArtCache::new();
        let source = SourceId::radio_browser();
        let texture = fake_texture();

        cache.insert(
            Some(&source),
            Some(1),
            cache.content_generation(),
            "shared-album",
            48,
            texture.clone(),
        );
        assert!(cache
            .get(Some(&source), Some(1), "shared-album", 48)
            .is_some());
        assert!(
            cache
                .get(Some(&source), Some(2), "shared-album", 48)
                .is_none(),
            "a new source epoch must not alias the previous epoch's texture"
        );

        // The reactivated source re-fetches under epoch 2; both entries
        // coexist as distinct keys until bounded eviction drops the old one.
        cache.insert(
            Some(&source),
            Some(2),
            cache.content_generation(),
            "shared-album",
            48,
            texture,
        );
        assert_eq!(cache.len(), 2);
        assert!(cache
            .get(Some(&source), Some(1), "shared-album", 48)
            .is_some());
        assert!(cache
            .get(Some(&source), Some(2), "shared-album", 48)
            .is_some());

        // An un-epoched row never aliases an epoched remote row for the
        // same album key.
        assert!(cache.get(Some(&source), None, "shared-album", 48).is_none());
    }

    /// A single decoded texture larger than the ENTIRE byte budget must
    /// be refused outright. The former eviction loop stopped at one
    /// entry, so exactly such a texture was admitted with the cache
    /// pinned permanently over its cap (2026-09-10 review finding).
    /// No eviction order can make room for it, so `insert` declines
    /// retention while the row keeps displaying the texture it painted.
    #[test]
    fn cache_rejects_a_single_texture_larger_than_the_whole_budget() {
        let cache = AlbumArtCache::new();
        // Derive the fixtures from the budget itself: 4096-wide RGBA
        // rows of exactly `cap_height` fill the whole budget, one extra
        // row exceeds it. (4096×2048 RGBA = 33 554 432 = MAX_CACHE_BYTES.)
        let width = 4096_i32;
        let cap_height = (MAX_CACHE_BYTES / 4 / width as u64) as i32;
        let cap_surface = width as u64 * cap_height as u64 * 4;
        assert_eq!(cap_surface, MAX_CACHE_BYTES);
        cache.insert(
            None,
            None,
            cache.content_generation(),
            "huge-album",
            48,
            solid_texture(width, cap_height + 1),
        );
        assert_eq!(cache.len(), 0, "the oversized texture must not be retained");
        assert_eq!(cache.approximate_byte_total(), 0);
        assert!(cache.get(None, None, "huge-album", 48).is_none());

        // The exact-budget texture is the largest admissible single entry.
        cache.insert(
            None,
            None,
            cache.content_generation(),
            "cap-album",
            48,
            solid_texture(width, cap_height),
        );
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.approximate_byte_total(), MAX_CACHE_BYTES);
        assert!(cache.get(None, None, "cap-album", 48).is_some());

        // A subsequent small insert evicts the cap-sized entry cleanly:
        // the cache never rests above its budget.
        cache.insert(
            None,
            None,
            cache.content_generation(),
            "small-album",
            48,
            fake_texture(),
        );
        assert_eq!(cache.len(), 1);
        assert!(cache.get(None, None, "cap-album", 48).is_none());
        assert!(cache.get(None, None, "small-album", 48).is_some());
        assert!(cache.approximate_byte_total() <= MAX_CACHE_BYTES);
    }

    /// The cache key must bind an artwork/content generation: a library
    /// rebuild (FullSync) inside the SAME source session bumps the
    /// generation, so covers changed by the new data are re-resolved
    /// instead of serving the pre-sync pixels (2026-09-10 review
    /// finding — the key carried the source epoch but no content
    /// generation, so a same-session FullSync left changed covers
    /// stale). Old-generation entries stay resident only until bounded
    /// eviction drops them; no lookup can ever query them.
    #[test]
    fn cache_content_generation_bump_invalidates_changed_covers() {
        let cache = AlbumArtCache::new();
        let texture = fake_texture();
        cache.insert(
            None,
            None,
            cache.content_generation(),
            "same-album",
            48,
            texture.clone(),
        );
        assert!(cache.get(None, None, "same-album", 48).is_some());

        // FullSync lands: the browser rebuild bumps the generation.
        cache.bump_content_generation();
        assert!(
            cache.get(None, None, "same-album", 48).is_none(),
            "after a content bump the previous cover must not be served"
        );

        // The changed cover re-resolves and re-enters under the new
        // generation; the old-generation entry is unreachable but still
        // resident until bounded eviction drops it (same contract as the
        // epoch test above).
        cache.insert(
            None,
            None,
            cache.content_generation(),
            "same-album",
            48,
            texture,
        );
        assert!(cache.get(None, None, "same-album", 48).is_some());
        assert_eq!(cache.len(), 2);

        // A fresh cache starts at generation 0 and never aliases across
        // a bump even for local (epoch-less) rows.
        assert_eq!(AlbumArtCache::new().content_generation(), 0);
    }
}
