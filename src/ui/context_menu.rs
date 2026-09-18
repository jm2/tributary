//! Pointer and keyboard context menu on the tracklist `ColumnView`.
//!
//! Handles "Remove from Playlist", "Add to Playlist", and "Properties…"
//! actions triggered from right-clicking selected tracks or pressing the
//! platform context-menu key / Shift+F10.

use adw::prelude::*;
use gtk::gio::prelude::ActionExt;
use gtk::glib;
use std::cell::RefCell;
use std::rc::Rc;

use super::objects::{SourceObject, TrackObject};
use super::properties_dialog::SaveTarget;
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
    // GTK/GDK calls the physical key `Menu`; the GTK accessible shortcut
    // grammar uses the standardized `ContextMenu` token.
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

#[derive(Clone, Debug, Eq, PartialEq, glib::Boxed)]
#[boxed_type(name = "TributaryPlaylistDragPayload")]
struct PlaylistDragPayload {
    candidates: Vec<PlaylistAddCandidate>,
}

impl PlaylistDragPayload {
    fn from_selection(sm: &gtk::SortListModel, selection: &gtk::MultiSelection) -> Option<Self> {
        let selected = selection.selection();
        let snapshot = SelectionSnapshot::from_positions(
            (0..sm.n_items()).filter(|position| selected.contains(*position)),
        )?;
        Some(Self {
            candidates: collect_selected_add_candidates(sm, &snapshot)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlaylistMutationOutcome {
    Committed,
    Rejected,
    Failed,
}

/// What the UI must do after a playlist add attempt resolves.
///
/// Extracted from [`PlaylistMutationContext::add_candidates_to_playlist`]'s
/// result branch so the outcome → toast/refresh dispatch is unit-testable
/// without a live window, while the real handler keeps the single production
/// path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlaylistAddFeedback {
    /// Commit succeeded: show the success toast, then refresh the playlist.
    Added,
    /// The destination refused the write: show the unsupported dialog.
    Unsupported,
    /// The write failed or the worker vanished: show the failure dialog.
    Failed,
}

fn playlist_add_feedback(
    outcome: &Result<PlaylistMutationOutcome, async_channel::RecvError>,
) -> PlaylistAddFeedback {
    match outcome {
        Ok(PlaylistMutationOutcome::Committed) => PlaylistAddFeedback::Added,
        Ok(PlaylistMutationOutcome::Rejected) => PlaylistAddFeedback::Unsupported,
        Ok(PlaylistMutationOutcome::Failed) | Err(_) => PlaylistAddFeedback::Failed,
    }
}

#[derive(Clone)]
struct PlaylistMutationContext {
    window: gtk::glib::WeakRef<adw::ApplicationWindow>,
    toast_overlay: adw::ToastOverlay,
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
            toast_overlay: state.toast_overlay.clone(),
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

    fn show_added(&self, count: usize, playlist_name: &str) {
        let message = playlist_add_success_message(&rust_i18n::locale(), count, playlist_name);
        self.toast_overlay.add_toast(adw::Toast::new(&message));
    }

    fn add_candidates_to_playlist(
        &self,
        playlist_id: String,
        playlist_name: String,
        candidates: Vec<PlaylistAddCandidate>,
    ) {
        if !playlist_is_editable_regular(&self.sidebar_store, &playlist_id) {
            self.show_unsupported();
            return;
        }
        let Ok(plan) = prepare_playlist_add_plan(&self.source_registry, &candidates) else {
            self.show_unsupported();
            return;
        };

        let (result_tx, result_rx) = async_channel::bounded(1);
        spawn_playlist_add_worker(
            &self.rt_handle,
            self.source_registry.clone(),
            playlist_id.clone(),
            plan,
            result_tx,
        );

        let context = self.clone();
        let count = candidates.len();
        gtk::glib::MainContext::default().spawn_local(async move {
            match playlist_add_feedback(&result_rx.recv().await) {
                PlaylistAddFeedback::Added => {
                    context.show_added(count, &playlist_name);
                    context.refresh_playlist_after_commit(&playlist_id);
                }
                PlaylistAddFeedback::Unsupported => context.show_unsupported(),
                PlaylistAddFeedback::Failed => context.show_mutation_failed(),
            }
        });
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

/// CLDR plural category for the whole-number counts the playlist toast carries.
///
/// `rust-i18n` interpolates `count` but does not choose plural forms, so the
/// form is selected here. Only the categories the shipped catalogs use are
/// modelled: every catalog carries `one`/`other`, and Polish and Russian add
/// `few`/`many`. Counts are integers, so the fractional `other` branch of
/// those two languages never applies.
fn plural_category(locale: &str, count: usize) -> &'static str {
    let language: String = locale
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect();
    match language.as_str() {
        "pl" => few_many_category(count, count == 1),
        "ru" => few_many_category(count, count % 10 == 1 && count % 100 != 11),
        _ if count == 1 => "one",
        _ => "other",
    }
}

/// `one`/`few`/`many` split shared by Polish and Russian for whole numbers;
/// only the `one` rule differs, so the caller supplies it.
fn few_many_category(count: usize, one: bool) -> &'static str {
    let tens = count % 10;
    let hundreds = count % 100;
    if one {
        "one"
    } else if (2..=4).contains(&tens) && !(12..=14).contains(&hundreds) {
        "few"
    } else {
        "many"
    }
}

fn playlist_add_success_message(locale: &str, count: usize, playlist_name: &str) -> String {
    let key = format!(
        "context.playlist_add_success.{}",
        plural_category(locale, count)
    );
    // `AdwToast` titles are Pango markup by default. Keep the user-controlled
    // name escaped on the same path that selects and renders the translation.
    rust_i18n::t!(
        key.as_str(),
        locale = locale,
        count = count,
        playlist = gtk::glib::markup_escape_text(playlist_name)
    )
    .into_owned()
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

/// Shared closure type for one-shot context-menu popups. Returns whether a
/// non-empty menu was opened; keyboard consumers decide propagation from it.
type ContextMenuPopupFn = Rc<dyn Fn(&gtk::ColumnView, Option<gtk::gdk::Rectangle>) -> bool>;

/// Maximum natural height of a custom context-menu viewport before its
/// vertical scrollbar takes over.
const CONTEXT_MENU_MAX_CONTENT_HEIGHT: i32 = 480;

/// Wire pointer and keyboard context-menu access on the tracklist.
///
/// Right-click retains its exact pointer anchor. The Menu key and Shift+F10
/// open the same selection-snapshotted action model relative to the focused
/// tracklist, and are consumed only when a non-empty menu was opened.
pub fn setup_context_menu(
    state: &WindowState,
    playlist_row_drop: Rc<RefCell<Option<PlaylistRowDropContext>>>,
) {
    setup_playlist_transfer(state, playlist_row_drop);
    let sm = state.sort_model.clone();
    let sidebar_store = state.sidebar_store.clone();
    let active_source_key = state.active_source_key.clone();
    let mutation_context = PlaylistMutationContext::from_window(state);

    let popup_menu: ContextMenuPopupFn = Rc::new(
        move |cv: &gtk::ColumnView, anchor: Option<gtk::gdk::Rectangle>| {
            let session = ContextMenuPopupSession {
                mutation_context: &mutation_context,
                sidebar_store: &sidebar_store,
                sm: &sm,
                column_view: cv,
            };
            open_context_menu_popup(&session, &active_source_key, anchor)
        },
    );
    attach_pointer_context_menu(state, &popup_menu);
    attach_keyboard_context_menu(state, popup_menu);
    expose_context_menu_accessibility(&state.column_view);
}

/// Immutable view of everything a one-shot context-menu popup renders against.
struct ContextMenuPopupSession<'a> {
    mutation_context: &'a PlaylistMutationContext,
    sidebar_store: &'a gtk::gio::ListStore,
    sm: &'a gtk::SortListModel,
    column_view: &'a gtk::ColumnView,
}

/// Freeze exact selected row identities for a one-shot popup.
///
/// No later mutation consults a URI, source label, or whatever rows happen
/// to occupy these GTK positions.
fn snapshot_popup_plan(session: &ContextMenuPopupSession<'_>) -> Option<ContextMenuPopupPlan> {
    let sel = session
        .column_view
        .model()
        .and_then(|m| m.downcast::<gtk::MultiSelection>().ok())?;
    let selected = sel.selection();
    ContextMenuPopupPlan::from_positions(
        (0..session.sm.n_items()).filter(|position| selected.contains(*position)),
    )
}

/// Populate a fresh menu model with the selection-scoped actions.
fn append_context_menu_actions(
    session: &ContextMenuPopupSession<'_>,
    menu: &gtk::gio::Menu,
    action_group: &gtk::gio::SimpleActionGroup,
    active_key: &str,
    is_playlist_view: bool,
    popup_plan: &ContextMenuPopupPlan,
    interaction_request: Option<&super::source_navigation::SourceRequest>,
) {
    if is_playlist_view {
        build_remove_from_playlist_action(
            menu,
            action_group,
            active_key,
            session.sm,
            &popup_plan.selection,
            interaction_request,
            session.mutation_context,
        );
    }
    // The track drag source is installed on every tracklist view, so tracks
    // shown by a regular or smart playlist can be dragged onto another
    // regular playlist. Keyboard users need the same destinations from the
    // menu, so the add actions are offered in playlist views as well.
    build_add_to_playlist_actions(
        menu,
        action_group,
        session.sidebar_store,
        session.sm,
        &popup_plan.selection,
        interaction_request,
        session.mutation_context,
    );

    // ── Properties… ──────────────────────────────────────────
    let automatic_device = active_source_is_automatic_device(session.sidebar_store, active_key);
    build_properties_action(
        menu,
        action_group,
        session.sm,
        &popup_plan.selection,
        automatic_device,
        session.mutation_context,
    );
}

/// Open the selection-snapshotted context-menu popover on the session's
/// column view. Returns `true` only when a non-empty menu was presented.
fn open_context_menu_popup(
    session: &ContextMenuPopupSession<'_>,
    active_source_key: &Rc<RefCell<String>>,
    anchor: Option<gtk::gdk::Rectangle>,
) -> bool {
    let active_key = active_source_key.borrow().clone();
    let is_playlist_view = active_key.starts_with("playlist:");
    let Some(popup_plan) = snapshot_popup_plan(session) else {
        return false;
    };

    let menu = gtk::gio::Menu::new();
    let action_group = gtk::gio::SimpleActionGroup::new();
    let interaction_request = session.mutation_context.current_request(&active_key);

    append_context_menu_actions(
        session,
        &menu,
        &action_group,
        &active_key,
        is_playlist_view,
        &popup_plan,
        interaction_request.as_ref(),
    );

    if menu.n_items() == 0 {
        return false;
    }

    let popover = popover_from_menu_model(session.column_view, &menu, &action_group);
    if let Some(anchor) = anchor {
        popover.set_pointing_to(Some(&anchor));
    }
    popover.popup();
    true
}

fn attach_pointer_context_menu(state: &WindowState, popup_menu: &ContextMenuPopupFn) {
    let gesture = gtk::GestureClick::new();
    gesture.set_button(3); // right-click
    {
        let popup_menu = Rc::clone(popup_menu);
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
}

fn attach_keyboard_context_menu(state: &WindowState, popup_menu: ContextMenuPopupFn) {
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
}

/// Per-row drop-target payload used by the sidebar factory to install the
/// playlist drop handler. The sidebar's `ListView` resolves the destination
/// from the *drop coordinates* (via `list_item.position()`), not from the
/// current sidebar selection: rows that are non-selectable (headers, servers,
/// pull mirrors) still need a usable hit target, and dropping on a different
/// playlist row than the one currently focused must add to the row the
/// pointer actually landed on, not the row the keyboard cursor last visited.
pub struct PlaylistRowDropContext {
    store: gtk::gio::ListStore,
    on_drop: PlaylistDropSink,
}

/// Receives the exactly-resolved destination and payload of an accepted
/// per-row drop. Production wires this straight to
/// [`PlaylistMutationContext::add_candidates_to_playlist`]; tests inject an
/// observer so the real `connect_drop` handler can be driven without a live
/// window or database.
type PlaylistDropSink = Rc<dyn Fn(String, String, Vec<PlaylistAddCandidate>)>;

impl PlaylistRowDropContext {
    pub fn from_window(state: &WindowState) -> Self {
        let context = PlaylistMutationContext::from_window(state);
        Self {
            store: state.sidebar_store.clone(),
            on_drop: Rc::new(move |playlist_id, playlist_name, candidates| {
                context.add_candidates_to_playlist(playlist_id, playlist_name, candidates);
            }),
        }
    }

    /// Test-only constructor: same drop resolution as production, but the
    /// accepted destination and payload go to `on_drop` instead of the live
    /// mutation context.
    #[cfg(test)]
    fn for_test(store: gtk::gio::ListStore, on_drop: PlaylistDropSink) -> Self {
        Self { store, on_drop }
    }
}

/// Attach a `DropTarget` to a single sidebar row.
///
/// The drop handler resolves the destination from `list_item.position()`,
/// which reflects the row currently bound to this widget — even for the
/// non-selectable header rows that the `ListView`-level selection never
/// tracks. Installing the target on `row_box` (per row) keeps the active
/// drag hit area scoped to whichever row the pointer is over.
pub fn attach_playlist_drop_target(
    row_box: &gtk::Box,
    list_item: &gtk::ListItem,
    drop: &PlaylistRowDropContext,
) {
    let drop_target = gtk::DropTarget::new(
        PlaylistDragPayload::static_type(),
        gtk::gdk::DragAction::COPY,
    );
    let store_for_accept = drop.store.clone();
    let list_item_for_accept = list_item.clone();
    drop_target.connect_accept(move |_, drop| {
        playlist_row_accepts_drop(
            &store_for_accept,
            &list_item_for_accept,
            &drop.formats(),
            drop.actions(),
        )
    });
    let store_for_drop = drop.store.clone();
    let list_item_for_drop = list_item.clone();
    let sink_for_drop = drop.on_drop.clone();
    drop_target.connect_drop(move |_, value, _, _| {
        let Some(resolved) = resolve_playlist_drop(&store_for_drop, &list_item_for_drop, value)
        else {
            return false;
        };
        sink_for_drop(
            resolved.playlist_id,
            resolved.playlist_name,
            resolved.candidates,
        );
        true
    });
    row_box.add_controller(drop_target);
}

/// A per-row track drop resolved to the playlist currently bound to the row
/// under the pointer.
///
/// Extracted verbatim from the `connect_drop` handler so the production
/// resolution path — payload decode, `position_source`, and the
/// editable-regular refusal — can be driven directly in tests without a live
/// mutation context.
struct ResolvedPlaylistDrop {
    playlist_id: String,
    playlist_name: String,
    candidates: Vec<PlaylistAddCandidate>,
}

fn resolve_playlist_drop(
    store: &gtk::gio::ListStore,
    list_item: &gtk::ListItem,
    value: &gtk::glib::Value,
) -> Option<ResolvedPlaylistDrop> {
    let payload = value.get::<PlaylistDragPayload>().ok()?;
    let source = position_source(store, list_item)?;
    if !source.is_editable_regular_playlist() {
        return None;
    }
    Some(ResolvedPlaylistDrop {
        playlist_id: source.playlist_id(),
        playlist_name: source.name(),
        candidates: payload.candidates,
    })
}

/// Full production `connect_accept` decision for one row: the format/action
/// compatibility check combined with resolving the row under the pointer to
/// its sidebar source.
fn playlist_row_accepts_drop(
    store: &gtk::gio::ListStore,
    list_item: &gtk::ListItem,
    formats: &gtk::gdk::ContentFormats,
    actions: gtk::gdk::DragAction,
) -> bool {
    playlist_drop_is_acceptable(
        formats,
        actions,
        position_source(store, list_item)
            .is_some_and(|source| source.is_editable_regular_playlist()),
    )
}

fn position_source(store: &gtk::gio::ListStore, list_item: &gtk::ListItem) -> Option<SourceObject> {
    let pos = list_item.position();
    store.item(pos).and_downcast::<SourceObject>()
}

/// Decide whether the per-row track drop target may claim an in-flight drag.
///
/// Connecting a custom `accept` handler replaces GTK's default format and
/// action compatibility check, so that check must be re-applied here. The
/// same sidebar row also hosts the playlist-reorder target (string playlist
/// id under `MOVE`), and GTK consults the most recently added controller
/// first. Answering on row editability alone would let this target claim a
/// reorder drag entering an editable regular playlist; the drop would then
/// fail to decode as `PlaylistDragPayload` and reordering onto that row
/// would silently break.
fn playlist_drop_is_acceptable(
    formats: &gtk::gdk::ContentFormats,
    actions: gtk::gdk::DragAction,
    editable_regular_playlist: bool,
) -> bool {
    editable_regular_playlist
        && formats.contains_type(PlaylistDragPayload::static_type())
        && actions.contains(gtk::gdk::DragAction::COPY)
}

/// Run the DB-backed playlist add on the blocking runtime and report the
/// outcome through `result_tx`.
///
/// Authorization revalidation (`acquire_regular_playlist_commit_authority`)
/// happens inside the write transaction, so a stale menu snapshot can never
/// commit against a playlist that lost editability between click and commit.
fn spawn_playlist_add_worker(
    rt_handle: &tokio::runtime::Handle,
    registry: SourceRegistry,
    playlist_id: String,
    plan: PlaylistAddPlan,
    result_tx: async_channel::Sender<PlaylistMutationOutcome>,
) {
    rt_handle.spawn(async move {
        let outcome = match crate::db::connection::init_db().await {
            Ok(db) => {
                let manager = crate::local::playlist_manager::PlaylistManager::new(db);
                match manager
                    .add_entries_if_authorized(&playlist_id, &plan.inputs, || {
                        registry.acquire_regular_playlist_commit_authority(&plan.authority)
                    })
                    .await
                {
                    Ok(PlaylistEntryAddOutcome::Committed(_)) => PlaylistMutationOutcome::Committed,
                    Ok(PlaylistEntryAddOutcome::Rejected) => PlaylistMutationOutcome::Rejected,
                    Err(error) => {
                        tracing::error!(%error, playlist = %playlist_id, "Failed to add exact playlist occurrences");
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
}

fn setup_playlist_transfer(
    state: &WindowState,
    playlist_row_drop: Rc<RefCell<Option<PlaylistRowDropContext>>>,
) {
    use gtk::glib::prelude::ToValue;

    // Publish the per-row drop context so any sidebar row that is realized
    // after this point gets a `DropTarget` resolving to its own `position()`.
    // Rows already realized will not retroactively gain a target, so the
    // sidebar factory must consult this slot lazily on every row setup.
    *playlist_row_drop.borrow_mut() = Some(PlaylistRowDropContext::from_window(state));

    let drag_source = gtk::DragSource::builder()
        .actions(gtk::gdk::DragAction::COPY)
        .build();
    let sort_model = state.sort_model.clone();
    let column_view = state.column_view.clone();
    drag_source.connect_prepare(move |_, x, y| {
        // The source is installed on the whole `ColumnView`, whose header
        // owns GTK's column reorder and resize gestures. Only a press inside
        // the data-row area may become a track drag.
        let picked = column_view.pick(x, y, gtk::PickFlags::DEFAULT);
        if !drag_origin_is_data_row(&column_view, picked) {
            return None;
        }
        let selection = column_view
            .model()?
            .downcast::<gtk::MultiSelection>()
            .ok()?;
        let payload = PlaylistDragPayload::from_selection(&sort_model, &selection)?;
        Some(gtk::gdk::ContentProvider::for_value(&payload.to_value()))
    });
    state.column_view.add_controller(drag_source);
    state.column_view.update_property(&[
        gtk::accessible::Property::Description(
            rust_i18n::t!("context.playlist_drag_description").as_ref(),
        ),
        gtk::accessible::Property::KeyShortcuts("Shift+F10 ContextMenu"),
    ]);
}

/// Whether a drag starting at `picked`, the widget under the pointer as
/// resolved by [`gtk::prelude::WidgetExt::pick`] on `column_view`, begins in
/// the data-row area.
///
/// `GtkColumnView` parents two children directly: a header row widget, which
/// hosts the column reorder and resize gestures, and an internal
/// `GtkListView` subclass holding the data rows. A track drag may only start
/// from a widget inside that list view; the header, empty space, and the
/// column view itself refuse.
fn drag_origin_is_data_row(column_view: &gtk::ColumnView, picked: Option<gtk::Widget>) -> bool {
    let Some(picked) = picked.filter(|widget| !widget.is::<gtk::ListView>()) else {
        // An internal ListView pick is its unused viewport, not a data row.
        return false;
    };
    let column_view = column_view.upcast_ref::<gtk::Widget>();
    let mut widget = picked.parent();
    let mut inside_rows = false;
    while let Some(current) = widget {
        if &current == column_view {
            return inside_rows;
        }
        inside_rows |= current.is::<gtk::ListView>();
        widget = current.parent();
    }
    false
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
                let action_playlist_name = pl_name.clone();
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
                    context.add_candidates_to_playlist(
                        pid.clone(),
                        action_playlist_name.clone(),
                        candidates,
                    );
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
    sm: &gtk::SortListModel,
    selection: &SelectionSnapshot,
    automatic_device: bool,
    mutation_context: &PlaylistMutationContext,
) {
    // A removable device can own rows of any view: a playlist can contain
    // removable entries while the active view is the playlist, not the
    // device, so removable ownership is decided per row from the sidebar's
    // exact SourceId metadata — never from which view happens to be active.
    let sidebar_store = &mutation_context.sidebar_store;

    // Snapshot the exact selection while building the menu. Properties is an
    // all-or-none operation: silently dropping a malformed, remote, or
    // pathless lifecycle row would let a batch edit only an unexpected
    // subset. Local rows snapshot their validated native path; rows owned by
    // a known removable device snapshot their exact source-scoped identity
    // for resolution through the live session when the action fires.
    let mut track_infos = Vec::new();
    for &position in &selection.positions {
        let Some(item) = sm.item(position) else {
            return;
        };
        let Some(track) = item.downcast_ref::<TrackObject>() else {
            return;
        };
        let Some(target) = properties_save_target(track, sidebar_store) else {
            return;
        };
        track_infos.push(super::properties_dialog::TrackInfo {
            target,
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
    let win_for_props: Option<adw::ApplicationWindow> = mutation_context
        .column_view
        .root()
        .and_then(|root| root.downcast::<adw::ApplicationWindow>().ok());
    tracing::debug!(
        has_win = win_for_props.is_some(),
        track_count = track_infos.len(),
        removable = track_infos.iter().any(|info| {
            matches!(
                info.target,
                SaveTarget::PendingRemovable(_) | SaveTarget::Removable(_)
            )
        }),
        "build_properties_action"
    );
    let registry_for_props = mutation_context.source_registry.clone();
    let rt_handle_for_props = mutation_context.rt_handle.clone();
    let failure_context_for_props = mutation_context.clone();

    props_action.connect_activate(move |_, _| {
        let Some(ref win) = win_for_props else {
            tracing::warn!("properties action: win_for_props is None, cannot show dialog");
            return;
        };
        if track_infos
            .iter()
            .all(|info| matches!(info.target, SaveTarget::LocalPath(_)))
        {
            // A local-path-only selection can never write through a removable
            // authority, so no post-mutation catalogue refresh can apply.
            super::properties_dialog::show_properties_dialog(
                win,
                &track_infos,
                automatic_device,
                None,
            );
            return;
        }

        // Exchange every distinct pending removable identity for a retained
        // mutation authority through its exact live session. The action is
        // all-or-none: one unavailable device, retired session, or changed
        // epoch cancels the dialog entirely rather than editing a subset,
        // and a native mount location is never surfaced.
        let pending = distinct_pending_mutations(&track_infos);
        let registry = registry_for_props.clone();
        let registry_for_catalogue = registry.clone();
        let rt_handle = rt_handle_for_props.clone();
        let win = win.clone();
        let track_infos_for_resolve = track_infos.clone();
        let failure_context = failure_context_for_props.clone();
        let (tx, rx) = async_channel::bounded::<
            Option<
                std::collections::HashMap<
                    (SourceId, String),
                    crate::source_registry::RemovableMutationTarget,
                >,
            >,
        >(1);
        rt_handle.spawn(async move {
            let mut resolved: std::collections::HashMap<
                (SourceId, String),
                crate::source_registry::RemovableMutationTarget,
            > = std::collections::HashMap::new();
            for mutation in &pending {
                match registry
                    .resolve_mutation_target(
                        mutation.source_id,
                        mutation.session_epoch,
                        mutation.track_id.clone(),
                    )
                    .await
                {
                    Ok(target) => {
                        resolved.insert(
                            (mutation.source_id, mutation.track_id.as_str().to_owned()),
                            target,
                        );
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            source = %mutation.source_id,
                            track = mutation.track_id.as_str(),
                            "removable properties resolution failed; surfacing the cancelled action"
                        );
                        let _ = tx.send_blocking(None);
                        return;
                    }
                }
            }
            let _ = tx.send_blocking(Some(resolved));
        });
        glib::MainContext::default().spawn_local(async move {
            // Every other failure path in this file presents an alert; a
            // resolution refused between menu build and activation (device
            // removed, session retired, epoch changed) must be visible too —
            // the user activated Properties and must not watch the popover
            // silently close.
            let Ok(Some(resolved)) = rx.recv().await else {
                failure_context.show_mutation_failed();
                return;
            };
            let mut infos = track_infos_for_resolve;
            for info in &mut infos {
                if let SaveTarget::PendingRemovable(pending) = &info.target {
                    if let Some(target) =
                        resolved.get(&(pending.source_id, pending.track_id.as_str().to_owned()))
                    {
                        info.target = SaveTarget::Removable(target.clone());
                    }
                }
            }
            if infos
                .iter()
                .any(|info| matches!(info.target, SaveTarget::PendingRemovable(_)))
            {
                // Unreachable: every pending identity in `infos` came from
                // the same resolved set. A retry-resolution must be exact,
                // never partial.
                tracing::warn!("removable properties resolution left an identity unresolved");
                return;
            }
            // Successful removable writes republish refreshed metadata for
            // exactly the written identities; the dialog triggers the
            // registry's catalogue refresh lane itself.
            super::properties_dialog::show_properties_dialog(
                &win,
                &infos,
                automatic_device,
                Some(registry_for_catalogue),
            );
        });
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

/// The removable device that owns one row, matched against exact sidebar
/// metadata.
///
/// The match mirrors `active_source_is_automatic_device`: an opaque logical
/// GIO key or mount-path spelling is not a navigation identity, so the
/// sidebar's exact SourceId decides. Ownership is read from the row's own
/// source identity — never from the active view — because a playlist can
/// display removable rows while the active navigation key is the playlist,
/// not the device.
fn removable_row_source(
    track: &TrackObject,
    sidebar_store: &gtk::gio::ListStore,
) -> Option<SourceId> {
    let row_source = track.source_id()?;
    (0..sidebar_store.n_items())
        .filter_map(|position| sidebar_store.item(position).and_downcast::<SourceObject>())
        .find(|source| {
            source.backend_type() == "usb-device" && source.source_id() == Some(row_source)
        })
        .and_then(|source| source.source_id())
}

/// The exact save target one selected row will be written through.
///
/// `None` means the row cannot be authorized for a Properties edit at all;
/// the whole action is dropped rather than editing an unexpected subset.
fn properties_save_target(
    track: &TrackObject,
    sidebar_store: &gtk::gio::ListStore,
) -> Option<super::properties_dialog::SaveTarget> {
    // A row owned by a known removable device is pathless by design, in this
    // or any other view: its edit authorization is the exact source-scoped
    // identity, exchanged for a retained mutation authority through the live
    // session when the action fires. A row without a session epoch is a
    // wiring fault and aborts the whole selection.
    if let Some(source_id) = removable_row_source(track, sidebar_store) {
        let track_id = TrackId::new(track.track_id()).ok()?;
        return Some(SaveTarget::PendingRemovable(
            super::properties_dialog::PendingRemovableMutation {
                source_id,
                session_epoch: track.source_session_epoch()?,
                track_id,
            },
        ));
    }
    // Every other row remains a path-authorized local-file edit.
    let path = local_file_path(&track.uri())?;
    Some(SaveTarget::LocalPath(path))
}

/// One pending mutation per distinct removable identity, in selection order.
///
/// Repeated playlist rows may refer to the same removable file; resolving
/// the identity twice would retain two authorities over one exact object.
///
/// The identity is the complete `(source, track)` pair, not the track alone:
/// `TrackId::removable_relative` is scoped to one source's mount root, so two
/// devices exposing the same relative path produce equal track IDs. Keying on
/// the track string alone would drop one device's pending resolution and
/// rebind its rows to the surviving device's authority.
fn distinct_pending_mutations(
    track_infos: &[super::properties_dialog::TrackInfo],
) -> Vec<super::properties_dialog::PendingRemovableMutation> {
    let mut seen = std::collections::HashSet::new();
    let mut pending = Vec::new();
    for info in track_infos {
        if let SaveTarget::PendingRemovable(mutation) = &info.target {
            if seen.insert((mutation.source_id, mutation.track_id.as_str().to_owned())) {
                pending.push(mutation.clone());
            }
        }
    }
    pending
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

/// Build a `gtk::Popover` from a `gio::Menu` model and an action group.
///
/// Each menu item with an enabled action becomes a flat `gtk::Button`;
/// disabled actions render as section-header labels. The popover is
/// parented to `parent` and self-unparents on close.
///
/// Submenus are not supported — items with no action reference are
/// silently skipped.
///
/// This is a deliberate departure from `gtk::PopoverMenu::from_model`:
/// on some Linux desktop configurations that binding can produce an
/// invisible popover (no widget tree attached), which manifested as
/// Track Properties / New Playlist / New Smart Playlist dialogs failing
/// to open after the user clicked their menu entries. Hosting plain
/// buttons in a bounded `gtk::ScrolledWindow` inside a generic `gtk::Popover`
/// keeps the same one-shot lifecycle without relying on the broken binding or
/// making later playlist actions unreachable on a short display.
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
                let pop = popover.downgrade();
                btn.connect_clicked(move |_| {
                    act.activate(None::<&glib::Variant>);
                    if let Some(pop) = pop.upgrade() {
                        pop.popdown();
                    }
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

    let viewport = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_width(true)
        .propagate_natural_height(true)
        .max_content_height(CONTEXT_MENU_MAX_CONTENT_HEIGHT)
        .child(&vbox)
        .build();
    popover.set_child(Some(&viewport));
    popover.connect_closed(|popover| popover.unparent());
    popover
}

#[cfg(test)]
pub mod tests {
    use std::path::PathBuf;

    use super::*;
    // `super` inside this test module is `context_menu`, so the sibling
    // dialog module must be named from the `ui` parent directly.
    use crate::ui::properties_dialog;

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

    /// A pending removable mutation is identified by the complete
    /// `(source, track)` pair. `TrackId::removable_relative` is scoped to one
    /// source's mount root, so two devices exposing the same relative path
    /// produce equal track IDs; keying on the track string alone would drop
    /// one device's pending resolution and rebind its rows to the surviving
    /// device's authority.
    #[test]
    fn distinct_pending_mutations_key_on_source_and_track_not_track_alone() {
        fn info_with_target(target: SaveTarget) -> properties_dialog::TrackInfo {
            properties_dialog::TrackInfo {
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

        let pending_on = |source_id: SourceId| properties_dialog::PendingRemovableMutation {
            source_id,
            session_epoch: 1,
            track_id: TrackId::new("unix:616c62756d2f736f6e67").expect("track id"),
        };
        let device_one = SourceId::random();
        let device_two = SourceId::random();
        let shared_track_on_device_one = pending_on(device_one);

        // Two devices exposing the same relative path, plus a repeat of the
        // first device's row: two distinct identities, not one.
        let infos = vec![
            info_with_target(SaveTarget::PendingRemovable(
                shared_track_on_device_one.clone(),
            )),
            info_with_target(SaveTarget::PendingRemovable(pending_on(device_two))),
            info_with_target(SaveTarget::PendingRemovable(
                shared_track_on_device_one.clone(),
            )),
        ];
        let pending = distinct_pending_mutations(&infos);
        assert_eq!(
            pending.len(),
            2,
            "the same relative path on two devices must stay distinct"
        );
        assert_eq!(pending[0].source_id, device_one);
        assert_eq!(pending[1].source_id, device_two);

        // The same source and track repeated still collapses to one.
        let repeated = vec![
            info_with_target(SaveTarget::PendingRemovable(
                shared_track_on_device_one.clone(),
            )),
            info_with_target(SaveTarget::PendingRemovable(shared_track_on_device_one)),
        ];
        assert_eq!(
            distinct_pending_mutations(&repeated).len(),
            1,
            "repeated rows over one identity must still deduplicate"
        );
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
    fn playlist_add_success_selects_plural_form_and_escapes_playlist_markup() {
        assert_eq!(
            playlist_add_success_message("en", 1, "R&B <Mix>"),
            "1 track added to R&amp;B &lt;Mix&gt;."
        );
        assert_eq!(
            playlist_add_success_message("en", 2, "R&B <Mix>"),
            "2 tracks added to R&amp;B &lt;Mix&gt;."
        );

        for locale in rust_i18n::available_locales!() {
            let one = playlist_add_success_message(&locale, 1, "R&B <Mix>");
            let other = playlist_add_success_message(&locale, 2, "R&B <Mix>");
            let expected_one = rust_i18n::t!(
                "context.playlist_add_success.one",
                locale = locale,
                count = 1,
                playlist = "R&amp;B &lt;Mix&gt;"
            );
            // Count 2 is `other` in most catalogs but `few` in Polish and
            // Russian; the catalog form must match whatever the selector picks.
            let other_key = format!(
                "context.playlist_add_success.{}",
                plural_category(&locale, 2)
            );
            let expected_other = rust_i18n::t!(
                other_key.as_str(),
                locale = locale,
                count = 2,
                playlist = "R&amp;B &lt;Mix&gt;"
            );

            assert_eq!(one, expected_one, "{locale}: singular form");
            assert_eq!(other, expected_other, "{locale}: plural form");
            assert!(!one.contains("%{count}"), "{locale}: singular count");
            assert!(!other.contains("%{count}"), "{locale}: plural count");
            assert!(
                one.contains("R&amp;B &lt;Mix&gt;"),
                "{locale}: escaped name"
            );
            assert!(
                other.contains("R&amp;B &lt;Mix&gt;"),
                "{locale}: escaped name"
            );
        }
    }

    #[test]
    fn playlist_add_success_uses_locale_plural_categories() {
        assert_eq!(plural_category("pl", 1), "one");
        assert_eq!(plural_category("pl", 2), "few");
        assert_eq!(plural_category("pl", 5), "many");
        assert_eq!(plural_category("pl", 12), "many");
        assert_eq!(plural_category("pl", 22), "few");
        assert_eq!(plural_category("ru", 1), "one");
        assert_eq!(plural_category("ru", 21), "one");
        assert_eq!(plural_category("ru", 11), "many");
        assert_eq!(plural_category("ru", 3), "few");
        assert_eq!(plural_category("ru", 5), "many");
        assert_eq!(plural_category("en", 21), "other");
        assert_eq!(plural_category("pt-BR", 1), "one");
        assert_eq!(plural_category("zh-CN", 2), "other");

        assert_eq!(
            playlist_add_success_message("pl", 3, "Mix"),
            "Dodano 3 utwory do Mix."
        );
        assert_eq!(
            playlist_add_success_message("pl", 5, "Mix"),
            "Dodano 5 utworów do Mix."
        );
        assert_eq!(
            playlist_add_success_message("ru", 21, "Mix"),
            "Добавлена 21 композиция в Mix."
        );
        assert_eq!(
            playlist_add_success_message("ru", 3, "Mix"),
            "Добавлено 3 композиции в Mix."
        );
        assert_eq!(
            playlist_add_success_message("ru", 5, "Mix"),
            "Добавлено 5 композиций в Mix."
        );

        // Every category the selector can pick must resolve in every catalog;
        // a missing form would surface the raw key in the toast.
        for locale in rust_i18n::available_locales!() {
            for count in 1..=25 {
                let message = playlist_add_success_message(&locale, count, "Mix");
                assert!(
                    !message.contains("playlist_add_success"),
                    "{locale}: no catalog form for count {count}"
                );
                assert!(
                    message.contains(&count.to_string()),
                    "{locale}: count {count} missing"
                );
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

    /// Regression test for the "dialogs not appearing on Linux" bug:
    /// `gtk::PopoverMenu::from_model` can produce an invisible popover on
    /// some Linux desktop configurations, which manifested as Track
    /// Properties / New Playlist / New Smart Playlist dialogs failing to
    /// open after the user clicked their menu entries. The fix routes all
    /// menu display through `popover_from_menu_model`, which always sets a
    /// non-null child widget built from the menu's actions.
    ///
    /// This contract is exercised end-to-end: with a populated
    /// menu and matching action group, the resulting popover MUST have a
    /// bounded scrolling child containing one button per enabled action. If a
    /// future change drops the child assignment or scrolling constraint (e.g.
    /// by re-introducing `gtk::PopoverMenu::from_model` or attaching the menu
    /// box directly), the consolidated GTK test will fail.
    ///
    /// Headless CI (cargo test in the Fedora container with no X/Wayland socket)
    /// cannot initialize GTK, so the contract skips with a printed reason when
    /// no display session is available or GTK cannot acquire a display.
    /// macOS is excluded because GTK's Quartz backend panics when
    /// initialized from the test harness worker thread. The contract still
    /// holds on any machine with a display — it is therefore
    /// meaningful on a developer box and harmless in CI.
    ///
    /// This contract is NOT its own `#[test]`: the
    /// `ui::widget_test_session` mutex serializes GTK-initializing tests
    /// but does not give them thread affinity, so a second GTK-touching
    /// `#[test]` would run on a different libtest worker thread than the
    /// one that ran `gtk::init` and construct widgets off the
    /// initializing thread, tripping gtk-rs main-thread checks
    /// (2026-09-09 review rejection, PR #179). It is instead invoked from
    /// the crate's single consolidated GTK test in `browser.rs`, whose
    /// `acquire` call owns the display gate, the single `gtk::init`, and
    /// the serialization lock across this body.
    #[cfg(not(target_os = "macos"))]
    pub fn popover_from_menu_model_attaches_a_visible_child_widget() {
        assert_track_drags_start_only_from_the_data_row_area();

        let harness = popover_menu_harness();
        let parent = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let popover = popover_from_menu_model(&parent, &harness.menu, &harness.actions);

        let viewport = assert_popover_wraps_scrolling_viewport(&popover);
        let vbox = menu_box_inside(&viewport);
        clicking_first_button_fires_only_its_action(&vbox, &harness.prop_rx, &harness.add_rx);
    }

    /// Menu model + action group backing the popover contract, with one
    /// receiver per action so assertions can observe exactly which
    /// action a click activated.
    #[cfg(not(target_os = "macos"))]
    struct PopoverMenuHarness {
        menu: gtk::gio::Menu,
        actions: gtk::gio::SimpleActionGroup,
        prop_rx: async_channel::Receiver<()>,
        add_rx: async_channel::Receiver<()>,
    }

    #[cfg(not(target_os = "macos"))]
    fn popover_menu_harness() -> PopoverMenuHarness {
        let menu = gtk::gio::Menu::new();
        menu.append(Some("Open Properties"), Some("ctx.properties"));
        menu.append(Some("Add to Playlist"), Some("ctx.add"));

        let actions = gtk::gio::SimpleActionGroup::new();
        let prop_action = gtk::gio::SimpleAction::new("properties", None);
        let (prop_tx, prop_rx) = async_channel::unbounded::<()>();
        prop_action.connect_activate(move |_, _| {
            let _ = prop_tx.send_blocking(());
        });
        actions.add_action(&prop_action);

        let add_action = gtk::gio::SimpleAction::new("add", None);
        let (add_tx, add_rx) = async_channel::unbounded::<()>();
        add_action.connect_activate(move |_, _| {
            let _ = add_tx.send_blocking(());
        });
        actions.add_action(&add_action);

        PopoverMenuHarness {
            menu,
            actions,
            prop_rx,
            add_rx,
        }
    }

    /// The popover must wrap its menu in a scrolling viewport with the
    /// documented policy/size contract; returns the viewport so callers
    /// can descend to the menu box.
    #[cfg(not(target_os = "macos"))]
    fn assert_popover_wraps_scrolling_viewport(popover: &gtk::Popover) -> gtk::ScrolledWindow {
        let child = popover
            .child()
            .expect("popover must have a non-null child after construction");
        let viewport = child
            .downcast::<gtk::ScrolledWindow>()
            .expect("popover child must be a scrolling viewport");
        assert_eq!(viewport.hscrollbar_policy(), gtk::PolicyType::Never);
        assert_eq!(viewport.vscrollbar_policy(), gtk::PolicyType::Automatic);
        assert!(viewport.propagates_natural_width());
        assert!(viewport.propagates_natural_height());
        assert_eq!(
            viewport.max_content_height(),
            CONTEXT_MENU_MAX_CONTENT_HEIGHT
        );
        viewport
    }

    /// The menu box is not `GtkScrollable`, so `ScrolledWindow::set_child`
    /// wraps it in an auto-added `GtkViewport`, and `child()` returns that
    /// viewport rather than the box. Look through the wrapper when present.
    #[cfg(not(target_os = "macos"))]
    fn menu_box_inside(viewport: &gtk::ScrolledWindow) -> gtk::Box {
        let vbox = viewport
            .child()
            .and_then(|child| match child.downcast::<gtk::Box>() {
                Ok(vbox) => Some(vbox),
                Err(child) => child
                    .downcast::<gtk::Viewport>()
                    .ok()?
                    .child()?
                    .downcast::<gtk::Box>()
                    .ok(),
            })
            .expect("scrolling viewport must contain the menu's Box");
        assert_eq!(
            vbox.observe_children().n_items(),
            2,
            "one button per enabled action"
        );
        vbox
    }

    /// Clicking the first button must activate the corresponding
    /// action and close the popover — that's the user-visible fix.
    #[cfg(not(target_os = "macos"))]
    fn clicking_first_button_fires_only_its_action(
        vbox: &gtk::Box,
        prop_rx: &async_channel::Receiver<()>,
        add_rx: &async_channel::Receiver<()>,
    ) {
        let first_button = vbox
            .observe_children()
            .item(0)
            .and_then(|o| o.downcast::<gtk::Button>().ok())
            .expect("first child must be a button");
        assert_eq!(first_button.label().as_deref(), Some("Open Properties"));

        first_button.emit_clicked();
        assert_eq!(prop_rx.try_recv(), Ok(()), "properties action must fire");
        assert_eq!(
            add_rx.try_recv(),
            Err(async_channel::TryRecvError::Empty),
            "non-clicked actions must not fire"
        );
    }

    /// The per-row drop target attached by `attach_playlist_drop_target`
    /// resolves the destination from `list_item.position()` and asks
    /// `is_editable_regular_playlist()` of that exact row — not the row the
    /// sidebar selection happens to point at. This test pins the data-layer
    /// invariant that drives that decision: the only sidebar rows that the
    /// drop handler accepts are local regular playlists, so the lookup never
    /// silently routes a drop to a different row's playlist.
    #[test]
    fn per_row_drop_target_accepts_only_editable_regular_playlists() {
        use crate::db::entities::server_playlist_link::{
            ServerPlaylistLocalState, ServerPlaylistRemoteState,
        };
        use crate::local::playlist_sidebar::PlaylistSidebarEntry;
        use crate::local::playlist_sidebar::PlaylistSidebarKind;
        use crate::ui::objects::HeaderKind;

        let regular = SourceObject::playlist_entry(&PlaylistSidebarEntry::new(
            "regular-1",
            "My Mix",
            PlaylistSidebarKind::EditableRegular,
        ));
        let smart = SourceObject::playlist_entry(&PlaylistSidebarEntry::new(
            "smart-1",
            "Smart Mix",
            PlaylistSidebarKind::EditableSmart,
        ));
        let pull_mirror = SourceObject::playlist_entry(&PlaylistSidebarEntry::new(
            "mirror-1",
            "Linked",
            PlaylistSidebarKind::PullMirror {
                local_state: ServerPlaylistLocalState::Clean,
                remote_state: ServerPlaylistRemoteState::Present,
            },
        ));
        let server = SourceObject::discovered("Mini", "subsonic", "http://mini.local:4533");
        let header = SourceObject::header("Wiedergabelisten", HeaderKind::Playlists);

        // Only an editable regular playlist may receive a drop. The other
        // kinds model every non-regular sidebar row the per-row factory
        // setup installs: smart playlists, pull mirrors, server sources,
        // and headers. The `position_source` resolver and the
        // `connect_drop`/`connect_accept` handlers both depend on this
        // exact split, so a regression here would re-introduce the
        // rejected behavior where dropping on a non-regular row falls
        // through to whatever regular playlist the keyboard selection
        // last visited.
        assert!(regular.is_editable_regular_playlist());
        assert!(!smart.is_editable_regular_playlist());
        assert!(!pull_mirror.is_editable_regular_playlist());
        assert!(!server.is_editable_regular_playlist());
        assert!(!header.is_editable_regular_playlist());
    }

    /// Widget-constructing contract for the tracklist drag origin. Called from
    /// [`popover_from_menu_model_attaches_a_visible_child_widget`] rather than
    /// being its own `#[test]`: the harness runs tests on a pool of threads,
    /// so a second GTK-initializing test would have to coordinate through the
    /// `ui::widget_test_session` lock with a test in another module; folding
    /// it into the one GTK session this module already holds keeps the
    /// contract exercised without a cross-module handshake.
    #[cfg(not(target_os = "macos"))]
    fn assert_track_drags_start_only_from_the_data_row_area() {
        let (window, column_view, label) = realized_tracklist_for_drag_test();

        // GTK parents the header row widget first and the internal list view
        // last; the header owns column reordering and resizing.
        let header = column_view.first_child().expect("header row widget");
        let rows = column_view.last_child().expect("internal list view");
        assert!(
            !header.is::<gtk::ListView>(),
            "first child must be the header"
        );
        assert!(
            rows.is::<gtk::ListView>(),
            "last child must be the list view"
        );

        assert!(!drag_origin_is_data_row(&column_view, Some(rows)));
        assert!(drag_origin_is_data_row(
            &column_view,
            Some(label.clone().upcast())
        ));
        assert!(!drag_origin_is_data_row(
            &gtk::ColumnView::new(None::<gtk::SelectionModel>),
            Some(label.upcast())
        ));
        assert!(!drag_origin_is_data_row(&column_view, Some(header)));
        assert!(!drag_origin_is_data_row(
            &column_view,
            Some(column_view.clone().upcast())
        ));
        assert!(!drag_origin_is_data_row(&column_view, None));
        // A widget outside the column view never qualifies.
        assert!(!drag_origin_is_data_row(
            &column_view,
            Some(gtk::Label::new(None).upcast())
        ));
        window.close();
    }

    #[cfg(not(target_os = "macos"))]
    fn realized_tracklist_for_drag_test() -> (gtk::Window, gtk::ColumnView, gtk::Label) {
        let store = gtk::gio::ListStore::new::<gtk::StringObject>();
        store.append(&gtk::StringObject::new("a"));
        let selection = gtk::MultiSelection::new(Some(store));
        let factory = gtk::SignalListItemFactory::new();
        let row_label = Rc::new(RefCell::new(None::<gtk::Label>));
        let created_label = row_label.clone();
        factory.connect_setup(move |_, item| {
            let label = gtk::Label::new(Some("a"));
            item.downcast_ref::<gtk::ListItem>()
                .expect("factory list item")
                .set_child(Some(&label));
            *created_label.borrow_mut() = Some(label);
        });
        let column_view = gtk::ColumnView::new(Some(selection));
        column_view.append_column(&gtk::ColumnViewColumn::new(Some("Title"), Some(factory)));
        let window = gtk::Window::builder().child(&column_view).build();
        window.present();
        // Pump the widget test session's thread-default main context —
        // never the process-global default. Parallel non-widget tests (the
        // audio suite in particular) leave thread-affine glib sources
        // pending on the global default context (production code schedules
        // position timers and debounced saves there, and tests never run a
        // main loop). Dispatching one of those here — on this test's
        // worker thread — trips glib's ThreadGuard inside a non-unwinding
        // C trampoline and aborts the whole test binary (tr-8wtab). The
        // session context only ever holds sources this same thread
        // scheduled, so pumping it is safe; the expect documents the
        // invariant that this helper runs inside a widget test session.
        let context = glib::MainContext::thread_default()
            .expect("widget_test_session::with_session pushes a thread-default context");
        while context.pending() {
            context.iteration(false);
        }

        let label = row_label.borrow().clone().expect("realized data-row label");
        (window, column_view, label)
    }

    #[test]
    fn per_row_drop_target_leaves_reorder_drags_to_the_reorder_target() {
        use gtk::gdk::{ContentFormatsBuilder, DragAction};

        let track_drag = ContentFormatsBuilder::new()
            .add_type(PlaylistDragPayload::static_type())
            .build();
        let reorder_drag = ContentFormatsBuilder::new()
            .add_type(glib::Type::STRING)
            .build();

        assert!(playlist_drop_is_acceptable(
            &track_drag,
            DragAction::COPY,
            true
        ));
        // The sidebar's playlist-reorder drag carries a string id under MOVE
        // on the same row widget. A custom accept handler bypasses GTK's
        // default format check, so this predicate must refuse it explicitly
        // or the reorder target behind it never sees the drag.
        assert!(!playlist_drop_is_acceptable(
            &reorder_drag,
            DragAction::MOVE,
            true
        ));
        assert!(!playlist_drop_is_acceptable(
            &reorder_drag,
            DragAction::COPY,
            true
        ));
        assert!(!playlist_drop_is_acceptable(
            &track_drag,
            DragAction::MOVE,
            true
        ));
        assert!(!playlist_drop_is_acceptable(
            &track_drag,
            DragAction::COPY,
            false
        ));
    }

    #[test]
    fn playlist_views_offer_add_destinations_alongside_removal() {
        let source = include_str!("context_menu.rs");
        let body = source
            .split_once("fn append_context_menu_actions(")
            .and_then(|(_, rest)| rest.split_once("\n}\n"))
            .map(|(body, _)| body)
            .expect("append_context_menu_actions body");
        let add_marker = ["build_add_to_", "playlist_actions("].concat();
        let remove_marker = ["build_remove_from_", "playlist_action("].concat();

        assert_eq!(body.matches(&add_marker).count(), 1);
        assert_eq!(body.matches(&remove_marker).count(), 1);
        // Removal stays gated on the playlist view, but the add destinations
        // are the keyboard equivalent of the tracklist drag source, which is
        // installed on every view, so they must not sit in an `else` branch.
        assert!(
            !body.contains("} else {"),
            "add-to-playlist actions must be offered in every tracklist view"
        );
    }

    /// Outcome → feedback dispatch: a committed write must take the success
    /// toast plus playlist-refresh path, a refused write the unsupported
    /// dialog, and a failed (or vanished) worker the failure dialog.
    /// `add_candidates_to_playlist` dispatches on exactly this mapping, so a
    /// regression that swapped or dropped an arm would be caught here even
    /// without a live window.
    #[test]
    fn playlist_add_feedback_maps_outcomes_to_toast_or_dialog() {
        assert_eq!(
            playlist_add_feedback(&Ok(PlaylistMutationOutcome::Committed)),
            PlaylistAddFeedback::Added
        );
        assert_eq!(
            playlist_add_feedback(&Ok(PlaylistMutationOutcome::Rejected)),
            PlaylistAddFeedback::Unsupported
        );
        assert_eq!(
            playlist_add_feedback(&Ok(PlaylistMutationOutcome::Failed)),
            PlaylistAddFeedback::Failed
        );

        // A worker that vanishes without reporting closes the channel; the
        // `Err` arm must still resolve, to the failure dialog.
        let (result_tx, result_rx) = async_channel::bounded::<PlaylistMutationOutcome>(1);
        drop(result_tx);
        let closed = result_rx.recv_blocking();
        assert!(closed.is_err(), "a closed channel must report Err");
        assert_eq!(playlist_add_feedback(&closed), PlaylistAddFeedback::Failed);
    }

    // ── Production per-row drop path (GTK session) ───────────────────────
    //
    // The contracts below construct GtkListModel widgets, whose constructors
    // assert `gtk::is_initialized()`. They therefore cannot be their own
    // `#[test]` functions; they join the crate's single consolidated GTK test
    // in `browser.rs`, which already owns the display gate and the one
    // `gtk::init` for the process.

    /// The real drag-source payload builder must hand the mutation path the
    /// selection in displayed order — the order the rows appear under the
    /// current sort — not store or insertion order.
    #[cfg(not(target_os = "macos"))]
    pub fn drag_payload_preserves_displayed_selection_order() {
        let (sort_model, selection) = sorted_track_selection_models();

        let payload = PlaylistDragPayload::from_selection(&sort_model, &selection)
            .expect("a non-empty selection must produce a drag payload");
        assert_eq!(
            payload
                .candidates
                .iter()
                .map(|candidate| candidate.media_key().track_id.as_str())
                .collect::<Vec<_>>(),
            ["a-track", "c-track"],
            "candidates must follow displayed position order"
        );
    }

    /// Store order (c, a, b) displayed sorted (a, b, c), first and last
    /// displayed rows selected out of store order — the non-trivial selection
    /// shared by the drag-payload and keyboard-equivalence contracts.
    #[cfg(not(target_os = "macos"))]
    fn sorted_track_selection_models() -> (gtk::SortListModel, gtk::MultiSelection) {
        let store = gtk::gio::ListStore::new::<TrackObject>();
        for (track_id, title) in [
            ("c-track", "Third"),
            ("a-track", "First"),
            ("b-track", "Second"),
        ] {
            let track = TrackObject::new(
                1, title, 60, "Artist", "Album", "", "", 0, "", 0, 0, 0, "", "",
            );
            track.set_track_id(track_id);
            assert!(track.set_source_id(SourceId::local()));
            store.append(&track);
        }

        // Sort by track id, so the displayed order (a, b, c) differs from the
        // store order (c, a, b).
        let sorter = gtk::CustomSorter::new(|left, right| {
            let id = |object: &glib::Object| {
                object
                    .downcast_ref::<TrackObject>()
                    .expect("TrackObject row")
                    .track_id()
            };
            id(left).cmp(&id(right)).into()
        });
        let sort_model = gtk::SortListModel::new(Some(store), Some(sorter));
        let selection = gtk::MultiSelection::new(Some(sort_model.clone()));
        // Select the first and last displayed rows, out of store order. The
        // second call must pass `unselect_rest = false`: the flag clears every
        // other row, so chaining two `true` calls would leave only the last
        // row selected.
        selection.select_item(0, true);
        selection.select_item(2, false);
        // Guard the fixture itself: before any payload is built from this
        // selection, exactly the first and last displayed rows must carry the
        // selection the contracts assert against.
        assert!(
            selection.is_selected(0) && !selection.is_selected(1) && selection.is_selected(2),
            "fixture must select exactly the first and last displayed rows"
        );
        (sort_model, selection)
    }

    /// Keyboard-equivalence contract for the "Add to Playlist" context-menu
    /// actions — the keyboard/menu route to a playlist add, required to
    /// behave identically to the per-row drop. The action builder collects
    /// its candidates with `collect_selected_add_candidates` over the popup
    /// selection snapshot and the drag source collects them through
    /// `PlaylistDragPayload::from_selection`; both must carry the identical
    /// candidates in displayed position order, and the keyboard destination
    /// guard `playlist_is_editable_regular` must accept exactly the editable
    /// regular playlists the drop target's `position_source` check accepts.
    #[cfg(not(target_os = "macos"))]
    pub fn keyboard_add_action_matches_the_drag_payload_contract() {
        let (sort_model, selection) = sorted_track_selection_models();
        // The popup-plan selection snapshot: the same displayed positions the
        // context menu captures when it is opened over the selection.
        let keyboard_selection =
            SelectionSnapshot::from_positions([0u32, 2]).expect("a non-empty selection snapshot");

        fn track_ids(candidates: &[PlaylistAddCandidate]) -> Vec<&str> {
            candidates
                .iter()
                .map(|candidate| candidate.media_key().track_id.as_str())
                .collect()
        }

        let drag = PlaylistDragPayload::from_selection(&sort_model, &selection)
            .expect("a non-empty selection must produce a drag payload");
        let keyboard = collect_selected_add_candidates(&sort_model, &keyboard_selection)
            .expect("selected rows must resolve to add candidates");

        assert_eq!(
            track_ids(&drag.candidates),
            ["a-track", "c-track"],
            "the drag payload must follow displayed position order"
        );
        assert_eq!(
            track_ids(&keyboard),
            ["a-track", "c-track"],
            "the keyboard action must carry the identical displayed-order candidates"
        );
        assert_eq!(
            drag.candidates, keyboard,
            "drag and keyboard routes must produce identical candidates"
        );

        // Keyboard destination guard: the action exists only for editable
        // regular playlists, so activation can only mutate what the drop
        // target's `position_source` check would have accepted.
        let sidebar = gtk::gio::ListStore::new::<SourceObject>();
        sidebar.append(&regular_playlist_source());
        sidebar.append(&smart_playlist_source());
        sidebar.append(&header_source());
        assert!(playlist_is_editable_regular(&sidebar, "regular-id"));
        assert!(
            !playlist_is_editable_regular(&sidebar, "smart-id"),
            "smart playlists must refuse the keyboard add, like the drop"
        );
        assert!(
            !playlist_is_editable_regular(&sidebar, "missing-id"),
            "an unknown playlist id must refuse the keyboard add"
        );
    }

    /// Drives the production per-row drop path through the real widgets: a
    /// `GtkListView` whose factory installs the same
    /// `attach_playlist_drop_target` the sidebar uses, real `GtkListItem`
    /// positions, and the actual `connect_drop` handler reached by emitting
    /// the `drop` signal. Two distinct editable regular playlists are
    /// realized, with the keyboard focus resting on one while the drop is
    /// emitted on the other (and then reversed), so a destination resolved
    /// from the focus or hardcoded to the first editable row fails. This is
    /// the coverage the merged #182/#242 review thread called out as missing
    /// when only `is_editable_regular_playlist()` was asserted.
    #[cfg(not(target_os = "macos"))]
    pub fn per_row_playlist_drop_target_drives_the_production_drop_path() {
        drag_payload_preserves_displayed_selection_order();
        let harness = PerRowDropHarness::new();
        harness.assert_accept_resolution();
        harness.assert_drop_routes_to_the_pointer_row();
    }

    #[cfg(not(target_os = "macos"))]
    type RecordedDrops = Rc<RefCell<Vec<(String, String, Vec<PlaylistAddCandidate>)>>>;

    #[cfg(not(target_os = "macos"))]
    struct RealizedDropRow {
        position: u32,
        list_item: gtk::ListItem,
        drop_target: gtk::DropTarget,
    }

    /// A realized sidebar-like view: one `GtkListItem` per source row, each
    /// carrying the production drop target bound to its own position.
    #[cfg(not(target_os = "macos"))]
    struct PerRowDropHarness {
        _window: gtk::Window,
        store: gtk::gio::ListStore,
        selection: gtk::SingleSelection,
        rows: Vec<RealizedDropRow>,
        dropped: RecordedDrops,
    }

    #[cfg(not(target_os = "macos"))]
    impl PerRowDropHarness {
        fn new() -> Self {
            let store = gtk::gio::ListStore::new::<SourceObject>();
            store.append(&regular_playlist_source());
            store.append(&smart_playlist_source());
            store.append(&second_regular_playlist_source());
            store.append(&header_source());

            // Sidebar-like keyboard focus resting on the first editable
            // regular playlist: every drop below is then issued on a row
            // other than the focused one, so a destination resolved from the
            // focus (or from "the first editable row in the store") records
            // the wrong playlist and fails these assertions.
            let selection = gtk::SingleSelection::new(Some(store.clone()));
            selection.set_autoselect(false);
            selection.select_item(0, true);

            let dropped: RecordedDrops = Rc::new(RefCell::new(Vec::new()));
            let on_drop = recorded_drop_sink(Rc::clone(&dropped));
            let drop_context = PlaylistRowDropContext::for_test(store.clone(), on_drop);
            let (window, recorded) = realize_drop_list_view(selection.clone(), drop_context);

            Self {
                _window: window,
                store,
                selection,
                rows: realized_drop_rows(&recorded),
                dropped,
            }
        }

        /// The accept decision sees the row under the pointer: only the two
        /// editable regular playlist rows accept a track drag, while the
        /// smart playlist and header rows refuse it, and a playlist-reorder
        /// drag is always left to the reorder target.
        fn assert_accept_resolution(&self) {
            use gtk::gdk::{ContentFormatsBuilder, DragAction};

            assert_eq!(
                self.rows.iter().map(|row| row.position).collect::<Vec<_>>(),
                vec![0, 1, 2, 3],
                "every row must be bound to its real list position"
            );

            let track_drag = ContentFormatsBuilder::new()
                .add_type(PlaylistDragPayload::static_type())
                .build();
            let reorder_drag = ContentFormatsBuilder::new()
                .add_type(glib::Type::STRING)
                .build();
            let accepts = |index: usize, formats: &gtk::gdk::ContentFormats, action| {
                playlist_row_accepts_drop(&self.store, &self.rows[index].list_item, formats, action)
            };

            assert!(accepts(0, &track_drag, DragAction::COPY));
            assert!(!accepts(1, &track_drag, DragAction::COPY));
            assert!(accepts(2, &track_drag, DragAction::COPY));
            assert!(!accepts(3, &track_drag, DragAction::COPY));
            assert!(!accepts(0, &reorder_drag, DragAction::MOVE));
            assert!(!accepts(0, &track_drag, DragAction::MOVE));
        }

        /// The drop handler resolves the destination from the row under the
        /// pointer — never from the focused row, never from "the first
        /// editable playlist" — and forwards the exact displayed candidate
        /// order there; noneditable rows and unrelated (cancelled) drags
        /// mutate nothing.
        fn assert_drop_routes_to_the_pointer_row(&self) {
            let payload = PlaylistDragPayload {
                candidates: drag_candidates(["second-track", "first-track"]),
            };
            let value = payload.to_value();
            let e = |index: usize, value: &glib::Value| {
                emit_installed_drop(&self.rows[index].drop_target, value)
            };

            // Focus rests on the first editable playlist (set in `new`); the
            // drop lands on the second. The sink must record the pointer
            // row's playlist — an implementation that resolves the focused
            // row or the first editable row would record "regular-id" here.
            assert!(
                e(2, &value),
                "the second editable regular playlist row must accept the drop"
            );
            assert_eq!(
                self.dropped.borrow().as_slice(),
                [(
                    "other-regular-id".to_string(),
                    "Road Trip".to_string(),
                    payload.candidates.clone()
                )],
                "the drop must reach the pointer row's playlist, not the focused row"
            );

            // Reverse trip: focus the second editable playlist and drop on
            // the first, so a destination hardcoded to either end of the
            // sidebar fails exactly one of the two drops.
            self.selection.select_item(2, true);
            assert!(
                e(0, &value),
                "the first editable regular playlist row must accept the drop"
            );
            assert_eq!(
                self.dropped.borrow().as_slice(),
                [
                    (
                        "other-regular-id".to_string(),
                        "Road Trip".to_string(),
                        payload.candidates.clone()
                    ),
                    (
                        "regular-id".to_string(),
                        "My Mix".to_string(),
                        payload.candidates.clone()
                    ),
                ],
                "each drop must reach the row under the pointer, in payload order"
            );

            // Noneditable rows refuse without reaching the mutation sink;
            // a cancelled or unrelated drag carries no playlist payload.
            self.assert_refusals_leave_the_sink_untouched(&e, &value);
        }

        /// The refusal tail of the routing contract: the noneditable rows
        /// (smart playlist, header) refuse the track drag without reaching
        /// the mutation sink, a cancelled or unrelated drag carries no
        /// playlist payload and is declined as well, and only the two
        /// accepted drops above may have mutated a playlist.
        fn assert_refusals_leave_the_sink_untouched(
            &self,
            emit: &impl Fn(usize, &glib::Value) -> bool,
            payload_value: &glib::Value,
        ) {
            assert!(!emit(1, payload_value));
            assert!(!emit(3, payload_value));
            let unrelated = "playlist-reorder".to_value();
            assert!(!emit(0, &unrelated));
            assert_eq!(
                self.dropped.borrow().len(),
                2,
                "only the accepted drops may mutate a playlist"
            );
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn recorded_drop_sink(dropped: RecordedDrops) -> PlaylistDropSink {
        Rc::new(move |playlist_id, playlist_name, candidates| {
            dropped
                .borrow_mut()
                .push((playlist_id, playlist_name, candidates));
        })
    }

    /// Emits the installed per-row `drop` handler the way GTK itself does.
    ///
    /// `gtk_drop_target_emit_drop` calls `g_signal_emit_by_name(target,
    /// "drop", value, x, y, &ret)` passing the payload-typed `GValue*`; the
    /// signal's declared `G_TYPE_VALUE` parameter collect copies that
    /// payload into the argument `GValue`, so the connected handler receives
    /// exactly the `GValue` GTK delivered. glib-rs' argument validation
    /// requires each emitted argument to carry the declared parameter type
    /// itself and rejects the payload-typed value GTK's own C emission
    /// passes, so the payload travels in the declared boxed `G_TYPE_VALUE`
    /// container instead ([`glib::BoxedValue`]): validation accepts the
    /// declared type, and the same `G_TYPE_VALUE` collect step unboxes it,
    /// handing the installed `connect_drop` handler the identical payload
    /// `GValue` — via `g_signal_emitv`, the same C emission machinery
    /// `g_signal_emit_by_name` drives.
    #[cfg(not(target_os = "macos"))]
    fn emit_installed_drop(drop_target: &gtk::DropTarget, value: &glib::Value) -> bool {
        let boxed = glib::BoxedValue(value.clone());
        drop_target.emit_by_name::<bool>("drop", &[&boxed, &1.0f64, &1.0f64])
    }

    #[cfg(not(target_os = "macos"))]
    fn realize_drop_list_view(
        selection: gtk::SingleSelection,
        drop_context: PlaylistRowDropContext,
    ) -> (gtk::Window, Rc<RefCell<Vec<gtk::ListItem>>>) {
        let recorded: Rc<RefCell<Vec<gtk::ListItem>>> = Rc::new(RefCell::new(Vec::new()));
        let factory = gtk::SignalListItemFactory::new();
        {
            let recorded = Rc::clone(&recorded);
            factory.connect_setup(move |_, item| {
                let list_item = item
                    .downcast_ref::<gtk::ListItem>()
                    .expect("factory list item")
                    .clone();
                let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                list_item.set_child(Some(&row_box));
                attach_playlist_drop_target(&row_box, &list_item, &drop_context);
                recorded.borrow_mut().push(list_item);
            });
        }

        let expected_rows = selection.n_items() as usize;
        let list_view = gtk::ListView::new(Some(selection), Some(factory));
        let window = gtk::Window::builder().child(&list_view).build();
        window.present();
        // Item binding happens on the view's frame clock after the window
        // maps, so a single pending-event sweep can exit before the factory
        // has seen every row: pump until all rows are recorded, yielding to
        // the frame clock between sweeps (bounded, then asserted by the
        // caller's position check).
        let context = glib::MainContext::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while recorded.borrow().len() < expected_rows && std::time::Instant::now() < deadline {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        while context.pending() {
            context.iteration(false);
        }
        (window, recorded)
    }

    #[cfg(not(target_os = "macos"))]
    fn realized_drop_rows(recorded: &Rc<RefCell<Vec<gtk::ListItem>>>) -> Vec<RealizedDropRow> {
        let mut rows = recorded
            .borrow()
            .iter()
            .map(|list_item| {
                let row_box = list_item
                    .child()
                    .and_downcast::<gtk::Box>()
                    .expect("row must carry the sidebar row box");
                RealizedDropRow {
                    position: list_item.position(),
                    list_item: list_item.clone(),
                    drop_target: row_drop_target(&row_box),
                }
            })
            .collect::<Vec<_>>();
        rows.sort_by_key(|row| row.position);
        rows
    }

    #[cfg(not(target_os = "macos"))]
    fn row_drop_target(row_box: &gtk::Box) -> gtk::DropTarget {
        let controllers = row_box.observe_controllers();
        (0..controllers.n_items())
            .find_map(|index| controllers.item(index).and_downcast::<gtk::DropTarget>())
            .expect("the row must carry the production playlist drop target")
    }

    #[cfg(not(target_os = "macos"))]
    fn drag_candidates(ids: [&str; 2]) -> Vec<PlaylistAddCandidate> {
        ids.into_iter()
            .map(|id| {
                PlaylistAddCandidate::Local(MediaKey::new(
                    SourceId::local(),
                    TrackId::new(id).expect("track id"),
                ))
            })
            .collect()
    }

    #[cfg(not(target_os = "macos"))]
    fn regular_playlist_source() -> SourceObject {
        use crate::local::playlist_sidebar::{PlaylistSidebarEntry, PlaylistSidebarKind};
        SourceObject::playlist_entry(&PlaylistSidebarEntry::new(
            "regular-id",
            "My Mix",
            PlaylistSidebarKind::EditableRegular,
        ))
    }

    #[cfg(not(target_os = "macos"))]
    fn second_regular_playlist_source() -> SourceObject {
        use crate::local::playlist_sidebar::{PlaylistSidebarEntry, PlaylistSidebarKind};
        SourceObject::playlist_entry(&PlaylistSidebarEntry::new(
            "other-regular-id",
            "Road Trip",
            PlaylistSidebarKind::EditableRegular,
        ))
    }

    #[cfg(not(target_os = "macos"))]
    fn smart_playlist_source() -> SourceObject {
        use crate::local::playlist_sidebar::{PlaylistSidebarEntry, PlaylistSidebarKind};
        SourceObject::playlist_entry(&PlaylistSidebarEntry::new(
            "smart-id",
            "Smart Mix",
            PlaylistSidebarKind::EditableSmart,
        ))
    }

    #[cfg(not(target_os = "macos"))]
    fn header_source() -> SourceObject {
        use crate::ui::objects::HeaderKind;
        SourceObject::header("Playlists", HeaderKind::Playlists)
    }
}
