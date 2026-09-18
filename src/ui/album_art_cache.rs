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

// The cache's regression suite lives in `album_art_cache/tests.rs` —
// split verbatim out of this file so each module stays under the
// file-size budget. No assertion or boundary check moved.
#[cfg(test)]
mod tests;
