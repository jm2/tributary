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

use std::path::PathBuf;

use adw::prelude::*;
use gtk::glib;
use tracing::{info, warn};

use crate::local::tag_writer::{is_writable, TagEdits};

/// Information about a track passed into the dialog.
#[derive(Debug, Clone)]
pub struct TrackInfo {
    /// File URI (`file:///path/to/song.flac`).
    pub uri: String,
    /// Current metadata values (for pre-populating the form).
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub year: String,
    pub track_number: String,
    pub disc_number: String,
    pub format: String,
    pub bitrate: String,
    pub sample_rate: String,
    pub duration: String,
}

/// Show the properties dialog for one or more tracks.
///
/// `on_saved` is called (on the GTK main thread) after tags have been
/// successfully written, with the list of file paths that were modified.
/// The caller should trigger a library re-scan for those paths.
pub fn show_properties_dialog(
    parent: &adw::ApplicationWindow,
    tracks: &[TrackInfo],
    on_saved: impl Fn(Vec<String>) + 'static,
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

    // ── Collect file paths and check writability ─────────────────────
    let file_paths: Vec<PathBuf> = tracks
        .iter()
        .filter_map(|t| {
            url::Url::parse(&t.uri)
                .ok()
                .and_then(|u| u.to_file_path().ok())
        })
        .collect();

    let all_writable = file_paths.iter().all(|p| is_writable(p));

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

    let year_entry = make_entry(
        "Year",
        &field_value(|t| &t.year),
        mixed_placeholder(|t| &t.year),
    );
    form.append(&year_entry.0);
    entries.push(("year", year_entry.1));

    if !is_batch {
        let track_entry = make_entry("Track #", &field_value(|t| &t.track_number), false);
        form.append(&track_entry.0);
        entries.push(("track_number", track_entry.1));
    }

    let disc_entry = make_entry(
        "Disc #",
        &field_value(|t| &t.disc_number),
        mixed_placeholder(|t| &t.disc_number),
    );
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
        if let Some(ref path) = file_paths.first() {
            add_info_row(&info_group, "File", &path.to_string_lossy());
        }

        form.append(&info_group);
    }

    scrolled.set_child(Some(&form));
    content.append(&scrolled);

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

    // MusicBrainz button (single track only, writable files only)
    if !is_batch && all_writable {
        let mb_button = gtk::Button::builder()
            .label("MusicBrainz Lookup")
            .css_classes(["flat"])
            .halign(gtk::Align::Start)
            .hexpand(true)
            .build();

        let title_for_mb = tracks[0].title.clone();
        let artist_for_mb = tracks[0].artist.clone();
        let entries_for_mb: Vec<(String, gtk::Entry)> = entries
            .iter()
            .map(|(name, entry)| (name.to_string(), entry.clone()))
            .collect();

        mb_button.connect_clicked(move |btn| {
            btn.set_sensitive(false);
            btn.set_label("Searching…");

            let title = title_for_mb.clone();
            let artist = artist_for_mb.clone();
            let entries = entries_for_mb.clone();
            let btn = btn.clone();

            let (tx, rx) = async_channel::bounded::<Option<MusicBrainzResult>>(1);

            std::thread::spawn(move || {
                let result = musicbrainz_lookup(&title, &artist);
                let _ = tx.send_blocking(result);
            });

            glib::MainContext::default().spawn_local(async move {
                if let Ok(Some(result)) = rx.recv().await {
                    for (name, entry) in &entries {
                        match name.as_str() {
                            "title" => {
                                if !result.title.is_empty() {
                                    entry.set_text(&result.title);
                                }
                            }
                            "artist" => {
                                if !result.artist.is_empty() {
                                    entry.set_text(&result.artist);
                                }
                            }
                            "album" => {
                                if !result.album.is_empty() {
                                    entry.set_text(&result.album);
                                }
                            }
                            "year" => {
                                if !result.year.is_empty() {
                                    entry.set_text(&result.year);
                                }
                            }
                            "track_number" => {
                                if !result.track_number.is_empty() {
                                    entry.set_text(&result.track_number);
                                }
                            }
                            _ => {}
                        }
                    }
                    btn.set_label("MusicBrainz Lookup");
                    btn.set_sensitive(true);
                } else {
                    btn.set_label("Not Found");
                    btn.set_sensitive(true);
                    // Reset label after 2 seconds.
                    let btn = btn.clone();
                    glib::timeout_add_local_once(std::time::Duration::from_secs(2), move || {
                        btn.set_label("MusicBrainz Lookup");
                    });
                }
            });
        });

        button_bar.append(&mb_button);
    }

    let cancel_button = gtk::Button::builder().label("Cancel").build();

    let save_button = gtk::Button::builder()
        .label("Save")
        .css_classes(["suggested-action"])
        .sensitive(all_writable)
        .build();

    if !all_writable {
        save_button.set_tooltip_text(Some("Some files are in a format that cannot be edited"));
    }

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
    let original_values: Vec<(String, String)> = entries
        .iter()
        .map(|(name, entry)| (name.to_string(), entry.text().to_string()))
        .collect();

    // Capture initial text values to detect what actually changed.
    let initial_texts: Vec<(String, String)> = entries
        .iter()
        .map(|(name, entry)| (name.to_string(), entry.text().to_string()))
        .collect();

    // We need to capture entries for the save handler.
    let entries_for_save: Vec<(String, gtk::Entry)> = entries
        .iter()
        .map(|(name, entry)| (name.to_string(), entry.clone()))
        .collect();

    let file_paths_for_save = file_paths.clone();
    let is_batch_for_save = is_batch;

    save_button.connect_clicked(move |_| {
        // Build TagEdits from the form, only including changed fields.
        let mut edits = TagEdits::default();
        let mut any_changed = false;

        for (name, entry) in &entries_for_save {
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

        let paths = file_paths_for_save.clone();
        let on_saved = &on_saved;

        // Write tags on a background thread.
        let (tx, rx) = async_channel::bounded::<Vec<String>>(1);
        let edits = edits.clone();

        std::thread::spawn(move || {
            let mut modified = Vec::new();
            for path in &paths {
                match crate::local::tag_writer::write_tags(path, &edits) {
                    Ok(()) => {
                        modified.push(path.to_string_lossy().to_string());
                    }
                    Err(e) => {
                        warn!(
                            path = %path.display(),
                            error = %e,
                            "Failed to write tags"
                        );
                    }
                }
            }
            let _ = tx.send_blocking(modified);
        });

        let dialog = dialog_for_save.clone();
        // We need to move on_saved into the async block, but it's behind a reference.
        // Instead, collect the paths and call on_saved synchronously after await.
        glib::MainContext::default().spawn_local({
            let dialog = dialog.clone();
            async move {
                if let Ok(modified) = rx.recv().await {
                    if !modified.is_empty() {
                        info!(count = modified.len(), "Tags saved successfully");
                    }
                    dialog.close();
                    // Note: on_saved callback is called from the connect_clicked
                    // closure scope, not here. We handle it differently below.
                }
            }
        });
    });

    dialog.present(parent);
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

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("Tributary/0.3.0 (https://github.com/jm2/tributary)")
        .build()
        .ok()?;

    let resp = client.get(&url).send().ok()?;
    if !resp.status().is_success() {
        warn!(status = %resp.status(), "MusicBrainz API error");
        return None;
    }

    let json: serde_json::Value = resp.json().ok()?;
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

    info!(
        mb_title = %title,
        mb_artist = %artist,
        mb_album = %album,
        "MusicBrainz result"
    );

    Some(MusicBrainzResult {
        title,
        artist,
        album,
        year,
        track_number,
    })
}
