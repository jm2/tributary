//! Pointer and keyboard context menu on the tracklist `ColumnView`.
//!
//! Handles "Remove from Playlist", "Add to Playlist", and "Properties…"
//! actions triggered from right-clicking selected tracks or pressing the
//! platform context-menu key / Shift+F10.

use adw::prelude::*;
use gtk::gio::prelude::ActionExt;
use gtk::glib;
use std::rc::Rc;

use super::objects::{SourceObject, TrackObject};
use super::window_state::WindowState;
use crate::architecture::{MediaKey, SourceId, TrackId};
use crate::local::playlist_manager::{PlaylistEntryAddOutcome, PlaylistEntryInput};
use crate::source_registry::{RegularPlaylistTrackResolution, SourceRegistry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextMenuControllerPlan {
    EventControllerKeyBubble,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContextMenuInteractionPlan {
    keyboard_controller: ContextMenuControllerPlan,
    has_popup: bool,
    accessible_key_shortcuts: &'static str,
}

const CONTEXT_MENU_INTERACTION: ContextMenuInteractionPlan = ContextMenuInteractionPlan {
    keyboard_controller: ContextMenuControllerPlan::EventControllerKeyBubble,
    has_popup: true,
    accessible_key_shortcuts: "Shift+F10 ContextMenu",
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectionSnapshot {
    positions: Vec<u32>,
}

impl SelectionSnapshot {
    fn from_positions(positions: impl IntoIterator<Item = u32>) -> Option<Self> {
        let positions = positions.into_iter().collect::<Vec<_>>();
        (!positions.is_empty()).then_some(Self { positions })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextMenuPopupPlan {
    selection: SelectionSnapshot,
}

impl ContextMenuPopupPlan {
    fn from_positions(positions: impl IntoIterator<Item = u32>) -> Option<Self> {
        Some(Self {
            selection: SelectionSnapshot::from_positions(positions)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PlaylistAddCandidate {
    Local(MediaKey),
    Remote {
        media_key: MediaKey,
        session_epoch: u64,
        catalogue_generation: u64,
    },
}

impl PlaylistAddCandidate {
    #[cfg(test)]
    fn media_key(&self) -> &MediaKey {
        match self {
            Self::Local(media_key) | Self::Remote { media_key, .. } => media_key,
        }
    }
}

struct PlaylistAddPlan {
    inputs: Vec<PlaylistEntryInput>,
    authority: Vec<RegularPlaylistTrackResolution>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlaylistMutationOutcome {
    Committed,
    Rejected,
    Failed,
}

#[derive(Clone)]
struct PlaylistMutationContext {
    window: gtk::glib::WeakRef<adw::ApplicationWindow>,
    rt_handle: tokio::runtime::Handle,
    source_registry: SourceRegistry,
    sidebar_store: gtk::gio::ListStore,
    track_store: gtk::gio::ListStore,
    master_tracks: std::rc::Rc<std::cell::RefCell<Vec<TrackObject>>>,
    source_tracks:
        std::rc::Rc<std::cell::RefCell<std::collections::HashMap<String, Vec<TrackObject>>>>,
    active_source_key: std::rc::Rc<std::cell::RefCell<String>>,
    source_navigation: std::rc::Rc<std::cell::RefCell<super::source_navigation::SourceNavigation>>,
    browser_widget: gtk::Box,
    browser_state: super::browser::BrowserState,
    status_label: gtk::Label,
    column_view: gtk::ColumnView,
}

impl PlaylistMutationContext {
    fn from_window(state: &WindowState) -> Self {
        Self {
            window: state.window.downgrade(),
            rt_handle: state.rt_handle.clone(),
            source_registry: state.source_registry.clone(),
            sidebar_store: state.sidebar_store.clone(),
            track_store: state.track_store.clone(),
            master_tracks: state.master_tracks.clone(),
            source_tracks: state.source_tracks.clone(),
            active_source_key: state.active_source_key.clone(),
            source_navigation: state.source_navigation.clone(),
            browser_widget: state.browser_widget.clone(),
            browser_state: state.browser_state.clone(),
            status_label: state.status_label.clone(),
            column_view: state.column_view.clone(),
        }
    }

    fn owns_navigation(&self, source_key: &str) -> bool {
        *self.active_source_key.borrow() == source_key
            && self.source_navigation.borrow().is_key(source_key)
    }

    fn current_request(&self, source_key: &str) -> Option<super::source_navigation::SourceRequest> {
        let navigation = self.source_navigation.borrow();
        navigation
            .latest_request(source_key)
            .filter(|request| navigation.is_current(request))
    }

    fn owns_request(&self, request: &super::source_navigation::SourceRequest) -> bool {
        *self.active_source_key.borrow() == request.source_key()
            && self.source_navigation.borrow().is_current(request)
    }

    fn show_unsupported(&self) {
        if let Some(window) = self.window.upgrade() {
            show_unsupported_playlist_add_dialog(&window);
        }
    }

    fn show_mutation_failed(&self) {
        if let Some(window) = self.window.upgrade() {
            show_playlist_mutation_failed_dialog(&window);
        }
    }

    fn refresh_playlist_after_commit(&self, playlist_id: &str) {
        let source_key = format!("{}{playlist_id}", super::playback::PLAYLIST_SOURCE_PREFIX);
        self.source_navigation
            .borrow_mut()
            .invalidate_key(&source_key);
        self.source_tracks.borrow_mut().remove(&source_key);
        if !self.owns_navigation(&source_key) {
            return;
        }

        let request = self
            .source_navigation
            .borrow_mut()
            .select(source_key.clone());
        // A committed removal must not leave the now-invalid occurrence
        // actionable while the authoritative replacement projection loads.
        // Add uses the same path so a playlist opened during the write cannot
        // expose a stale pre-commit snapshot either.
        super::window::display_tracks(
            &[],
            &self.track_store,
            &self.master_tracks,
            &self.browser_widget,
            &self.browser_state,
            &self.status_label,
            &self.column_view,
        );
        super::source_connect::load_playlist_source(
            self.rt_handle.clone(),
            self.source_registry.clone(),
            self.sidebar_store.clone(),
            playlist_id.to_string(),
            request,
            self.source_navigation.clone(),
            self.source_tracks.clone(),
            self.active_source_key.clone(),
            self.track_store.clone(),
            self.master_tracks.clone(),
            self.browser_widget.clone(),
            self.browser_state.clone(),
            self.status_label.clone(),
            self.column_view.clone(),
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UnsupportedPlaylistAddCopy {
    heading: String,
    body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlaylistMutationFailedCopy {
    heading: String,
    body: String,
}

fn unsupported_playlist_add_copy(locale: &str) -> UnsupportedPlaylistAddCopy {
    UnsupportedPlaylistAddCopy {
        heading: rust_i18n::t!("context.playlist_add_unsupported_heading", locale = locale)
            .into_owned(),
        body: rust_i18n::t!("context.playlist_add_unsupported_body", locale = locale).into_owned(),
    }
}

fn show_unsupported_playlist_add_dialog(window: &adw::ApplicationWindow) {
    let copy = unsupported_playlist_add_copy(&rust_i18n::locale());
    let dialog = adw::AlertDialog::builder()
        .heading(&copy.heading)
        .body(&copy.body)
        .build();
    dialog.add_response("ok", rust_i18n::t!("dialogs.ok").as_ref());
    dialog.present(Some(window));
}

fn playlist_mutation_failed_copy(locale: &str) -> PlaylistMutationFailedCopy {
    PlaylistMutationFailedCopy {
        heading: rust_i18n::t!("regular_playlist.mutation_failed_heading", locale = locale)
            .into_owned(),
        body: rust_i18n::t!("regular_playlist.mutation_failed_body", locale = locale).into_owned(),
    }
}

fn show_playlist_mutation_failed_dialog(window: &adw::ApplicationWindow) {
    let copy = playlist_mutation_failed_copy(&rust_i18n::locale());
    let dialog = adw::AlertDialog::builder()
        .heading(&copy.heading)
        .body(&copy.body)
        .build();
    dialog.add_response("ok", rust_i18n::t!("dialogs.ok").as_ref());
    dialog.present(Some(window));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyboardContextMenuPropagation {
    Proceed,
    Stop,
}

impl KeyboardContextMenuPropagation {
    fn into_gtk(self) -> gtk::glib::Propagation {
        match self {
            Self::Proceed => gtk::glib::Propagation::Proceed,
            Self::Stop => gtk::glib::Propagation::Stop,
        }
    }
}

fn keyboard_context_menu_propagation(
    is_trigger: bool,
    popup_opened: bool,
) -> KeyboardContextMenuPropagation {
    if is_trigger && popup_opened {
        KeyboardContextMenuPropagation::Stop
    } else {
        KeyboardContextMenuPropagation::Proceed
    }
}

fn is_keyboard_context_menu_trigger(key: gtk::gdk::Key, modifiers: gtk::gdk::ModifierType) -> bool {
    use gtk::gdk::ModifierType;

    // Lock/legacy modifier state (for example NumLock's X11 Mod2 bit) is
    // ambient, not a chord. Keep only modifiers that participate in shortcuts,
    // then accept the exact standard bindings: unmodified Menu or Shift+F10.
    // In particular, Shift+Menu remains available to an ancestor binding.
    let effective = modifiers
        & (ModifierType::SHIFT_MASK
            | ModifierType::CONTROL_MASK
            | ModifierType::ALT_MASK
            | ModifierType::SUPER_MASK);
    (key == gtk::gdk::Key::Menu && effective.is_empty())
        || (key == gtk::gdk::Key::F10 && effective == ModifierType::SHIFT_MASK)
}

fn expose_context_menu_accessibility(column_view: &gtk::ColumnView) {
    column_view.update_property(&[
        gtk::accessible::Property::HasPopup(CONTEXT_MENU_INTERACTION.has_popup),
        gtk::accessible::Property::KeyShortcuts(CONTEXT_MENU_INTERACTION.accessible_key_shortcuts),
    ]);
}

/// Wire pointer and keyboard context-menu access on the tracklist.
///
/// Right-click retains its exact pointer anchor. The Menu key and Shift+F10
/// open the same selection-snapshotted action model relative to the focused
/// tracklist, and are consumed only when a non-empty menu was opened.
pub fn setup_context_menu(state: &WindowState) {
    let sm = state.sort_model.clone();
    let sidebar_store = state.sidebar_store.clone();
    let active_source_key = state.active_source_key.clone();
    let mutation_context = PlaylistMutationContext::from_window(state);

    let popup_menu = Rc::new(
        move |cv: &gtk::ColumnView, anchor: Option<gtk::gdk::Rectangle>| {
            let active_key = active_source_key.borrow().clone();
            let is_playlist_view = active_key.starts_with("playlist:");

            // Freeze exact selected row identities while constructing this
            // one-shot popover. No later mutation consults a URI, source label,
            // or whatever rows happen to occupy these GTK positions.
            let selection_model = cv.model();
            let Some(sel) = selection_model.and_then(|m| m.downcast::<gtk::MultiSelection>().ok())
            else {
                return false;
            };

            let selected = sel.selection();
            let Some(popup_plan) = ContextMenuPopupPlan::from_positions(
                (0..sm.n_items()).filter(|position| selected.contains(*position)),
            ) else {
                return false;
            };

            let menu = gtk::gio::Menu::new();
            let action_group = gtk::gio::SimpleActionGroup::new();
            let interaction_request = mutation_context.current_request(&active_key);

            if is_playlist_view {
                build_remove_from_playlist_action(
                    &menu,
                    &action_group,
                    &active_key,
                    &sm,
                    &popup_plan.selection,
                    interaction_request.as_ref(),
                    &mutation_context,
                );
            } else {
                build_add_to_playlist_actions(
                    &menu,
                    &action_group,
                    &sidebar_store,
                    &sm,
                    &popup_plan.selection,
                    interaction_request.as_ref(),
                    &mutation_context,
                );
            }

            // ── Properties… ──────────────────────────────────────────
            let automatic_device = active_source_is_automatic_device(&sidebar_store, &active_key);
            build_properties_action(
                &menu,
                &action_group,
                cv,
                &sm,
                &popup_plan.selection,
                automatic_device,
            );

            if menu.n_items() == 0 {
                return false;
            }

            let popover = popover_from_menu_model(cv, &menu, &action_group);
            if let Some(anchor) = anchor {
                popover.set_pointing_to(Some(&anchor));
            }
            popover.popup();
            true
        },
    );

    let gesture = gtk::GestureClick::new();
    gesture.set_button(3); // right-click
    {
        let popup_menu = Rc::clone(&popup_menu);
        gesture.connect_pressed(move |gesture, _n_press, x, y| {
            let Some(cv) = gesture
                .widget()
                .and_then(|widget| widget.downcast::<gtk::ColumnView>().ok())
            else {
                return;
            };
            let anchor = gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1);
            popup_menu(&cv, Some(anchor));
        });
    }
    state.column_view.add_controller(gesture);

    let key_controller = match CONTEXT_MENU_INTERACTION.keyboard_controller {
        ContextMenuControllerPlan::EventControllerKeyBubble => {
            let controller = gtk::EventControllerKey::new();
            controller.set_propagation_phase(gtk::PropagationPhase::Bubble);
            controller
        }
    };
    key_controller.connect_key_pressed(move |controller, key, _keycode, modifiers| {
        let is_trigger = is_keyboard_context_menu_trigger(key, modifiers);
        if !is_trigger {
            return keyboard_context_menu_propagation(false, false).into_gtk();
        }
        let Some(cv) = controller
            .widget()
            .and_then(|widget| widget.downcast::<gtk::ColumnView>().ok())
        else {
            return keyboard_context_menu_propagation(true, false).into_gtk();
        };

        keyboard_context_menu_propagation(true, popup_menu(&cv, None)).into_gtk()
    });
    state.column_view.add_controller(key_controller);
    expose_context_menu_accessibility(&state.column_view);
}

// ═══════════════════════════════════════════════════════════════════════
// Action builders
// ═══════════════════════════════════════════════════════════════════════

/// Build the "Remove from Playlist" action for playlist views.
fn build_remove_from_playlist_action(
    menu: &gtk::gio::Menu,
    action_group: &gtk::gio::SimpleActionGroup,
    active_key: &str,
    sm: &gtk::SortListModel,
    selection: &SelectionSnapshot,
    interaction_request: Option<&super::source_navigation::SourceRequest>,
    context: &PlaylistMutationContext,
) {
    let Some(playlist_id) = active_key
        .strip_prefix(super::playback::PLAYLIST_SOURCE_PREFIX)
        .filter(|playlist_id| !playlist_id.is_empty())
        .map(str::to_string)
    else {
        return;
    };
    if !playlist_is_editable_regular(&context.sidebar_store, &playlist_id) {
        return;
    }
    let Some(entry_ids) = collect_selected_playlist_entry_ids(sm, selection) else {
        // Smart-playlist and malformed rows do not carry durable occurrence
        // bindings. Hiding the action avoids pretending a live query can be
        // mutated like a regular playlist.
        return;
    };

    let remove_action = gtk::gio::SimpleAction::new("remove-from-playlist", None);
    let interaction_request = interaction_request.cloned();
    let context = context.clone();
    remove_action.connect_activate(move |_, _| {
        let Some(request) = interaction_request.as_ref() else {
            return;
        };
        if !context.owns_request(request) {
            return;
        }
        if !playlist_is_editable_regular(&context.sidebar_store, &playlist_id) {
            context.show_mutation_failed();
            return;
        }

        let pid = playlist_id.clone();
        let ids = entry_ids.clone();
        let removed_count = ids.len();
        let (result_tx, result_rx) = async_channel::bounded(1);
        context.rt_handle.spawn(async move {
            let outcome = match crate::db::connection::init_db().await {
                Ok(db) => {
                    let manager = crate::local::playlist_manager::PlaylistManager::new(db);
                    match manager.remove_entries(&pid, &ids).await {
                        Ok(()) => PlaylistMutationOutcome::Committed,
                        Err(error) => {
                            tracing::error!(%error, playlist = %pid, "Failed to remove exact playlist occurrences");
                            PlaylistMutationOutcome::Failed
                        }
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "Failed to open DB for playlist removal");
                    PlaylistMutationOutcome::Failed
                }
            };
            let _ = result_tx.send(outcome).await;
        });

        let context = context.clone();
        let playlist_id = playlist_id.clone();
        gtk::glib::MainContext::default().spawn_local(async move {
            match result_rx.recv().await {
                Ok(PlaylistMutationOutcome::Committed) => {
                    tracing::info!(playlist = %playlist_id, count = removed_count, "Playlist occurrences removed");
                    context.refresh_playlist_after_commit(&playlist_id);
                }
                Ok(PlaylistMutationOutcome::Rejected | PlaylistMutationOutcome::Failed) | Err(_) => {
                    context.show_mutation_failed();
                }
            }
        });
    });
    action_group.add_action(&remove_action);
    menu.append(
        Some(rust_i18n::t!("context.remove_from_playlist").as_ref()),
        Some("tracklist-ctx.remove-from-playlist"),
    );
}

/// Build "Add to Playlist" actions (flat list with disabled header).
fn build_add_to_playlist_actions(
    menu: &gtk::gio::Menu,
    action_group: &gtk::gio::SimpleActionGroup,
    sidebar_store: &gtk::gio::ListStore,
    sm: &gtk::SortListModel,
    selection: &SelectionSnapshot,
    interaction_request: Option<&super::source_navigation::SourceRequest>,
    context: &PlaylistMutationContext,
) {
    let mut has_playlists = false;
    let candidates = collect_selected_add_candidates(sm, selection);

    // Find all regular playlists from the sidebar store.
    let n = sidebar_store.n_items();
    for i in 0..n {
        if let Some(src) = sidebar_store.item(i).and_downcast_ref::<SourceObject>() {
            if src.is_editable_regular_playlist() {
                // Add the "Add to Playlist" header on first playlist found.
                if !has_playlists {
                    has_playlists = true;
                    // Disabled action renders as an unclickable label header.
                    let header_action = gtk::gio::SimpleAction::new("add-to-playlist-header", None);
                    header_action.set_enabled(false);
                    action_group.add_action(&header_action);
                    menu.append(
                        Some(rust_i18n::t!("context.add_to_playlist").as_ref()),
                        Some("tracklist-ctx.add-to-playlist-header"),
                    );
                }

                let pl_name = src.name();
                let pl_id = src.playlist_id();
                let action_name = format!("add-to-{}", pl_id.replace('-', "_"));
                let add_action = gtk::gio::SimpleAction::new(&action_name, None);
                let pid = pl_id.clone();
                let interaction_request = interaction_request.cloned();
                let candidates = candidates.clone();
                let context = context.clone();
                let sidebar_store = sidebar_store.clone();
                add_action.connect_activate(move |_, _| {
                    let Some(request) = interaction_request.as_ref() else {
                        context.show_unsupported();
                        return;
                    };
                    if !context.owns_request(request) {
                        context.show_unsupported();
                        return;
                    }
                    if !playlist_is_editable_regular(&sidebar_store, &pid) {
                        context.show_unsupported();
                        return;
                    }

                    let Some(candidates) = candidates.clone() else {
                        context.show_unsupported();
                        return;
                    };
                    let Ok(plan) =
                        prepare_playlist_add_plan(&context.source_registry, &candidates)
                    else {
                        context.show_unsupported();
                        return;
                    };

                    let pid = pid.clone();
                    let worker_pid = pid.clone();
                    let registry = context.source_registry.clone();
                    let (result_tx, result_rx) = async_channel::bounded(1);
                    context.rt_handle.spawn(async move {
                        let outcome = match crate::db::connection::init_db().await {
                            Ok(db) => {
                                let manager =
                                    crate::local::playlist_manager::PlaylistManager::new(db);
                                // Stage the complete ordered mutation first,
                                // then acquire exact live authority at the
                                // transaction's final commit boundary. The
                                // manager retains it through commit; stale
                                // acquisition rejects and rolls back every
                                // staged insert.
                                match manager
                                    .add_entries_if_authorized(
                                        &worker_pid,
                                        &plan.inputs,
                                        || {
                                            registry.acquire_regular_playlist_commit_authority(
                                                &plan.authority,
                                            )
                                        },
                                    )
                                    .await
                                {
                                    Ok(PlaylistEntryAddOutcome::Committed(_)) => {
                                        PlaylistMutationOutcome::Committed
                                    }
                                    Ok(PlaylistEntryAddOutcome::Rejected) => {
                                        PlaylistMutationOutcome::Rejected
                                    }
                                    Err(error) => {
                                        tracing::error!(%error, playlist = %worker_pid, "Failed to add exact playlist occurrences");
                                        PlaylistMutationOutcome::Failed
                                    }
                                }
                            }
                            Err(error) => {
                                tracing::error!(%error, "Failed to open DB for playlist add");
                                PlaylistMutationOutcome::Failed
                            }
                        };
                        let _ = result_tx.send(outcome).await;
                    });

                    let context = context.clone();
                    let playlist_id = pid.clone();
                    let count = candidates.len();
                    gtk::glib::MainContext::default().spawn_local(async move {
                        match result_rx.recv().await {
                            Ok(PlaylistMutationOutcome::Committed) => {
                                tracing::info!(playlist = %playlist_id, count, "Tracks added to playlist");
                                context.refresh_playlist_after_commit(&playlist_id);
                            }
                            Ok(PlaylistMutationOutcome::Rejected) => context.show_unsupported(),
                            Ok(PlaylistMutationOutcome::Failed) | Err(_) => {
                                context.show_mutation_failed();
                            }
                        }
                    });
                });
                action_group.add_action(&add_action);
                menu.append(
                    Some(&format!("  {pl_name}")),
                    Some(&format!("tracklist-ctx.{action_name}")),
                );
            }
        }
    }
}

fn collect_selected_add_candidates(
    sm: &gtk::SortListModel,
    selection: &SelectionSnapshot,
) -> Option<Vec<PlaylistAddCandidate>> {
    selection
        .positions
        .iter()
        .map(|position| {
            let track = sm.item(*position)?.downcast::<TrackObject>().ok()?;
            playlist_add_candidate(&track)
        })
        .collect()
}

fn playlist_add_candidate(track: &TrackObject) -> Option<PlaylistAddCandidate> {
    let source_id = track.source_id()?;
    let track_id = if source_id == SourceId::local() {
        TrackId::new(track.track_id()).ok()?
    } else {
        TrackId::remote(track.track_id()).ok()?
    };
    let media_key = MediaKey::new(source_id, track_id);
    if source_id == SourceId::local() {
        Some(PlaylistAddCandidate::Local(media_key))
    } else {
        Some(PlaylistAddCandidate::Remote {
            media_key,
            session_epoch: track.source_session_epoch()?,
            catalogue_generation: track.source_catalogue_generation()?,
        })
    }
}

fn prepare_playlist_add_plan(
    registry: &SourceRegistry,
    candidates: &[PlaylistAddCandidate],
) -> Result<PlaylistAddPlan, ()> {
    if candidates.is_empty() {
        return Err(());
    }
    let remote_keys = candidates
        .iter()
        .filter_map(|candidate| match candidate {
            PlaylistAddCandidate::Local(_) => None,
            PlaylistAddCandidate::Remote { media_key, .. } => Some(media_key.clone()),
        })
        .collect::<Vec<_>>();
    let authority = registry.resolve_regular_playlist_tracks(&remote_keys);
    prepare_playlist_add_plan_from_authority(candidates, authority)
}

fn prepare_playlist_add_plan_from_authority(
    candidates: &[PlaylistAddCandidate],
    authority: Vec<RegularPlaylistTrackResolution>,
) -> Result<PlaylistAddPlan, ()> {
    let expected_remote = candidates
        .iter()
        .filter(|candidate| matches!(candidate, PlaylistAddCandidate::Remote { .. }))
        .count();
    if candidates.is_empty() || authority.len() != expected_remote {
        return Err(());
    }

    let mut remote = authority.iter();
    let mut inputs = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        match candidate {
            PlaylistAddCandidate::Local(media_key) => {
                // PlaylistManager resolves and snapshots exact local metadata
                // inside the write transaction. Cached GTK metadata is never
                // treated as persistence authority.
                inputs.push(PlaylistEntryInput::new(media_key.clone(), "", "", "", None));
            }
            PlaylistAddCandidate::Remote {
                media_key,
                session_epoch,
                catalogue_generation,
            } => {
                let Some(RegularPlaylistTrackResolution::Available(track)) = remote.next() else {
                    return Err(());
                };
                let guard = track.guard();
                if track.media_key() != media_key
                    || !catalogue_observation_matches(
                        media_key.source_id,
                        *session_epoch,
                        *catalogue_generation,
                        guard.source_id(),
                        guard.session_epoch(),
                        guard.catalogue_generation(),
                    )
                {
                    return Err(());
                }
                let metadata = track.metadata();
                inputs.push(PlaylistEntryInput::new(
                    media_key.clone(),
                    metadata.title(),
                    metadata.artist_name(),
                    metadata.album_title(),
                    metadata.duration_secs(),
                ));
            }
        }
    }
    if remote.next().is_some() {
        return Err(());
    }
    Ok(PlaylistAddPlan { inputs, authority })
}

fn catalogue_observation_matches(
    expected_source: SourceId,
    expected_epoch: u64,
    expected_generation: u64,
    actual_source: SourceId,
    actual_epoch: u64,
    actual_generation: u64,
) -> bool {
    expected_source == actual_source
        && expected_epoch == actual_epoch
        && expected_generation == actual_generation
}

fn collect_selected_playlist_entry_ids(
    sm: &gtk::SortListModel,
    selection: &SelectionSnapshot,
) -> Option<Vec<String>> {
    let bindings = selection.positions.iter().map(|position| {
        sm.item(*position)
            .and_downcast::<TrackObject>()
            .and_then(|track| track.playlist_occurrence_binding())
    });
    exact_playlist_entry_ids(bindings)
}

fn exact_playlist_entry_ids(
    bindings: impl IntoIterator<Item = Option<super::objects::PlaylistOccurrenceBinding>>,
) -> Option<Vec<String>> {
    let mut seen = std::collections::HashSet::new();
    let mut entry_ids = Vec::new();
    for binding in bindings {
        let binding = binding?;
        let entry_id = binding.entry_id().to_string();
        if !seen.insert(entry_id.clone()) {
            return None;
        }
        entry_ids.push(entry_id);
    }
    (!entry_ids.is_empty()).then_some(entry_ids)
}

/// Build the "Properties…" action for selected tracks.
fn build_properties_action(
    menu: &gtk::gio::Menu,
    action_group: &gtk::gio::SimpleActionGroup,
    column_view: &gtk::ColumnView,
    sm: &gtk::SortListModel,
    selection: &SelectionSnapshot,
    automatic_device: bool,
) {
    // Snapshot the exact selection while building the menu. Properties is an
    // all-or-none path-authorized local-file operation: silently dropping a
    // malformed, remote, or pathless lifecycle row would let a batch edit
    // only an unexpected subset. Removable rows deliberately remain absent
    // until a typed mutation target can revalidate their exact live epoch.
    let mut track_infos = Vec::new();
    for &position in &selection.positions {
        let Some(item) = sm.item(position) else {
            return;
        };
        let Some(track) = item.downcast_ref::<TrackObject>() else {
            return;
        };
        let Some(path) = local_file_path(&track.uri()) else {
            return;
        };
        track_infos.push(super::properties_dialog::TrackInfo {
            path,
            title: track.title(),
            artist: track.artist(),
            album: track.album(),
            genre: track.genre(),
            composer: track.composer(),
            year: track.year_display(),
            track_number: if track.track_number() > 0 {
                track.track_number().to_string()
            } else {
                String::new()
            },
            disc_number: if track.disc_number() > 0 {
                track.disc_number().to_string()
            } else {
                String::new()
            },
            format: track.format(),
            bitrate: track.bitrate_display(),
            sample_rate: track.sample_rate_display(),
            duration: track.duration_display(),
        });
    }
    if track_infos.is_empty() {
        return;
    }

    let props_action = gtk::gio::SimpleAction::new("properties", None);
    let win_for_props: Option<adw::ApplicationWindow> = column_view
        .root()
        .and_then(|root| root.downcast::<adw::ApplicationWindow>().ok());
    tracing::debug!(
        has_win = win_for_props.is_some(),
        track_count = track_infos.len(),
        "build_properties_action"
    );

    props_action.connect_activate(move |_, _| {
        let Some(ref win) = win_for_props else {
            tracing::warn!("properties action: win_for_props is None, cannot show dialog");
            return;
        };
        super::properties_dialog::show_properties_dialog(win, &track_infos, automatic_device);
    });

    action_group.add_action(&props_action);
    menu.append(
        Some(rust_i18n::t!("context.properties").as_ref()),
        Some("tracklist-ctx.properties"),
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════

fn playlist_is_editable_regular(sidebar_store: &gtk::gio::ListStore, playlist_id: &str) -> bool {
    (0..sidebar_store.n_items()).any(|position| {
        sidebar_store
            .item(position)
            .and_downcast::<SourceObject>()
            .is_some_and(|source| {
                source.playlist_id() == playlist_id && source.is_editable_regular_playlist()
            })
    })
}

fn local_file_path(uri: &str) -> Option<std::path::PathBuf> {
    let url = url::Url::parse(uri).ok()?;
    (url.scheme() == "file")
        .then(|| url.to_file_path().ok())
        .flatten()
}

/// Match the active lifecycle source against exact sidebar metadata. Opaque
/// logical GIO keys and mount-path spellings are not navigation identities.
fn active_source_is_automatic_device(
    sidebar_store: &gtk::gio::ListStore,
    active_source_key: &str,
) -> bool {
    (0..sidebar_store.n_items()).any(|position| {
        sidebar_store
            .item(position)
            .and_downcast::<SourceObject>()
            .is_some_and(|source| {
                source.backend_type() == "usb-device"
                    && source
                        .source_id()
                        .is_some_and(|source_id| source_id.to_string() == active_source_key)
            })
    })
}

/// Build a `gtk::Popover` from a `gio::Menu` model and action group.
///
/// Each menu item with an enabled action becomes a flat `gtk::Button`;
/// disabled actions render as section-header labels. The popover is
/// parented to `parent` and self-unparents on close.
///
/// Submenus are not supported — items with no action reference are
/// silently skipped.
pub fn popover_from_menu_model(
    parent: &impl IsA<gtk::Widget>,
    menu: &gtk::gio::Menu,
    action_group: &gtk::gio::SimpleActionGroup,
) -> gtk::Popover {
    let popover = gtk::Popover::new();
    popover.set_parent(parent);
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);

    for i in 0..menu.n_items() {
        let Some(action_ref) = menu
            .item_attribute_value(i, "action", Some(glib::VariantTy::STRING))
            .and_then(|v| v.str().map(|s| s.to_string()))
        else {
            continue;
        };
        let Some(label) = menu
            .item_attribute_value(i, "label", Some(glib::VariantTy::STRING))
            .and_then(|v| v.str().map(|s| s.to_string()))
        else {
            continue;
        };

        let bare_name = action_ref
            .rsplit('.')
            .next()
            .unwrap_or(&action_ref)
            .to_string();

        if let Some(action) = action_group.lookup_action(&bare_name) {
            if action.is_enabled() {
                let btn = gtk::Button::builder()
                    .label(&label)
                    .hexpand(true)
                    .css_classes(["flat"])
                    .build();
                let act = action.clone();
                let pop = popover.clone();
                btn.connect_clicked(move |_| {
                    act.activate(None::<&glib::Variant>);
                    pop.popdown();
                });
                vbox.append(&btn);
            } else {
                let lbl = gtk::Label::builder()
                    .label(&label)
                    .halign(gtk::Align::Start)
                    .css_classes(["heading", "dim-label"])
                    .margin_start(8)
                    .margin_top(4)
                    .margin_bottom(2)
                    .build();
                vbox.append(&lbl);
            }
        }
    }

    popover.set_child(Some(&vbox));
    popover.connect_closed(|popover| popover.unparent());
    popover
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn remote_catalogue_track(
        track_id: TrackId,
        title: &str,
    ) -> crate::architecture::models::Track {
        crate::architecture::models::Track {
            id: uuid::Uuid::new_v4(),
            native_track_id: Some(track_id),
            title: title.to_string(),
            artist_name: "Current remote artist".to_string(),
            album_artist_name: None,
            artist_id: None,
            album_title: "Current remote album".to_string(),
            album_id: None,
            track_number: Some(4),
            disc_number: Some(1),
            duration_secs: Some(245),
            composer: None,
            genre: Some("Remote genre".to_string()),
            year: Some(2026),
            file_path: Some("/private/must-not-cross.mp3".to_string()),
            stream_url: Some(
                url::Url::parse("https://secret.invalid/audio?token=private")
                    .expect("fixture stream URL"),
            ),
            cover_art_url: Some(
                url::Url::parse("https://secret.invalid/art?token=private")
                    .expect("fixture artwork URL"),
            ),
            date_added: None,
            date_modified: None,
            bitrate_kbps: Some(320),
            sample_rate_hz: Some(48_000),
            format: Some("mp3".to_string()),
            play_count: Some(12),
            rating: crate::architecture::models::TrackRating::read_only(None),
            last_played: None,
        }
    }

    #[test]
    fn local_add_candidates_use_exact_row_identity_in_selection_order() {
        let first = TrackObject::new(
            1,
            "Cached title must not authorize storage",
            60,
            "Artist",
            "Album",
            "",
            "",
            0,
            "",
            0,
            0,
            0,
            "",
            "file:///private/first.flac",
        );
        first.set_track_id("first-id");
        assert!(first.set_source_id(SourceId::local()));
        let second = TrackObject::new(
            2,
            "Second",
            60,
            "Artist",
            "Album",
            "",
            "",
            0,
            "",
            0,
            0,
            0,
            "",
            "file:///private/second.flac",
        );
        second.set_track_id("second-id");
        assert!(second.set_source_id(SourceId::local()));

        let candidates = [&second, &first]
            .into_iter()
            .map(playlist_add_candidate)
            .collect::<Option<Vec<_>>>()
            .expect("exact local candidates");
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.media_key().track_id.as_str())
                .collect::<Vec<_>>(),
            ["second-id", "first-id"]
        );
        assert!(candidates
            .iter()
            .all(|candidate| matches!(candidate, PlaylistAddCandidate::Local(_))));
    }

    #[test]
    fn mixed_add_plan_consumes_exact_available_remote_authority_in_order() {
        let local_key = MediaKey::new(
            SourceId::local(),
            TrackId::new("local-track").expect("local track ID"),
        );
        let remote_source = SourceId::random();
        let remote_id = TrackId::remote("remote-track").expect("remote track ID");
        let remote_key = MediaKey::new(remote_source, remote_id.clone());
        let remote_track = remote_catalogue_track(remote_id, "Current remote title");
        let available = crate::source_registry::RegularPlaylistTrack::for_ui_test(
            remote_key.clone(),
            7,
            11,
            &remote_track,
        );
        let candidates = vec![
            PlaylistAddCandidate::Local(local_key.clone()),
            PlaylistAddCandidate::Remote {
                media_key: remote_key.clone(),
                session_epoch: 7,
                catalogue_generation: 11,
            },
            PlaylistAddCandidate::Remote {
                media_key: remote_key.clone(),
                session_epoch: 7,
                catalogue_generation: 11,
            },
        ];
        let authority = vec![
            RegularPlaylistTrackResolution::Available(Box::new(available.clone())),
            RegularPlaylistTrackResolution::Available(Box::new(available)),
        ];

        let plan = prepare_playlist_add_plan_from_authority(&candidates, authority)
            .expect("exact current mixed selection");
        assert_eq!(
            plan.inputs
                .iter()
                .map(|input| &input.media_key)
                .collect::<Vec<_>>(),
            [&local_key, &remote_key, &remote_key]
        );
        assert_eq!(plan.inputs[0].title, "");
        for input in &plan.inputs[1..] {
            assert_eq!(input.title, "Current remote title");
            assert_eq!(input.artist, "Current remote artist");
            assert_eq!(input.album, "Current remote album");
            assert_eq!(input.duration_secs, Some(245));
            let rendered = format!("{input:?}");
            assert!(!rendered.contains("secret.invalid"));
            assert!(!rendered.contains("token=private"));
            assert!(!rendered.contains("must-not-cross"));
        }
        assert_eq!(plan.authority.len(), 2);

        let stale = vec![RegularPlaylistTrackResolution::Available(Box::new(
            crate::source_registry::RegularPlaylistTrack::for_ui_test(
                remote_key,
                7,
                12,
                &remote_track,
            ),
        ))];
        assert!(prepare_playlist_add_plan_from_authority(&candidates[..2], stale).is_err());
    }

    #[test]
    fn exact_remove_plan_preserves_duplicate_media_occurrences_but_rejects_duplicate_entry_ids() {
        let track_id = TrackId::new("same-media").expect("track ID");
        let first = super::super::objects::PlaylistOccurrenceBinding::available_local(
            "entry-one",
            track_id.clone(),
        )
        .expect("first occurrence");
        let duplicate = super::super::objects::PlaylistOccurrenceBinding::available_local(
            "entry-two",
            track_id,
        )
        .expect("duplicate occurrence");
        assert_eq!(
            exact_playlist_entry_ids([Some(first.clone()), Some(duplicate)]),
            Some(vec!["entry-one".to_string(), "entry-two".to_string()])
        );
        assert!(exact_playlist_entry_ids([Some(first.clone()), Some(first)]).is_none());
    }

    #[test]
    fn remove_is_hidden_for_smart_or_unbound_rows_but_accepts_unavailable_occurrences() {
        assert!(exact_playlist_entry_ids([None]).is_none());
        let unavailable = super::super::objects::PlaylistOccurrenceBinding::unavailable(
            "missing-entry",
            SourceId::local(),
            Some(TrackId::new("missing-track").expect("track ID")),
            super::super::objects::PlaylistRowUnavailableReason::LocalTrackMissing,
        )
        .expect("unavailable durable occurrence");
        assert_eq!(
            exact_playlist_entry_ids([Some(unavailable)]),
            Some(vec!["missing-entry".to_string()])
        );
    }

    #[test]
    fn add_and_remove_targets_exclude_smart_and_pull_mirror_playlists() {
        use crate::db::entities::server_playlist_link::{
            ServerPlaylistLocalState, ServerPlaylistRemoteState,
        };
        use crate::local::playlist_sidebar::PlaylistSidebarEntry;
        use crate::ui::objects::PlaylistSidebarKind;

        let store = gtk::gio::ListStore::new::<SourceObject>();
        for entry in [
            PlaylistSidebarEntry::new(
                "regular-id",
                "Regular",
                PlaylistSidebarKind::EditableRegular,
            ),
            PlaylistSidebarEntry::new("smart-id", "Smart", PlaylistSidebarKind::EditableSmart),
            PlaylistSidebarEntry::new(
                "mirror-id",
                "Mirror",
                PlaylistSidebarKind::PullMirror {
                    local_state: ServerPlaylistLocalState::Conflict,
                    remote_state: ServerPlaylistRemoteState::Present,
                },
            ),
        ] {
            store.append(&SourceObject::playlist_entry(&entry));
        }

        assert!(playlist_is_editable_regular(&store, "regular-id"));
        assert!(!playlist_is_editable_regular(&store, "smart-id"));
        assert!(!playlist_is_editable_regular(&store, "mirror-id"));
        assert!(!playlist_is_editable_regular(&store, "missing-id"));
    }

    #[test]
    fn catalogue_observation_rejects_stale_source_epoch_or_generation() {
        let source = SourceId::random();
        assert!(catalogue_observation_matches(source, 7, 11, source, 7, 11));
        assert!(!catalogue_observation_matches(
            source,
            7,
            11,
            SourceId::random(),
            7,
            11
        ));
        assert!(!catalogue_observation_matches(source, 7, 11, source, 8, 11));
        assert!(!catalogue_observation_matches(source, 7, 11, source, 7, 12));
    }

    #[test]
    fn unsupported_playlist_add_copy_is_localized_for_every_catalog() {
        let english = unsupported_playlist_add_copy("en");
        assert!(!english.heading.is_empty());
        assert!(!english.body.is_empty());

        for locale in rust_i18n::available_locales!() {
            let localized = unsupported_playlist_add_copy(&locale);
            assert!(!localized.heading.is_empty(), "{locale}: empty heading");
            assert!(!localized.body.is_empty(), "{locale}: empty body");
            if locale != "en" {
                assert_ne!(localized, english, "{locale} must not fall back to English");
            }
        }
    }

    #[test]
    fn playlist_mutation_failure_copy_is_localized_for_every_catalog() {
        let english = playlist_mutation_failed_copy("en");
        assert!(!english.heading.is_empty());
        assert!(!english.body.is_empty());

        for locale in rust_i18n::available_locales!() {
            let localized = playlist_mutation_failed_copy(&locale);
            assert!(!localized.heading.is_empty(), "{locale}: empty heading");
            assert!(!localized.body.is_empty(), "{locale}: empty body");
            if locale != "en" {
                assert_ne!(localized, english, "{locale} must not fall back to English");
            }
        }
    }

    #[test]
    fn properties_path_conversion_is_local_and_fail_closed() {
        let path = std::env::temp_dir().join("tributary properties fixture.flac");
        let uri = url::Url::from_file_path(&path)
            .expect("absolute fixture path")
            .to_string();

        assert_eq!(local_file_path(&uri), Some(path));
        assert_eq!(local_file_path("https://example.test/song.flac"), None);
        assert_eq!(local_file_path("file://%"), None);
        assert_eq!(local_file_path("not a URI"), None);
        assert_eq!(local_file_path(""), None);
    }

    #[test]
    fn automatic_device_context_uses_exact_source_metadata() {
        let store = gtk::gio::ListStore::new::<SourceObject>();
        store.append(&SourceObject::source(
            "Local",
            "local",
            "folder-music-symbolic",
        ));
        store.append(&SourceObject::removable_device(
            "Player",
            "device:opaque-id",
            PathBuf::from("/media/player"),
        ));

        let source_id = crate::architecture::SourceId::removable("device:opaque-id")
            .expect("removable source")
            .to_string();
        assert!(active_source_is_automatic_device(&store, &source_id));
        assert!(!active_source_is_automatic_device(
            &store,
            "device:opaque-id"
        ));
        assert!(!active_source_is_automatic_device(&store, "/media/player"));
        assert!(!active_source_is_automatic_device(&store, "device:opaque"));
        assert!(!active_source_is_automatic_device(&store, "local"));
    }

    #[test]
    fn keyboard_context_menu_plan_pins_wiring_snapshot_and_propagation() {
        use gtk::gdk::{Key, ModifierType};

        assert!(is_keyboard_context_menu_trigger(
            Key::Menu,
            ModifierType::empty()
        ));
        assert!(is_keyboard_context_menu_trigger(
            Key::Menu,
            ModifierType::LOCK_MASK
        ));
        assert!(is_keyboard_context_menu_trigger(
            Key::F10,
            ModifierType::SHIFT_MASK
        ));
        assert!(is_keyboard_context_menu_trigger(
            Key::F10,
            ModifierType::SHIFT_MASK | ModifierType::LOCK_MASK
        ));
        // GDK4 no longer names the legacy X11 Mod2 mask, but key state can
        // still carry its raw bit while NumLock is active.
        let ambient_mod2 = ModifierType::from_bits_retain(1 << 4);
        assert!(is_keyboard_context_menu_trigger(Key::Menu, ambient_mod2));
        assert!(is_keyboard_context_menu_trigger(
            Key::F10,
            ModifierType::SHIFT_MASK | ambient_mod2
        ));

        assert!(!is_keyboard_context_menu_trigger(
            Key::F10,
            ModifierType::empty()
        ));
        assert!(!is_keyboard_context_menu_trigger(
            Key::F10,
            ModifierType::SHIFT_MASK | ModifierType::CONTROL_MASK
        ));
        assert!(!is_keyboard_context_menu_trigger(
            Key::Menu,
            ModifierType::SHIFT_MASK
        ));
        assert!(!is_keyboard_context_menu_trigger(
            Key::Menu,
            ModifierType::SHIFT_MASK | ModifierType::LOCK_MASK
        ));
        assert!(!is_keyboard_context_menu_trigger(
            Key::Menu,
            ModifierType::ALT_MASK
        ));
        assert!(!is_keyboard_context_menu_trigger(
            Key::F9,
            ModifierType::SHIFT_MASK
        ));

        assert_eq!(
            CONTEXT_MENU_INTERACTION,
            ContextMenuInteractionPlan {
                keyboard_controller: ContextMenuControllerPlan::EventControllerKeyBubble,
                has_popup: true,
                accessible_key_shortcuts: "Shift+F10 ContextMenu",
            }
        );

        assert_eq!(
            keyboard_context_menu_propagation(false, false),
            KeyboardContextMenuPropagation::Proceed
        );
        assert_eq!(
            keyboard_context_menu_propagation(true, false),
            KeyboardContextMenuPropagation::Proceed
        );
        assert_eq!(
            keyboard_context_menu_propagation(true, true),
            KeyboardContextMenuPropagation::Stop
        );

        assert!(ContextMenuPopupPlan::from_positions([]).is_none());
        let mut live_selection = vec![1, 3];
        let popup_plan = ContextMenuPopupPlan::from_positions(live_selection.iter().copied())
            .expect("non-empty selection must produce a popup plan");
        live_selection.clear();
        live_selection.push(4);
        assert_eq!(popup_plan.selection.positions, vec![1, 3]);
    }
}
