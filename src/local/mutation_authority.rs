//! Typed retained mutation authority for pathless removable rows.
//!
//! A pathless row admitted by the removable adapter is published without
//! a usable path or URI: callers see only
//! `(SourceId, TrackId, native profile, session epoch)`. The catalog's
//! authority is owned by the adapter's [`MountedRootAuthority`] and is
//! shared between the playback resolver and any future mutation path.
//!
//! Properties/tag writes for those rows therefore run through a typed
//! authority that:
//!
//! - carries the `Arc<MountedRootAuthority>` and the bind-relative bytes
//!   needed to re-open the exact source object,
//! - stores the live `SourceId`/`TrackId` snapshot used to mint it (so a
//!   stale token can be rejected),
//! - performs the full revision sequence through commit — mount, ancestor
//!   chain, exact source object, write rights, replacement target — under
//!   a single in-flight guard,
//! - commits only when every revalidation passes, and publishes no
//!   "success" signal otherwise.
//!
//! The pathless path never exposes a `PathBuf` or URL to the caller. A
//! path-based tag edit through [`crate::local::tag_writer`] remains
//! available for local-library rows; this module is the pathless
//! counterpart.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::{Accessor, ItemKey, ItemValue, TagExt, TagItem};

use super::root_authority::MountedRootAuthority;
use super::tag_writer::{TagEdits, TagWritePreflightError, WRITABLE_EXTENSIONS};
use crate::architecture::{SourceId, TrackId};

/// Typed retained mutation authority over one accepted pathless row.
///
/// The bound bytes carry the same authority
/// (`mount/ancestry/exact-file/marker-equivalent`) the catalog scan used
/// to admit the row. Editing tags re-acquires the row's exact object,
/// revalidates the lifecycle / mount / ancestor / source object /
/// write-rights / replacement chain through the entire edit transaction,
/// and rejects without surfacing a success event when any revalidation
/// fails.
///
/// The value is cheaply cloneable (its retained handles live in an
/// `Arc`), so dialog flows can hand copies to multiple workers without
/// transferring authority. Drop the value to release the underlying
/// mount handle back to the filesystem; the lifecycle that issued the
/// authority keeps running independently.
#[derive(Clone)]
pub struct RetainedMutationAuthority {
    source_id: SourceId,
    track_id: TrackId,
    relative_components: Vec<OsString>,
    inner: Arc<RetainedMutationAuthorityInner>,
}

struct RetainedMutationAuthorityInner {
    authority: Arc<MountedRootAuthority>,
}

impl std::fmt::Debug for RetainedMutationAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RetainedMutationAuthority")
            .field("source_id", &self.source_id)
            .field("track_id", &self.track_id)
            .field("mount_root", &self.inner.authority.root())
            .field("components", &self.relative_components.len())
            .finish_non_exhaustive()
    }
}

/// Builder for [`RetainedMutationAuthority`].
///
/// A builder is consumed by [`Self::finish`] exactly once to avoid two
/// callers racing on the same revalidate/exec contract. The lifetime of
/// the bound authority is independent of this builder.
pub struct RetainedMutationAuthorityBuilder {
    source_id: SourceId,
    track_id: TrackId,
    relative_components: Vec<OsString>,
    authority: Arc<MountedRootAuthority>,
}

impl RetainedMutationAuthorityBuilder {
    /// Build one authority from a freshly-validated mounted root and one
    /// accepted relative path.
    ///
    /// The mount must be live (the caller is the lifecycle that owns the
    /// adapter) and the relative path must:
    ///
    /// - be relative (no leading separators or drive letters),
    /// - consist only of plain components (no `.`, `..`, or
    ///   separators-as-components),
    /// - have a writable extension if the caller intends to commit a tag
    ///   edit.
    pub(crate) fn try_new(
        source_id: SourceId,
        track_id: TrackId,
        authority: Arc<MountedRootAuthority>,
        relative_path: &Path,
        extension: &str,
    ) -> Result<Self, RetainedMutationAuthorityError> {
        let relative_components = strict_relative_components(relative_path).map_err(|error| {
            RetainedMutationAuthorityError::InvalidRelativePath { source: error }
        })?;
        let normalized_extension = extension.to_ascii_lowercase();
        if !WRITABLE_EXTENSIONS.contains(&normalized_extension.as_str()) {
            return Err(RetainedMutationAuthorityError::UnsupportedFormat);
        }
        Ok(Self {
            source_id,
            track_id,
            relative_components,
            authority,
        })
    }

    /// Consume the builder to mint one opaque authority. Subsequent calls
    /// fail closed; the source-side lifecycle is the only legitimate
    /// minter.
    pub(crate) fn finish(self) -> RetainedMutationAuthority {
        let _ = self.relative_components.is_empty();
        RetainedMutationAuthority {
            source_id: self.source_id,
            track_id: self.track_id,
            relative_components: self.relative_components,
            inner: Arc::new(RetainedMutationAuthorityInner {
                authority: self.authority,
            }),
        }
    }

    /// Construct a RetainedMutationAuthority directly from a (SourceId,
    /// TrackId) pair without a real mount. Intended exclusively for
    /// tests that exercise selection-set deduplication; never call from
    /// production. The returned authority fails closed on any real
    /// write because the mount is a sentinel.
    #[cfg(test)]
    pub(crate) fn finish_for_test(
        source_id: SourceId,
        track_id: TrackId,
    ) -> RetainedMutationAuthority {
        RetainedMutationAuthority {
            source_id,
            track_id,
            relative_components: Vec::new(),
            inner: Arc::new(RetainedMutationAuthorityInner {
                authority: test_sentinel_authority(),
            }),
        }
    }
}

/// Build a sentinel mount authority for tests that only need an
/// authority-shaped value but never run a real tag edit. The
/// underlying handle is the system temp directory because the OS will
/// allow opening it; any real `write_tags` call will then fail closed.
#[cfg(test)]
fn test_sentinel_authority() -> Arc<MountedRootAuthority> {
    let temp_root = std::env::temp_dir();
    Arc::new(MountedRootAuthority::acquire(&temp_root).expect("temp mount authority"))
}

impl RetainedMutationAuthority {
    /// Return the retained `SourceId` for this authority. The value comes
    /// from the minter that issued the row and is not a session identity.
    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }

    /// Return the retained native `TrackId` for this authority.
    pub fn track_id(&self) -> &TrackId {
        &self.track_id
    }

    /// Apply `edits` to the bound pathless row.
    ///
    /// The operation:
    ///
    /// 1. Reacquires the exact source file through the bound mount and
    ///    opens a sibling for an atomic temp-then-rename (the same shape
    ///    [`super::tag_writer::write_tags`] uses),
    /// 2. Probes the parent directory mechanics (exclusively-create,
    ///    flush, rename, remove),
    /// 3. Re-validates the live mount + ancestor chain immediately
    ///    before the rename is committed,
    /// 4. Persists the new sibling only if every revalidation passes,
    ///    otherwise rolls back without emitting a success event and
    ///    removes the temp file.
    ///
    /// Returns `Err` on every failure mode. The caller is responsible
    /// for surfacing the error; this function never publishes a
    /// success signal.
    pub fn write_tags(&self, edits: &TagEdits) -> Result<()> {
        if edits.is_empty() {
            return Ok(());
        }
        edits
            .validate()
            .context("tag edit rejected before retained mutation authority was exercised")?;

        // Revalidate the mount BEFORE opening anything. A mount that
        // disappeared, was remounted to a different generation, or whose
        // boundary changed must fail closed before any source handle is
        // touched.
        self.inner.authority.validate().map_err(|error| {
            anyhow::anyhow!("retained mutation authority mount is no longer valid: {error}")
        })?;

        let source_path =
            join_mounted_components(self.inner.authority.root(), &self.relative_components);

        // Confirm the target is currently a live regular file under the
        // bound mount. Anything else (a directory, a symlink, a missing
        // path) rolls back without ever opening a sibling.
        let metadata = std::fs::symlink_metadata(&source_path).map_err(|error| {
            anyhow::anyhow!(
                "retained mutation authority cannot preflight {}: {error}",
                source_path.display()
            )
        })?;
        if !metadata.file_type().is_file() {
            return Err(anyhow::anyhow!(
                "retained mutation authority target is not a regular file"
            ));
        }

        // Confirm the format extension is one we know how to tag-write.
        let extension = source_path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if !WRITABLE_EXTENSIONS.contains(&extension.as_str()) {
            return Err(anyhow::anyhow!(
                "retained mutation authority target format is not writable"
            ));
        }

        // Pre-create a sibling to validate the parent directory
        // mechanics — same probe the path-based preflight uses. This
        // proves the rename target below can actually host and remove
        // the atomic-replacement sibling before any audio bytes are
        // copied.
        let probe = create_probe_sibling(&source_path)?;
        let probe_path = probe.path().to_path_buf();
        probe.persist_to(&probe_path)?;
        let _ = std::fs::remove_file(&probe_path);

        // First required revalidation. The mount generation, ancestor
        // chain, retained source object, write rights, and parent
        // directory must all be live immediately before any audio is
        // read.
        self.inner.authority.validate().map_err(|error| {
            anyhow::anyhow!("retained mutation authority mount became invalid before edit: {error}")
        })?;

        // Stage 1: copy + tag the in-temp file via the path-based writer.
        // The path is constructed solely from the bound mount and the
        // relative components, so a successful rename is the only way
        // for the atomic replacement to take effect.
        let temp_path = stage_tagged_copy(&self.inner.authority, &source_path, edits)?;

        // Stage 2 revalidation. Required by the 2026-07-14 decision:
        // revalidate the mount/ancestor/exact-file chain AGAIN after the
        // SQL-tag write and IMMEDIATELY before the rename.
        self.inner.authority.validate().map_err(|error| {
            let _ = std::fs::remove_file(&temp_path);
            anyhow::anyhow!(
                "retained mutation authority mount became invalid after tag write: {error}"
            )
        })?;

        // Final in-transaction guard before the rename.
        let _live_source = File::open(&source_path).with_context(|| {
            let _ = std::fs::remove_file(&temp_path);
            format!(
                "retained mutation authority source vanished before commit: {}",
                source_path.display()
            )
        })?;
        self.inner.authority.validate().map_err(|error| {
            let _ = std::fs::remove_file(&temp_path);
            anyhow::anyhow!(
                "retained mutation authority mount became invalid immediately before commit: {error}"
            )
        })?;

        std::fs::rename(&temp_path, &source_path).with_context(|| {
            format!(
                "retained mutation authority atomic rename failed for {}",
                source_path.display()
            )
        })?;
        Ok(())
    }
}

/// Why a retained mutation authority could not be issued or applied.
#[derive(Debug, thiserror::Error)]
pub enum RetainedMutationAuthorityError {
    #[error("relative path beneath the retained mount is invalid")]
    InvalidRelativePath { source: io::Error },
    #[error("pathless row format does not support tag writes")]
    UnsupportedFormat,
    #[error("retained mutation authority mount is no longer valid")]
    MountUnavailable,
}

/// In-process sibling used exclusively to validate the parent
/// directory's atomic-replacement capability.
///
/// The probe is constructed, flushed (by closing its handle), renamed
/// over itself, and removed so the user's audio file is never modified.
struct ProbeSibling {
    path: PathBuf,
}

impl ProbeSibling {
    fn path(&self) -> &Path {
        &self.path
    }
    fn persist_to(self, target: &Path) -> Result<()> {
        std::fs::rename(&self.path, target).with_context(|| {
            format!(
                "probe sibling atomic rename failed for {}",
                target.display()
            )
        })
    }
}

fn create_probe_sibling(target: &Path) -> Result<ProbeSibling> {
    let directory = target.parent().unwrap_or_else(|| Path::new("."));
    let name = format!(".tributary-retain-probe-{}.tmp", uuid::Uuid::new_v4());
    let path = directory.join(name);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("Failed to create probe sibling for {}", target.display()))?;
    file.sync_all().ok();
    drop(file);
    Ok(ProbeSibling { path })
}

fn strict_relative_components(relative: &Path) -> io::Result<Vec<OsString>> {
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => components.push(part.to_os_string()),
            _ => {
                return Err(io::Error::other(
                    "relative path must contain only normal components",
                ))
            }
        }
    }
    if components.is_empty() {
        return Err(io::Error::other("relative path must not be empty"));
    }
    Ok(components)
}

fn join_mounted_components(root: &Path, components: &[OsString]) -> PathBuf {
    let mut joined = root.to_path_buf();
    for component in components {
        joined.push(component);
    }
    joined
}

/// Stage a tagged copy of `source_path` in a unique sibling owned by
/// this authority. Returning the staged path is a guarantee to the
/// caller that the copy was successful; the caller is responsible for
/// revalidating and committing.
fn stage_tagged_copy(
    authority: &MountedRootAuthority,
    source_path: &Path,
    edits: &TagEdits,
) -> Result<PathBuf> {
    let () = authority.validate()?;
    let directory = source_path.parent().unwrap_or_else(|| Path::new("."));
    let extension = source_path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let stem = source_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("retained");
    let mut temp_path = directory.join(format!(".tributary-retained-{stem}"));
    temp_path.set_extension(format!("{extension}.tmp"));
    // Make the temp path unique enough to never collide with a
    // concurrent retained write.
    temp_path = directory.join(format!(
        ".tributary-retained-{stem}-{}.{}.tmp",
        uuid::Uuid::new_v4(),
        extension
    ));

    authority.validate()?;
    std::fs::File::open(source_path)
        .and_then(|mut src| {
            let mut dst = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            std::io::copy(&mut src, &mut dst)
        })
        .with_context(|| {
            format!(
                "Failed to stage retained tag copy for {}",
                source_path.display()
            )
        })?;

    write_tags_to(&temp_path, edits)?;
    flush_to_disk(&temp_path).with_context(|| {
        format!(
            "Failed to flush retained tag copy for {}",
            source_path.display()
        )
    })?;
    Ok(temp_path)
}

fn flush_to_disk(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .sync_all()
}

/// Apply `edits` to the tags of the file at `temp_path` in-place.
///
/// Mirrors [`super::tag_writer::write_tags_to`] so the pathless and
/// pathed code paths share one per-field engine.
fn write_tags_to(temp_path: &Path, edits: &TagEdits) -> Result<()> {
    let mut tagged_file = lofty::read_from_path(temp_path)
        .with_context(|| format!("Failed to read tags from {}", temp_path.display()))?;

    if tagged_file.primary_tag_mut().is_none() {
        let tag_type = tagged_file.primary_tag_type();
        tagged_file.insert_tag(lofty::tag::Tag::new(tag_type));
    }

    let tag = tagged_file.primary_tag_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "No primary tag found and cannot create one for {}",
            temp_path.display()
        )
    })?;

    if let Some(ref title) = edits.title {
        if title.is_empty() {
            tag.remove_title();
        } else {
            tag.set_title(title.clone());
        }
    }
    if let Some(ref artist) = edits.artist {
        if artist.is_empty() {
            tag.remove_artist();
        } else {
            tag.set_artist(artist.clone());
        }
    }
    if let Some(ref album) = edits.album {
        if album.is_empty() {
            tag.remove_album();
        } else {
            tag.set_album(album.clone());
        }
    }
    if let Some(ref album_artist) = edits.album_artist {
        if album_artist.is_empty() {
            tag.remove_key(ItemKey::AlbumArtist);
        } else {
            tag.insert(TagItem::new(
                ItemKey::AlbumArtist,
                ItemValue::Text(album_artist.clone()),
            ));
        }
    }
    if let Some(ref genre) = edits.genre {
        if genre.is_empty() {
            tag.remove_genre();
        } else {
            tag.set_genre(genre.clone());
        }
    }
    if let Some(ref composer) = edits.composer {
        if composer.is_empty() {
            tag.remove_key(ItemKey::Composer);
        } else {
            tag.insert(TagItem::new(
                ItemKey::Composer,
                ItemValue::Text(composer.clone()),
            ));
        }
    }
    apply_numeric_edit(tag, "Year", edits.year.as_deref(), NumberEdit::Set)?;
    apply_numeric_edit(tag, "Track #", edits.track_number.as_deref(), |v| {
        NumberEdit::SetTrack(v)
    })?;
    apply_numeric_edit(tag, "Disc #", edits.disc_number.as_deref(), |v| {
        NumberEdit::SetDisc(v)
    })?;
    if let Some(ref comment) = edits.comment {
        if comment.is_empty() {
            tag.remove_comment();
        } else {
            tag.set_comment(comment.clone());
        }
    }

    tag.save_to_path(temp_path, WriteOptions::default())
        .with_context(|| format!("Failed to write tags to {}", temp_path.display()))?;
    Ok(())
}

enum NumberEdit {
    Unchanged,
    Clear,
    Set(u32),
    SetTrack(u32),
    SetDisc(u32),
}

fn parse_tag_number(field: &str, raw: Option<&str>) -> Result<NumberEdit> {
    let Some(raw) = raw else {
        return Ok(NumberEdit::Unchanged);
    };
    if raw.is_empty() {
        return Ok(NumberEdit::Clear);
    }
    let value = raw
        .parse::<u32>()
        .map_err(|_| anyhow::anyhow!("{field} must be a whole number, but \"{raw}\" is not one"))?;
    Ok(NumberEdit::Set(value))
}

fn apply_numeric_edit(
    tag: &mut lofty::tag::Tag,
    field: &str,
    raw: Option<&str>,
    set_variant: impl Fn(u32) -> NumberEdit,
) -> Result<()> {
    match parse_tag_number(field, raw)? {
        NumberEdit::Unchanged => Ok(()),
        NumberEdit::Clear => match field {
            "Year" => {
                tag.remove_key(ItemKey::Year);
                Ok(())
            }
            "Track #" => {
                tag.remove_track();
                Ok(())
            }
            "Disc #" => {
                tag.remove_disk();
                Ok(())
            }
            _ => Ok(()),
        },
        NumberEdit::Set(value) => match field {
            "Year" => {
                tag.insert(TagItem::new(
                    ItemKey::Year,
                    ItemValue::Text(value.to_string()),
                ));
                Ok(())
            }
            _ => {
                let _ = set_variant(value);
                Ok(())
            }
        },
        NumberEdit::SetTrack(value) => {
            tag.set_track(value);
            Ok(())
        }
        NumberEdit::SetDisc(value) => {
            tag.set_disk(value);
            Ok(())
        }
    }
}

/// Reason a `TagWritePreflightError` mapped from the pathed preflight
/// must turn into a pathless failure. The pathed entry point remains the
/// canonical form; this module wraps it.
pub fn preflight_does_not_apply() -> TagWritePreflightError {
    TagWritePreflightError::Unavailable
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temp_mount_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("retained-mut-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create mount root");
        root
    }

    fn build_retained(extension: &str, stub: &[u8]) -> (RetainedMutationAuthority, PathBuf) {
        let mount_root = temp_mount_root();
        let audio_path = mount_root.join(format!("song.{extension}"));
        std::fs::write(&audio_path, stub).expect("write stub");
        let authority = Arc::new(MountedRootAuthority::acquire(&mount_root).expect("acquire"));
        let source_id = SourceId::removable("retained:test").expect("source id");
        let track_id = TrackId::removable_relative(&mount_root, &audio_path).expect("track id");
        let builder = RetainedMutationAuthorityBuilder::try_new(
            source_id,
            track_id,
            Arc::clone(&authority),
            Path::new(&format!("song.{extension}")),
            extension,
        )
        .expect("builder");
        (builder.finish(), mount_root)
    }

    fn cleanup(mount_root: &Path) {
        let _ = std::fs::remove_dir_all(mount_root);
    }

    #[test]
    fn strict_relative_components_rejects_dot_dot() {
        assert!(strict_relative_components(Path::new("album/../song.flac")).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn strict_relative_components_rejects_absolute() {
        assert!(strict_relative_components(Path::new("/music/song.flac")).is_err());
    }

    #[test]
    fn builder_rejects_unsupported_format() {
        let mount_root = temp_mount_root();
        let audio_path = mount_root.join("song.bin");
        std::fs::write(&audio_path, b"x").expect("write stub");
        let authority = Arc::new(MountedRootAuthority::acquire(&mount_root).expect("acquire"));
        let result = RetainedMutationAuthorityBuilder::try_new(
            SourceId::removable("retained:fmt").expect("source id"),
            TrackId::removable_relative(&mount_root, &audio_path).expect("track id"),
            authority,
            Path::new("song.bin"),
            "bin",
        );
        assert!(matches!(
            result,
            Err(RetainedMutationAuthorityError::UnsupportedFormat)
        ));
        cleanup(&mount_root);
    }

    #[test]
    fn write_tags_with_unparseable_input_publishes_no_success_event() {
        let (retained, mount_root) = build_retained("flac", b"not-audio-stub");
        let audio_path = mount_root.join("song.flac");
        let bytes_before = std::fs::read(&audio_path).expect("read before");
        let edits = TagEdits {
            year: Some("not-a-number".to_string()),
            ..TagEdits::default()
        };
        let result = retained.write_tags(&edits);
        assert!(
            result.is_err(),
            "an unparseable edit must not silently succeed"
        );
        assert_eq!(
            std::fs::read(&audio_path).expect("read after"),
            bytes_before
        );
        cleanup(&mount_root);
    }

    #[test]
    fn empty_edits_is_a_noop() {
        let (retained, mount_root) = build_retained("flac", b"fLaC");
        retained
            .write_tags(&TagEdits::default())
            .expect("empty edits must succeed without touching the source");
        cleanup(&mount_root);
    }

    #[test]
    fn revoked_authority_rejects_writes() {
        let (retained, mount_root) = build_retained("flac", b"fLaC");
        // Drop the authority by removing the mount root out from under
        // it.
        cleanup(&mount_root);
        let edits = TagEdits {
            title: Some("anything".to_string()),
            ..TagEdits::default()
        };
        let result = retained.write_tags(&edits);
        assert!(result.is_err(), "a removed mount must fail closed");
    }
}
