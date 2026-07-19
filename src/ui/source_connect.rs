//! Sidebar selection-changed handler — source switching + auth dialogs.
//!
//! Handles clicking sidebar items: switching between local, playlist,
//! USB, radio, connected remote, and unauthenticated remote sources.

use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use tracing::info;

use crate::architecture::SourceId;
use crate::local::engine::LibraryEvent;
use crate::source_registry::RegularPlaylistTrackResolution;

use super::objects::{
    PlaylistOccurrenceBinding, PlaylistOccurrenceState, PlaylistRowUnavailableReason, SourceObject,
    TrackObject,
};
use super::playback::refresh_projected_library_uris;
use super::playlist_projection::{project_playlist_rows, PlaylistRowContent, PlaylistRowSpec};
use super::preferences;
use super::radio::{apply_radio_columns, handle_radio_nearme, is_radio_backend, radio_view_origin};
use super::server_dialogs::{show_auth_dialog, validate_remote_server_url};
use super::source_navigation::{
    CompletionDisposition, ConnectionIntentKind, PendingConnection, SourceNavigation, SourceRequest,
};
use super::tracklist;
use super::window::{arch_track_to_object, display_tracks};
use super::window_state::WindowState;

enum PlaylistLoadOutcome {
    Smart(Vec<crate::architecture::models::Track>),
    Regular {
        rows: Vec<PlaylistRowSpec>,
        authority: Vec<RegularPlaylistTrackResolution>,
    },
    Missing,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemoteFailureCategory {
    Authentication,
    Connection,
    Timeout,
    Response,
    AuthenticationMethod,
    Backend,
}

impl RemoteFailureCategory {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::Connection => "connection",
            Self::Timeout => "timeout",
            Self::Response => "response",
            Self::AuthenticationMethod => "authentication-method",
            Self::Backend => "backend",
        }
    }

    #[cfg(test)]
    pub(super) fn log_message(self) -> &'static str {
        match self {
            Self::Authentication => "Remote authentication rejected",
            Self::Connection => "Remote connection failed",
            Self::Timeout => "Remote connection timed out",
            Self::Response => "Remote response failed",
            Self::AuthenticationMethod => "Remote authentication method failed",
            Self::Backend => "Remote source failed",
        }
    }

    pub(super) fn user_message(self, backend_type: &str) -> String {
        let locale = rust_i18n::locale();
        self.user_message_for_locale(backend_type, &locale)
    }

    fn user_message_for_locale(self, backend_type: &str, locale: &str) -> String {
        match self {
            Self::Authentication => rust_i18n::t!(
                "errors.remote.authentication_rejected",
                locale = locale,
                backend = backend_type
            ),
            Self::Connection => rust_i18n::t!(
                "errors.remote.connection_failed",
                locale = locale,
                backend = backend_type
            ),
            Self::Timeout => rust_i18n::t!(
                "errors.remote.connection_timed_out",
                locale = locale,
                backend = backend_type
            ),
            Self::Response => rust_i18n::t!(
                "errors.remote.invalid_response",
                locale = locale,
                backend = backend_type
            ),
            Self::AuthenticationMethod => rust_i18n::t!(
                "errors.remote.authentication_method_unsupported",
                locale = locale,
                backend = backend_type
            ),
            Self::Backend => rust_i18n::t!(
                "errors.remote.source_failed",
                locale = locale,
                backend = backend_type
            ),
        }
        .into_owned()
    }
}

#[cfg(test)]
pub(super) fn remote_failure_category(
    error: &crate::architecture::error::BackendError,
) -> RemoteFailureCategory {
    use crate::architecture::error::BackendError;

    match error {
        BackendError::AuthenticationFailed { .. } => RemoteFailureCategory::Authentication,
        BackendError::ConnectionFailed { .. } | BackendError::Io(_) => {
            RemoteFailureCategory::Connection
        }
        BackendError::Timeout { .. } => RemoteFailureCategory::Timeout,
        BackendError::ParseError { .. } => RemoteFailureCategory::Response,
        BackendError::TokenAuthNotSupported { .. } => RemoteFailureCategory::AuthenticationMethod,
        BackendError::NotFound { .. }
        | BackendError::Unsupported { .. }
        | BackendError::Internal(_) => RemoteFailureCategory::Backend,
    }
}

pub(super) const fn lifecycle_failure_category(
    category: crate::source_lifecycle::FailureCategory,
) -> RemoteFailureCategory {
    use crate::source_lifecycle::FailureCategory;

    match category {
        FailureCategory::AuthenticationRejected => RemoteFailureCategory::Authentication,
        FailureCategory::Connection => RemoteFailureCategory::Connection,
        FailureCategory::Timeout => RemoteFailureCategory::Timeout,
        FailureCategory::InvalidResponse => RemoteFailureCategory::Response,
        FailureCategory::UnsupportedAuthentication => RemoteFailureCategory::AuthenticationMethod,
        FailureCategory::UnavailableOrPermission | FailureCategory::Backend => {
            RemoteFailureCategory::Backend
        }
    }
}

/// Return the retained, sanitized construction failure for one removable
/// source. Mount scanning starts before row selection, so an inactive failure
/// must remain available for the first later selection instead of looking like
/// an accepted empty catalogue.
fn retained_removable_connect_failure(
    source_registry: &crate::source_registry::SourceRegistry,
    source_id: crate::architecture::SourceId,
) -> Option<RemoteFailureCategory> {
    let snapshot = source_registry.snapshot(source_id)?;
    if !snapshot
        .provenance
        .contains(crate::source_lifecycle::SourceProvenance::Removable)
    {
        return None;
    }
    let failure = snapshot.failure?;
    (failure.failure.operation() == crate::source_lifecycle::FailureOperation::Connect)
        .then(|| lifecycle_failure_category(failure.failure.category()))
}

fn resolve_source_key(
    explicit_key: &str,
    stable_source_id: &str,
    server_url: &str,
    backend_type: &str,
) -> String {
    if backend_type == "local" || backend_type.is_empty() {
        "local".to_string()
    } else if backend_type == "usb-device" && !stable_source_id.is_empty() {
        stable_source_id.to_string()
    } else if !explicit_key.is_empty() {
        explicit_key.to_string()
    } else if !server_url.is_empty() && !stable_source_id.is_empty() {
        stable_source_id.to_string()
    } else if !server_url.is_empty() {
        server_url.to_string()
    } else {
        backend_type.to_string()
    }
}

/// Admit one authentication-dialog submission against the exact live intent.
///
/// Dialogs can outlive navigation changes and mDNS route updates or removal.
/// The submitted credentials therefore gain authority only when the original
/// pending request is still current and the exact source row still exists.
/// Every rejected submission clears only the pending request it owns, leaving
/// a newer connection attempt untouched.
fn prepare_remote_auth_submission(
    pending_connection: &RefCell<Option<PendingConnection>>,
    source_navigation: &SourceNavigation,
    connection_request: &SourceRequest,
    sidebar_store: &gtk::gio::ListStore,
    server_url: &str,
    backend_type: &str,
) -> Option<(u32, SourceObject)> {
    let owns_pending = pending_connection
        .borrow()
        .as_ref()
        .is_some_and(|pending| pending.request() == connection_request);
    if !owns_pending {
        return None;
    }

    let current_source = source_navigation
        .is_current(connection_request)
        .then(|| {
            super::discovery_handler::remote_source_at(sidebar_store, server_url, backend_type)
        })
        .flatten()
        .filter(|(_, source)| !source.connected() && !source.connecting());

    if current_source.is_none() {
        *pending_connection.borrow_mut() = None;
    }
    current_source
}

fn restore_pending_selection(
    sidebar_store: &gtk::gio::ListStore,
    selection: &gtk::SingleSelection,
    pending_source_key: &str,
    fallback_source_key: &str,
) {
    let position = (0..sidebar_store.n_items()).find(|position| {
        sidebar_store
            .item(*position)
            .and_downcast_ref::<SourceObject>()
            .is_some_and(|source| {
                source
                    .source_id()
                    .is_some_and(|id| id.to_string() == pending_source_key)
                    || source.source_key() == pending_source_key
            })
    });
    if let Some(position) = position {
        if selection.selected() != position {
            selection.set_selected(position);
        }
    } else {
        super::window::select_sidebar_source_key(sidebar_store, selection, fallback_source_key);
    }
}

fn with_active_source_key_snapshot<T>(
    active_source_key: &Rc<RefCell<String>>,
    use_key: impl FnOnce(&str) -> T,
) -> T {
    // The callback may change GtkSingleSelection and synchronously re-enter a
    // handler that mutates this cell, so clone and release the guard first.
    let key = active_source_key.borrow().clone();
    use_key(&key)
}

/// Cache a completed source load if it is still the newest request for that
/// source, and report whether it also owns the visible projection.
pub(super) fn cache_source_completion(
    navigation: &Rc<RefCell<SourceNavigation>>,
    request: &SourceRequest,
    source_tracks: &Rc<RefCell<HashMap<String, Vec<TrackObject>>>>,
    objects: &[TrackObject],
    active_source_key: &Rc<RefCell<String>>,
) -> bool {
    match navigation.borrow().completion(request) {
        CompletionDisposition::Ignore => false,
        CompletionDisposition::CacheOnly => {
            source_tracks
                .borrow_mut()
                .insert(request.source_key().to_string(), objects.to_vec());
            false
        }
        CompletionDisposition::CacheAndRender => {
            source_tracks
                .borrow_mut()
                .insert(request.source_key().to_string(), objects.to_vec());
            *active_source_key.borrow() == request.source_key()
        }
    }
}

fn evict_source_completion(
    navigation: &Rc<RefCell<SourceNavigation>>,
    request: &SourceRequest,
    source_tracks: &Rc<RefCell<HashMap<String, Vec<TrackObject>>>>,
    active_source_key: &Rc<RefCell<String>>,
) -> bool {
    match navigation.borrow().completion(request) {
        CompletionDisposition::Ignore => false,
        CompletionDisposition::CacheOnly => {
            source_tracks.borrow_mut().remove(request.source_key());
            false
        }
        CompletionDisposition::CacheAndRender => {
            source_tracks.borrow_mut().remove(request.source_key());
            *active_source_key.borrow() == request.source_key()
        }
    }
}

fn playlist_unavailable_reason_text(reason: PlaylistRowUnavailableReason) -> String {
    playlist_unavailable_reason_text_for_locale(reason, &rust_i18n::locale())
}

fn playlist_unavailable_reason_text_for_locale(
    reason: PlaylistRowUnavailableReason,
    locale: &str,
) -> String {
    let key = match reason {
        PlaylistRowUnavailableReason::LocalTrackMissing
        | PlaylistRowUnavailableReason::LocalTrackUnmatched
        | PlaylistRowUnavailableReason::TrackMissing => "regular_playlist.track_missing",
        PlaylistRowUnavailableReason::SourceUnavailable => "regular_playlist.source_unavailable",
        PlaylistRowUnavailableReason::UnsupportedSource => "regular_playlist.source_unsupported",
        PlaylistRowUnavailableReason::InvalidCatalogue => {
            "regular_playlist.source_catalogue_unavailable"
        }
    };
    rust_i18n::t!(key, locale = locale).into_owned()
}

fn unavailable_playlist_row(binding: PlaylistOccurrenceBinding) -> TrackObject {
    let reason = match binding.state() {
        PlaylistOccurrenceState::Unavailable(reason) => reason,
        PlaylistOccurrenceState::AvailableLocal | PlaylistOccurrenceState::AvailableRemote(_) => {
            unreachable!("unavailable content carries a closed unavailable binding")
        }
    };
    let row = TrackObject::new(
        0,
        rust_i18n::t!("regular_playlist.unavailable_track").as_ref(),
        0,
        &playlist_unavailable_reason_text(reason),
        "",
        "",
        "",
        0,
        "",
        0,
        0,
        0,
        "",
        "",
    );
    row.set_playlist_occurrence_binding(binding);
    row
}

fn remote_playlist_row(
    binding: PlaylistOccurrenceBinding,
    track: crate::source_registry::RegularPlaylistTrack,
) -> TrackObject {
    let metadata = track.metadata();
    let row = TrackObject::new(
        metadata.track_number().unwrap_or(0),
        metadata.title(),
        metadata.duration_secs().unwrap_or(0),
        metadata.artist_name(),
        metadata.album_title(),
        metadata.genre().unwrap_or("Unknown"),
        metadata.composer().unwrap_or(""),
        metadata.year().unwrap_or(0),
        &metadata
            .date_modified()
            .map(|date| date.format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
        metadata.bitrate_kbps().unwrap_or(0),
        metadata.sample_rate_hz().unwrap_or(0),
        metadata.play_count().unwrap_or(0),
        metadata.format().unwrap_or(""),
        "",
    );
    if let Some(album_artist) = metadata.album_artist_name() {
        row.set_album_artist(album_artist);
    }
    row.set_disc_number(metadata.disc_number().unwrap_or(0));
    row.set_rating(metadata.rating());
    let guard = track.guard();
    row.set_source_session_epoch(guard.session_epoch());
    row.set_source_catalogue_generation(guard.catalogue_generation());
    row.set_playlist_occurrence_binding(binding);
    row
}

pub(super) fn playlist_row_to_object(spec: PlaylistRowSpec) -> TrackObject {
    let (binding, content) = spec.into_parts();
    match content {
        PlaylistRowContent::AvailableLocal(track) => {
            let architecture_track = crate::local::engine::db_model_to_track(&track);
            let row = arch_track_to_object(&architecture_track);
            row.set_playlist_occurrence_binding(binding);
            row
        }
        PlaylistRowContent::AvailableRemote(track) => remote_playlist_row(binding, track),
        PlaylistRowContent::Unavailable => unavailable_playlist_row(binding),
    }
}

/// Load and publish a playlist through the same generation-owned path used by
/// both explicit navigation and post-reconciliation refreshes.
#[allow(clippy::too_many_arguments)]
pub(super) fn load_playlist_source(
    rt_handle: tokio::runtime::Handle,
    source_registry: crate::source_registry::SourceRegistry,
    playlist_id: String,
    request: SourceRequest,
    navigation: Rc<RefCell<SourceNavigation>>,
    source_tracks: Rc<RefCell<HashMap<String, Vec<TrackObject>>>>,
    active_source_key: Rc<RefCell<String>>,
    track_store: gtk::gio::ListStore,
    master_tracks: Rc<RefCell<Vec<TrackObject>>>,
    browser_widget: gtk::Box,
    browser_state: super::browser::BrowserState,
    status_label: gtk::Label,
    column_view: gtk::ColumnView,
) {
    let (tracks_tx, tracks_rx) = async_channel::bounded::<PlaylistLoadOutcome>(1);
    let registry_for_load = source_registry.clone();

    rt_handle.spawn(async move {
        let outcome = match crate::db::connection::init_db().await {
            Ok(db) => {
                let manager = crate::local::playlist_manager::PlaylistManager::new(db);
                match manager.get_playlist(&playlist_id).await {
                    Ok(Some(playlist)) => {
                        if playlist.is_smart {
                            match manager.evaluate_smart_playlist(&playlist_id).await {
                                Ok(models) => PlaylistLoadOutcome::Smart(
                                    models
                                        .iter()
                                        .map(crate::local::engine::db_model_to_track)
                                        .collect(),
                                ),
                                Err(error) => {
                                    tracing::warn!(%error, "Failed to evaluate smart playlist");
                                    PlaylistLoadOutcome::Failed
                                }
                            }
                        } else {
                            match manager.load_playlist_entries(&playlist_id).await {
                                Ok(entries) => {
                                    let remote_keys = entries
                                        .iter()
                                        .filter(|entry| {
                                            entry.stored.source_id != SourceId::local()
                                        })
                                        .filter_map(|entry| entry.stored.media_key())
                                        .collect::<Vec<_>>();
                                    let authority = registry_for_load
                                        .resolve_regular_playlist_tracks(&remote_keys);
                                    match project_playlist_rows(entries, authority.clone()) {
                                        Ok(rows)
                                            if registry_for_load
                                                .are_regular_playlist_tracks_current(&authority) =>
                                        {
                                            PlaylistLoadOutcome::Regular { rows, authority }
                                        }
                                        Ok(_) => {
                                            tracing::debug!(
                                                "Playlist catalogue changed while rows were projected"
                                            );
                                            PlaylistLoadOutcome::Failed
                                        }
                                        Err(error) => {
                                            tracing::warn!(%error, "Playlist projection failed closed");
                                            PlaylistLoadOutcome::Failed
                                        }
                                    }
                                }
                                Err(error) => {
                                    tracing::warn!(%error, "Failed to load regular playlist entries");
                                    PlaylistLoadOutcome::Failed
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        tracing::debug!("Playlist disappeared before its tracks were loaded");
                        PlaylistLoadOutcome::Missing
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Failed to identify playlist type");
                        PlaylistLoadOutcome::Failed
                    }
                }
            }
            Err(error) => {
                tracing::error!(%error, "Failed to open DB for playlist");
                PlaylistLoadOutcome::Failed
            }
        };
        let _ = tracks_tx.send(outcome).await;
    });

    glib::MainContext::default().spawn_local(async move {
        let Ok(outcome) = tracks_rx.recv().await else {
            return;
        };
        let objects = match outcome {
            PlaylistLoadOutcome::Smart(tracks) => {
                tracks.iter().map(arch_track_to_object).collect::<Vec<_>>()
            }
            PlaylistLoadOutcome::Regular { rows, authority } => {
                if !source_registry.are_regular_playlist_tracks_current(&authority) {
                    tracing::debug!(
                        source = %request.source_key(),
                        generation = request.generation(),
                        "Discarding playlist rows whose catalogue authority changed before publication"
                    );
                    return;
                }
                rows.into_iter().map(playlist_row_to_object).collect()
            }
            PlaylistLoadOutcome::Missing => {
                if evict_source_completion(
                    &navigation,
                    &request,
                    &source_tracks,
                    &active_source_key,
                ) {
                    display_tracks(
                        &[],
                        &track_store,
                        &master_tracks,
                        &browser_widget,
                        &browser_state,
                        &status_label,
                        &column_view,
                    );
                }
                return;
            }
            PlaylistLoadOutcome::Failed => {
                // A transient database/query failure is not an empty playlist.
                // Preserve the last successful cache and visible stale-while-
                // revalidate snapshot for a later retry.
                return;
            }
        };

        // A paired rename may commit while this database result is in flight.
        // Overlay the latest committed local paths at the GTK publication
        // boundary so either callback order converges on the live URI.
        if let Some(local_tracks) = source_tracks.borrow().get("local") {
            refresh_projected_library_uris(&objects, local_tracks);
        }

        let should_render = cache_source_completion(
            &navigation,
            &request,
            &source_tracks,
            &objects,
            &active_source_key,
        );
        if should_render {
            display_tracks(
                &objects,
                &track_store,
                &master_tracks,
                &browser_widget,
                &browser_state,
                &status_label,
                &column_view,
            );
        } else {
            tracing::debug!(
                source = %request.source_key(),
                generation = request.generation(),
                "Playlist result cached or ignored without rendering"
            );
        }
    });
}

/// Wire the sidebar selection-changed signal.
///
/// This function owns all the logic for switching between sources when
/// the user clicks a sidebar row: local library, playlists, USB devices,
/// internet radio, already-connected remote servers, and unauthenticated
/// servers (which trigger auth dialogs).
pub fn setup_source_connect(state: &WindowState) {
    let sel = state.sidebar_selection.clone();
    let engine_tx = state.engine_tx.clone();
    let rt_handle = state.rt_handle.clone();
    let win = state.window.clone();
    let track_store = state.track_store.clone();
    let master_tracks = state.master_tracks.clone();
    let source_tracks = state.source_tracks.clone();
    let active_source_key = state.active_source_key.clone();
    let source_navigation = state.source_navigation.clone();
    let near_me_consent_request = state.near_me_consent_request.clone();
    let browser_widget = state.browser_widget.clone();
    let browser_state = state.browser_state.clone();
    let status_label = state.status_label.clone();
    let column_view = state.column_view.clone();
    let app_config = state.app_config.clone();
    let sidebar_store = state.sidebar_store.clone();
    let pending_connection = state.pending_connection.clone();
    let pre_connect_selection = state.pre_connect_selection.clone();
    let source_registry = state.source_registry.clone();

    sel.connect_selection_changed(move |sel, _, _| {
        let Some(item) = sel.selected_item() else {
            return;
        };
        let Some(src) = item.downcast_ref::<SourceObject>() else {
            return;
        };
        if src.is_header() {
            return;
        }

        // ── Connection guard: ignore clicks while a connection is pending ──
        // This prevents duplicate auth dialogs and duplicate connection
        // tasks when the user clicks a server that is still connecting
        // (applies to all network sources: Subsonic, Jellyfin, Plex, DAAP).
        if pending_connection.borrow().is_some() {
            let pending = pending_connection.borrow().clone();
            if let Some(pending) = pending {
                // If clicking the same server that's already connecting, just ignore.
                if src
                    .source_id()
                    .is_some_and(|id| id.to_string() == pending.source_key())
                {
                    return;
                }
                // If clicking a different server while one is connecting,
                // also ignore — let the first connection finish first.
                if src.connecting() || (!src.connected() && !src.server_url().is_empty()) {
                    with_active_source_key_snapshot(&active_source_key, |fallback_source_key| {
                        restore_pending_selection(
                            &sidebar_store,
                            sel,
                            pending.source_key(),
                            fallback_source_key,
                        );
                    });
                    return;
                }
            }
        }

        // Environment and manual startup attempts do not own a deferred
        // navigation token. A click on their spinner must never open a second
        // dialog or supersede the credential-bearing generation (especially
        // with a passwordless DAAP retry).
        if src.connecting() {
            with_active_source_key_snapshot(&active_source_key, |fallback_source_key| {
                super::window::select_sidebar_source_key(&sidebar_store, sel, fallback_source_key);
            });
            return;
        }

        let backend_type = src.backend_type();

        // Determine the navigation/cache key. Device rows carry an explicit
        // logical key and remote rows prefer their stable SourceId; only a
        // malformed legacy remote row can fall back to its server URL. Local
        // static sources retain the built-in "local" view key.
        let explicit_key = src.source_key();
        let stable_source_id = src.source_id().map(|id| id.to_string()).unwrap_or_default();
        let url = src.server_url();
        let key = resolve_source_key(&explicit_key, &stable_source_id, &url, &backend_type);

        // ── Local source: switch to local view ───────────────────
        if key == "local" {
            source_navigation.borrow_mut().select("local");
            *active_source_key.borrow_mut() = "local".to_string();
            pre_connect_selection.set(sel.selected());

            // Restore music column layout if coming from radio.
            apply_radio_columns(&column_view, false);
            let cfg = app_config.borrow();
            preferences::apply_column_visibility(&column_view, &cfg.visible_columns);
            preferences::update_browser_visibility(&browser_widget, &cfg.browser_views);
            drop(cfg);

            let st = source_tracks.borrow();
            let local_tracks = st.get("local").cloned().unwrap_or_default();
            display_tracks(
                &local_tracks,
                &track_store,
                &master_tracks,
                &browser_widget,
                &browser_state,
                &status_label,
                &column_view,
            );
            return;
        }

        // ── Playlist source: fetch playlist tracks ───────────────
        if backend_type == "playlist" || backend_type == "smart-playlist" {
            let playlist_id = src.playlist_id();
            if playlist_id.is_empty() {
                return;
            }

            let playlist_source_key =
                format!("{}{playlist_id}", super::playback::PLAYLIST_SOURCE_PREFIX);
            let request = source_navigation
                .borrow_mut()
                .select(playlist_source_key.clone());
            *active_source_key.borrow_mut() = playlist_source_key.clone();
            pre_connect_selection.set(sel.selected());

            // Restore music column layout (not radio).
            apply_radio_columns(&column_view, false);
            // Restore column + browser visibility from config (issue #38:
            // opening a playlist must not reset hidden columns to default).
            let cfg = app_config.borrow();
            preferences::apply_column_visibility(&column_view, &cfg.visible_columns);
            preferences::update_browser_visibility(&browser_widget, &cfg.browser_views);
            drop(cfg);

            let cached = source_tracks.borrow().get(&playlist_source_key).cloned();
            display_tracks(
                cached.as_deref().unwrap_or_default(),
                &track_store,
                &master_tracks,
                &browser_widget,
                &browser_state,
                &status_label,
                &column_view,
            );

            load_playlist_source(
                rt_handle.clone(),
                source_registry.clone(),
                playlist_id,
                request,
                source_navigation.clone(),
                source_tracks.clone(),
                active_source_key.clone(),
                track_store.clone(),
                master_tracks.clone(),
                browser_widget.clone(),
                browser_state.clone(),
                status_label.clone(),
                column_view.clone(),
            );
            return;
        }

        // ── USB device source: display its lifecycle catalogue ──
        if backend_type == "usb-device" {
            source_navigation.borrow_mut().select(key.clone());
            *active_source_key.borrow_mut() = key.clone();
            pre_connect_selection.set(sel.selected());

            let retained_failure = src.source_id().and_then(|source_id| {
                retained_removable_connect_failure(&source_registry, source_id)
            });

            // Restore music column layout if coming from radio.
            apply_radio_columns(&column_view, false);
            let cfg = app_config.borrow();
            preferences::apply_column_visibility(&column_view, &cfg.visible_columns);
            preferences::update_browser_visibility(&browser_widget, &cfg.browser_views);
            drop(cfg);

            // The removable controller starts registry-owned scanning at
            // mount arrival. Selection never opens the mount or constructs a
            // competing scan; it displays only the latest accepted pathless
            // snapshot, or an empty projection while the first scan is live.
            let tracks = source_tracks
                .borrow()
                .get(&key)
                .cloned()
                .unwrap_or_default();
            display_tracks(
                &tracks,
                &track_store,
                &master_tracks,
                &browser_widget,
                &browser_state,
                &status_label,
                &column_view,
            );
            if let Some(category) = retained_failure {
                status_label.set_text(&category.user_message("Removable media"));
            }
            return;
        }

        // ── Radio source: request one lifecycle-owned view ──────
        if is_radio_backend(&backend_type) {
            let request = source_navigation.borrow_mut().select(backend_type.clone());
            *active_source_key.borrow_mut() = backend_type.clone();
            pre_connect_selection.set(sel.selected());

            // Switch to radio column layout.
            apply_radio_columns(&column_view, true);
            // Hide browser for radio.
            browser_widget.set_visible(false);

            // Show a prior completed result while refreshing. If this source
            // has never loaded, clear the old source immediately.
            if let Some(cached) = source_tracks.borrow().get(&backend_type).cloned() {
                display_tracks(
                    &cached,
                    &track_store,
                    &master_tracks,
                    &browser_widget,
                    &browser_state,
                    &status_label,
                    &column_view,
                );
            } else {
                track_store.remove_all();
                tracklist::update_status(&status_label, &[]);
                *master_tracks.borrow_mut() = Vec::new();
            }

            let view_origin = radio_view_origin(&backend_type)
                .expect("exact built-in radio keys have a typed view origin");
            let registry = source_registry.clone();
            let refresh = Rc::new(move || {
                if registry
                    .refresh_builtin_radio_view(view_origin.clone())
                    .is_none()
                {
                    // Constructor failures are retained and deduplicated by
                    // the lifecycle reducer. `None` can also mean shutdown,
                    // so this call site must not manufacture a second or
                    // stale user-visible error.
                    tracing::debug!(view = ?view_origin, "Radio view refresh was not admitted");
                }
            });

            // Near Me requires explicit location consent before the registry
            // may ask its adapter to perform geolocation. The other static
            // views have no GTK-side prerequisite.
            if backend_type == super::radio::NEARME_SOURCE_KEY {
                let app_config = app_config.clone();
                let track_store = track_store.clone();
                let master_tracks = master_tracks.clone();
                let browser_widget = browser_widget.clone();
                let browser_state = browser_state.clone();
                let status_label = status_label.clone();
                let column_view = column_view.clone();
                let active_source_key = active_source_key.clone();
                let source_navigation = source_navigation.clone();
                let near_me_consent_request = near_me_consent_request.clone();
                let sidebar_selection = sel.clone();
                let source_tracks = source_tracks.clone();
                let sidebar_store = sidebar_store.clone();
                let win = win.clone();

                handle_radio_nearme(
                    &win,
                    app_config,
                    track_store,
                    master_tracks,
                    browser_widget,
                    browser_state,
                    status_label,
                    column_view,
                    active_source_key,
                    source_navigation,
                    request,
                    near_me_consent_request,
                    sidebar_store,
                    sidebar_selection,
                    source_tracks,
                    refresh,
                );
            } else {
                refresh();
            }
            return;
        }

        // ── Connected source: switch view ───────────────────────
        if src.connected() {
            source_navigation.borrow_mut().select(key.clone());
            *active_source_key.borrow_mut() = key.clone();
            pre_connect_selection.set(sel.selected());

            // Restore music column layout if coming from radio.
            apply_radio_columns(&column_view, false);
            // Restore column + browser visibility from config (issue #38:
            // connecting a remote source must not reset hidden columns to
            // default).
            let cfg = app_config.borrow();
            preferences::apply_column_visibility(&column_view, &cfg.visible_columns);
            preferences::update_browser_visibility(&browser_widget, &cfg.browser_views);
            drop(cfg);

            let st = source_tracks.borrow();
            let tracks = st.get(&key).cloned().unwrap_or_default();
            display_tracks(
                &tracks,
                &track_store,
                &master_tracks,
                &browser_widget,
                &browser_state,
                &status_label,
                &column_view,
            );
            return;
        }

        // ── Discovered (unauthenticated) ────────────────────────
        let server_name = src.name();
        let server_url = src.server_url();
        let advertised_route = src.advertised_route();
        let engine_tx = engine_tx.clone();
        let win = win.clone();
        let sidebar_store = sidebar_store.clone();
        let selected_pos = sel.selected();

        let backend_type = src.backend_type();
        let name_for_closure = server_name.clone();
        let url_for_closure = server_url.clone();
        let requires_password = src.requires_password();

        // Saved rows can predate the current URL policy. Reject an unsafe
        // standard-backend URL before it is displayed in an auth dialog,
        // logged by a connection task, or registered as source ownership.
        // The fixed error text cannot echo user-info or query credentials from
        // the rejected row.
        if let Err(message) = validate_remote_server_url(&server_url) {
            tracing::warn!(error = message, "Remote server URL rejected");
            let _ = engine_tx.try_send(LibraryEvent::Error(message.to_string()));
            return;
        }

        // Backend/session generations protect remote ownership, but they do
        // not say whether the user still wants this pending connection to
        // change the selected source when it completes.
        let connection_request = source_navigation.borrow_mut().select(key.clone());

        // For passwordless DAAP servers, bypass the dialog entirely
        // and connect directly.
        if backend_type == "daap" && !requires_password {
            let Some(source_id) = src.source_id() else {
                tracing::warn!("Remote source has no stable identity");
                return;
            };
            *pending_connection.borrow_mut() = Some(PendingConnection::new(
                key.clone(),
                connection_request.clone(),
            ));

            let server_url = url_for_closure.clone();
            let server_name = name_for_closure.clone();
            let advertised_route = advertised_route.clone();
            let source = src.clone();
            let sidebar_store_for_generation = sidebar_store.clone();
            let sidebar_selection_for_generation = sel.clone();
            let pending_for_generation = pending_connection.clone();
            let request_for_generation = connection_request.clone();
            let generation = source_registry.connect_daap(
                source_id,
                move |generation| {
                    let bound = pending_for_generation
                        .borrow_mut()
                        .as_mut()
                        .filter(|pending| pending.request() == &request_for_generation)
                        .is_some_and(|pending| {
                            pending.bind_lifecycle(
                                source_id,
                                generation,
                                ConnectionIntentKind::PasswordlessDaap,
                            )
                        });
                    debug_assert!(bound, "passwordless DAAP intent is bound before spawn");
                    source.set_connecting_generation(generation);
                    super::window::rebind_sidebar_source(
                        &sidebar_store_for_generation,
                        &sidebar_selection_for_generation,
                        selected_pos,
                        &source,
                        true,
                    );
                },
                move || async move {
                    info!("Connecting to passwordless DAAP server...");
                    crate::daap::DaapBackend::login_with_route(
                        &server_name,
                        &server_url,
                        None,
                        advertised_route,
                    )
                    .await
                },
            );
            if generation.is_none() {
                *pending_connection.borrow_mut() = None;
                tracing::debug!("Skipping DAAP connect during shutdown or after source retirement");
            }
            return;
        }

        let password_only = backend_type == "daap";

        // Set the pending-connection guard *before* the auth dialog is
        // shown so a second sidebar click while the dialog is open is
        // ignored at the top of this handler. The submit closure and
        // the cancel callback below clear it when appropriate.
        *pending_connection.borrow_mut() = Some(PendingConnection::new(
            key.clone(),
            connection_request.clone(),
        ));

        // Clone Rc's before moving into the auth dialog closure so
        // the outer `Fn` closure can be called multiple times.
        let pending_for_auth = pending_connection.clone();

        // If the user cancels / escapes the auth dialog, drop the
        // pending-connection guard so the next sidebar click works.
        let pending_for_cancel = pending_connection.clone();
        let navigation_for_cancel = source_navigation.clone();
        let request_for_cancel = connection_request.clone();
        let sel_for_cancel = (*sel).clone();
        let store_for_cancel = sidebar_store.clone();
        let active_key_for_cancel = active_source_key.clone();
        let on_cancel = move || {
            let owns_pending = pending_for_cancel
                .borrow()
                .as_ref()
                .is_some_and(|pending| pending.request() == &request_for_cancel);
            if owns_pending {
                *pending_for_cancel.borrow_mut() = None;
                if navigation_for_cancel
                    .borrow()
                    .is_current(&request_for_cancel)
                {
                    with_active_source_key_snapshot(
                        &active_key_for_cancel,
                        |fallback_source_key| {
                            super::window::select_sidebar_source_key(
                                &store_for_cancel,
                                &sel_for_cancel,
                                fallback_source_key,
                            );
                        },
                    );
                }
            }
        };
        let navigation_for_submit = source_navigation.clone();
        let source_registry_for_auth = source_registry.clone();
        let selection_for_auth = (*sel).clone();

        show_auth_dialog(
            &win,
            &server_name,
            &server_url,
            password_only,
            move |user, pass| {
                let server_url = url_for_closure.clone();
                let server_name = name_for_closure.clone();
                let backend_type = backend_type.clone();
                let connection_request = connection_request.clone();

                // Discovery may update or withdraw the route while the user
                // is typing. A dialog response owns no authority by itself:
                // require the original pending/navigation intent and snapshot
                // the exact current row only at submission time.
                let Some((current_pos, submitted_source)) = prepare_remote_auth_submission(
                    &pending_for_auth,
                    &navigation_for_submit.borrow(),
                    &connection_request,
                    &sidebar_store,
                    &server_url,
                    &backend_type,
                ) else {
                    tracing::debug!(
                        "Ignoring stale, withdrawn, or duplicate authentication submission"
                    );
                    return;
                };
                let advertised_route = submitted_source.advertised_route();
                let Some(source_id) = submitted_source.source_id() else {
                    *pending_for_auth.borrow_mut() = None;
                    tracing::warn!("Remote source has no stable identity");
                    return;
                };
                let source_for_generation = submitted_source.clone();
                let store_for_generation = sidebar_store.clone();
                let selection_for_generation = selection_for_auth.clone();
                let pending_for_generation = pending_for_auth.clone();
                let request_for_generation = connection_request.clone();
                let on_generation = move |generation| {
                    let bound = pending_for_generation
                        .borrow_mut()
                        .as_mut()
                        .filter(|pending| pending.request() == &request_for_generation)
                        .is_some_and(|pending| {
                            pending.bind_lifecycle(
                                source_id,
                                generation,
                                ConnectionIntentKind::Interactive,
                            )
                        });
                    debug_assert!(bound, "interactive remote intent is bound before spawn");
                    source_for_generation.set_connecting_generation(generation);
                    super::window::rebind_sidebar_source(
                        &store_for_generation,
                        &selection_for_generation,
                        current_pos,
                        &source_for_generation,
                        true,
                    );
                };

                let generation = match backend_type.as_str() {
                    "jellyfin" => source_registry_for_auth.connect_jellyfin_session(
                        source_id,
                        on_generation,
                        move || async move {
                            info!("Authenticating with Jellyfin...");
                            let client =
                                crate::jellyfin::client::JellyfinClient::authenticate_with_route(
                                    &server_url,
                                    &user,
                                    &pass,
                                    advertised_route,
                                )
                                .await?;
                            Ok(crate::jellyfin::JellyfinBackend::stage_authenticated(
                                &server_name,
                                client,
                            ))
                        },
                    ),
                    "plex" => source_registry_for_auth.connect_standard(
                        source_id,
                        on_generation,
                        move || async move {
                            info!("Authenticating with Plex...");
                            let client = crate::plex::client::PlexClient::authenticate_with_route(
                                &server_url,
                                &user,
                                &pass,
                                advertised_route,
                            )
                            .await?;
                            crate::plex::PlexBackend::from_client(&server_name, client).await
                        },
                    ),
                    "daap" => source_registry_for_auth.connect_daap(
                        source_id,
                        on_generation,
                        move || async move {
                            info!("Connecting to DAAP server...");
                            let password = (!pass.is_empty()).then_some(pass.as_str());
                            crate::daap::DaapBackend::login_with_route(
                                &server_name,
                                &server_url,
                                password,
                                advertised_route,
                            )
                            .await
                        },
                    ),
                    _ => source_registry_for_auth.connect_standard(
                        source_id,
                        on_generation,
                        move || async move {
                            info!("Authenticating with Subsonic...");
                            crate::subsonic::SubsonicBackend::connect_with_route(
                                &server_name,
                                &server_url,
                                &user,
                                &pass,
                                advertised_route,
                            )
                            .await
                        },
                    ),
                };
                if generation.is_none() {
                    *pending_for_auth.borrow_mut() = None;
                    tracing::debug!(
                        backend = %backend_type,
                        "Skipping remote connect during shutdown or after source retirement"
                    );
                }
            },
            on_cancel,
        );
    });
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::rc::Rc;

    use url::Url;

    use super::{
        cache_source_completion, evict_source_completion,
        playlist_unavailable_reason_text_for_locale, prepare_remote_auth_submission,
        remote_failure_category, resolve_source_key, retained_removable_connect_failure,
        with_active_source_key_snapshot, PendingConnection, PlaylistRowUnavailableReason,
        RemoteFailureCategory, SourceNavigation, TrackObject,
    };
    use crate::architecture::error::BackendError;
    use crate::architecture::AdvertisedHttpRoute;

    fn projected_track(label: &str) -> TrackObject {
        TrackObject::new(
            1,
            label,
            60,
            "Fixture Artist",
            "Fixture Album",
            "Fixture Genre",
            "",
            2026,
            "2026-07-17",
            320,
            48_000,
            0,
            "flac",
            &format!("file:///fixture/{label}.flac"),
        )
    }

    #[test]
    fn stable_removable_identity_and_explicit_view_keys_precede_legacy_fallbacks() {
        assert_eq!(
            resolve_source_key(
                "device:uuid:123",
                "stable-id",
                "file:///legacy",
                "usb-device"
            ),
            "stable-id"
        );
        assert_eq!(
            resolve_source_key("", "stable-id", "https://music.example.test", "subsonic"),
            "stable-id"
        );
        assert_eq!(
            resolve_source_key("", "", "", "radio-topvote"),
            "radio-topvote"
        );
        assert_eq!(resolve_source_key("", "stable-id", "", "local"), "local");
        assert_eq!(resolve_source_key("", "", "", ""), "local");
    }

    #[test]
    fn connecting_and_auth_cancel_selection_release_active_key_before_reentry() {
        let active_source_key = Rc::new(RefCell::new("local".to_string()));

        // Both the connecting-row guard and auth-cancel callback use this
        // boundary immediately before a synchronous selection change. Model
        // the re-entered Local handler by mutably borrowing the same cell.
        for replacement in ["local-after-connecting", "local-after-cancel"] {
            with_active_source_key_snapshot(&active_source_key, |snapshot| {
                assert!(snapshot.starts_with("local"));
                *active_source_key.borrow_mut() = replacement.to_string();
            });
        }

        assert_eq!(&*active_source_key.borrow(), "local-after-cancel");
    }

    #[test]
    fn production_completion_boundary_rejects_reversed_source_results() {
        let navigation = Rc::new(RefCell::new(SourceNavigation::new("local")));
        let source_tracks = Rc::new(RefCell::new(HashMap::new()));
        let active_source_key = Rc::new(RefCell::new("local".to_string()));

        let stale = navigation.borrow_mut().select("playlist:fixture");
        navigation.borrow_mut().select("local");
        let current = navigation.borrow_mut().select("playlist:fixture");
        *active_source_key.borrow_mut() = "playlist:fixture".to_string();

        assert!(cache_source_completion(
            &navigation,
            &current,
            &source_tracks,
            &[projected_track("current")],
            &active_source_key,
        ));
        assert!(!cache_source_completion(
            &navigation,
            &stale,
            &source_tracks,
            &[projected_track("stale")],
            &active_source_key,
        ));
        assert!(
            !evict_source_completion(&navigation, &stale, &source_tracks, &active_source_key,),
            "a stale missing-result callback must not evict the newer projection"
        );
        assert_eq!(
            source_tracks.borrow()["playlist:fixture"][0].title(),
            "current"
        );

        let background = navigation.borrow_mut().select("radio-topvote");
        navigation.borrow_mut().select("local");
        *active_source_key.borrow_mut() = "local".to_string();
        assert!(!cache_source_completion(
            &navigation,
            &background,
            &source_tracks,
            &[projected_track("background")],
            &active_source_key,
        ));
        assert_eq!(
            source_tracks.borrow()["radio-topvote"][0].title(),
            "background"
        );
    }

    #[tokio::test]
    async fn removable_selection_replays_a_retained_background_scan_failure() {
        let registry =
            crate::source_registry::SourceRegistry::new(tokio::runtime::Handle::current());
        let source_id = crate::architecture::SourceId::removable("test:retained-scan-failure")
            .expect("removable source identity");
        let claim = registry
            .claim_provenance(
                source_id,
                crate::source_lifecycle::SourceProvenance::Removable,
            )
            .expect("claim removable provenance");
        let missing_mount = std::env::temp_dir().join(format!(
            "tributary-missing-removable-{}",
            uuid::Uuid::new_v4()
        ));
        registry
            .connect_removable(source_id, missing_mount, |_| {})
            .expect("admit failing removable scan");

        let category = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(category) = retained_removable_connect_failure(&registry, source_id) {
                    break category;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("removable failure becomes visible");
        assert_eq!(category, RemoteFailureCategory::Backend);
        let message = category.user_message("Removable media");
        assert!(message.contains("Removable media"));
        assert!(!message.contains("tributary-missing-removable"));
        assert_eq!(
            retained_removable_connect_failure(&registry, source_id),
            Some(RemoteFailureCategory::Backend),
            "reading the UI status must not consume the retained lifecycle failure"
        );

        assert!(registry.release_provenance(source_id, claim));
        registry.shutdown().wait().await;
    }

    fn advertised_route(address: &str) -> AdvertisedHttpRoute {
        AdvertisedHttpRoute::new(
            &Url::parse("http://mini.local:4533").expect("route origin"),
            [address.parse::<SocketAddr>().expect("route address")],
        )
        .expect("advertised route")
    }

    #[test]
    fn auth_submission_snapshots_the_current_discovery_route() {
        let store = gtk::gio::ListStore::new::<super::SourceObject>();
        let source = super::SourceObject::manual(
            "Subsonic",
            "subsonic",
            "http://mini.local:4533",
            crate::architecture::SourceId::random(),
        );
        source.set_advertised_route(Some(advertised_route("127.0.0.1:1")));
        store.append(&source);

        let mut navigation = SourceNavigation::new("local");
        let request = navigation.select("http://mini.local:4533");
        let pending = RefCell::new(Some(PendingConnection::new(
            "http://mini.local:4533",
            request.clone(),
        )));

        let refreshed_route = advertised_route("127.0.0.2:2");
        source.set_advertised_route(Some(refreshed_route.clone()));
        let (_, admitted) = prepare_remote_auth_submission(
            &pending,
            &navigation,
            &request,
            &store,
            "HTTP://MINI.LOCAL:4533/",
            "subsonic",
        )
        .expect("current submission");

        assert_eq!(admitted.advertised_route(), Some(refreshed_route));
        assert!(pending.borrow().is_some());
    }

    #[test]
    fn stale_or_withdrawn_auth_submission_releases_only_its_own_guard() {
        let store = gtk::gio::ListStore::new::<super::SourceObject>();
        let mut navigation = SourceNavigation::new("local");
        let stale = navigation.select("http://mini.local:4533");
        let pending = RefCell::new(Some(PendingConnection::new(
            "http://mini.local:4533",
            stale.clone(),
        )));
        navigation.select("local");

        assert!(prepare_remote_auth_submission(
            &pending,
            &navigation,
            &stale,
            &store,
            "http://mini.local:4533",
            "subsonic",
        )
        .is_none());
        assert!(pending.borrow().is_none());

        let old = navigation.select("http://mini.local:4533");
        let newer = navigation.select("http://mini.local:4533");
        *pending.borrow_mut() = Some(PendingConnection::new(
            "http://mini.local:4533",
            newer.clone(),
        ));
        assert!(prepare_remote_auth_submission(
            &pending,
            &navigation,
            &old,
            &store,
            "http://mini.local:4533",
            "subsonic",
        )
        .is_none());
        assert_eq!(
            pending.borrow().as_ref().map(PendingConnection::request),
            Some(&newer)
        );

        assert!(prepare_remote_auth_submission(
            &pending,
            &navigation,
            &newer,
            &store,
            "http://mini.local:4533",
            "subsonic",
        )
        .is_none());
        assert!(pending.borrow().is_none());
    }

    #[test]
    fn remote_failures_have_typed_secret_free_categories_and_messages() {
        const SECRET: &str = "server-supplied-secret";
        let connection = BackendError::ConnectionFailed {
            message: SECRET.to_string(),
            source: None,
        };
        let cases = [
            (connection, RemoteFailureCategory::Connection),
            (
                BackendError::Timeout { duration_secs: 10 },
                RemoteFailureCategory::Timeout,
            ),
            (
                BackendError::AuthenticationFailed {
                    message: SECRET.to_string(),
                },
                RemoteFailureCategory::Authentication,
            ),
            (
                BackendError::ParseError {
                    message: SECRET.to_string(),
                    source: None,
                },
                RemoteFailureCategory::Response,
            ),
        ];
        for (error, expected) in cases {
            let category = remote_failure_category(&error);
            assert_eq!(category, expected);
            assert!(!category.as_str().contains(SECRET));
            assert!(!category.log_message().contains(SECRET));
            assert!(!category.user_message("Subsonic").contains(SECRET));
        }
    }

    #[test]
    fn remote_failure_messages_are_localized_for_every_catalog() {
        const BACKEND: &str = "TestBackend";
        let categories = [
            RemoteFailureCategory::Authentication,
            RemoteFailureCategory::Connection,
            RemoteFailureCategory::Timeout,
            RemoteFailureCategory::Response,
            RemoteFailureCategory::AuthenticationMethod,
            RemoteFailureCategory::Backend,
        ];

        for category in categories {
            let english = category.user_message_for_locale(BACKEND, "en");
            assert!(english.contains(BACKEND));
            assert!(!english.contains("%{backend}"));

            for locale in rust_i18n::available_locales!() {
                let localized = category.user_message_for_locale(BACKEND, &locale);
                assert!(
                    localized.contains(BACKEND),
                    "{locale} must interpolate the backend name for {category:?}"
                );
                assert!(!localized.contains("%{backend}"));
                if locale != "en" {
                    assert_ne!(
                        localized, english,
                        "{locale} must not fall back to English for {category:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn every_playlist_unavailable_state_has_non_fallback_localized_copy() {
        let reasons = [
            PlaylistRowUnavailableReason::LocalTrackMissing,
            PlaylistRowUnavailableReason::LocalTrackUnmatched,
            PlaylistRowUnavailableReason::SourceUnavailable,
            PlaylistRowUnavailableReason::UnsupportedSource,
            PlaylistRowUnavailableReason::InvalidCatalogue,
            PlaylistRowUnavailableReason::TrackMissing,
        ];
        let english_title =
            rust_i18n::t!("regular_playlist.unavailable_track", locale = "en").into_owned();
        let english_reasons =
            reasons.map(|reason| playlist_unavailable_reason_text_for_locale(reason, "en"));

        for locale in rust_i18n::available_locales!() {
            let title =
                rust_i18n::t!("regular_playlist.unavailable_track", locale = locale).into_owned();
            assert!(!title.is_empty(), "{locale}: empty unavailable title");
            for (reason, english) in reasons.into_iter().zip(english_reasons.iter()) {
                let localized = playlist_unavailable_reason_text_for_locale(reason, &locale);
                assert!(!localized.is_empty(), "{locale}: empty {reason:?} copy");
                if locale != "en" {
                    assert_ne!(
                        localized,
                        english.as_str(),
                        "{locale}: fallback for {reason:?}"
                    );
                }
            }
            if locale != "en" {
                assert_ne!(title, english_title, "{locale}: unavailable title fallback");
            }
        }
    }
}
