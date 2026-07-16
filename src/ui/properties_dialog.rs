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

use std::cell::Cell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use tracing::{info, warn};

use crate::local::tag_writer::{
    preflight_tag_write, validate_tag_write_target, TagEdits, TagWritePreflightError,
};

/// Information about a track passed into the dialog.
#[derive(Debug, Clone)]
pub struct TrackInfo {
    /// Validated native path for this local track.
    pub path: PathBuf,
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
        };
        rust_i18n::t!(key).into_owned()
    }
}

#[derive(Debug)]
enum SaveOutcome {
    Blocked(TagEditingAvailability),
    Finished {
        modified: usize,
        failed: usize,
        current_availability: TagEditingAvailability,
    },
}

fn unique_track_paths(tracks: &[TrackInfo]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    tracks
        .iter()
        .filter(|track| seen.insert(track.path.clone()))
        .map(|track| track.path.clone())
        .collect()
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
    // is more specific than a generic access failure.
    let priority = |availability| match availability {
        TagEditingAvailability::UnsupportedFormat => 3,
        TagEditingAvailability::InvalidFile => 2,
        TagEditingAvailability::Unavailable => 1,
        TagEditingAvailability::Checking
        | TagEditingAvailability::Saving
        | TagEditingAvailability::Ready => 0,
    };
    if priority(candidate) > priority(current) {
        candidate
    } else {
        current
    }
}

fn preflight_distinct_parents(
    paths: &[PathBuf],
    mut probe: impl FnMut(&std::path::Path) -> Result<(), TagWritePreflightError>,
) -> TagEditingAvailability {
    let mut parents = HashSet::new();
    for path in paths {
        let Some(parent) = path.parent() else {
            return TagEditingAvailability::InvalidFile;
        };
        if !parents.insert(parent.to_path_buf()) {
            continue;
        }
        if let Err(failure) = probe(path) {
            // Once the selection is blocked, probing later directories can no
            // longer change the outcome and would only add blocking I/O and
            // writer-sibling activity.
            return merge_preflight_failure(TagEditingAvailability::Ready, failure);
        }
    }
    TagEditingAvailability::Ready
}

/// Probe a complete, exact-deduplicated selection on a worker thread.
fn preflight_paths(paths: &[PathBuf]) -> TagEditingAvailability {
    if paths.is_empty() {
        return TagEditingAvailability::InvalidFile;
    }

    let validation = paths
        .iter()
        .fold(
            TagEditingAvailability::Ready,
            |result, path| match validate_tag_write_target(path) {
                Ok(()) => result,
                Err(failure) => merge_preflight_failure(result, failure),
            },
        );
    if validation != TagEditingAvailability::Ready {
        return validation;
    }

    // Directory mechanics are parent-scoped. Rehearse the flushed atomic
    // replacement once per exact parent while retaining the per-file checks
    // above (including Windows read-only attributes).
    preflight_distinct_parents(paths, preflight_tag_write)
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
    );
    if blocked {
        capability_label.add_css_class("error");
        save_button.set_tooltip_text(Some(&message));
    } else {
        capability_label.remove_css_class("error");
        save_button.set_tooltip_text(None);
    }
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
) {
    if tracks.is_empty() {
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

    // Repeated playlist entries may refer to the same file. Probe and write
    // each exact path once while retaining every selected row for batch-field
    // presentation.
    let file_paths = unique_track_paths(tracks);

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

        // Show file path
        if let Some(path) = file_paths.first() {
            add_info_row(&info_group, "File", &path.to_string_lossy());
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

    let file_paths_for_save = file_paths.clone();
    let entries_for_save_state = entries_for_save.clone();
    let musicbrainz_for_save = musicbrainz_button.clone();
    let capability_for_save = capability_label.clone();
    let cancel_for_save = cancel_button.clone();
    let generation_for_save = operation_generation.clone();

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

        let paths = file_paths_for_save.clone();

        // Re-probe the entire selection before the first write, then track
        // both the files that were written and the ones that failed.
        let (tx, rx) = async_channel::bounded::<SaveOutcome>(1);
        let edits = edits.clone();

        std::thread::spawn(move || {
            let availability = preflight_paths(&paths);
            if availability != TagEditingAvailability::Ready {
                let _ = tx.send_blocking(SaveOutcome::Blocked(availability));
                return;
            }

            let mut modified = 0usize;
            let mut failed = 0usize;
            for path in &paths {
                match crate::local::tag_writer::write_tags(path, &edits) {
                    Ok(()) => {
                        modified += 1;
                    }
                    Err(e) => {
                        warn!(
                            path = %path.display(),
                            error = %e,
                            "Failed to write tags"
                        );
                        failed += 1;
                    }
                }
            }
            let current_availability = if failed == 0 {
                TagEditingAvailability::Ready
            } else {
                preflight_paths(&paths)
            };
            let _ = tx.send_blocking(SaveOutcome::Finished {
                modified,
                failed,
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
        glib::MainContext::default().spawn_local(async move {
            let outcome = rx.recv().await;
            if operation_generation.get() != save_generation {
                return;
            }

            match outcome {
                Ok(SaveOutcome::Blocked(availability)) => {
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
                }
                Ok(SaveOutcome::Finished {
                    modified,
                    failed,
                    current_availability,
                }) => {
                    if modified > 0 {
                        info!(count = modified, "Tags saved successfully");
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
                    // open so they can retry.
                    let total = modified + failed;
                    let body = format!(
                        "{failed} of {total} file(s) could not be saved and were left unchanged."
                    );
                    let alert = adw::AlertDialog::builder()
                        .heading("Could Not Save Some Files")
                        .body(&body)
                        .build();
                    alert.add_response("ok", "OK");
                    alert.present(Some(&parent));
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
    let paths_for_preflight = file_paths;
    std::thread::spawn(move || {
        let _ = preflight_tx.send_blocking(preflight_paths(&paths_for_preflight));
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

    fn track(path: PathBuf) -> TrackInfo {
        TrackInfo {
            path,
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

    #[test]
    fn repeated_playlist_rows_probe_and_write_one_exact_path() {
        let first = PathBuf::from("/music/album/song.flac");
        let second = PathBuf::from("/music/other.flac");
        let tracks = vec![
            track(first.clone()),
            track(first.clone()),
            track(second.clone()),
        ];

        assert_eq!(unique_track_paths(&tracks), vec![first, second]);
    }

    #[test]
    fn availability_controls_are_fail_closed_until_ready() {
        for availability in [
            TagEditingAvailability::Checking,
            TagEditingAvailability::Saving,
            TagEditingAvailability::UnsupportedFormat,
            TagEditingAvailability::InvalidFile,
            TagEditingAvailability::Unavailable,
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
    fn directory_preflight_stops_after_the_first_failure() {
        let paths = vec![
            PathBuf::from("/first/song.flac"),
            PathBuf::from("/second/song.flac"),
            PathBuf::from("/third/song.flac"),
        ];
        let mut probed = Vec::new();

        let availability = preflight_distinct_parents(&paths, |path| {
            probed.push(path.to_path_buf());
            Err(TagWritePreflightError::Unavailable)
        });

        assert_eq!(availability, TagEditingAvailability::Unavailable);
        assert_eq!(probed, vec![paths[0].clone()]);
    }

    #[test]
    fn empty_and_mixed_preflight_selections_fail_closed() {
        assert_eq!(preflight_paths(&[]), TagEditingAvailability::InvalidFile);

        let directory = tempfile::tempdir().expect("create preflight fixture");
        let supported = directory.path().join("song.flac");
        let unsupported = directory.path().join("song.wav");
        std::fs::write(&supported, b"audio").expect("write supported fixture");
        std::fs::write(&unsupported, b"audio").expect("write unsupported fixture");

        assert_eq!(
            preflight_paths(&[supported, unsupported]),
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
}
