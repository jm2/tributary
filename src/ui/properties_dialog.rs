//! Song Properties dialog — metadata viewing and editing.
//!
//! Supports single-track and batch (multi-track) editing.  All edits
//! are staged in the dialog and only written to disk when the user
//! clicks **Save**.  Cancel discards all changes.
//!
//! Batch mode only exposes fields that make sense to set uniformly
//! across multiple tracks (artist, album, album artist, genre, year,
//! disc number, comment).  Fields with mixed values show a "Mixed"
//! placeholder.
//!
//! An optional **MusicBrainz Lookup** button (single-track only) queries
//! the MusicBrainz API and populates the form — but still requires the
//! user to click Save.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use tracing::{info, warn};

use crate::architecture::{SourceId, TrackId};
use crate::local::tag_writer::{
    LocalMutationTarget, LocalTagPreflightError, LocalTagWriteConflict, TagEdits,
    TagWritePreflightError,
};
use crate::source_registry::{RemovableMutationTarget, SourceRegistry};

/// Exactly what one selected row is saved through.
#[derive(Clone, Debug)]
pub enum SaveTarget {
    /// Retained identity, containing directory, and content revision for one
    /// exact local-library file. A bare pathname is never enough: the file the
    /// user selected must still be the file the save replaces.
    Local(LocalMutationTarget),
    /// A local row's validated native pathname awaiting exact-object
    /// admission. The context menu admits it — off the UI thread — and
    /// replaces this with [`SaveTarget::Local`] before the dialog can open;
    /// an unresolved value never reaches a preflight or a write.
    PendingLocal(PendingLocalMutation),
    /// A removable row's identity awaiting resolution through its exact live
    /// source session. The context menu replaces this with
    /// [`SaveTarget::Removable`] before the dialog can open; an unresolved
    /// value never reaches a preflight or a write.
    PendingRemovable(PendingRemovableMutation),
    /// Retained mutation authority over one exact removable file.
    Removable(RemovableMutationTarget),
}

impl PartialEq for SaveTarget {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Local(left), Self::Local(right)) => left == right,
            (Self::PendingLocal(left), Self::PendingLocal(right)) => left == right,
            (Self::PendingRemovable(left), Self::PendingRemovable(right)) => left == right,
            // Retained authorities are equal when they name the same exact
            // source-scoped file, never when they merely hold equal evidence.
            (Self::Removable(left), Self::Removable(right)) => {
                left.source_id() == right.source_id() && left.track_id() == right.track_id()
            }
            _ => false,
        }
    }
}

impl Eq for SaveTarget {}

/// The native pathname a dialog save must admit to an exact object — never a
/// bare pathname — before it may commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLocalMutation {
    pub path: PathBuf,
}

/// The removable identity a dialog save must resolve before it may commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRemovableMutation {
    pub source_id: crate::architecture::SourceId,
    pub session_epoch: u64,
    pub track_id: crate::architecture::TrackId,
}

/// Deduplication identity for one save target. Repeated playlist rows may
/// refer to the same file or the same removable identity; each exact target
/// is probed and written once.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum SaveTargetKey {
    Local(PathBuf),
    Removable(crate::architecture::SourceId, String),
}

fn save_target_key(target: &SaveTarget) -> Option<SaveTargetKey> {
    match target {
        SaveTarget::Local(local) => Some(SaveTargetKey::Local(local.path().to_path_buf())),
        SaveTarget::Removable(authority) => Some(SaveTargetKey::Removable(
            authority.source_id(),
            authority.track_id().as_str().to_owned(),
        )),
        // An unresolved pending value has no identity to deduplicate and must
        // never reach the probe or write phase. Both pending kinds are
        // admitted to their retained exact-object form before the dialog
        // opens, so reaching this arm is a caller wiring fault.
        SaveTarget::PendingLocal(_) | SaveTarget::PendingRemovable(_) => None,
    }
}

/// One exactly-deduplicated save target per distinct file or removable
/// identity, in selection order.
fn unique_save_targets(tracks: &[TrackInfo]) -> Vec<SaveTarget> {
    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    for track in tracks {
        let Some(key) = save_target_key(&track.target) else {
            continue;
        };
        if seen.insert(key) {
            targets.push(track.target.clone());
        }
    }
    targets
}

/// Release-enforced gate on the ORIGINAL selection, before any
/// deduplication: a row still carrying an unresolved pending identity — a
/// local pathname not yet admitted to an exact object, or a removable
/// identity not yet resolved through its live session — is a caller wiring
/// fault. A pending row has no deduplication identity, so dedup alone would
/// silently drop that row's edit and report success over a partial write
/// set; the check therefore runs on the original selection, where the whole
/// dialog refuses instead.
fn selection_has_unresolved_target(tracks: &[TrackInfo]) -> bool {
    tracks.iter().any(|track| {
        matches!(
            track.target,
            SaveTarget::PendingLocal(_) | SaveTarget::PendingRemovable(_)
        )
    })
}

/// Information about a track passed into the dialog.
#[derive(Debug, Clone)]
pub struct TrackInfo {
    /// Exact save target for this row: a validated local path or a retained
    /// removable mutation authority.
    pub target: SaveTarget,
    /// Current metadata values (for pre-populating the form).
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub composer: String,
    pub year: String,
    pub track_number: String,
    pub disc_number: String,
    pub format: String,
    pub bitrate: String,
    pub sample_rate: String,
    pub duration: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagEditingAvailability {
    Checking,
    Saving,
    Ready,
    UnsupportedFormat,
    InvalidFile,
    Unavailable,
    /// At least one selected file changed on disk since Properties opened.
    /// This is a localized conflict, not a read-only/unavailable failure:
    /// the competing file/update was preserved and reopening Properties is
    /// the only way to edit the current files.
    Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TagEditingControls {
    inputs_enabled: bool,
    save_enabled: bool,
    musicbrainz_enabled: bool,
}

impl TagEditingAvailability {
    fn controls(self, is_batch: bool) -> TagEditingControls {
        let ready = self == Self::Ready;
        TagEditingControls {
            inputs_enabled: ready,
            save_enabled: ready,
            musicbrainz_enabled: ready && !is_batch,
        }
    }

    fn message(self, automatic_device: bool) -> String {
        let key = match self {
            Self::Checking => "properties.write_checking",
            Self::Saving => "properties.write_saving",
            Self::Ready => "properties.write_ready",
            Self::UnsupportedFormat => "properties.write_unsupported",
            Self::InvalidFile => "properties.write_invalid_file",
            Self::Unavailable if automatic_device => "properties.write_device_unavailable",
            Self::Unavailable => "properties.write_unavailable",
            Self::Conflict => "properties.write_conflict",
        };
        rust_i18n::t!(key).into_owned()
    }
}

#[derive(Debug)]
enum SaveOutcome {
    /// The whole-selection pre-write probe refused before any byte was
    /// written. A positive conflict count is a localized changed-on-disk
    /// conflict that must be explained with reopen guidance rather than the
    /// generic read-only/unavailable message.
    Blocked {
        availability: TagEditingAvailability,
        conflicts: usize,
        total: usize,
    },
    Finished {
        modified: usize,
        failed: usize,
        /// How many of the failures were localized conflicts: the file was
        /// replaced or edited on disk since the dialog selected it. These are
        /// surfaced distinctly because reopening Properties is the fix.
        conflicts: usize,
        /// The exact targets whose writes committed, so the dialog can drop
        /// them before a retry. A retried write would otherwise re-prove the
        /// already-replaced file and report a spurious conflict.
        written_keys: Vec<SaveTargetKey>,
        current_availability: TagEditingAvailability,
    },
}

/// The verdict of the whole-selection pre-write probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionPreflight {
    /// Every target is currently writable; the save may proceed.
    Ready,
    /// At least one selected target changed on disk since selection. Nothing
    /// may be written, and reopening Properties is the fix.
    Changed { conflicts: usize, total: usize },
    /// A capability failure unrelated to a changed selection.
    Unavailable(TagEditingAvailability),
}

impl SelectionPreflight {
    /// The availability the dialog applies to the controls and label.
    fn availability(self) -> TagEditingAvailability {
        match self {
            Self::Ready => TagEditingAvailability::Ready,
            Self::Changed { .. } => TagEditingAvailability::Conflict,
            Self::Unavailable(availability) => availability,
        }
    }
}

fn merge_preflight_failure(
    current: TagEditingAvailability,
    failure: TagWritePreflightError,
) -> TagEditingAvailability {
    let candidate = match failure {
        TagWritePreflightError::UnsupportedFormat => TagEditingAvailability::UnsupportedFormat,
        TagWritePreflightError::NotRegularFile => TagEditingAvailability::InvalidFile,
        TagWritePreflightError::Unavailable => TagEditingAvailability::Unavailable,
    };

    // Report a deterministic, actionable reason independent of selection
    // order. An unsupported format is most specific; a missing/non-file path
    // is more specific than a generic access failure. A conflict is tracked
    // separately from this capability ladder and never reaches here.
    let priority = |availability| match availability {
        TagEditingAvailability::UnsupportedFormat => 3,
        TagEditingAvailability::InvalidFile => 2,
        TagEditingAvailability::Unavailable => 1,
        TagEditingAvailability::Checking
        | TagEditingAvailability::Saving
        | TagEditingAvailability::Ready
        | TagEditingAvailability::Conflict => 0,
    };
    if priority(candidate) > priority(current) {
        candidate
    } else {
        current
    }
}

/// Probe a complete, exact-deduplicated selection on a worker thread.
///
/// Every target is authority-based now: a local file revalidates its retained
/// directory, object identity, and content revision and rehearses the complete
/// atomic replacement beside the exact file; a removable target revalidates
/// its retained authority the same way. No path-scoped rehearsal runs, because
/// no target is a bare pathname that could resolve to a different file.
///
/// A changed local selection is reported as [`SelectionPreflight::Changed`],
/// not folded into the capability ladder, so the dialog can show localized
/// changed-on-disk guidance before any write. If a changed selection and a
/// capability failure are both present, the conflict wins: it is the
/// condition that requires reopening Properties, and it is the one #248
/// requires to be surfaced.
fn preflight_save_targets(targets: &[SaveTarget]) -> SelectionPreflight {
    if targets.is_empty() {
        return SelectionPreflight::Unavailable(TagEditingAvailability::InvalidFile);
    }
    if targets.iter().any(|target| {
        matches!(
            target,
            SaveTarget::PendingLocal(_) | SaveTarget::PendingRemovable(_)
        )
    }) {
        // Unresolved pending identities are a wiring fault: the context menu
        // must admit the local pathname to an exact object and resolve every
        // removable identity through its live session first.
        return SelectionPreflight::Unavailable(TagEditingAvailability::Unavailable);
    }

    let mut availability = TagEditingAvailability::Ready;
    let mut conflicts = 0usize;
    for target in targets {
        match target {
            SaveTarget::Local(local) => match local.preflight_write_capability() {
                Ok(()) => {}
                Err(LocalTagPreflightError::Unavailable(failure)) => {
                    availability = merge_preflight_failure(availability, failure);
                }
                Err(LocalTagPreflightError::Conflict(_)) => conflicts += 1,
            },
            SaveTarget::Removable(authority) => {
                if let Err(failure) = authority.preflight_write_capability() {
                    availability = merge_preflight_failure(availability, failure);
                }
            }
            SaveTarget::PendingLocal(_) | SaveTarget::PendingRemovable(_) => {
                availability = TagEditingAvailability::Unavailable;
            }
        }
    }

    if conflicts > 0 {
        SelectionPreflight::Changed {
            conflicts,
            total: targets.len(),
        }
    } else if availability == TagEditingAvailability::Ready {
        SelectionPreflight::Ready
    } else {
        SelectionPreflight::Unavailable(availability)
    }
}

/// The targets still needing a write after the listed keys committed.
///
/// A successful local write intentionally replaces its selected object, so
/// the original entry no longer proves the selection evidence. Keeping it
/// would make the post-save probe report a conflict for a file that was just
/// saved and disable retry for the files that still need it.
fn remaining_save_targets(
    targets: &[SaveTarget],
    written_keys: &[SaveTargetKey],
) -> Vec<SaveTarget> {
    targets
        .iter()
        .filter(|target| match save_target_key(target) {
            Some(key) => !written_keys.contains(&key),
            None => true,
        })
        .cloned()
        .collect()
}

/// Drop every target whose write committed from a stored target list, so a
/// retry never re-proves (and never rewrites) an already-saved file.
fn retain_pending_save_targets(targets: &mut Vec<SaveTarget>, written_keys: &[SaveTargetKey]) {
    targets.retain(|target| match save_target_key(target) {
        Some(key) => !written_keys.contains(&key),
        None => true,
    });
}

/// Derive the availability shown after a partial save over only the targets
/// that still need a write, never over the already-replaced ones.
fn post_save_availability(
    targets: &[SaveTarget],
    written_keys: &[SaveTargetKey],
    failed: usize,
) -> TagEditingAvailability {
    if failed == 0 {
        return TagEditingAvailability::Ready;
    }
    preflight_save_targets(&remaining_save_targets(targets, written_keys)).availability()
}

fn apply_tag_editing_availability(
    entries: &[(String, gtk::Entry)],
    save_button: &gtk::Button,
    musicbrainz_button: Option<&gtk::Button>,
    capability_label: &gtk::Label,
    availability: TagEditingAvailability,
    is_batch: bool,
    automatic_device: bool,
) {
    let controls = availability.controls(is_batch);
    for (_, entry) in entries {
        entry.set_sensitive(controls.inputs_enabled);
    }
    save_button.set_sensitive(controls.save_enabled);
    if let Some(button) = musicbrainz_button {
        button.set_label(rust_i18n::t!("properties.musicbrainz_lookup").as_ref());
        button.set_sensitive(controls.musicbrainz_enabled);
    }

    let message = availability.message(automatic_device);
    capability_label.set_label(&message);
    let blocked = matches!(
        availability,
        TagEditingAvailability::UnsupportedFormat
            | TagEditingAvailability::InvalidFile
            | TagEditingAvailability::Unavailable
            | TagEditingAvailability::Conflict
    );
    if blocked {
        capability_label.add_css_class("error");
        save_button.set_tooltip_text(Some(&message));
    } else {
        capability_label.remove_css_class("error");
        save_button.set_tooltip_text(None);
    }
}

/// Localized copy for one save that did not fully apply.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SaveFailureCopy {
    heading: String,
    body: String,
}

/// Build the localized heading/body for a save that did not fully apply.
///
/// A localized conflict (the selected file changed on disk and was preserved)
/// gets changed-on-disk guidance; a plain I/O failure keeps the generic
/// explanation. Counts are interpolated placeholders so every catalog
/// translates the whole sentence instead of inheriting an English `format!`.
fn save_failure_copy(
    locale: &str,
    modified: usize,
    failed: usize,
    conflicts: usize,
) -> SaveFailureCopy {
    let total = modified + failed;
    if conflicts > 0 && conflicts == failed {
        SaveFailureCopy {
            heading: rust_i18n::t!("properties.save_conflict_heading", locale = locale)
                .into_owned(),
            body: rust_i18n::t!(
                "properties.save_conflict_all",
                locale = locale,
                conflicts = conflicts,
                total = total
            )
            .into_owned(),
        }
    } else if conflicts > 0 {
        SaveFailureCopy {
            heading: rust_i18n::t!("properties.save_conflict_heading", locale = locale)
                .into_owned(),
            body: rust_i18n::t!(
                "properties.save_conflict_mixed",
                locale = locale,
                conflicts = conflicts,
                other = failed - conflicts,
                total = total
            )
            .into_owned(),
        }
    } else {
        SaveFailureCopy {
            heading: rust_i18n::t!("properties.save_failure_heading", locale = locale).into_owned(),
            body: rust_i18n::t!(
                "properties.save_failure",
                locale = locale,
                failed = failed,
                total = total
            )
            .into_owned(),
        }
    }
}

/// Present the localized explanation for a save that did not fully apply.
fn show_save_failure_alert(
    parent: &adw::ApplicationWindow,
    modified: usize,
    failed: usize,
    conflicts: usize,
) {
    let copy = save_failure_copy(&rust_i18n::locale(), modified, failed, conflicts);
    let alert = adw::AlertDialog::builder()
        .heading(&copy.heading)
        .body(&copy.body)
        .build();
    alert.add_response("ok", "OK");
    alert.present(Some(parent));
}

/// Show the properties dialog for one or more tracks.
///
/// On **Save**, changed tags are written to the files on a background
/// thread. There is no explicit rescan callback: the library DB and the
/// open tracklist are refreshed asynchronously by the filesystem watcher
/// for files inside a watched library folder. If any file fails to write,
/// the user is notified and the dialog stays open so they can retry.
pub fn show_properties_dialog(
    parent: &adw::ApplicationWindow,
    tracks: &[TrackInfo],
    automatic_device: bool,
    catalogue_refresh: Option<SourceRegistry>,
) {
    if tracks.is_empty() {
        return;
    }

    // Fail closed on the original selection, before any deduplication: an
    // unresolved pending identity (an unadmitted local pathname or an
    // unresolved removable identity) is a caller wiring fault, and dedup
    // cannot see such a row — the fault would silently shrink the write
    // set. This gate is release-enforced (not a debug assertion): the
    // dialog never opens and the refusal is surfaced, while
    // `preflight_save_targets`' pending refusal stays as the layered
    // defense behind it.
    if selection_has_unresolved_target(tracks) {
        tracing::warn!(
            selection = tracks.len(),
            "properties dialog refused: selection still carries an unresolved pending identity (caller wiring fault)"
        );
        let refusal = adw::AlertDialog::builder()
            .heading("Cannot Edit These Files")
            .body(TagEditingAvailability::Unavailable.message(automatic_device))
            .build();
        refusal.add_response("ok", "OK");
        refusal.present(Some(parent));
        return;
    }

    let is_batch = tracks.len() > 1;
    let heading = if is_batch {
        format!("Properties — {} tracks", tracks.len())
    } else {
        format!("Properties — {}", tracks[0].title)
    };

    let dialog = adw::Dialog::builder()
        .title(&heading)
        .content_width(480)
        .content_height(if is_batch { 400 } else { 520 })
        .build();
    let operation_generation = Rc::new(Cell::new(0u64));
    let generation_for_close = operation_generation.clone();
    dialog.connect_closed(move |_| {
        generation_for_close.set(generation_for_close.get().wrapping_add(1));
    });

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .build();

    // ── Header bar inside the dialog ─────────────────────────────────
    let header = adw::HeaderBar::builder()
        .show_end_title_buttons(true)
        .show_start_title_buttons(true)
        .build();
    content.append(&header);

    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .build();

    let form = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_start(16)
        .margin_end(16)
        .margin_top(12)
        .margin_bottom(12)
        .build();

    // Repeated playlist rows may refer to the same file or the same
    // removable identity. Probe and write each exact save target once while
    // retaining every selected row for batch-field presentation. Unresolved
    // pending identities were refused above, on the original selection,
    // before this deduplication could silently drop one.
    let save_targets = unique_save_targets(tracks);

    // ── Helper to compute initial value for a field ──────────────────
    let field_value = |getter: fn(&TrackInfo) -> &str| -> String {
        if !is_batch {
            return getter(&tracks[0]).to_string();
        }
        let first = getter(&tracks[0]);
        if tracks.iter().all(|t| getter(t) == first) {
            first.to_string()
        } else {
            String::new() // mixed — will show placeholder
        }
    };

    let mixed_placeholder = |getter: fn(&TrackInfo) -> &str| -> bool {
        if !is_batch {
            return false;
        }
        let first = getter(&tracks[0]);
        !tracks.iter().all(|t| getter(t) == first)
    };

    // ── Editable fields ──────────────────────────────────────────────
    // Single-track: title, artist, album, genre, year, track#, disc#, comment
    // Batch: artist, album, genre, year, disc#, comment (no title, no track#)

    let mut entries: Vec<(&str, gtk::Entry)> = Vec::new();

    if !is_batch {
        let title_entry = make_entry("Title", &field_value(|t| &t.title), false);
        form.append(&title_entry.0);
        entries.push(("title", title_entry.1));
    }

    let artist_entry = make_entry(
        "Artist",
        &field_value(|t| &t.artist),
        mixed_placeholder(|t| &t.artist),
    );
    form.append(&artist_entry.0);
    entries.push(("artist", artist_entry.1));

    let album_entry = make_entry(
        "Album",
        &field_value(|t| &t.album),
        mixed_placeholder(|t| &t.album),
    );
    form.append(&album_entry.0);
    entries.push(("album", album_entry.1));

    let genre_entry = make_entry(
        "Genre",
        &field_value(|t| &t.genre),
        mixed_placeholder(|t| &t.genre),
    );
    form.append(&genre_entry.0);
    entries.push(("genre", genre_entry.1));

    let composer_entry = make_entry(
        "Composer",
        &field_value(|t| &t.composer),
        mixed_placeholder(|t| &t.composer),
    );
    form.append(&composer_entry.0);
    entries.push(("composer", composer_entry.1));

    let year_entry = make_entry(
        "Year",
        &field_value(|t| &t.year),
        mixed_placeholder(|t| &t.year),
    );
    year_entry.1.set_input_purpose(gtk::InputPurpose::Digits);
    form.append(&year_entry.0);
    entries.push(("year", year_entry.1));

    if !is_batch {
        let track_entry = make_entry("Track #", &field_value(|t| &t.track_number), false);
        track_entry.1.set_input_purpose(gtk::InputPurpose::Digits);
        form.append(&track_entry.0);
        entries.push(("track_number", track_entry.1));
    }

    let disc_entry = make_entry(
        "Disc #",
        &field_value(|t| &t.disc_number),
        mixed_placeholder(|t| &t.disc_number),
    );
    disc_entry.1.set_input_purpose(gtk::InputPurpose::Digits);
    form.append(&disc_entry.0);
    entries.push(("disc_number", disc_entry.1));

    // ── Read-only info section (single track only) ───────────────────
    if !is_batch {
        let sep = gtk::Separator::new(gtk::Orientation::Horizontal);
        sep.set_margin_top(8);
        sep.set_margin_bottom(8);
        form.append(&sep);

        let info_group = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .build();

        let t = &tracks[0];
        add_info_row(&info_group, "Format", &t.format);
        add_info_row(&info_group, "Bitrate", &t.bitrate);
        add_info_row(&info_group, "Sample Rate", &t.sample_rate);
        add_info_row(&info_group, "Duration", &t.duration);

        // Show the native file path only for a local-library target. A
        // removable row shows its mount-relative identity — the one pathname
        // that may cross the source boundary; its native mount location
        // never enters the UI.
        match save_targets.first() {
            Some(SaveTarget::Local(local)) => {
                add_info_row(&info_group, "File", &local.path().to_string_lossy());
            }
            Some(SaveTarget::Removable(authority)) => {
                add_info_row(
                    &info_group,
                    "File",
                    &authority.relative_path().to_string_lossy(),
                );
            }
            _ => {}
        }

        form.append(&info_group);
    }

    for (_, entry) in &entries {
        entry.set_sensitive(false);
    }

    let capability_label = gtk::Label::builder()
        .label(TagEditingAvailability::Checking.message(automatic_device))
        .halign(gtk::Align::Start)
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .margin_start(16)
        .margin_end(16)
        .margin_top(8)
        .build();

    scrolled.set_child(Some(&form));
    content.append(&scrolled);
    content.append(&capability_label);

    // ── Button bar ───────────────────────────────────────────────────
    let button_bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .margin_start(16)
        .margin_end(16)
        .margin_top(8)
        .margin_bottom(12)
        .build();

    // MusicBrainz button (single track only). It remains disabled until the
    // same capability check that gates editing has completed successfully.
    let musicbrainz_button = if !is_batch {
        let mb_button = gtk::Button::builder()
            .label(rust_i18n::t!("properties.musicbrainz_lookup").as_ref())
            .css_classes(["flat"])
            .halign(gtk::Align::Start)
            .hexpand(true)
            .sensitive(false)
            .build();

        let title_for_mb = tracks[0].title.clone();
        let artist_for_mb = tracks[0].artist.clone();
        let entries_for_mb: Vec<(String, gtk::Entry)> = entries
            .iter()
            .map(|(name, entry)| ((*name).to_string(), entry.clone()))
            .collect();
        let generation_for_mb = operation_generation.clone();

        mb_button.connect_clicked(move |btn| {
            btn.set_sensitive(false);
            btn.set_label(rust_i18n::t!("properties.searching").as_ref());

            let title = title_for_mb.clone();
            let artist = artist_for_mb.clone();
            let entries = entries_for_mb.clone();
            let btn = btn.clone();
            let operation_generation = generation_for_mb.clone();
            // Every lookup owns a distinct generation. In particular, a
            // delayed "Not Found" label reset from the previous lookup must
            // not overwrite a newer lookup's "Searching…" state.
            let lookup_generation = operation_generation.get().wrapping_add(1);
            operation_generation.set(lookup_generation);

            let (tx, rx) = async_channel::bounded::<Option<MusicBrainzResult>>(1);

            std::thread::spawn(move || {
                let result = musicbrainz_lookup(&title, &artist);
                let _ = tx.send_blocking(result);
            });

            glib::MainContext::default().spawn_local(async move {
                let result = rx.recv().await;
                if operation_generation.get() != lookup_generation {
                    return;
                }
                if let Ok(Some(result)) = result {
                    for (name, entry) in &entries {
                        match name.as_str() {
                            "title" if !result.title.is_empty() => {
                                entry.set_text(&result.title);
                            }
                            "artist" if !result.artist.is_empty() => {
                                entry.set_text(&result.artist);
                            }
                            "album" if !result.album.is_empty() => {
                                entry.set_text(&result.album);
                            }
                            "year" if !result.year.is_empty() => {
                                entry.set_text(&result.year);
                            }
                            "track_number" if !result.track_number.is_empty() => {
                                entry.set_text(&result.track_number);
                            }
                            _ => {}
                        }
                    }
                    btn.set_label(rust_i18n::t!("properties.musicbrainz_lookup").as_ref());
                    btn.set_sensitive(true);
                } else {
                    btn.set_label(rust_i18n::t!("properties.not_found").as_ref());
                    btn.set_sensitive(true);
                    // Reset label after 2 seconds.
                    let btn = btn.clone();
                    let operation_generation = operation_generation.clone();
                    glib::timeout_add_local_once(std::time::Duration::from_secs(2), move || {
                        if operation_generation.get() == lookup_generation {
                            btn.set_label(rust_i18n::t!("properties.musicbrainz_lookup").as_ref());
                        }
                    });
                }
            });
        });

        button_bar.append(&mb_button);
        Some(mb_button)
    } else {
        None
    };

    let cancel_button = gtk::Button::builder().label("Cancel").build();

    let save_button = gtk::Button::builder()
        .label("Save")
        .css_classes(["suggested-action"])
        .sensitive(false)
        .build();

    button_bar.append(&cancel_button);
    button_bar.append(&save_button);
    content.append(&button_bar);

    dialog.set_child(Some(&content));

    // ── Cancel ───────────────────────────────────────────────────────
    let dialog_for_cancel = dialog.clone();
    cancel_button.connect_clicked(move |_| {
        dialog_for_cancel.close();
    });

    // ── Save ─────────────────────────────────────────────────────────
    let dialog_for_save = dialog.clone();
    let parent_for_save = parent.clone();

    // Capture initial text values to detect what actually changed.
    let initial_texts: Vec<(String, String)> = entries
        .iter()
        .map(|(name, entry)| ((*name).to_string(), entry.text().to_string()))
        .collect();

    // We need to capture entries for the save handler.
    let entries_for_save: Vec<(String, gtk::Entry)> = entries
        .iter()
        .map(|(name, entry)| ((*name).to_string(), entry.clone()))
        .collect();

    // The target list is shared so a partial save can drop the files that
    // already committed before the next attempt; re-proving an
    // already-replaced file would report a spurious conflict.
    let save_targets_for_save: Rc<RefCell<Vec<SaveTarget>>> =
        Rc::new(RefCell::new(save_targets.clone()));
    let save_targets_for_save_state = save_targets_for_save.clone();
    let entries_for_save_state = entries_for_save.clone();
    let musicbrainz_for_save = musicbrainz_button.clone();
    let capability_for_save = capability_label.clone();
    let cancel_for_save = cancel_button.clone();
    let generation_for_save = operation_generation.clone();
    let catalogue_refresh_for_save = catalogue_refresh.clone();

    save_button.connect_clicked(move |button| {
        // Build TagEdits from the form, only including changed fields.
        let mut edits = TagEdits::default();
        let mut any_changed = false;

        for (name, entry) in &entries_for_save_state {
            let current = entry.text().to_string();
            let original = initial_texts
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.as_str())
                .unwrap_or("");

            // In batch mode with mixed values, the initial text is empty.
            // Only apply if the user typed something (non-empty and different
            // from the original).
            if current == original {
                continue;
            }

            any_changed = true;
            let value = Some(current);
            match name.as_str() {
                "title" => edits.title = value,
                "artist" => edits.artist = value,
                "album" => edits.album = value,
                "genre" => edits.genre = value,
                "composer" => edits.composer = value,
                "year" => edits.year = value,
                "track_number" => edits.track_number = value,
                "disc_number" => edits.disc_number = value,
                _ => {}
            }
        }

        if !any_changed {
            dialog_for_save.close();
            return;
        }

        // Reject a malformed number here, while the user can still fix it and
        // before a single file is opened. Letting it through would rewrite
        // every selected file, discard the bad field, and report success.
        if let Err(error) = edits.validate() {
            let alert = adw::AlertDialog::builder()
                .heading("Check the Highlighted Field")
                .body(error.to_string())
                .build();
            alert.add_response("ok", "OK");
            alert.present(Some(&parent_for_save));
            return;
        }

        let save_generation = generation_for_save.get().wrapping_add(1);
        generation_for_save.set(save_generation);
        dialog_for_save.set_can_close(false);
        cancel_for_save.set_sensitive(false);

        apply_tag_editing_availability(
            &entries_for_save_state,
            button,
            musicbrainz_for_save.as_ref(),
            &capability_for_save,
            TagEditingAvailability::Saving,
            is_batch,
            automatic_device,
        );

        let targets = save_targets_for_save_state.borrow().clone();
        let catalogue_refresh_for_save = catalogue_refresh_for_save.clone();

        // Re-probe the entire selection before the first write, then track
        // both the files that were written and the ones that failed.
        let (tx, rx) = async_channel::bounded::<SaveOutcome>(1);
        let edits = edits.clone();

        std::thread::spawn(move || {
            match preflight_save_targets(&targets) {
                SelectionPreflight::Ready => {}
                SelectionPreflight::Changed { conflicts, total } => {
                    // A changed selection must be explained as a localized
                    // conflict before any write runs; nothing is written.
                    let _ = tx.send_blocking(SaveOutcome::Blocked {
                        availability: TagEditingAvailability::Conflict,
                        conflicts,
                        total,
                    });
                    return;
                }
                SelectionPreflight::Unavailable(availability) => {
                    let _ = tx.send_blocking(SaveOutcome::Blocked {
                        availability,
                        conflicts: 0,
                        total: 0,
                    });
                    return;
                }
            }

            let mut modified = 0usize;
            let mut failed = 0usize;
            let mut conflicts = 0usize;
            let mut written: Vec<(SourceId, TrackId)> = Vec::new();
            let mut written_keys: Vec<SaveTargetKey> = Vec::new();
            for target in &targets {
                // An unresolved pending identity cannot reach the write loop:
                // the preflight above refuses the whole selection first.
                let outcome = match target {
                    SaveTarget::Local(local) => local.write_tags(&edits),
                    SaveTarget::Removable(authority) => authority.write_tags(&edits),
                    SaveTarget::PendingLocal(_) | SaveTarget::PendingRemovable(_) => {
                        failed += 1;
                        continue;
                    }
                };
                match outcome {
                    Ok(()) => {
                        modified += 1;
                        if let Some(key) = save_target_key(target) {
                            written_keys.push(key);
                        }
                        if let SaveTarget::Removable(authority) = target {
                            written.push((authority.source_id(), authority.track_id().clone()));
                        }
                    }
                    Err(e) => {
                        // A localized conflict is the file the dialog selected no
                        // longer being the file at that pathname. It is reported
                        // separately: retrying the same dialog state cannot help,
                        // but the competing file/update was preserved.
                        if e.downcast_ref::<LocalTagWriteConflict>().is_some() {
                            conflicts += 1;
                        }
                        // Removable failures must never log their native mount
                        // location; identity is the source-scoped pair.
                        match target {
                            SaveTarget::Local(local) => {
                                warn!(path = %local.path().display(), error = %e, "Failed to write tags");
                            }
                            SaveTarget::Removable(authority) => {
                                warn!(
                                    source = %authority.source_id(),
                                    track = authority.track_id().as_str(),
                                    error = %e,
                                    "Failed to write tags to removable target"
                                );
                            }
                            SaveTarget::PendingLocal(_) | SaveTarget::PendingRemovable(_) => {}
                        }
                        failed += 1;
                    }
                }
            }

            // Publish refreshed metadata for exactly the identities whose
            // writes committed, before any completion branch can forget
            // them. The trigger is fire-and-forget: publication flows through
            // the registry's catalogue refresh lane, whose settlement
            // revalidates the exact live session, so a completion that lands
            // after a disconnect or replacement can never repopulate a stale
            // mount or overwrite a newer generation.
            if let Some(registry) = &catalogue_refresh_for_save {
                if !written.is_empty() {
                    registry.refresh_catalogue_after_mutation(&written);
                }
            }

            let current_availability = post_save_availability(&targets, &written_keys, failed);
            let _ = tx.send_blocking(SaveOutcome::Finished {
                modified,
                failed,
                conflicts,
                written_keys,
                current_availability,
            });
        });

        let dialog = dialog_for_save.clone();
        let parent = parent_for_save.clone();
        let entries = entries_for_save_state.clone();
        let save_button = button.clone();
        let musicbrainz_button = musicbrainz_for_save.clone();
        let capability_label = capability_for_save.clone();
        let cancel_button = cancel_for_save.clone();
        let operation_generation = generation_for_save.clone();
        let targets_for_completion = save_targets_for_save_state.clone();
        glib::MainContext::default().spawn_local(async move {
            let outcome = rx.recv().await;
            if operation_generation.get() != save_generation {
                return;
            }

            match outcome {
                Ok(SaveOutcome::Blocked {
                    availability,
                    conflicts,
                    total: _blocked_total,
                }) => {
                    dialog.set_can_close(true);
                    cancel_button.set_sensitive(true);
                    apply_tag_editing_availability(
                        &entries,
                        &save_button,
                        musicbrainz_button.as_ref(),
                        &capability_label,
                        availability,
                        is_batch,
                        automatic_device,
                    );
                    // A changed selection is refused before the first write;
                    // explain it with the same localized changed-on-disk
                    // guidance the post-write path uses, not the generic
                    // unavailable message the availability ladder produces.
                    // Only the known-bad targets are reported: nothing was
                    // attempted, so the clean remainder of the selection must
                    // not be counted as failed writes.
                    if conflicts > 0 {
                        show_save_failure_alert(&parent, 0, conflicts, conflicts);
                    }
                }
                Ok(SaveOutcome::Finished {
                    modified,
                    failed,
                    conflicts,
                    written_keys,
                    current_availability,
                }) => {
                    if modified > 0 {
                        info!(count = modified, "Tags saved successfully");
                    }

                    // Drop every target whose write committed before any retry,
                    // so a retry re-probes only the files that still need it.
                    // Re-proving an already-replaced file would report a
                    // spurious conflict instead of the real remaining failure.
                    if !written_keys.is_empty() {
                        let mut stored = targets_for_completion.borrow_mut();
                        retain_pending_save_targets(&mut stored, &written_keys);
                    }

                    if failed == 0 {
                        dialog.set_can_close(true);
                        cancel_button.set_sensitive(true);
                        dialog.close();
                        return;
                    }

                    dialog.set_can_close(true);
                    cancel_button.set_sensitive(true);

                    apply_tag_editing_availability(
                        &entries,
                        &save_button,
                        musicbrainz_button.as_ref(),
                        &capability_label,
                        current_availability,
                        is_batch,
                        automatic_device,
                    );

                    // Surface the failure instead of closing silently, so the
                    // user knows the edit didn't fully apply. Keep the dialog
                    // open so they can retry. A localized conflict means the
                    // file changed on disk since it was selected and was
                    // preserved untouched — retrying the same dialog state
                    // cannot succeed, so say so instead of inviting a retry.
                    show_save_failure_alert(&parent, modified, failed, conflicts);
                }
                Err(_) => {
                    dialog.set_can_close(true);
                    cancel_button.set_sensitive(true);
                    apply_tag_editing_availability(
                        &entries,
                        &save_button,
                        musicbrainz_button.as_ref(),
                        &capability_label,
                        TagEditingAvailability::Unavailable,
                        is_batch,
                        automatic_device,
                    );
                }
            }
        });
    });

    // Filesystem probing can block on removable and network media, so the
    // dialog starts fail-closed and the complete selection is checked on a
    // worker. The result is advisory and is rechecked in the Save worker.
    let (preflight_tx, preflight_rx) = async_channel::bounded(1);
    let targets_for_preflight = save_targets;
    std::thread::spawn(move || {
        let _ = preflight_tx
            .send_blocking(preflight_save_targets(&targets_for_preflight).availability());
    });

    let entries_for_preflight = entries_for_save;
    let save_for_preflight = save_button;
    let musicbrainz_for_preflight = musicbrainz_button;
    let capability_for_preflight = capability_label;
    let preflight_generation = operation_generation.get();
    let generation_for_preflight = operation_generation;
    glib::MainContext::default().spawn_local(async move {
        let availability = preflight_rx
            .recv()
            .await
            .unwrap_or(TagEditingAvailability::Unavailable);
        if generation_for_preflight.get() != preflight_generation {
            return;
        }
        apply_tag_editing_availability(
            &entries_for_preflight,
            &save_for_preflight,
            musicbrainz_for_preflight.as_ref(),
            &capability_for_preflight,
            availability,
            is_batch,
            automatic_device,
        );
    });

    dialog.present(Some(parent));
}

// ── Form helpers ────────────────────────────────────────────────────────

/// Create a labeled entry row. Returns (row_box, entry).
fn make_entry(label: &str, value: &str, mixed: bool) -> (gtk::Box, gtk::Entry) {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();

    let lbl = gtk::Label::builder()
        .label(label)
        .width_chars(10)
        .halign(gtk::Align::End)
        .css_classes(["dim-label"])
        .build();

    let entry = gtk::Entry::builder().text(value).hexpand(true).build();

    if mixed {
        entry.set_placeholder_text(Some("Mixed"));
    }

    row.append(&lbl);
    row.append(&entry);

    (row, entry)
}

/// Add a read-only info row to a container.
fn add_info_row(container: &gtk::Box, label: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();

    let lbl = gtk::Label::builder()
        .label(label)
        .width_chars(10)
        .halign(gtk::Align::End)
        .css_classes(["dim-label"])
        .build();

    let val = gtk::Label::builder()
        .label(value)
        .halign(gtk::Align::Start)
        .selectable(true)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .hexpand(true)
        .build();

    row.append(&lbl);
    row.append(&val);
    container.append(&row);
}

// ── MusicBrainz lookup ──────────────────────────────────────────────────

const MUSICBRAINZ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const MAX_MUSICBRAINZ_BODY_BYTES: u64 = 4 * 1024 * 1024;

/// Result from a MusicBrainz recording search.
#[derive(Debug, Clone, Default)]
struct MusicBrainzResult {
    title: String,
    artist: String,
    album: String,
    year: String,
    track_number: String,
}

/// Query the MusicBrainz API for a recording matching title + artist.
///
/// This is a blocking HTTP call — run on a background thread.
/// Respects MusicBrainz rate limiting via User-Agent header.
fn musicbrainz_lookup(title: &str, artist: &str) -> Option<MusicBrainzResult> {
    let query = if artist.is_empty() || artist == "Unknown Artist" {
        format!("recording:\"{}\"", title)
    } else {
        format!("recording:\"{}\" AND artist:\"{}\"", title, artist)
    };

    let url = format!(
        "https://musicbrainz.org/ws/2/recording?query={}&fmt=json&limit=1",
        urlencoding::encode(&query)
    );

    let client = crate::http_security::public_blocking_client_builder()
        .timeout(MUSICBRAINZ_TIMEOUT)
        .user_agent("Tributary/0.3.0 (https://github.com/jm2/tributary)")
        .build()
        .ok()?;

    let resp = client.get(&url).timeout(MUSICBRAINZ_TIMEOUT).send().ok()?;
    if !resp.status().is_success() {
        warn!(status = %resp.status(), "MusicBrainz API error");
        return None;
    }

    let body = crate::http_body::read_limited_blocking(
        resp,
        MAX_MUSICBRAINZ_BODY_BYTES,
        MUSICBRAINZ_TIMEOUT,
    )
    .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&body).ok()?;
    let recordings = json.get("recordings")?.as_array()?;
    let recording = recordings.first()?;

    let title = recording
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let artist = recording
        .get("artist-credit")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|ac| ac.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Get album from first release
    let releases = recording.get("releases").and_then(|v| v.as_array());
    let (album, year, track_number) = if let Some(releases) = releases {
        if let Some(release) = releases.first() {
            let album = release
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let year = release
                .get("date")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .chars()
                .take(4) // Extract just the year from "YYYY-MM-DD"
                .collect::<String>();
            let track_number = release
                .get("media")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|m| m.get("track"))
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|t| t.get("number"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            (album, year, track_number)
        } else {
            (String::new(), String::new(), String::new())
        }
    } else {
        (String::new(), String::new(), String::new())
    };

    tracing::debug!("MusicBrainz lookup returned a result");

    Some(MusicBrainzResult {
        title,
        artist,
        album,
        year,
        track_number,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(target: SaveTarget) -> TrackInfo {
        TrackInfo {
            target,
            title: "Title".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            genre: String::new(),
            composer: String::new(),
            year: String::new(),
            track_number: String::new(),
            disc_number: String::new(),
            format: "FLAC".to_string(),
            bitrate: String::new(),
            sample_rate: String::new(),
            duration: String::new(),
        }
    }

    fn local_track(path: PathBuf) -> TrackInfo {
        track(SaveTarget::Local(LocalMutationTarget::capture(&path)))
    }

    #[test]
    fn repeated_playlist_rows_probe_and_write_one_exact_target() {
        let first = PathBuf::from("/music/album/song.flac");
        let second = PathBuf::from("/music/other.flac");
        let tracks = vec![
            local_track(first.clone()),
            local_track(first.clone()),
            local_track(second.clone()),
        ];

        assert_eq!(
            unique_save_targets(&tracks),
            vec![
                SaveTarget::Local(LocalMutationTarget::capture(&first)),
                SaveTarget::Local(LocalMutationTarget::capture(&second)),
            ]
        );
    }

    #[test]
    fn unresolved_pending_identities_never_reach_the_probe_or_write_set() {
        let pending = PendingRemovableMutation {
            source_id: crate::architecture::SourceId::local(),
            session_epoch: 3,
            track_id: crate::architecture::TrackId::new("unix:616c62756d").expect("track id"),
        };
        let local = PathBuf::from("/music/album/song.flac");
        let tracks = vec![
            track(SaveTarget::PendingRemovable(pending.clone())),
            track(SaveTarget::PendingRemovable(pending.clone())),
            local_track(local.clone()),
        ];

        // Fail closed on the ORIGINAL selection, before any deduplication:
        // an unresolved pending identity is a caller wiring fault, so the
        // dialog refuses entirely instead of opening with a partial write
        // set — in release builds too, not behind a debug assertion.
        assert!(selection_has_unresolved_target(&tracks));

        // The reason the gate must precede dedup: a pending value has no
        // deduplication identity, so dedup alone would silently drop the
        // row rather than refuse it.
        let keyed = save_target_key(&SaveTarget::PendingRemovable(pending));
        assert!(keyed.is_none());

        // A fully resolved selection passes the gate, and deduplication
        // keeps its exact semantics for identities that carry one.
        let resolved = vec![
            local_track(local.clone()),
            local_track(local.clone()),
            local_track(local),
        ];
        assert!(!selection_has_unresolved_target(&resolved));
        assert_eq!(
            unique_save_targets(&resolved),
            vec![SaveTarget::Local(LocalMutationTarget::capture(
                &PathBuf::from("/music/album/song.flac")
            ))]
        );

        // Defense in depth: the preflight still refuses any unresolved
        // identity that reaches it directly, so a wiring fault can never
        // authorize a write even past the entry-point gate.
        assert_eq!(
            preflight_save_targets(&[SaveTarget::PendingRemovable(PendingRemovableMutation {
                source_id: crate::architecture::SourceId::local(),
                session_epoch: 1,
                track_id: crate::architecture::TrackId::new("unix:01").expect("track id"),
            })])
            .availability(),
            TagEditingAvailability::Unavailable
        );
    }

    #[test]
    fn an_unadmitted_local_pathname_is_an_unresolved_pending_identity() {
        // A PendingLocal row is the local twin of a PendingRemovable row: a
        // validated pathname that has not yet been admitted to an exact
        // object. The same release-enforced gate and no-dedup-identity rules
        // apply, so a wiring fault can never open a dialog whose write set
        // silently lost the row.
        let path = PathBuf::from("/music/album/song.flac");
        let tracks = vec![
            track(SaveTarget::PendingLocal(PendingLocalMutation {
                path: path.clone(),
            })),
            local_track(path.clone()),
        ];
        assert!(selection_has_unresolved_target(&tracks));
        assert!(
            save_target_key(&SaveTarget::PendingLocal(PendingLocalMutation {
                path: path.clone()
            }))
            .is_none()
        );
        assert_eq!(
            preflight_save_targets(&[SaveTarget::PendingLocal(PendingLocalMutation { path })])
                .availability(),
            TagEditingAvailability::Unavailable
        );
    }

    #[test]
    fn a_mixed_selection_with_one_pending_identity_is_refused_in_release() {
        // The release shape of the wiring fault: one local row plus one
        // unresolved removable identity. Dedup alone would cover only the
        // local row and report success while losing the pending edit — so
        // the release-enforced entry-point gate fires on the original
        // selection first and the whole dialog refuses; no partial write
        // set is ever built.
        let pending = PendingRemovableMutation {
            source_id: crate::architecture::SourceId::local(),
            session_epoch: 7,
            track_id: crate::architecture::TrackId::new("unix:6d69786564").expect("track id"),
        };
        let local = PathBuf::from("/music/album/song.flac");
        let tracks = vec![
            local_track(local),
            track(SaveTarget::PendingRemovable(pending)),
        ];

        assert!(selection_has_unresolved_target(&tracks));
    }

    #[test]
    fn availability_controls_are_fail_closed_until_ready() {
        for availability in [
            TagEditingAvailability::Checking,
            TagEditingAvailability::Saving,
            TagEditingAvailability::UnsupportedFormat,
            TagEditingAvailability::InvalidFile,
            TagEditingAvailability::Unavailable,
            TagEditingAvailability::Conflict,
        ] {
            assert_eq!(
                availability.controls(false),
                TagEditingControls {
                    inputs_enabled: false,
                    save_enabled: false,
                    musicbrainz_enabled: false,
                }
            );
        }

        assert_eq!(
            TagEditingAvailability::Ready.controls(false),
            TagEditingControls {
                inputs_enabled: true,
                save_enabled: true,
                musicbrainz_enabled: true,
            }
        );
        assert!(
            !TagEditingAvailability::Ready
                .controls(true)
                .musicbrainz_enabled
        );
    }

    #[test]
    fn mixed_batch_failure_reason_is_deterministic() {
        let failures = [
            TagWritePreflightError::Unavailable,
            TagWritePreflightError::UnsupportedFormat,
            TagWritePreflightError::NotRegularFile,
        ];

        let forward = failures
            .iter()
            .copied()
            .fold(TagEditingAvailability::Ready, merge_preflight_failure);
        let reverse = failures
            .iter()
            .rev()
            .copied()
            .fold(TagEditingAvailability::Ready, merge_preflight_failure);

        assert_eq!(forward, TagEditingAvailability::UnsupportedFormat);
        assert_eq!(reverse, forward);
    }

    #[test]
    fn empty_and_mixed_preflight_selections_fail_closed() {
        assert_eq!(
            preflight_save_targets(&[]).availability(),
            TagEditingAvailability::InvalidFile
        );

        let directory = tempfile::tempdir().expect("create preflight fixture");
        let supported = directory.path().join("song.flac");
        let unsupported = directory.path().join("song.wav");
        std::fs::write(&supported, b"audio").expect("write supported fixture");
        std::fs::write(&unsupported, b"audio").expect("write unsupported fixture");

        assert_eq!(
            preflight_save_targets(&[
                SaveTarget::Local(LocalMutationTarget::capture(&supported)),
                SaveTarget::Local(LocalMutationTarget::capture(&unsupported)),
            ])
            .availability(),
            TagEditingAvailability::UnsupportedFormat
        );
        assert!(
            std::fs::read_dir(directory.path())
                .expect("read fixture")
                .all(|entry| !crate::local::tag_writer::is_tag_write_temp_file(
                    &entry.expect("directory entry").path()
                )),
            "a blocked batch must leave no private sibling"
        );
    }

    #[test]
    fn automatic_device_failure_has_specific_guidance() {
        let generic = TagEditingAvailability::Unavailable.message(false);
        let device = TagEditingAvailability::Unavailable.message(true);

        assert_ne!(generic, device);
        assert!(!generic.is_empty());
        assert!(!device.is_empty());
    }

    /// The `silence.flac` fixture the local-editor tests select.
    fn selection_fixture_bytes() -> &'static [u8] {
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/audio/silence.flac"
        ))
    }

    /// Write a fresh valid FLAC named `name` under `directory`.
    fn writable_selection(directory: &std::path::Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, selection_fixture_bytes()).expect("write selection fixture");
        path
    }

    /// No reserved tag-write sibling may survive a blocked preflight.
    fn assert_no_preflight_residue(directory: &std::path::Path) {
        assert!(
            std::fs::read_dir(directory)
                .expect("read fixture")
                .all(|entry| !crate::local::tag_writer::is_tag_write_temp_file(
                    &entry.expect("directory entry").path()
                )),
            "a blocked preflight must leave no private sibling"
        );
    }

    /// #248 F1: a file replaced before Save is reported as a localized
    /// changed-on-disk conflict by the pre-write probe — not folded into the
    /// generic unavailable result — and no byte is written.
    #[test]
    fn a_file_replaced_before_save_is_a_preflight_conflict_before_any_write() {
        let directory = tempfile::tempdir().expect("create selection fixture");
        let selected = writable_selection(directory.path(), "song.flac");
        let target = LocalMutationTarget::capture(&selected);

        let displaced = directory.path().join("displaced.flac");
        std::fs::rename(&selected, &displaced).expect("displace the selection");
        let stranger = b"a different file now occupies the selected pathname".to_vec();
        std::fs::write(&selected, &stranger).expect("install a replacement");

        let targets = vec![SaveTarget::Local(target)];
        let verdict = preflight_save_targets(&targets);
        assert_eq!(
            verdict,
            SelectionPreflight::Changed {
                conflicts: 1,
                total: 1
            }
        );
        assert_eq!(verdict.availability(), TagEditingAvailability::Conflict);

        // The probe must explain the conflict and leave both files exactly
        // as the competing writer left them.
        assert_eq!(
            std::fs::read(&selected).expect("read the replacement"),
            stranger,
            "the replacement must be preserved byte-for-byte"
        );
        assert_eq!(
            std::fs::read(&displaced).expect("read the displaced selection"),
            selection_fixture_bytes(),
            "the admitted selection must be byte-for-byte untouched"
        );
        assert_no_preflight_residue(directory.path());
    }

    /// #248 F1: an in-place edit before Save is the localized `TargetEdited`
    /// conflict, distinct from a plain I/O failure, and nothing is written.
    #[test]
    fn an_in_place_edit_before_save_is_a_preflight_conflict() {
        let directory = tempfile::tempdir().expect("create selection fixture");
        let selected = writable_selection(directory.path(), "song.flac");
        let target = LocalMutationTarget::capture(&selected);

        let competing = b"a competing in-place edit landed after selection".to_vec();
        std::fs::write(&selected, &competing).expect("edit the selection in place");

        let verdict = preflight_save_targets(&[SaveTarget::Local(target)]);
        assert_eq!(
            verdict,
            SelectionPreflight::Changed {
                conflicts: 1,
                total: 1
            }
        );
        assert_eq!(verdict.availability(), TagEditingAvailability::Conflict);
        assert_eq!(
            std::fs::read(&selected).expect("read the competing edit"),
            competing,
            "the competing edit must be preserved byte-for-byte"
        );
        assert_no_preflight_residue(directory.path());
    }

    /// #248 F2 scenario shared by the retry-availability tests: two writable
    /// local targets whose first has already committed through the real
    /// writer, exactly as the save worker leaves it, so the original list's
    /// retained evidence is deliberately stale.
    struct CommittedFirstOfTwo {
        targets: Vec<SaveTarget>,
        written_keys: Vec<SaveTargetKey>,
        second_target: SaveTarget,
        first_path: PathBuf,
        second_path: PathBuf,
        committed_first_bytes: Vec<u8>,
    }

    fn committed_first_of_two(directory: &std::path::Path) -> CommittedFirstOfTwo {
        let first = writable_selection(directory, "first.flac");
        let second = writable_selection(directory, "second.flac");

        let first_target = LocalMutationTarget::capture(&first);
        let second_target = LocalMutationTarget::capture(&second);
        let targets = vec![
            SaveTarget::Local(first_target.clone()),
            SaveTarget::Local(second_target.clone()),
        ];

        // The first target commits and its selected object is replaced, as
        // the real save worker leaves it.
        let first_edits = TagEdits {
            year: Some("2026".to_string()),
            ..Default::default()
        };
        first_target
            .write_tags(&first_edits)
            .expect("first target commits");
        let committed_first_bytes = std::fs::read(&first).expect("read the committed first target");

        CommittedFirstOfTwo {
            targets,
            written_keys: vec![SaveTargetKey::Local(first.clone())],
            second_target: SaveTarget::Local(second_target),
            first_path: first.clone(),
            second_path: second,
            committed_first_bytes,
        }
    }

    /// #248 F2: probing the ORIGINAL list after one target commits can no
    /// longer be Ready — the committed file's retained evidence is
    /// deliberately stale, which is exactly why the old code disabled retry —
    /// while the post-save availability recomputed over only the unwritten
    /// targets keeps Save enabled.
    #[test]
    fn post_save_availability_is_recomputed_over_unwritten_targets() {
        let directory = tempfile::tempdir().expect("create selection fixture");
        let scenario = committed_first_of_two(directory.path());

        assert_ne!(
            preflight_save_targets(&scenario.targets).availability(),
            TagEditingAvailability::Ready
        );
        assert_eq!(
            post_save_availability(&scenario.targets, &scenario.written_keys, 1),
            TagEditingAvailability::Ready
        );
        assert_no_preflight_residue(directory.path());
    }

    /// #248 F2: a retry derives its write set from the remaining targets —
    /// only the recoverable second — and never rewrites the committed first
    /// target.
    #[test]
    fn a_retry_rewrites_only_the_remaining_target() {
        let directory = tempfile::tempdir().expect("create selection fixture");
        let scenario = committed_first_of_two(directory.path());

        let remaining = remaining_save_targets(&scenario.targets, &scenario.written_keys);
        assert_eq!(
            remaining,
            vec![scenario.second_target.clone()],
            "only the recoverable target is retried"
        );

        let second_edits = TagEdits {
            year: Some("2027".to_string()),
            ..Default::default()
        };
        for target in &remaining {
            if let SaveTarget::Local(local) = target {
                local
                    .write_tags(&second_edits)
                    .expect("the retry writes the remaining target");
            }
        }

        assert_eq!(
            std::fs::read(&scenario.first_path).expect("read the first target after the retry"),
            scenario.committed_first_bytes,
            "the committed first target must not be rewritten by the retry"
        );
        assert_ne!(
            std::fs::read(&scenario.second_path).expect("read the retried second target"),
            selection_fixture_bytes(),
            "the retry must write the remaining target"
        );
        assert_no_preflight_residue(directory.path());
    }

    /// #248 F3: the English copy distinguishes the all-conflict,
    /// mixed-conflict, and I/O failure shapes, with every count placeholder
    /// interpolated.
    #[test]
    fn english_save_failure_copy_separates_all_mixed_and_io_conflicts() {
        let english_all = save_failure_copy("en", 0, 2, 2);
        let english_mixed = save_failure_copy("en", 0, 3, 1);
        let english_io = save_failure_copy("en", 0, 2, 0);

        assert!(!english_all.heading.is_empty());
        assert!(!english_all.body.is_empty());
        assert!(!english_mixed.heading.is_empty());
        assert!(!english_mixed.body.is_empty());
        assert_ne!(english_all.body, english_mixed.body);
        assert_ne!(english_all.heading, english_io.heading);
        assert!(!english_all.body.contains("%{"));
        assert!(!english_mixed.body.contains("%{"));
    }

    /// The preflight-refusal alert reports only the targets known to have
    /// changed on disk. The refusal fires before the first write, so the
    /// clean remainder of a mixed selection was never attempted and must not
    /// be counted as failed: the alert is built from
    /// `show_save_failure_alert(&parent, 0, conflicts, conflicts)` — never
    /// `(0, total, conflicts)`, which fabricated `total - conflicts` "other
    /// file(s) could not be saved" for files the save never touched.
    #[test]
    fn preflight_refusal_alert_counts_only_conflicted_targets() {
        // A three-target selection in which exactly one changed on disk. The
        // refusal alert's arguments are (modified = 0, failed = conflicts,
        // conflicts), so its copy is the all-conflict framing over the one
        // conflicted file.
        let refusal = save_failure_copy("en", 0, 1, 1);

        assert!(
            !refusal.body.contains("other file"),
            "an unattempted clean target must not be reported as a failed write: {}",
            refusal.body
        );
        assert_eq!(
            refusal.body,
            english_all_conflict_body(1),
            "the refusal must describe exactly the conflicted subset"
        );

        // The defective arguments, kept here as the regression pin: counting
        // the whole selection as attempted is what produced the fabricated
        // "2 other file(s) could not be saved" clause.
        let defective = save_failure_copy("en", 0, 3, 1);
        assert_ne!(refusal.body, defective.body);
        assert!(defective.body.contains("other file"));
    }

    /// The English all-conflict body for `conflicts` files — the framing the
    /// preflight refusal must produce.
    fn english_all_conflict_body(conflicts: usize) -> String {
        rust_i18n::t!(
            "properties.save_conflict_all",
            locale = "en",
            conflicts = conflicts,
            total = conflicts
        )
        .into_owned()
    }

    /// #248 F3: the conflict alert copy is localized in every shipped
    /// catalog, for both the all-conflict and the mixed-conflict result, and
    /// its count placeholders are interpolated.
    #[test]
    fn save_failure_copy_localizes_all_and_mixed_conflicts() {
        let english_all = save_failure_copy("en", 0, 2, 2);
        let english_mixed = save_failure_copy("en", 0, 3, 1);

        for locale in rust_i18n::available_locales!() {
            let all = save_failure_copy(&locale, 0, 2, 2);
            let mixed = save_failure_copy(&locale, 0, 3, 1);
            assert!(!all.heading.is_empty(), "{locale}: empty all heading");
            assert!(!all.body.is_empty(), "{locale}: empty all body");
            assert!(!mixed.heading.is_empty(), "{locale}: empty mixed heading");
            assert!(!mixed.body.is_empty(), "{locale}: empty mixed body");
            if locale != "en" {
                assert_ne!(
                    all.heading, english_all.heading,
                    "{locale}: all heading fell back to English"
                );
                assert_ne!(
                    all.body, english_all.body,
                    "{locale}: all body fell back to English"
                );
                assert_ne!(
                    mixed.body, english_mixed.body,
                    "{locale}: mixed body fell back to English"
                );
            }
            assert!(
                !all.body.contains("%{"),
                "{locale}: all-conflict placeholder not interpolated"
            );
            assert!(
                !mixed.body.contains("%{"),
                "{locale}: mixed-conflict placeholder not interpolated"
            );
        }
    }

    /// #248 F3: the capability label's changed-on-disk guidance is localized
    /// in every shipped catalog and differs from the generic unavailable
    /// explanation.
    #[test]
    fn conflict_guidance_is_localized_for_every_catalog() {
        let english = rust_i18n::t!("properties.write_conflict", locale = "en").into_owned();
        assert!(!english.is_empty());
        assert_ne!(english, TagEditingAvailability::Unavailable.message(false));

        for locale in rust_i18n::available_locales!() {
            let localized =
                rust_i18n::t!("properties.write_conflict", locale = locale).into_owned();
            assert!(!localized.is_empty(), "{locale}: empty conflict guidance");
            if locale != "en" {
                assert_ne!(
                    localized, english,
                    "{locale} must not fall back to English conflict guidance"
                );
            }
        }
    }
}
