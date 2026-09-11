//! The album pane's artwork resolution tree.
//!
//! Split verbatim from `album_pane_art.rs` so each module stays under
//! the file-size budget. Every boundary is unchanged: a row that
//! carries a registry identity resolves its `file://` artwork through
//! the retained local-media authority (exact retained file
//! capability, never a reopened pathname); remote rows go through
//! the lease-isolated `SourceRegistry::resolve_artwork`; only rows
//! with no authority chain at all keep the transitional direct path,
//! and every failure fails closed for the local path.

use crate::architecture::media::ResolvedHttpRequest;
use crate::architecture::SourceId;

use crate::ui::objects::AlbumArtCandidate;

pub(super) enum ResolvedArtKind {
    NoArtwork,
    /// A retained exact-file capability resolved through the source
    /// registry — the pane's authority-correct local extraction input.
    ResolvedFile {
        media: crate::local::resolver::ResolvedLocalMedia,
    },
    /// Transitional raw `file://` path for rows with no retained
    /// authority chain. Never produced for rows that carry a source
    /// identity.
    DirectFile {
        uri: String,
    },
    DirectUrl {
        url: String,
    },
    ResolvedRequest(Box<ResolvedHttpRequest>),
}

/// Which extraction route a `file://` album row must take.
///
/// A row that carries a registry identity (source + session epoch) MUST
/// resolve its artwork through the retained local-media authority: the
/// former code returned the raw `file://` URI and freshly opened its
/// pathname, silently bypassing retained removable-media authority
/// (2026-09-10 review finding). Only rows with no authority chain at all
/// (external OS-opened files) keep the transitional direct path.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum LocalFileArtRoute {
    /// Resolve `ResolvedLocalMedia` through the registry, then extract
    /// through the retained handle.
    RetainedAuthority,
    /// No authority chain exists; keep the transitional direct path.
    TransitionalDirect,
}

pub(super) fn local_file_art_route(has_registry_identity: bool) -> LocalFileArtRoute {
    if has_registry_identity {
        LocalFileArtRoute::RetainedAuthority
    } else {
        LocalFileArtRoute::TransitionalDirect
    }
}

/// The pane's registry-backed identity for one row: the registry handle,
/// the source id, and the source session epoch.
type PaneRegistryIdentity<'a> = (&'a crate::source_registry::SourceRegistry, SourceId, u64);

/// Build the architecture `TrackId` for one pane resolution, logging and
/// rejecting the row when its track id is invalid.
fn pane_track_id(candidate: &AlbumArtCandidate) -> Option<crate::architecture::TrackId> {
    match crate::architecture::TrackId::new(candidate.track_id.clone()) {
        Ok(track_id) => Some(track_id),
        Err(error) => {
            tracing::debug!(
                %error,
                track_id = %candidate.track_id,
                "Album pane skipped invalid track id while resolving artwork"
            );
            None
        }
    }
}

pub(super) async fn resolve_kind(
    source_registry: Option<crate::source_registry::SourceRegistry>,
    source_id: Option<SourceId>,
    source_epoch: Option<u64>,
    candidate: &AlbumArtCandidate,
    cover_art_url: String,
    uri: String,
) -> ResolvedArtKind {
    let registry_identity = match (source_registry.as_ref(), source_id, source_epoch) {
        (Some(registry), Some(id), Some(epoch)) => Some((registry, id, epoch)),
        _ => None,
    };
    // Retained local-media authority first: a row whose playable locator
    // is a file:// URI keeps its authority through the album pane — no
    // remote resolver is consulted first, no opaque credentials are
    // minted, and no pathname is reopened.
    if uri.starts_with("file://") {
        if let Some(resolved) =
            resolve_local_file_art(registry_identity.as_ref(), candidate, &uri).await
        {
            return resolved;
        }
    }
    // Lease-isolated remote resolver.
    if let Some(resolved) =
        resolve_remote_artwork(registry_identity.as_ref(), candidate, &cover_art_url).await
    {
        return resolved;
    }
    // Legacy direct URL fallback for rows that ship one and do not
    // resolve through any source registry.
    if !cover_art_url.is_empty() {
        return ResolvedArtKind::DirectUrl { url: cover_art_url };
    }
    ResolvedArtKind::NoArtwork
}

/// The `file://` arm of the pane resolver. A row that carries a registry
/// identity resolves an exact retained file capability and extracts
/// through it — never a reopened pathname (2026-09-10 review finding).
/// Only rows with no authority chain at all (external OS-opened files)
/// take the transitional direct path.
///
/// Returns `None` when resolution should fall through to the remote
/// artwork resolver: the authority reported the stream as remote, or the
/// retained authority refused and the lease-isolated route may still
/// serve the artwork. Any `Some(_)` is terminal.
async fn resolve_local_file_art(
    registry_identity: Option<&PaneRegistryIdentity<'_>>,
    candidate: &AlbumArtCandidate,
    uri: &str,
) -> Option<ResolvedArtKind> {
    match local_file_art_route(registry_identity.is_some()) {
        LocalFileArtRoute::RetainedAuthority => {
            let Some((registry, id, epoch)) = registry_identity else {
                // Unreachable by construction — the route above was chosen
                // from this very predicate — but fail closed regardless:
                // never fall back to a raw pathname open.
                return Some(ResolvedArtKind::NoArtwork);
            };
            resolve_retained_file_art(registry, id, *epoch, candidate).await
        }
        LocalFileArtRoute::TransitionalDirect => {
            // Transitional path for rows with NO retained authority
            // chain (e.g., OS-opened external files).
            Some(ResolvedArtKind::DirectFile {
                uri: uri.to_string(),
            })
        }
    }
}

/// Resolve one identity-carrying `file://` row through the retained
/// local-media authority: an exact retained file capability is resolved
/// and the artwork extracted through it — never a reopened pathname.
///
/// Returns `None` when resolution should fall through to the remote
/// artwork resolver: the authority reported the stream as remote, or the
/// retained authority refused and the lease-isolated route may still
/// serve the artwork. Any `Some(_)` is terminal.
async fn resolve_retained_file_art(
    registry: &crate::source_registry::SourceRegistry,
    id: &SourceId,
    epoch: u64,
    candidate: &AlbumArtCandidate,
) -> Option<ResolvedArtKind> {
    let Some(track_id) = pane_track_id(candidate) else {
        // Fail closed for an invalid id — never fall back to a raw
        // pathname open.
        return Some(ResolvedArtKind::NoArtwork);
    };
    match registry.resolve_stream(*id, epoch, track_id).await {
        Ok(crate::source_registry::ResolvedSourceStream::File(media)) => {
            Some(ResolvedArtKind::ResolvedFile { media })
        }
        Ok(crate::source_registry::ResolvedSourceStream::Http(_)) => {
            // The source's at-use resolution says this track's
            // stream is remote, so the file:// locator is stale;
            // defer to the remote artwork resolution below
            // instead of opening the stale path.
            tracing::debug!(
                source_id = %id,
                track_id = %candidate.track_id,
                "Album pane file row resolved to a remote stream; deferring to remote artwork"
            );
            None
        }
        Err(error) => {
            // The retained authority refused resolution. Fail
            // closed for the local path — never fall back to a
            // raw pathname open — and let the remote artwork
            // resolution below try the lease-isolated route.
            tracing::debug!(
                %error,
                source_id = %id,
                track_id = %candidate.track_id,
                "Album pane retained artwork authority unavailable"
            );
            None
        }
    }
}

/// The remote arm of the pane resolver: one lease-isolated
/// [`SourceRegistry::resolve_artwork`] call for a registry-backed row.
/// Returns `None` when resolution should fall through to the legacy
/// direct-URL path (no registry identity, or the backend errored);
/// any `Some(_)` is terminal.
async fn resolve_remote_artwork(
    registry_identity: Option<&PaneRegistryIdentity<'_>>,
    candidate: &AlbumArtCandidate,
    cover_art_url: &str,
) -> Option<ResolvedArtKind> {
    let (registry, id, epoch) = registry_identity?;
    let Some(track_id) = pane_track_id(candidate) else {
        return Some(ResolvedArtKind::NoArtwork);
    };
    match registry.resolve_artwork(*id, *epoch, track_id).await {
        Ok(Some(request)) => Some(ResolvedArtKind::ResolvedRequest(Box::new(request))),
        Ok(None) => {
            // Remote source returned no artwork for this track — try
            // the legacy embedded cover URL on the row before giving
            // up, so a row that has both a remote and a URL still
            // gets a thumbnail.
            if !cover_art_url.is_empty() {
                return Some(ResolvedArtKind::DirectUrl {
                    url: cover_art_url.to_string(),
                });
            }
            Some(ResolvedArtKind::NoArtwork)
        }
        Err(error) => {
            tracing::debug!(
                %error,
                source_id = %id,
                track_id = %candidate.track_id,
                "Album pane artwork resolver fell back after backend error"
            );
            None
        }
    }
}
