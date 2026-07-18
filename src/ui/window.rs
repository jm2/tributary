//! Main application window — assembles all UI components and bridges
//! the background library engine, the GStreamer player, and the OS
//! media controls to the GTK main thread.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use tracing::{info, warn};

use crate::audio::local_output::LocalOutput;
use crate::audio::output::AudioOutput;
use crate::audio::{PlayerEvent, PlayerState};
use crate::desktop_integration::MediaAction;
use crate::local::engine::{
    LibraryEngine, LibraryEvent, RootReauthorizationOutcome, RootReauthorizationRequest,
};
use crate::ui::header_bar::RepeatMode;

use super::browser;
use super::header_bar;
use super::objects::{SourceObject, TrackObject};
use super::output_dialogs::{load_saved_outputs, show_add_output_dialog};
use super::persistence::{
    extract_hwnd, load_css, load_repeat_mode, load_shuffle, load_window_geometry,
    restore_sort_state, save_repeat_mode, save_shuffle, save_sort_state, save_window_geometry,
};
use super::playback::{
    advance_track, advance_track_from_user, format_ms, play_or_start, play_track_at,
    previous_track_from_user, refresh_projected_library_uris, replay_current, stop_playback,
    toggle_or_start, BufferingTracker, PlaybackContext, PlaybackSession, QueueTrackRefresh,
    PLAYLIST_SOURCE_PREFIX,
};
use super::preferences;
use super::root_trust;
use super::server_dialogs::{load_saved_servers, remove_saved_server, show_add_server_dialog};
use super::sidebar;
use super::source_navigation::{
    ConnectionIntentKind, PendingConnection, SourceNavigation, SourceRequest,
};
use super::tracklist;
use super::window_state::WindowState;

/// Default window dimensions.
const DEFAULT_WIDTH: i32 = 1400;
const DEFAULT_HEIGHT: i32 = 850;

/// Sidebar paned default position (px from left).
const SIDEBAR_POS: i32 = 200;

/// Browser paned default position (px from top of right content area).
const BROWSER_POS: i32 = 220;

/// If the user presses Previous when more than this many ms into a track,
/// restart the current track instead of going back.
const PREV_RESTART_THRESHOLD_MS: u64 = 3000;

/// User trust decisions are serialized by the engine; this bounded queue
/// prevents a stalled engine from accumulating unbounded confirmations.
const LIBRARY_COMMAND_CAPACITY: usize = 16;

type SharedAudioOutput = Rc<RefCell<Box<dyn AudioOutput>>>;
type PlaybackUiReset = Rc<dyn Fn()>;
type SourcePlaybackInvalidator = Rc<dyn Fn(&str)>;

fn configured_server_url(variable: &'static str) -> Option<String> {
    let raw = std::env::var(variable).ok()?;
    match crate::http_security::parse_base_url(&raw) {
        Ok(url) => Some(url.to_string()),
        Err(error) => {
            // The rejected value may itself contain a password/token. Log only
            // the fixed validation category and the non-secret variable name.
            warn!(variable, error, "Ignoring invalid configured server URL");
            None
        }
    }
}

fn remote_source_id(backend_type: &str, server_url: &str) -> crate::architecture::SourceId {
    let parsed = crate::http_security::parse_base_url(server_url)
        .expect("configured and discovered remote URLs are prevalidated");
    crate::architecture::SourceId::remote(backend_type, &parsed)
        .expect("supported remote backend produces a stable source ID")
}

#[derive(Clone, Copy)]
struct EnvironmentConnectionAttempt {
    source_id: crate::architecture::SourceId,
}

/// Add an environment-configured row or reuse the saved/discovered owner of
/// the same canonical `(backend, endpoint)` pair.
///
/// Saved rows are loaded first, so their persisted identity wins over the
/// deterministic environment identity. The returned ID must be used by the
/// connection attempt; recomputing it from the URL would split one logical
/// source across two registry owners.
fn upsert_environment_source(
    sources: &mut Vec<SourceObject>,
    name: &str,
    backend_type: &str,
    server_url: &str,
) -> EnvironmentConnectionAttempt {
    if let Some(source) = sources.iter().find(|source| {
        super::server_dialogs::same_remote_endpoint(
            &source.backend_type(),
            &source.server_url(),
            backend_type,
            server_url,
        )
    }) {
        let source_id = source
            .source_id()
            .expect("validated remote source has a stable identity");
        source.set_connecting(true);
        return EnvironmentConnectionAttempt { source_id };
    }

    ensure_category_header_vec(sources, backend_type);
    let source = SourceObject::discovered(name, backend_type, server_url);
    let source_id = source
        .source_id()
        .unwrap_or_else(|| remote_source_id(backend_type, server_url));
    source.set_connecting(true);
    sources.push(source);
    EnvironmentConnectionAttempt { source_id }
}

fn set_remote_connecting_generation(
    store: &gtk::gio::ListStore,
    selection: &gtk::SingleSelection,
    source_id: crate::architecture::SourceId,
    generation: u64,
) {
    for index in 0..store.n_items() {
        let Some(source) = store.item(index).and_downcast::<SourceObject>() else {
            continue;
        };
        if source.source_id() != Some(source_id) {
            continue;
        }
        source.set_connecting_generation(generation);
        rebind_sidebar_source(store, selection, index, &source, true);
        return;
    }
}

#[derive(Clone)]
struct SourceReducerContext {
    source_registry: crate::source_registry::SourceRegistry,
    sidebar_store: gtk::gio::ListStore,
    sidebar_selection: gtk::SingleSelection,
    source_tracks: Rc<RefCell<HashMap<String, Vec<TrackObject>>>>,
    active_source_key: Rc<RefCell<String>>,
    source_navigation: Rc<RefCell<SourceNavigation>>,
    near_me_consent_request: Rc<RefCell<Option<SourceRequest>>>,
    pending_connection: Rc<RefCell<Option<PendingConnection>>>,
    track_store: gtk::gio::ListStore,
    master_tracks: Rc<RefCell<Vec<TrackObject>>>,
    browser_widget: gtk::Box,
    browser_state: browser::BrowserState,
    status_label: gtk::Label,
    column_view: gtk::ColumnView,
    app_config: Rc<RefCell<preferences::AppConfig>>,
    invalidate_source_playback: SourcePlaybackInvalidator,
}

#[derive(Default)]
struct SourceReducerState {
    published_catalogues: HashMap<crate::architecture::SourceId, (u64, u64)>,
    published_views: HashMap<
        (
            crate::architecture::SourceId,
            crate::architecture::ViewOrigin,
        ),
        (u64, u64),
    >,
    radio_session_epoch: Option<u64>,
    seen_failures:
        HashMap<crate::architecture::SourceId, (u64, crate::source_lifecycle::FailureCategory)>,
    seen_refresh_failures: HashMap<
        (
            crate::architecture::SourceId,
            crate::architecture::ViewOrigin,
        ),
        (u64, crate::source_lifecycle::FailureCategory),
    >,
}

struct SourceBaselinePlan {
    present_sources: HashSet<crate::architecture::SourceId>,
    hidden_sources: HashSet<crate::architecture::SourceId>,
    clear_projections: Vec<crate::architecture::SourceId>,
    clear_radio_projection: bool,
    radio_session_epoch: Option<u64>,
}

impl SourceBaselinePlan {
    fn new(
        reducer: &SourceReducerState,
        baseline: &crate::source_lifecycle::LifecycleBaseline<crate::source_registry::AcceptedView>,
        active_source_key: &str,
        radio_prerequisite_pending: bool,
        ui_owned_sources: &HashSet<crate::architecture::SourceId>,
    ) -> Self {
        let mut present_sources = HashSet::new();
        let mut catalogue_sources = HashSet::new();
        let mut hidden_sources = HashSet::new();
        for (source_id, snapshot) in &baseline.sources {
            if snapshot.visibility == crate::source_lifecycle::SourceVisibility::Hidden {
                hidden_sources.insert(*source_id);
            } else {
                present_sources.insert(*source_id);
                if *source_id != crate::architecture::SourceId::radio_browser()
                    && snapshot.catalogue.is_some()
                {
                    catalogue_sources.insert(*source_id);
                }
            }
        }

        let radio_id = crate::architecture::SourceId::radio_browser();
        // Hidden external-file registrations never publish an ordinary GTK
        // projection. A formerly visible source can still own a sidebar,
        // navigation, or pending-connection projection before its first
        // catalogue; clear only those explicit UI owners plus catalogues the
        // reducer actually published. Terminal external retirement belongs to
        // playback hooks, not observer visibility.
        let mut clear_projections: HashSet<_> = hidden_sources
            .intersection(ui_owned_sources)
            .copied()
            .collect();
        clear_projections.extend(
            reducer
                .published_catalogues
                .keys()
                .filter(|source_id| !catalogue_sources.contains(source_id))
                .copied(),
        );

        let mut clear_projections: Vec<_> = clear_projections.into_iter().collect();
        clear_projections.sort_by_key(ToString::to_string);
        let radio_snapshot = baseline
            .sources
            .iter()
            .find(|(source_id, _)| *source_id == radio_id)
            .map(|(_, snapshot)| snapshot);
        let radio_session_epoch = radio_snapshot
            .filter(|snapshot| {
                snapshot.visibility != crate::source_lifecycle::SourceVisibility::Hidden
            })
            .and_then(|snapshot| snapshot.session_epoch);
        let radio_connect_pending = radio_snapshot.is_some_and(|snapshot| {
            snapshot.visibility != crate::source_lifecycle::SourceVisibility::Hidden
                && snapshot.pending_connect.is_some()
        });
        let radio_was_projected = reducer
            .published_views
            .keys()
            .any(|(source_id, _)| *source_id == radio_id);
        let radio_is_selected = super::radio::is_radio_backend(active_source_key);
        let radio_epoch_replaced = reducer
            .radio_session_epoch
            .zip(radio_session_epoch)
            .is_some_and(|(previous, current)| previous != current);
        let radio_source_lost = radio_session_epoch.is_none()
            && !radio_connect_pending
            && !radio_prerequisite_pending
            && (reducer.radio_session_epoch.is_some() || radio_was_projected || radio_is_selected);

        Self {
            present_sources,
            hidden_sources,
            clear_projections,
            clear_radio_projection: radio_epoch_replaced || radio_source_lost,
            radio_session_epoch,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemotePublicationSelection {
    Inactive,
    AlreadyActivated,
    Select(u32),
    Reactivate(u32),
}

impl RemotePublicationSelection {
    fn activates(self) -> bool {
        self != Self::Inactive
    }

    fn apply(self, mut select: impl FnMut(u32)) {
        match self {
            Self::Inactive | Self::AlreadyActivated => {}
            Self::Select(index) => select(index),
            Self::Reactivate(index) => {
                select(gtk::INVALID_LIST_POSITION);
                select(index);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AcceptedRemotePublication {
    index: u32,
    rebound: bool,
}

impl SourceReducerContext {
    fn from_window(
        state: &WindowState,
        invalidate_source_playback: SourcePlaybackInvalidator,
    ) -> Self {
        Self {
            source_registry: state.source_registry.clone(),
            sidebar_store: state.sidebar_store.clone(),
            sidebar_selection: state.sidebar_selection.clone(),
            source_tracks: state.source_tracks.clone(),
            active_source_key: state.active_source_key.clone(),
            source_navigation: state.source_navigation.clone(),
            near_me_consent_request: state.near_me_consent_request.clone(),
            pending_connection: state.pending_connection.clone(),
            track_store: state.track_store.clone(),
            master_tracks: state.master_tracks.clone(),
            browser_widget: state.browser_widget.clone(),
            browser_state: state.browser_state.clone(),
            status_label: state.status_label.clone(),
            column_view: state.column_view.clone(),
            app_config: state.app_config.clone(),
            invalidate_source_playback,
        }
    }
}

fn sidebar_source_by_id(
    store: &gtk::gio::ListStore,
    source_id: crate::architecture::SourceId,
) -> Option<(u32, SourceObject)> {
    (0..store.n_items()).find_map(|index| {
        store
            .item(index)
            .and_downcast::<SourceObject>()
            .filter(|source| source.source_id() == Some(source_id))
            .map(|source| (index, source))
    })
}

fn rebind_remote_source(
    context: &SourceReducerContext,
    index: u32,
    source: &SourceObject,
    keep_selected: bool,
) {
    let was_selected = context.sidebar_selection.selected() == index;
    rebind_sidebar_source(
        &context.sidebar_store,
        &context.sidebar_selection,
        index,
        source,
        keep_selected,
    );
    if was_selected && !keep_selected {
        restore_visible_sidebar_selection(context);
    }
}

/// Refresh plain GTK-side source fields without letting remove/insert
/// transiently select an adjacent row and re-enter source navigation.
pub(super) fn rebind_sidebar_source(
    store: &gtk::gio::ListStore,
    selection: &gtk::SingleSelection,
    index: u32,
    source: &SourceObject,
    keep_selected: bool,
) {
    let was_selected = selection.selected() == index;
    if was_selected {
        // Prevent GtkSingleSelection from transiently selecting the adjacent
        // row while remove/insert refreshes plain GTK-side state.
        selection.set_selected(gtk::INVALID_LIST_POSITION);
    }
    store.remove(index);
    store.insert(index, source);
    if was_selected && keep_selected {
        selection.set_selected(index);
    }
}

fn source_matches_navigation_key(source: &SourceObject, key: &str) -> bool {
    if source.source_id().is_some_and(|id| id.to_string() == key) || source.source_key() == key {
        return true;
    }
    let backend = source.backend_type();
    (backend == "local" && key == "local")
        || (backend.starts_with("radio-") && backend == key)
        || (matches!(backend.as_str(), "playlist" | "smart-playlist")
            && format!(
                "{}{}",
                super::playback::PLAYLIST_SOURCE_PREFIX,
                source.playlist_id()
            ) == key)
}

/// Restore a selection by stable navigation identity rather than a list
/// position that category insertion/removal may have shifted asynchronously.
pub(super) fn select_sidebar_source_key(
    store: &gtk::gio::ListStore,
    selection: &gtk::SingleSelection,
    key: &str,
) -> bool {
    let Some(index) = (0..store.n_items()).find(|index| {
        store
            .item(*index)
            .and_downcast_ref::<SourceObject>()
            .is_some_and(|source| source_matches_navigation_key(source, key))
    }) else {
        return false;
    };
    if selection.selected() != index {
        selection.set_selected(index);
    }
    true
}

fn restore_visible_sidebar_selection(context: &SourceReducerContext) {
    let key = context.active_source_key.borrow().clone();
    if !select_sidebar_source_key(&context.sidebar_store, &context.sidebar_selection, &key) {
        select_sidebar_source_key(&context.sidebar_store, &context.sidebar_selection, "local");
    }
}

fn remote_backend_label(source: Option<&SourceObject>) -> &'static str {
    match source.map(SourceObject::backend_type).as_deref() {
        Some("subsonic") => "Subsonic",
        Some("jellyfin") => "Jellyfin",
        Some("plex") => "Plex",
        Some("daap") => "DAAP",
        _ => "Remote",
    }
}

fn remote_failure_category(
    category: crate::source_lifecycle::FailureCategory,
) -> super::source_connect::RemoteFailureCategory {
    use super::source_connect::RemoteFailureCategory;
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

fn apply_local_navigation_fallback(
    navigation: &mut SourceNavigation,
    active_source_key: &mut String,
    retired_key: &str,
) -> bool {
    if active_source_key != retired_key {
        return false;
    }
    navigation.select("local");
    *active_source_key = "local".to_string();
    true
}

fn display_local_fallback(context: &SourceReducerContext, retired_key: &str) {
    // Drop both RefCell guards before changing GtkSingleSelection: its signal
    // handlers run synchronously and may update these same navigation cells.
    let changed = {
        let mut navigation = context.source_navigation.borrow_mut();
        let mut active_source_key = context.active_source_key.borrow_mut();
        apply_local_navigation_fallback(&mut navigation, &mut active_source_key, retired_key)
    };
    if !changed {
        return;
    }
    select_sidebar_source_key(&context.sidebar_store, &context.sidebar_selection, "local");
    if super::radio::is_radio_backend(retired_key) {
        super::radio::apply_radio_columns(&context.column_view, false);
        let config = context.app_config.borrow();
        preferences::apply_column_visibility(&context.column_view, &config.visible_columns);
        preferences::update_browser_visibility(&context.browser_widget, &config.browser_views);
    }
    let local_tracks = context
        .source_tracks
        .borrow()
        .get("local")
        .cloned()
        .unwrap_or_default();
    display_tracks(
        &local_tracks,
        &context.track_store,
        &context.master_tracks,
        &context.browser_widget,
        &context.browser_state,
        &context.status_label,
        &context.column_view,
    );
}

fn clear_remote_projection(
    context: &SourceReducerContext,
    source_id: crate::architecture::SourceId,
) {
    let source_key = source_id.to_string();
    (context.invalidate_source_playback)(&source_key);
    context
        .source_navigation
        .borrow_mut()
        .invalidate_key(&source_key);
    context.source_tracks.borrow_mut().remove(&source_key);
    display_local_fallback(context, &source_key);
}

/// Revoke every Radio-Browser projection together because all three lanes
/// share one source epoch and one playback authority.
fn clear_radio_projections(context: &SourceReducerContext) {
    let source_id = crate::architecture::SourceId::radio_browser();
    (context.invalidate_source_playback)(&source_id.to_string());

    let active_radio_key = {
        let active = context.active_source_key.borrow();
        super::radio::is_radio_backend(&active).then(|| active.clone())
    };
    for key in [
        super::radio::TOP_CLICK_SOURCE_KEY,
        super::radio::TOP_VOTE_SOURCE_KEY,
        super::radio::NEARME_SOURCE_KEY,
    ] {
        context.source_navigation.borrow_mut().invalidate_key(key);
        context.source_tracks.borrow_mut().remove(key);
    }

    // Include the selected exact view even before its first accepted refresh.
    // Otherwise source-construction failure/loss would leave a blank radio
    // projection selected indefinitely merely because no cache existed yet.
    if let Some(active_radio_key) = active_radio_key {
        display_local_fallback(context, &active_radio_key);
    }
}

fn clear_remote_pending_intent(
    context: &SourceReducerContext,
    source_id: crate::architecture::SourceId,
) {
    let source_key = source_id.to_string();
    let pending = {
        let mut slot = context.pending_connection.borrow_mut();
        if slot
            .as_ref()
            .is_some_and(|pending| pending.source_key() == source_key)
        {
            slot.take()
        } else {
            None
        }
    };
    if pending.as_ref().is_some_and(|pending| {
        context
            .source_navigation
            .borrow()
            .is_current(pending.request())
    }) {
        // A remote intent can be current while the previous projection stays
        // visible during authentication. Return navigation ownership to that
        // visible projection without manufacturing a stale remote request.
        context
            .source_navigation
            .borrow_mut()
            .select(context.active_source_key.borrow().clone());
    }
}

fn reconcile_cancelled_remote_intent(
    context: &SourceReducerContext,
    source_id: crate::architecture::SourceId,
    generation: u64,
) {
    let pending_was_current = context
        .pending_connection
        .borrow()
        .as_ref()
        .filter(|pending| pending.matches_lifecycle(source_id, generation))
        .is_some_and(|pending| {
            context
                .source_navigation
                .borrow()
                .is_current(pending.request())
        });
    if !context
        .pending_connection
        .borrow()
        .as_ref()
        .is_some_and(|pending| pending.matches_lifecycle(source_id, generation))
    {
        return;
    }
    *context.pending_connection.borrow_mut() = None;

    if let Some((index, source)) = sidebar_source_by_id(&context.sidebar_store, source_id) {
        if source.clear_connecting_generation(generation) {
            rebind_remote_source(context, index, &source, source.connected());
        }
    }
    if pending_was_current {
        restore_visible_sidebar_selection(context);
    }
    tracing::debug!(%source_id, generation, "Remote connection intent was cancelled");
}

fn reconcile_remote_failure(
    context: &SourceReducerContext,
    source_id: crate::architecture::SourceId,
    generation: u64,
    category: crate::source_lifecycle::FailureCategory,
    show_status: bool,
) {
    let row = sidebar_source_by_id(&context.sidebar_store, source_id);
    let pending = context
        .pending_connection
        .borrow()
        .as_ref()
        .filter(|pending| pending.matches_lifecycle(source_id, generation))
        .cloned();
    let pending_was_current = pending.as_ref().is_some_and(|pending| {
        context
            .source_navigation
            .borrow()
            .is_current(pending.request())
    });
    let passwordless_reprompt = pending.as_ref().is_some_and(|pending| {
        pending.intent_kind_for(source_id, generation)
            == Some(ConnectionIntentKind::PasswordlessDaap)
            && category == crate::source_lifecycle::FailureCategory::AuthenticationRejected
    });

    if pending.is_some() {
        *context.pending_connection.borrow_mut() = None;
    }

    if let Some((index, source)) = row.as_ref() {
        if source.clear_connecting_generation(generation) {
            if passwordless_reprompt {
                source.set_requires_password(true);
            }
            rebind_remote_source(context, *index, source, source.connected());
        }
    }

    if pending_was_current {
        if passwordless_reprompt {
            if let Some((index, _)) = sidebar_source_by_id(&context.sidebar_store, source_id) {
                // The exact passwordless intent was rejected. Re-fire selection
                // only after requires_password=true is visible, so the normal
                // credential dialog owns the retry.
                context.sidebar_selection.set_selected(index);
            }
        } else {
            restore_visible_sidebar_selection(context);
        }
    }

    let ui_category = remote_failure_category(category);
    let backend = if source_id == crate::architecture::SourceId::radio_browser() {
        "Radio-Browser"
    } else {
        remote_backend_label(row.as_ref().map(|(_, source)| source))
    };
    tracing::error!(
        %source_id,
        generation,
        category = ui_category.as_str(),
        "Source connection failed"
    );
    if show_status {
        context
            .status_label
            .set_text(&ui_category.user_message(backend));
    }
}

fn reconcile_radio_refresh_failure(
    context: &SourceReducerContext,
    view: &crate::architecture::ViewOrigin,
    generation: u64,
    category: crate::source_lifecycle::FailureCategory,
) {
    let Some(source_key) = super::radio::radio_source_key(view) else {
        return;
    };
    tracing::error!(
        source = %crate::architecture::SourceId::radio_browser(),
        ?view,
        generation,
        category = ?category,
        "Radio-Browser view refresh failed"
    );
    if *context.active_source_key.borrow() == source_key
        && context.source_navigation.borrow().is_key(source_key)
    {
        // A failed refresh is not an accepted empty feed. Preserve any last
        // accepted rows in the cache/tracklist and expose a retryable error.
        let category = remote_failure_category(category);
        context
            .status_label
            .set_text(&category.user_message("Radio-Browser"));
    }
}

fn selected_radio_projection_owns_status(
    active_source_key: &str,
    navigation: &SourceNavigation,
) -> bool {
    super::radio::is_radio_backend(active_source_key) && navigation.is_key(active_source_key)
}

fn current_near_me_prerequisite(
    active_source_key: &str,
    navigation: &SourceNavigation,
    pending: Option<&SourceRequest>,
) -> bool {
    active_source_key == super::radio::NEARME_SOURCE_KEY
        && pending.is_some_and(|request| navigation.is_current(request))
}

fn publish_radio_view(
    context: &SourceReducerContext,
    view: &crate::architecture::ViewOrigin,
    accepted: &crate::source_lifecycle::AcceptedSnapshot<crate::source_registry::AcceptedView>,
) {
    let Some(source_key) = super::radio::radio_source_key(view) else {
        return;
    };
    let objects: Vec<TrackObject> = accepted
        .value
        .tracks()
        .iter()
        .map(|track| arch_remote_track_to_object(track, accepted.session_epoch))
        .collect();
    context
        .source_tracks
        .borrow_mut()
        .insert(source_key.to_string(), objects.clone());

    if *context.active_source_key.borrow() == source_key
        && context.source_navigation.borrow().is_key(source_key)
    {
        // This deliberately publishes an accepted empty feed as an empty
        // tracklist. Refresh failures take the separate stale-preserving path.
        display_tracks(
            &objects,
            &context.track_store,
            &context.master_tracks,
            &context.browser_widget,
            &context.browser_state,
            &context.status_label,
            &context.column_view,
        );
    }
}

fn reconcile_source_baseline(
    context: &SourceReducerContext,
    reducer: &mut SourceReducerState,
    baseline: crate::source_lifecycle::LifecycleBaseline<crate::source_registry::AcceptedView>,
) {
    let mut ui_owned_sources = HashSet::new();
    for index in 0..context.sidebar_store.n_items() {
        if let Some(source_id) = context
            .sidebar_store
            .item(index)
            .and_downcast::<SourceObject>()
            .and_then(|source| source.source_id())
        {
            ui_owned_sources.insert(source_id);
        }
    }
    if let Some(source_id) = context
        .pending_connection
        .borrow()
        .as_ref()
        .and_then(|pending| pending.source_key().parse().ok())
    {
        ui_owned_sources.insert(source_id);
    }
    if let Ok(source_id) = context.active_source_key.borrow().parse() {
        ui_owned_sources.insert(source_id);
    }

    let radio_failure_was_selected = {
        let active = context.active_source_key.borrow();
        selected_radio_projection_owns_status(&active, &context.source_navigation.borrow())
    };
    let radio_prerequisite_pending = {
        let active = context.active_source_key.borrow().clone();
        let pending = context.near_me_consent_request.borrow().clone();
        current_near_me_prerequisite(
            &active,
            &context.source_navigation.borrow(),
            pending.as_ref(),
        )
    };
    let SourceBaselinePlan {
        present_sources,
        hidden_sources,
        clear_projections,
        clear_radio_projection,
        radio_session_epoch,
    } = SourceBaselinePlan::new(
        reducer,
        &baseline,
        &context.active_source_key.borrow(),
        radio_prerequisite_pending,
        &ui_owned_sources,
    );

    // Projection loss is authoritative before any plain row-state rebind.
    // In particular, a selected retained passwordless DAAP row must already
    // have fallen back to Local before connected=false causes remove/insert;
    // otherwise stable-key restoration can reselect it and start a new login.
    for source_id in clear_projections {
        reducer.published_catalogues.remove(&source_id);
        if hidden_sources.contains(&source_id) {
            reducer.seen_failures.remove(&source_id);
            clear_remote_pending_intent(context, source_id);
        }
        clear_remote_projection(context, source_id);
    }
    if clear_radio_projection {
        clear_radio_projections(context);
        reducer.published_views.retain(|(source_id, _), _| {
            *source_id != crate::architecture::SourceId::radio_browser()
        });
        reducer.seen_refresh_failures.retain(|(source_id, _), _| {
            *source_id != crate::architecture::SourceId::radio_browser()
        });
    }
    reducer.radio_session_epoch = radio_session_epoch;

    for (source_id, snapshot) in baseline.sources {
        if hidden_sources.contains(&source_id) {
            continue;
        }

        let connect_failure = snapshot.failure.filter(|failure| {
            failure.failure.operation() == crate::source_lifecycle::FailureOperation::Connect
        });
        if let Some(failure) = connect_failure {
            let identity = (failure.correlation.generation, failure.failure.category());
            if reducer.seen_failures.get(&source_id) != Some(&identity) {
                reconcile_remote_failure(
                    context,
                    source_id,
                    failure.correlation.generation,
                    failure.failure.category(),
                    source_id != crate::architecture::SourceId::radio_browser()
                        || radio_failure_was_selected,
                );
                reducer.seen_failures.insert(source_id, identity);
            }
        } else {
            reducer.seen_failures.remove(&source_id);
        }

        let pending_generation = {
            context
                .pending_connection
                .borrow()
                .as_ref()
                .and_then(|pending| pending.lifecycle_generation_for(source_id))
        };
        if let Some(generation) = pending_generation {
            let generation_still_owned = snapshot.pending_connect == Some(generation)
                || snapshot
                    .catalogue
                    .as_ref()
                    .is_some_and(|catalogue| catalogue.generation == generation)
                || connect_failure
                    .is_some_and(|failure| failure.correlation.generation == generation);
            if !generation_still_owned {
                reconcile_cancelled_remote_intent(context, source_id, generation);
            }
        }

        if let Some((index, source)) = sidebar_source_by_id(&context.sidebar_store, source_id) {
            let connected = snapshot.session_epoch.is_some();
            let manually_added = snapshot
                .provenance
                .contains(crate::source_lifecycle::SourceProvenance::Saved);
            let connected_changed = source.connected() != connected;
            if connected_changed {
                source.set_connected(connected);
            }
            let manually_added_changed = source.manually_added() != manually_added;
            if manually_added_changed {
                source.set_manually_added(manually_added);
            }
            let connecting_changed = if let Some(generation) = snapshot.pending_connect {
                if source.connecting_generation() != Some(generation) {
                    source.set_connecting_generation(generation);
                    true
                } else {
                    false
                }
            } else if let Some(generation) = source.connecting_generation() {
                source.clear_connecting_generation(generation);
                true
            } else {
                false
            };
            let changed = connected_changed || manually_added_changed || connecting_changed;
            if changed {
                let ui_pending = context
                    .pending_connection
                    .borrow()
                    .as_ref()
                    .is_some_and(|pending| pending.source_key() == source_id.to_string());
                rebind_remote_source(
                    context,
                    index,
                    &source,
                    connected || snapshot.pending_connect.is_some() || ui_pending,
                );
            }
        }

        if let Some(catalogue) = (source_id != crate::architecture::SourceId::radio_browser())
            .then_some(snapshot.catalogue)
            .flatten()
        {
            let identity = (catalogue.generation, catalogue.session_epoch);
            if reducer.published_catalogues.get(&source_id) != Some(&identity) {
                if reducer
                    .published_catalogues
                    .get(&source_id)
                    .is_some_and(|(_, previous_epoch)| *previous_epoch != catalogue.session_epoch)
                {
                    let source_key = source_id.to_string();
                    (context.invalidate_source_playback)(&source_key);
                    context.source_tracks.borrow_mut().remove(&source_key);
                }
                let objects: Vec<TrackObject> = catalogue
                    .value
                    .tracks()
                    .iter()
                    .map(|track| arch_remote_track_to_object(track, catalogue.session_epoch))
                    .collect();
                publish_remote_library(
                    source_id,
                    catalogue.generation,
                    objects,
                    &context.source_tracks,
                    &context.sidebar_store,
                    &context.pending_connection,
                    &context.sidebar_selection,
                    &context.active_source_key,
                    &context.source_navigation,
                    &context.track_store,
                    &context.master_tracks,
                    &context.browser_widget,
                    &context.browser_state,
                    &context.status_label,
                    &context.column_view,
                );
                reducer.published_catalogues.insert(source_id, identity);
            }
        }

        if source_id == crate::architecture::SourceId::radio_browser() {
            for (view, accepted) in &snapshot.views {
                if super::radio::radio_source_key(view).is_none() {
                    continue;
                }
                let key = (source_id, view.clone());
                let identity = (accepted.generation, accepted.session_epoch);
                if reducer.published_views.get(&key) == Some(&identity) {
                    continue;
                }
                publish_radio_view(context, view, accepted);
                reducer.published_views.insert(key, identity);
            }

            let mut live_failures = HashSet::new();
            for (lane, failure) in &snapshot.refresh_failures {
                let crate::source_lifecycle::RefreshLane::View(view) = lane else {
                    continue;
                };
                if super::radio::radio_source_key(view).is_none() {
                    continue;
                }
                let key = (source_id, view.clone());
                live_failures.insert(key.clone());
                let identity = (failure.correlation.generation, failure.failure.category());
                if reducer.seen_refresh_failures.get(&key) == Some(&identity) {
                    continue;
                }
                reconcile_radio_refresh_failure(
                    context,
                    view,
                    failure.correlation.generation,
                    failure.failure.category(),
                );
                reducer.seen_refresh_failures.insert(key, identity);
            }
            reducer
                .seen_refresh_failures
                .retain(|key, _| key.0 != source_id || live_failures.contains(key));
        }
    }
    reducer
        .seen_failures
        .retain(|source_id, _| present_sources.contains(source_id));

    // A missing baseline row is authoritative proof that all provenance and
    // retirement work for that source was pruned. Remove any stale remote UI
    // row; Found/Add/environment publishers claim before inserting a row, so a
    // legitimate new row cannot fall into this branch.
    let mut index = context.sidebar_store.n_items();
    while index > 0 {
        index -= 1;
        let Some(source) = context
            .sidebar_store
            .item(index)
            .and_downcast::<SourceObject>()
        else {
            continue;
        };
        let backend = source.backend_type();
        if !matches!(backend.as_str(), "subsonic" | "jellyfin" | "plex" | "daap") {
            continue;
        }
        let Some(source_id) = source.source_id() else {
            continue;
        };
        if present_sources.contains(&source_id) {
            continue;
        }
        clear_remote_pending_intent(context, source_id);
        let backend = source.backend_type();
        if context.sidebar_selection.selected() == index {
            select_sidebar_source_key(&context.sidebar_store, &context.sidebar_selection, "local");
        }
        context.sidebar_store.remove(index);
        clear_remote_projection(context, source_id);
        remove_empty_category_header(&context.sidebar_store, category_for_backend(&backend));
    }

    if baseline.shutting_down {
        tracing::debug!(
            revision = baseline.revision,
            "Source lifecycle reducer observed shutdown gate"
        );
    }
}

fn setup_source_lifecycle_reducer(
    state: &WindowState,
    mut invalidations: tokio::sync::watch::Receiver<u64>,
    invalidate_source_playback: SourcePlaybackInvalidator,
) {
    let context = SourceReducerContext::from_window(state, invalidate_source_playback);
    glib::MainContext::default().spawn_local(async move {
        let mut reducer = SourceReducerState::default();
        let baseline = context.source_registry.snapshot_all();
        let mut revision = baseline.revision;
        let shutting_down = baseline.shutting_down;
        reconcile_source_baseline(&context, &mut reducer, baseline);
        if shutting_down {
            return;
        }

        loop {
            // Mark the newest watch value seen only after comparing it with
            // our atomic baseline. This closes the subscribe/baseline race:
            // an invalidation arriving between those operations is either in
            // the baseline or remains strictly newer here.
            let observed = *invalidations.borrow_and_update();
            if observed <= revision && invalidations.changed().await.is_err() {
                return;
            }

            let baseline = context.source_registry.snapshot_all();
            revision = baseline.revision;
            let shutting_down = baseline.shutting_down;
            reconcile_source_baseline(&context, &mut reducer, baseline);
            if shutting_down {
                return;
            }
        }
    });
}

/// Build and present the main Tributary window.
pub fn build_window(
    app: &adw::Application,
    rt_handle: tokio::runtime::Handle,
    engine_tx: async_channel::Sender<LibraryEvent>,
    engine_rx: async_channel::Receiver<LibraryEvent>,
) {
    info!("Building main window (Phase 4 — audio + desktop integration)");

    // Parse external source identities once, before they can be logged,
    // published to GTK, or handed to a connection registry. Invalid values
    // may contain credentials, so `configured_server_url` never returns or
    // formats them in an error.
    let subsonic_env = match (
        configured_server_url("SUBSONIC_URL"),
        std::env::var("SUBSONIC_USER"),
        std::env::var("SUBSONIC_PASS"),
    ) {
        (Some(url), Ok(user), Ok(pass)) => Some((url, user, pass)),
        _ => None,
    };
    let jellyfin_env = match (
        configured_server_url("JELLYFIN_URL"),
        std::env::var("JELLYFIN_API_KEY"),
        std::env::var("JELLYFIN_USER_ID"),
    ) {
        (Some(url), Ok(api_key), Ok(user_id)) => Some((url, api_key, user_id)),
        _ => None,
    };
    let plex_env = match (
        configured_server_url("PLEX_URL"),
        std::env::var("PLEX_TOKEN"),
    ) {
        (Some(url), Ok(token)) => Some((url, token)),
        _ => None,
    };
    let daap_env =
        configured_server_url("DAAP_URL").map(|url| (url, std::env::var("DAAP_PASSWORD").ok()));

    let source_registry = crate::source_registry::SourceRegistry::new(rt_handle.clone());
    // Subscribe before the first provenance claim or constructor. The GTK
    // reducer takes an atomic baseline later, then discards queued revisions
    // already represented by that baseline.
    let source_invalidations = source_registry.subscribe_invalidations();
    let remote_provenance = crate::source_registry::ProvenanceClaims::default();

    // ── Load and apply persisted preferences ─────────────────────────
    let app_config: Rc<RefCell<preferences::AppConfig>> =
        Rc::new(RefCell::new(preferences::load_config()));

    // ── Load custom CSS ──────────────────────────────────────────────
    load_css();

    // ── Sidebar sources ────────────────────────────────────────────────
    let sources = super::dummy_data::build_sources();
    let mut sources = sources;

    // Load manually-added servers from servers.json.
    let saved_servers = load_saved_servers();
    for entry in &saved_servers {
        ensure_category_header_vec(&mut sources, &entry.server_type);
        let src =
            SourceObject::manual(&entry.name, &entry.server_type, &entry.url, entry.source_id);
        let claimed = remote_provenance.ensure(
            &source_registry,
            entry.source_id,
            crate::source_lifecycle::SourceProvenance::Saved,
            "saved-config",
        );
        debug_assert!(claimed, "new lifecycle registry accepts saved provenance");
        sources.push(src);
        info!(
            name = %entry.name,
            backend = %entry.server_type,
            "Loaded saved server from servers.json"
        );
    }

    // If env vars are set, add pre-configured remote server entries
    // under their respective category headers.
    let subsonic_env_attempt = subsonic_env.as_ref().map(|(url, _user, _pass)| {
        upsert_environment_source(&mut sources, "Subsonic (env)", "subsonic", url)
    });
    if let Some(attempt) = subsonic_env_attempt.as_ref() {
        let claimed = remote_provenance.ensure(
            &source_registry,
            attempt.source_id,
            crate::source_lifecycle::SourceProvenance::Environment,
            "environment:SUBSONIC_URL",
        );
        debug_assert!(
            claimed,
            "new lifecycle registry accepts environment provenance"
        );
        info!("Subsonic server configured via env vars");
    }

    let jellyfin_env_attempt = jellyfin_env.as_ref().map(|(url, _key, _uid)| {
        upsert_environment_source(&mut sources, "Jellyfin (env)", "jellyfin", url)
    });
    if let Some(attempt) = jellyfin_env_attempt.as_ref() {
        let claimed = remote_provenance.ensure(
            &source_registry,
            attempt.source_id,
            crate::source_lifecycle::SourceProvenance::Environment,
            "environment:JELLYFIN_URL",
        );
        debug_assert!(
            claimed,
            "new lifecycle registry accepts environment provenance"
        );
        info!("Jellyfin server configured via env vars");
    }

    let plex_env_attempt = plex_env
        .as_ref()
        .map(|(url, _token)| upsert_environment_source(&mut sources, "Plex (env)", "plex", url));
    if let Some(attempt) = plex_env_attempt.as_ref() {
        let claimed = remote_provenance.ensure(
            &source_registry,
            attempt.source_id,
            crate::source_lifecycle::SourceProvenance::Environment,
            "environment:PLEX_URL",
        );
        debug_assert!(
            claimed,
            "new lifecycle registry accepts environment provenance"
        );
        info!("Plex server configured via env vars");
    }

    let daap_env_attempt = daap_env
        .as_ref()
        .map(|(url, _password)| upsert_environment_source(&mut sources, "DAAP (env)", "daap", url));
    if let Some(attempt) = daap_env_attempt.as_ref() {
        let claimed = remote_provenance.ensure(
            &source_registry,
            attempt.source_id,
            crate::source_lifecycle::SourceProvenance::Environment,
            "environment:DAAP_URL",
        );
        debug_assert!(
            claimed,
            "new lifecycle registry accepts environment provenance"
        );
        info!("DAAP server configured via env vars");
    }

    // ── Header Bar with all interactive widgets ──────────────────────
    let hb = header_bar::build_header_bar();

    let scan_spinner = gtk::Spinner::builder()
        .spinning(true)
        .tooltip_text("Scanning library…")
        .build();
    hb.header.pack_end(&scan_spinner);

    // ── Load saved outputs into the output selector popover ──────────
    {
        let saved_outputs = load_saved_outputs();
        for output in &saved_outputs {
            let icon = match output.output_type.as_str() {
                "mpd" => "network-server-symbolic",
                _ => "audio-speakers-symbolic",
            };
            let row = header_bar::build_output_row(&output.name, icon, false);
            hb.output_list.append(&row);
        }
        if !saved_outputs.is_empty() {
            info!(
                count = saved_outputs.len(),
                "Loaded saved outputs from outputs.json"
            );
        }
    }

    // ── Restore persisted playback modes ─────────────────────────────
    {
        let saved_repeat = load_repeat_mode();
        hb.repeat_mode.set(saved_repeat);
        let (icon, tooltip, active) = match saved_repeat {
            RepeatMode::Off => ("media-playlist-repeat-symbolic", "Repeat: Off", false),
            RepeatMode::All => ("media-playlist-repeat-symbolic", "Repeat: All", true),
            RepeatMode::One => ("media-playlist-repeat-song-symbolic", "Repeat: One", true),
        };
        hb.repeat_button.set_icon_name(icon);
        hb.repeat_button.set_tooltip_text(Some(tooltip));
        hb.repeat_button.set_active(active);

        hb.shuffle_button.set_active(load_shuffle());
    }

    // ── Sidebar ──────────────────────────────────────────────────────
    let (
        sidebar_widget,
        sidebar_store,
        sidebar_selection,
        disconnect_rx,
        delete_rx,
        add_button,
        playlist_action_rx,
    ) = sidebar::build_sidebar(&sources);

    // ── Tracklist (starts empty — populated by FullSync) ──────────────
    let empty_tracks: Vec<TrackObject> = Vec::new();
    let (tracklist_widget, track_store, status_label, column_view, sort_model) =
        tracklist::build_tracklist(&empty_tracks);

    // ── Shared playback state ────────────────────────────────────────
    let master_tracks: Rc<RefCell<Vec<TrackObject>>> = Rc::new(RefCell::new(Vec::new()));
    let playback_session = Rc::new(RefCell::new(PlaybackSession::default()));
    let seeking = Rc::new(Cell::new(false));
    let buffering_tracker = Rc::new(BufferingTracker::default());

    // Source discovery and deletion are wired before the audio output exists.
    // Keep indirection slots so those handlers can still retire playback
    // deterministically once the output/UI are installed later in this build.
    let active_output_slot: Rc<RefCell<Option<SharedAudioOutput>>> = Rc::new(RefCell::new(None));
    let playback_ui_reset_slot: Rc<RefCell<Option<PlaybackUiReset>>> = Rc::new(RefCell::new(None));
    let invalidate_source_playback: SourcePlaybackInvalidator = {
        let playback_session = playback_session.clone();
        let active_output_slot = active_output_slot.clone();
        let playback_ui_reset_slot = playback_ui_reset_slot.clone();
        Rc::new(move |source_key| {
            if !playback_session.borrow_mut().clear_if_source(source_key) {
                return;
            }

            if let Some(active_output) = active_output_slot.borrow().as_ref().cloned() {
                active_output.borrow().stop();
            }
            if let Some(clear_ui) = playback_ui_reset_slot.borrow().as_ref().cloned() {
                clear_ui();
            }
            info!("Stopped playback owned by a retired source");
        })
    };

    // ── Connection guard ─────────────────────────────────────────────
    // Tracks which stable source identity is currently being connected, and
    // the sidebar position that was active before the connection attempt.
    // Used to (a) only auto-select on a remote sync if the source matches
    // the pending connection, and (b) revert the sidebar on failure.
    let pending_connection = Rc::new(RefCell::new(None));
    let pre_connect_selection: Rc<Cell<u32>> = Rc::new(Cell::new(1)); // default: local (index 1)

    // ── Per-source track storage ────────────────────────────────────
    // Key: "local" for the built-in local view, stable SourceId text for
    // remotes, and the explicit view/device keys documented by WindowState.
    let source_tracks: Rc<RefCell<HashMap<String, Vec<TrackObject>>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let active_source_key: Rc<RefCell<String>> = Rc::new(RefCell::new("local".to_string()));
    let source_navigation = Rc::new(RefCell::new(SourceNavigation::new("local")));
    let near_me_consent_request: Rc<RefCell<Option<SourceRequest>>> = Rc::new(RefCell::new(None));

    // ── Browser (starts empty, updated by FullSync) ──────────────────
    let track_store_for_filter = track_store.clone();
    let status_label_for_filter = status_label.clone();
    let master_for_filter = master_tracks.clone();
    let app_config_for_filter = app_config.clone();
    let on_filter = Box::new(
        move |genre: Option<String>,
              artist: Option<String>,
              album: Option<String>,
              search_text: String| {
            let master = master_for_filter.borrow();
            let search_lower = search_text.to_lowercase();
            let use_album_artist = app_config_for_filter.borrow().group_by_album_artist;
            let filtered: Vec<TrackObject> = master
                .iter()
                .filter(|t| {
                    if let Some(ref g) = genre {
                        if &t.genre() != g {
                            return false;
                        }
                    }
                    if let Some(ref a) = artist {
                        // When album-artist grouping is on, match against
                        // the album-artist tag (falling back to track artist
                        // for tracks that lack one), so selecting an album
                        // artist returns every track on that artist's albums
                        // even on compilation discs.
                        let track_aa = t.album_artist();
                        let key = if use_album_artist && !track_aa.is_empty() {
                            track_aa
                        } else {
                            t.artist()
                        };
                        if &key != a {
                            return false;
                        }
                    }
                    if let Some(ref al) = album {
                        if &t.album() != al {
                            return false;
                        }
                    }
                    // Text search filter — match across title, artist, album, genre.
                    if !search_lower.is_empty() {
                        let matches = t.title().to_lowercase().contains(&search_lower)
                            || t.artist().to_lowercase().contains(&search_lower)
                            || t.album().to_lowercase().contains(&search_lower)
                            || t.genre().to_lowercase().contains(&search_lower);
                        if !matches {
                            return false;
                        }
                    }
                    true
                })
                // Clone bumps the GObject refcount, so the same instance may
                // live in both `master_tracks` and the store.
                .cloned()
                .collect();

            // Replace the whole store in a single splice. This emits one
            // `items-changed` signal instead of N appends and keeps the rows'
            // identity. Playback navigation uses its own immutable queue and
            // is deliberately unaffected by this view mutation.
            track_store_for_filter.splice(0, track_store_for_filter.n_items(), &filtered);
            tracklist::update_status(&status_label_for_filter, &filtered);
        },
    );

    let initial_use_album_artist = app_config.borrow().group_by_album_artist;
    let (browser_widget, browser_state) =
        browser::build_browser(&empty_tracks, initial_use_album_artist, on_filter);

    // ── Right content ────────────────────────────────────────────────
    let right_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Vertical)
        .position(BROWSER_POS)
        .wide_handle(true)
        .vexpand(true)
        .hexpand(true)
        .start_child(&browser_widget)
        .end_child(&tracklist_widget)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .build();

    let main_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .position(SIDEBAR_POS)
        .wide_handle(true)
        .vexpand(true)
        .hexpand(true)
        .start_child(&sidebar_widget)
        .end_child(&right_paned)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&hb.header);
    content.append(&main_paned);

    // The root-trust flow uses non-modal status feedback after a guarded
    // confirmation. Tributary previously had no window-level toast host.
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&content));

    // Restore persisted window geometry (size + maximized state).
    let saved_geo = load_window_geometry();
    let win_width = saved_geo.as_ref().map(|g| g.width).unwrap_or(DEFAULT_WIDTH);
    let win_height = saved_geo
        .as_ref()
        .map(|g| g.height)
        .unwrap_or(DEFAULT_HEIGHT);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Tributary")
        .default_width(win_width)
        .default_height(win_height)
        .content(&toast_overlay)
        .build();

    if saved_geo.is_some_and(|g| g.is_maximized) {
        window.maximize();
    }

    // Save geometry and synchronously close the sole source lifecycle gate.
    // Its persistent barrier joins every admitted constructor and owned
    // adapter teardown, including DAAP and interactive Jellyfin logout, before
    // close() is allowed to proceed.
    let shutdown_sources = source_registry.clone();
    let shutdown_playback = playback_session.clone();
    let shutdown_started = Rc::new(Cell::new(false));
    let shutdown_complete = Rc::new(Cell::new(false));
    window.connect_close_request(move |w| {
        save_window_geometry(w);
        if shutdown_complete.get() {
            return glib::Propagation::Proceed;
        }

        if !shutdown_started.replace(true) {
            super::open_files::invalidate_admission();
            let external_source = shutdown_playback.borrow().current_external_source_id();
            if let Some(source_id) = external_source {
                shutdown_playback.borrow_mut().clear();
                let _ = shutdown_sources.retire_external(source_id);
            }
            let barrier = shutdown_sources.shutdown();
            let window = w.clone();
            let shutdown_complete = shutdown_complete.clone();
            glib::MainContext::default().spawn_local(async move {
                barrier.wait().await;
                shutdown_complete.set(true);
                window.close();
            });
        }

        glib::Propagation::Stop
    });

    // Root trust is the only UI-to-library-engine command path. The engine
    // validates every request against fresh filesystem evidence before it can
    // change persisted trust; the GTK side only queues an affirmative intent.
    let (library_command_tx, library_command_rx) = async_channel::bounded(LIBRARY_COMMAND_CAPACITY);
    let root_trust_prompts =
        root_trust::RootTrustPromptController::new(&window, &toast_overlay, library_command_tx);

    // ── Start the library engine on tokio ────────────────────────────
    // Use the configured library paths from preferences, which default
    // to the XDG / platform music directory (e.g. ~/Musique on French
    // systems) via dirs::audio_dir() with a ~/Music fallback.
    let (music_dirs, pending_root_reauthorizations) = {
        let config = app_config.borrow();
        let music_dirs = config
            .library_paths
            .iter()
            .map(std::path::PathBuf::from)
            .collect();
        let pending = config
            .pending_root_reauthorizations
            .iter()
            .map(|request| {
                RootReauthorizationRequest::new(
                    &request.request_id,
                    std::path::PathBuf::from(&request.old_path),
                    std::path::PathBuf::from(&request.new_path),
                )
            })
            .collect();
        (music_dirs, pending)
    };

    let engine_tx_clone = engine_tx.clone();
    rt_handle.spawn(async move {
        match crate::db::connection::init_db().await {
            Ok(db) => {
                let engine = LibraryEngine::new(
                    db,
                    music_dirs,
                    pending_root_reauthorizations,
                    engine_tx_clone,
                    library_command_rx,
                );
                engine.run().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to initialise database");
                let _ = engine_tx_clone
                    .send(LibraryEvent::Error(format!("Database error: {e}")))
                    .await;
            }
        }
    });

    // ── Start Subsonic backend if configured via env vars ──────────
    if let Some((url, user, pass)) = subsonic_env {
        let EnvironmentConnectionAttempt { source_id } =
            subsonic_env_attempt.expect("configured Subsonic source attempt");
        source_registry.connect_standard(
            source_id,
            |generation| {
                set_remote_connecting_generation(
                    &sidebar_store,
                    &sidebar_selection,
                    source_id,
                    generation,
                );
            },
            move || async move {
                info!("Connecting to Subsonic server...");
                crate::subsonic::SubsonicBackend::connect("Subsonic", &url, &user, &pass).await
            },
        );
    }

    // ── Start Jellyfin backend if configured via env vars ──────────
    if let Some((url, api_key, user_id)) = jellyfin_env {
        let EnvironmentConnectionAttempt { source_id } =
            jellyfin_env_attempt.expect("configured Jellyfin source attempt");
        source_registry.connect_jellyfin_api_key(
            source_id,
            |generation| {
                set_remote_connecting_generation(
                    &sidebar_store,
                    &sidebar_selection,
                    source_id,
                    generation,
                );
            },
            move || async move {
                info!("Connecting to Jellyfin server...");
                crate::jellyfin::JellyfinBackend::connect("Jellyfin", &url, &api_key, &user_id)
                    .await
            },
        );
    }

    // ── Start Plex backend if configured via env vars ──────────────
    if let Some((url, token)) = plex_env {
        let EnvironmentConnectionAttempt { source_id } =
            plex_env_attempt.expect("configured Plex source attempt");
        source_registry.connect_standard(
            source_id,
            |generation| {
                set_remote_connecting_generation(
                    &sidebar_store,
                    &sidebar_selection,
                    source_id,
                    generation,
                );
            },
            move || async move {
                info!("Connecting to Plex server...");
                crate::plex::PlexBackend::connect("Plex", &url, &token).await
            },
        );
    }

    // ── Start DAAP backend if configured via env vars ──────────────
    if let Some((url, password)) = daap_env {
        let EnvironmentConnectionAttempt { source_id } =
            daap_env_attempt.expect("configured DAAP source attempt");
        source_registry.connect_daap(
            source_id,
            |generation| {
                set_remote_connecting_generation(
                    &sidebar_store,
                    &sidebar_selection,
                    source_id,
                    generation,
                );
            },
            move || async move {
                info!("Connecting to DAAP server...");
                crate::daap::DaapBackend::login("DAAP", &url, password.as_deref()).await
            },
        );
    }

    // ── mDNS zero-config discovery ─────────────────────────────────
    super::discovery_handler::setup_discovery(
        &WindowState {
            window: window.clone(),
            rt_handle: rt_handle.clone(),
            engine_tx: engine_tx.clone(),
            source_registry: source_registry.clone(),
            remote_provenance: remote_provenance.clone(),
            track_store: track_store.clone(),
            master_tracks: master_tracks.clone(),
            source_tracks: source_tracks.clone(),
            active_source_key: active_source_key.clone(),
            source_navigation: source_navigation.clone(),
            near_me_consent_request: near_me_consent_request.clone(),
            sidebar_store: sidebar_store.clone(),
            sidebar_selection: sidebar_selection.clone(),
            browser_widget: browser_widget.clone(),
            browser_state: browser_state.clone(),
            status_label: status_label.clone(),
            column_view: column_view.clone(),
            sort_model: sort_model.clone(),
            app_config: app_config.clone(),
            pending_connection: pending_connection.clone(),
            pre_connect_selection: pre_connect_selection.clone(),
        },
        &hb.output_list,
    );

    // ── Wire "+" add-server button ──────────────────────────────────
    {
        let win = window.clone();
        let store = sidebar_store.clone();
        let selection = sidebar_selection.clone();
        let engine_tx = engine_tx.clone();
        let source_registry = source_registry.clone();
        let remote_provenance = remote_provenance.clone();
        add_button.connect_clicked(move |_| {
            show_add_server_dialog(
                &win,
                &store,
                &selection,
                &engine_tx,
                &source_registry,
                &remote_provenance,
            );
        });
    }

    // ── Wire output selector "+" button (now that window exists) ─────
    {
        let win = window.clone();
        let output_list = hb.output_list.clone();
        if let Some(popover) = hb.output_button.popover() {
            if let Some(popover_box) = popover.child().and_then(|c| c.downcast::<gtk::Box>().ok()) {
                if let Some(add_btn) = popover_box
                    .last_child()
                    .and_then(|c| c.downcast::<gtk::Button>().ok())
                {
                    add_btn.connect_clicked(move |_| {
                        show_add_output_dialog(&win, &output_list);
                    });
                }
            }
        }
    }

    // ── Manual server delete (trash) handler ────────────────────────
    {
        let source_registry = source_registry.clone();
        let remote_provenance = remote_provenance.clone();

        glib::MainContext::default().spawn_local(async move {
            while let Ok(source_key) = delete_rx.recv().await {
                info!("Manual server delete requested");
                let Ok(source_id) = source_key.parse::<crate::architecture::SourceId>() else {
                    tracing::warn!("Ignoring delete for invalid source identity");
                    continue;
                };
                // Persisted absence is the authority for releasing Saved.
                // A failed write leaves both the row and claim untouched.
                if !remove_saved_server(source_id) {
                    tracing::warn!(%source_id, "Could not persist saved server removal");
                    continue;
                }
                if !remote_provenance.release(
                    &source_registry,
                    source_id,
                    crate::source_lifecycle::SourceProvenance::Saved,
                    "saved-config",
                ) {
                    tracing::warn!(%source_id, "Saved source claim was unavailable after removal");
                }

                // The lifecycle baseline reducer owns Saved demotion, final
                // projection clearing, row removal, and active-source fallback.
            }
        });
    }

    // ── DAAP disconnect (eject) handler ─────────────────────────────
    {
        let source_registry = source_registry.clone();

        glib::MainContext::default().spawn_local(async move {
            while let Ok(source_key) = disconnect_rx.recv().await {
                info!("DAAP disconnect requested");
                let Ok(source_id) = source_key.parse::<crate::architecture::SourceId>() else {
                    tracing::warn!("Ignoring disconnect for invalid source identity");
                    continue;
                };
                if source_registry.disconnect(source_id).is_none() {
                    tracing::warn!("DAAP source lifecycle entry was unavailable");
                }
            }
        });
    }

    // ── Sidebar selection: source switching + auth dialog ───────────
    let sidebar_store_for_events = sidebar_store.clone();
    let sidebar_sel_for_events = sidebar_selection.clone();
    let pending_connection_for_events = pending_connection.clone();
    let pre_connect_selection_for_events = pre_connect_selection.clone();
    let source_connection_state = WindowState {
        window: window.clone(),
        rt_handle: rt_handle.clone(),
        engine_tx: engine_tx.clone(),
        source_registry: source_registry.clone(),
        remote_provenance: remote_provenance.clone(),
        track_store: track_store.clone(),
        master_tracks: master_tracks.clone(),
        source_tracks: source_tracks.clone(),
        active_source_key: active_source_key.clone(),
        source_navigation: source_navigation.clone(),
        near_me_consent_request: near_me_consent_request.clone(),
        sidebar_store: sidebar_store.clone(),
        sidebar_selection: sidebar_selection.clone(),
        browser_widget: browser_widget.clone(),
        browser_state: browser_state.clone(),
        status_label: status_label.clone(),
        column_view: column_view.clone(),
        sort_model: sort_model.clone(),
        app_config: app_config.clone(),
        pending_connection: pending_connection.clone(),
        pre_connect_selection: pre_connect_selection.clone(),
    };
    super::source_connect::setup_source_connect(&source_connection_state);
    setup_source_lifecycle_reducer(
        &source_connection_state,
        source_invalidations,
        invalidate_source_playback.clone(),
    );

    // ═══════════════════════════════════════════════════════════════════
    // Phase 4: Audio Player + Desktop Integration
    // ═══════════════════════════════════════════════════════════════════

    // Present the window EARLY so that the native OS surface is
    // allocated.  On Windows, souvlaki needs the HWND which only
    // exists after the window has been realized and mapped.
    window.present();
    info!("Main window presented");

    // GVolumeMonitor must stay on the GTK main thread. Its cached mount
    // metadata drives an idempotent sidebar reconciliation, while selecting a
    // device still performs filesystem walking/tag parsing on a bounded worker.
    super::removable_media::setup_removable_media(
        &source_connection_state,
        invalidate_source_playback.clone(),
    );

    // ── Create GStreamer player ──────────────────────────────────────
    let (player, player_rx) = match crate::audio::Player::new(rt_handle.clone()) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!(error = %e, "Failed to create audio player — playback disabled");
            setup_library_events(
                engine_rx,
                rt_handle.clone(),
                track_store,
                status_label,
                master_tracks,
                source_tracks,
                active_source_key,
                source_navigation.clone(),
                &browser_widget,
                browser_state,
                &column_view,
                sidebar_store_for_events,
                sidebar_sel_for_events,
                scan_spinner,
                pending_connection_for_events.clone(),
                playback_session.clone(),
                root_trust_prompts.clone(),
                app_config.clone(),
            );
            return;
        }
    };
    // Grab the event sender before wrapping in LocalOutput — needed
    // to give MpdOutput (and future outputs) a sender into the same
    // player_rx event loop.
    let event_sender = player.event_sender();

    // Wrap the raw Player in LocalOutput → Box<dyn AudioOutput>.
    let local_output = LocalOutput::new(player);
    let active_output: SharedAudioOutput = Rc::new(RefCell::new(Box::new(local_output)));
    *active_output_slot.borrow_mut() = Some(active_output.clone());
    let active_output_target = Rc::new(RefCell::new(super::output_switch::OutputTarget::Local));

    // Parking slot for the local output when an MPD output is active.
    // When switching to MPD we move the LocalOutput out of active_output
    // into this slot; when switching back we move it back.
    let parked_local: Rc<RefCell<Option<Box<dyn AudioOutput>>>> = Rc::new(RefCell::new(None));

    // Sync the volume slider to the output's persisted volume.
    hb.volume_adj.set_value(active_output.borrow().volume());

    // ── Extract native window handle (HWND on Windows) ──────────────
    let hwnd = extract_hwnd(&window);

    // ── Enable Windows 11 Snap Layout ───────────────────────────────
    // Install a WM_NCHITTEST / WM_GETMINMAXINFO subclass on the
    // top-level HWND.
    //
    // `window.present()` is supposed to allocate the native surface,
    // but in practice on Windows the surface isn't always ready by the
    // time we read it back here. If `extract_hwnd` returns None, defer
    // the install to the first `notify::is-active`, which fires once
    // the window is mapped.
    #[cfg(target_os = "windows")]
    {
        if let Some(hwnd_ptr) = hwnd {
            tracing::info!("Installing Snap Layout subclass (HWND ready at present)");
            super::win32_snap::enable_snap_layout(hwnd_ptr, (win_width - 92, 0, 46, 36));

            window.connect_default_width_notify(move |win| {
                let (w, _) = win.default_size();
                super::win32_snap::update_maximize_rect((w - 92, 0, 46, 36));
            });
        } else {
            tracing::warn!(
                "HWND not available immediately after window.present() — deferring Snap Layout install to first notify::is-active"
            );
            let installed = std::rc::Rc::new(std::cell::Cell::new(false));
            let installed_for_handler = installed.clone();
            window.connect_is_active_notify(move |w| {
                if installed_for_handler.get() {
                    return;
                }
                let Some(hwnd_ptr) = extract_hwnd(w) else {
                    return;
                };
                tracing::info!("Installing Snap Layout subclass (deferred, HWND now ready)");
                let (cw, _) = w.default_size();
                super::win32_snap::enable_snap_layout(hwnd_ptr, (cw - 92, 0, 46, 36));
                installed_for_handler.set(true);

                w.connect_default_width_notify(move |win| {
                    let (cw, _) = win.default_size();
                    super::win32_snap::update_maximize_rect((cw - 92, 0, 46, 36));
                });
            });
        }
    }

    // ── Create OS media controls ────────────────────────────────────
    //
    // The Next / Previous handlers need a `PlaybackContext`, which in
    // turn references the album-art widget, title/artist labels, and
    // the OS media controller itself. We capture the fields up-front
    // into the spawn_local closure (cloned each iteration so the
    // PlaybackContext can be built fresh per event without moving the
    // captured Rc's out of the closure).
    let media_ctrl: Rc<RefCell<Option<crate::desktop_integration::MediaController>>> =
        Rc::new(RefCell::new(None));

    // Every terminal/reset path uses this one operation. Besides resetting the
    // visible controls it invalidates delayed spinner callbacks and both local
    // and remote artwork workers before installing the idle placeholder.
    let clear_playback_ui: PlaybackUiReset = {
        let play_button = hb.play_button.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let album_art = hb.album_art.clone();
        let progress_adj = hb.progress_adj.clone();
        let position_label = hb.position_label.clone();
        let duration_label = hb.duration_label.clone();
        let seeking = seeking.clone();
        let media_ctrl = media_ctrl.clone();
        let buffering_tracker = buffering_tracker.clone();
        Rc::new(move || {
            buffering_tracker.invalidate();
            play_button.set_child(Option::<&gtk::Widget>::None);
            play_button.set_icon_name("media-playback-start-symbolic");
            title_label.set_label("Not Playing");
            title_label.set_tooltip_text(Option::<&str>::None);
            artist_label.set_label("");
            artist_label.set_tooltip_text(Option::<&str>::None);
            super::album_art::invalidate();
            album_art.set_icon_name(Some("audio-x-generic-symbolic"));
            seeking.set(true);
            progress_adj.set_value(0.0);
            progress_adj.set_upper(1.0);
            seeking.set(false);
            position_label.set_label("0:00");
            duration_label.set_label("0:00");
            if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                ctrl.set_stopped();
            }
        })
    };
    *playback_ui_reset_slot.borrow_mut() = Some(clear_playback_ui.clone());

    match crate::desktop_integration::MediaController::new(hwnd) {
        Ok((ctrl, media_rx)) => {
            *media_ctrl.borrow_mut() = Some(ctrl);

            let active_output = active_output.clone();
            let album_art = hb.album_art.clone();
            let title_label = hb.title_label.clone();
            let artist_label = hb.artist_label.clone();
            let sm = sort_model.clone();
            let active_source_key = active_source_key.clone();
            let playback_session = playback_session.clone();
            let repeat_mode = hb.repeat_mode.clone();
            let shuffle = hb.shuffle_button.clone();
            let ctrl_for_ctx = media_ctrl.clone();
            let column_view_for_keys = column_view.clone();
            let clear_playback_ui = clear_playback_ui.clone();
            let playback_rt = rt_handle.clone();
            let playback_config = app_config.clone();
            let playback_source_registry = source_registry.clone();

            glib::MainContext::default().spawn_local(async move {
                while let Ok(action) = media_rx.recv().await {
                    info!(?action, "OS media key");
                    let ctx = PlaybackContext {
                        model: sm.clone(),
                        active_source_key: active_source_key.clone(),
                        active_output: active_output.clone(),
                        album_art: album_art.clone(),
                        title_label: title_label.clone(),
                        artist_label: artist_label.clone(),
                        media_ctrl: ctrl_for_ctx.clone(),
                        session: playback_session.clone(),
                        app_config: playback_config.clone(),
                        rt_handle: playback_rt.clone(),
                        column_view: column_view_for_keys.clone(),
                        source_registry: playback_source_registry.clone(),
                    };
                    match action {
                        MediaAction::Play => {
                            if play_or_start(&ctx, shuffle.is_active()) {
                                if let Some(ref mut ctrl) = *ctrl_for_ctx.borrow_mut() {
                                    ctrl.update_playback(true);
                                }
                            }
                        }
                        MediaAction::Pause => {
                            super::open_files::invalidate_admission();
                            if playback_session
                                .borrow_mut()
                                .cancel_pending_resolution_for_retry()
                            {
                                // No media has reached the output yet. Stop is
                                // cleanup only; the cancelled resolver cannot
                                // claim the session, and the next Play resolves
                                // the protected reference again.
                                active_output.borrow().stop();
                                if let Some(ref mut ctrl) = *ctrl_for_ctx.borrow_mut() {
                                    ctrl.update_playback(false);
                                }
                            } else if playback_session.borrow().has_current() {
                                active_output.borrow().pause();
                                if let Some(ref mut ctrl) = *ctrl_for_ctx.borrow_mut() {
                                    ctrl.update_playback(false);
                                }
                            }
                        }
                        MediaAction::Toggle => {
                            toggle_or_start(&ctx, shuffle.is_active());
                        }
                        MediaAction::Stop => {
                            stop_playback(&ctx);
                            clear_playback_ui();
                        }
                        MediaAction::Next => {
                            advance_track_from_user(&ctx, repeat_mode.get(), shuffle.is_active());
                        }
                        MediaAction::Previous => {
                            super::open_files::invalidate_admission();
                            // Mirror the header-bar heuristic: if we're past
                            // the restart threshold, restart the current track.
                            let position_ms = active_output.borrow().position_ms().unwrap_or(0);
                            if position_ms > PREV_RESTART_THRESHOLD_MS {
                                active_output.borrow().seek_to(0);
                            } else {
                                let stepped = previous_track_from_user(
                                    &ctx,
                                    repeat_mode.get(),
                                    shuffle.is_active(),
                                );
                                if !stepped {
                                    active_output.borrow().seek_to(0);
                                }
                            }
                        }
                    }
                }
            });
        }
        Err(e) => {
            warn!(error = %e, "Media controls unavailable — media keys disabled");
        }
    }

    // ── Wire play/pause button ──────────────────────────────────────
    // If nothing is playing, start from track 0 (or random if shuffle).
    {
        let active_output = active_output.clone();
        let media_ctrl = media_ctrl.clone();
        let album_art = hb.album_art.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let sort_model = sort_model.clone();
        let active_source_key = active_source_key.clone();
        let playback_session = playback_session.clone();
        let shuffle = hb.shuffle_button.clone();
        let column_view_c = column_view.clone();
        let playback_rt = rt_handle.clone();
        let playback_config = app_config.clone();
        let playback_source_registry = source_registry.clone();

        hb.play_button.connect_clicked(move |_| {
            toggle_or_start(
                &PlaybackContext {
                    model: sort_model.clone(),
                    active_source_key: active_source_key.clone(),
                    active_output: active_output.clone(),
                    album_art: album_art.clone(),
                    title_label: title_label.clone(),
                    artist_label: artist_label.clone(),
                    media_ctrl: media_ctrl.clone(),
                    session: playback_session.clone(),
                    app_config: playback_config.clone(),
                    rt_handle: playback_rt.clone(),
                    column_view: column_view_c.clone(),
                    source_registry: playback_source_registry.clone(),
                },
                shuffle.is_active(),
            );
        });
    }

    // ── Persist repeat/shuffle on change ────────────────────────────
    {
        let mode = hb.repeat_mode.clone();
        hb.repeat_button.connect_clicked(move |_| {
            save_repeat_mode(mode.get());
        });
    }
    hb.shuffle_button.connect_toggled(move |btn| {
        save_shuffle(btn.is_active());
    });

    // ── Wire output selector row-click handler ──────────────────────
    {
        super::output_switch::setup_output_selector(
            &hb.output_list,
            &hb.output_button,
            &active_output,
            &parked_local,
            &active_output_target,
            &playback_session,
            clear_playback_ui.clone(),
            &event_sender,
            &hb.volume_scale,
            &rt_handle,
            &source_registry,
        );
    }

    // ── Wire volume scale ───────────────────────────────────────────
    // Throttled (trailing): a slider drag emits a burst of value-changed
    // signals, and for MPD/Chromecast outputs each set_volume spawns a
    // worker thread + connection. Collapse the burst to ~one command per
    // window; the final value always lands within the window.
    {
        let active_output = active_output.clone();
        let pending: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(None));
        let scheduled = Rc::new(Cell::new(false));
        hb.volume_adj.connect_value_changed(move |adj| {
            pending.set(Some(adj.value()));
            if scheduled.replace(true) {
                return;
            }
            let active_output = active_output.clone();
            let pending = pending.clone();
            let scheduled = scheduled.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(60), move || {
                scheduled.set(false);
                if let Some(v) = pending.take() {
                    active_output.borrow_mut().set_volume(v);
                }
            });
        });
    }

    // ── Wire progress scrubber (seek on user interaction) ───────────
    // Same trailing-throttle as the volume slider, and skip programmatic
    // position-poll updates (guarded by `seeking`) so they never seek.
    {
        let active_output = active_output.clone();
        let seeking = seeking.clone();
        let pending: Rc<Cell<Option<u64>>> = Rc::new(Cell::new(None));
        let scheduled = Rc::new(Cell::new(false));
        hb.progress_adj.connect_value_changed(move |adj| {
            if seeking.get() {
                return;
            }
            super::open_files::invalidate_admission();
            pending.set(Some(adj.value() as u64));
            if scheduled.replace(true) {
                return;
            }
            let active_output = active_output.clone();
            let pending = pending.clone();
            let seeking = seeking.clone();
            let scheduled = scheduled.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(80), move || {
                scheduled.set(false);
                // Re-check the guard: don't fire a stale seek if a
                // programmatic update is in progress when the timer lands.
                if !seeking.get() {
                    if let Some(p) = pending.take() {
                        active_output.borrow().seek_to(p);
                    }
                }
            });
        });
    }

    // ── Persist and restore column sort ────────────────────────────
    restore_sort_state(&column_view);
    if let Some(sorter) = column_view.sorter() {
        let cv = column_view.clone();
        let active_source_key = active_source_key.clone();
        sorter.connect_changed(move |_, _| {
            // Don't persist sort state while viewing a radio station: in
            // radio mode the Artist/Album columns are renamed to
            // Country/State-Province, so the saved title could never be
            // re-matched against the music-mode columns on the next launch
            // (issue #38).
            if super::radio::is_radio_backend(&active_source_key.borrow()) {
                return;
            }
            save_sort_state(&cv);
        });
    }

    // ── Wire tracklist double-click → load track ────────────────────
    {
        let active_output = active_output.clone();
        let media_ctrl = media_ctrl.clone();
        let album_art = hb.album_art.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let sm = sort_model.clone();
        let active_source_key = active_source_key.clone();
        let playback_session = playback_session.clone();
        let cv = column_view.clone();
        let playback_rt = rt_handle.clone();
        let playback_config = app_config.clone();
        let playback_source_registry = source_registry.clone();

        column_view.connect_activate(move |_view, position| {
            play_track_at(
                position,
                &PlaybackContext {
                    model: sm.clone(),
                    active_source_key: active_source_key.clone(),
                    active_output: active_output.clone(),
                    album_art: album_art.clone(),
                    title_label: title_label.clone(),
                    artist_label: artist_label.clone(),
                    media_ctrl: media_ctrl.clone(),
                    session: playback_session.clone(),
                    app_config: playback_config.clone(),
                    rt_handle: playback_rt.clone(),
                    column_view: cv.clone(),
                    source_registry: playback_source_registry.clone(),
                },
            );
        });
    }

    // ── Right-click context menu on tracklist ────────────────────────
    super::context_menu::setup_context_menu(&WindowState {
        window: window.clone(),
        rt_handle: rt_handle.clone(),
        engine_tx: engine_tx.clone(),
        source_registry: source_registry.clone(),
        remote_provenance: remote_provenance.clone(),
        track_store: track_store.clone(),
        master_tracks: master_tracks.clone(),
        source_tracks: source_tracks.clone(),
        active_source_key: active_source_key.clone(),
        source_navigation: source_navigation.clone(),
        near_me_consent_request: near_me_consent_request.clone(),
        sidebar_store: sidebar_store_for_events.clone(),
        sidebar_selection: sidebar_sel_for_events.clone(),
        browser_widget: browser_widget.clone(),
        browser_state: browser_state.clone(),
        status_label: status_label.clone(),
        column_view: column_view.clone(),
        sort_model: sort_model.clone(),
        app_config: app_config.clone(),
        pending_connection: pending_connection_for_events.clone(),
        pre_connect_selection: pre_connect_selection_for_events.clone(),
    });

    // ── Wire Next button ────────────────────────────────────────────
    {
        let active_output = active_output.clone();
        let media_ctrl = media_ctrl.clone();
        let album_art = hb.album_art.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let sm = sort_model.clone();
        let active_source_key = active_source_key.clone();
        let playback_session = playback_session.clone();
        let repeat_mode = hb.repeat_mode.clone();
        let shuffle = hb.shuffle_button.clone();
        let cv = column_view.clone();
        let playback_rt = rt_handle.clone();
        let playback_config = app_config.clone();
        let playback_source_registry = source_registry.clone();

        hb.next_button.connect_clicked(move |_| {
            advance_track_from_user(
                &PlaybackContext {
                    model: sm.clone(),
                    active_source_key: active_source_key.clone(),
                    active_output: active_output.clone(),
                    album_art: album_art.clone(),
                    title_label: title_label.clone(),
                    artist_label: artist_label.clone(),
                    media_ctrl: media_ctrl.clone(),
                    session: playback_session.clone(),
                    app_config: playback_config.clone(),
                    rt_handle: playback_rt.clone(),
                    column_view: cv.clone(),
                    source_registry: playback_source_registry.clone(),
                },
                repeat_mode.get(),
                shuffle.is_active(),
            );
        });
    }

    // ── Wire Previous button ────────────────────────────────────────
    {
        let active_output = active_output.clone();
        let media_ctrl = media_ctrl.clone();
        let album_art = hb.album_art.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let sm = sort_model.clone();
        let active_source_key = active_source_key.clone();
        let playback_session = playback_session.clone();
        let repeat_mode = hb.repeat_mode.clone();
        let shuffle = hb.shuffle_button.clone();
        let cv = column_view.clone();
        let playback_rt = rt_handle.clone();
        let playback_config = app_config.clone();
        let playback_source_registry = source_registry.clone();

        hb.prev_button.connect_clicked(move |_| {
            super::open_files::invalidate_admission();
            // If more than 3 s into the track, restart it.
            let position_ms = active_output.borrow().position_ms().unwrap_or(0);
            if position_ms > PREV_RESTART_THRESHOLD_MS {
                active_output.borrow().seek_to(0);
                return;
            }

            let stepped = previous_track_from_user(
                &PlaybackContext {
                    model: sm.clone(),
                    active_source_key: active_source_key.clone(),
                    active_output: active_output.clone(),
                    album_art: album_art.clone(),
                    title_label: title_label.clone(),
                    artist_label: artist_label.clone(),
                    media_ctrl: media_ctrl.clone(),
                    session: playback_session.clone(),
                    app_config: playback_config.clone(),
                    rt_handle: playback_rt.clone(),
                    column_view: cv.clone(),
                    source_registry: playback_source_registry.clone(),
                },
                repeat_mode.get(),
                shuffle.is_active(),
            );

            // If we couldn't step back (track 0 with repeat off, or no
            // current track), restart whatever is playing instead.
            if !stepped {
                active_output.borrow().seek_to(0);
            }
        });
    }

    // ── Receive PlayerEvents on GTK main thread ─────────────────────
    {
        let play_btn = hb.play_button.clone();
        let album_art = hb.album_art.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let progress_adj = hb.progress_adj.clone();
        let position_label = hb.position_label.clone();
        let duration_label = hb.duration_label.clone();
        let repeat_mode = hb.repeat_mode.clone();
        let shuffle = hb.shuffle_button.clone();
        let seeking = seeking.clone();
        let media_ctrl = media_ctrl.clone();
        let active_output = active_output.clone();
        let sm = sort_model.clone();
        let active_source_key = active_source_key.clone();
        let playback_session = playback_session.clone();
        let cv = column_view.clone();
        let buffering_tracker = buffering_tracker.clone();
        let clear_playback_ui = clear_playback_ui.clone();
        let toast_overlay = toast_overlay.clone();
        let playback_rt = rt_handle.clone();
        let playback_config = app_config.clone();
        let playback_source_registry = source_registry.clone();

        // Pre-build a spinner widget for the buffering state.
        let buffering_spinner = gtk::Spinner::builder()
            .spinning(true)
            .width_request(16)
            .height_request(16)
            .build();

        // Debounce: only show the spinner if buffering persists for
        // longer than this threshold.  Increased from 100 ms to 300 ms
        // to prevent sub-100 ms blinking on fast-loading local files.
        const BUFFERING_DELAY_MS: u32 = 300;
        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = player_rx.recv().await {
                let event_generation = event.generation();
                if !playback_session
                    .borrow()
                    .accepts_event_generation(event_generation)
                {
                    tracing::debug!(?event_generation, "Ignoring stale player event");
                    continue;
                }
                match event {
                    PlayerEvent::StateChanged { state, .. } => {
                        match state {
                            PlayerState::Buffering => {
                                let generation = buffering_tracker.begin();
                                // Schedule the spinner after a short
                                // delay — if Playing arrives first the
                                // generation will have changed and the
                                // callback becomes a no-op.
                                let btn = play_btn.clone();
                                let spinner = buffering_spinner.clone();
                                let tracker = buffering_tracker.clone();
                                let session = playback_session.clone();
                                glib::timeout_add_local_once(
                                    Duration::from_millis(BUFFERING_DELAY_MS as u64),
                                    move || {
                                        if tracker.is_current(generation)
                                            && session
                                                .borrow()
                                                .accepts_event_generation(event_generation)
                                        {
                                            btn.set_child(Some(&spinner));
                                        }
                                    },
                                );
                            }
                            PlayerState::Playing => {
                                buffering_tracker.invalidate();
                                // Restore icon: show pause.
                                play_btn.set_child(Option::<&gtk::Widget>::None);
                                play_btn.set_icon_name("media-playback-pause-symbolic");
                            }
                            _ => {
                                buffering_tracker.invalidate();
                                // Stopped or Paused: show play.
                                play_btn.set_child(Option::<&gtk::Widget>::None);
                                play_btn.set_icon_name("media-playback-start-symbolic");
                            }
                        }

                        if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                            match state {
                                PlayerState::Playing => ctrl.update_playback(true),
                                PlayerState::Paused | PlayerState::Stopped => {
                                    ctrl.update_playback(false);
                                }
                                // OS media APIs do not expose Buffering. Keep
                                // the optimistic Playing state published when
                                // the session load was accepted.
                                PlayerState::Buffering => {}
                            }
                        }
                    }

                    PlayerEvent::PositionChanged {
                        position_ms,
                        duration_ms,
                        ..
                    } => {
                        // If we receive a position tick while still in
                        // the buffering state, audio is actually playing
                        // — clear the spinner definitively.  This is the
                        // sure-fire fix for remote streams where GStreamer
                        // never sends a clean Playing state change after
                        // buffering completes.
                        if buffering_tracker.is_buffering() {
                            buffering_tracker.invalidate();
                            play_btn.set_child(Option::<&gtk::Widget>::None);
                            play_btn.set_icon_name("media-playback-pause-symbolic");

                            if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                                ctrl.update_playback(true);
                            }
                        }

                        // Always update the elapsed time label.
                        position_label.set_label(&format_ms(position_ms));

                        // Only update the progress slider and duration label
                        // when the stream has a known duration (> 0).
                        // Live streams (radio) have duration_ms == 0.
                        seeking.set(true);
                        if duration_ms > 0 {
                            progress_adj.set_upper(duration_ms as f64);
                            progress_adj.set_value(position_ms as f64);
                            seeking.set(false);
                            duration_label.set_label(&format_ms(duration_ms));
                        } else {
                            // Live stream: keep slider at 0, show "LIVE" or
                            // blank for the duration label.
                            progress_adj.set_upper(1.0);
                            progress_adj.set_value(0.0);
                            seeking.set(false);
                            duration_label.set_label("LIVE");
                        }
                    }

                    PlayerEvent::TrackEnded { .. } => {
                        buffering_tracker.invalidate();
                        play_btn.set_child(Option::<&gtk::Widget>::None);
                        let mode = repeat_mode.get();

                        // Repeat-one: replay the same track.
                        if mode == RepeatMode::One
                            && replay_current(&PlaybackContext {
                                model: sm.clone(),
                                active_source_key: active_source_key.clone(),
                                active_output: active_output.clone(),
                                album_art: album_art.clone(),
                                title_label: title_label.clone(),
                                artist_label: artist_label.clone(),
                                media_ctrl: media_ctrl.clone(),
                                session: playback_session.clone(),
                                app_config: playback_config.clone(),
                                rt_handle: playback_rt.clone(),
                                column_view: cv.clone(),
                                source_registry: playback_source_registry.clone(),
                            })
                        {
                            continue;
                        }

                        // Auto-advance (shuffle-aware).
                        let advanced = advance_track(
                            &PlaybackContext {
                                model: sm.clone(),
                                active_source_key: active_source_key.clone(),
                                active_output: active_output.clone(),
                                album_art: album_art.clone(),
                                title_label: title_label.clone(),
                                artist_label: artist_label.clone(),
                                media_ctrl: media_ctrl.clone(),
                                session: playback_session.clone(),
                                app_config: playback_config.clone(),
                                rt_handle: playback_rt.clone(),
                                column_view: cv.clone(),
                                source_registry: playback_source_registry.clone(),
                            },
                            mode,
                            shuffle.is_active(),
                        );

                        if !advanced {
                            // End of playlist — invalidate the event generation
                            // before stopping the output. This also revokes a
                            // receiver-facing local-file lease after natural
                            // completion; any synchronous Stopped event is
                            // already stale.
                            let external_source = playback_session
                                .borrow()
                                .external_source_for_terminal(event_generation, false);
                            if external_source.is_some() {
                                super::open_files::invalidate_admission();
                            }
                            playback_session.borrow_mut().clear();
                            active_output.borrow().stop();
                            if let Some(source_id) = external_source {
                                let _ = playback_source_registry.retire_external(source_id);
                            }
                            clear_playback_ui();
                        }
                    }

                    PlayerEvent::Error { message, .. } => {
                        tracing::error!(error = %message, "Player error");
                        // Show the failure to the user. Outputs reduce every
                        // failure to a fixed category or fixed actionable
                        // string before it can reach a player event — never
                        // server text, a URL, or a credential — so the
                        // message is safe to display verbatim. Without this,
                        // a failed load is visible only in the logs.
                        toast_overlay.add_toast(adw::Toast::new(&message));
                        let external_source = playback_session
                            .borrow()
                            .external_source_for_terminal(event_generation, false);
                        if let Some(source_id) = external_source {
                            super::open_files::invalidate_admission();
                            playback_session.borrow_mut().clear();
                            active_output.borrow().stop();
                            let _ = playback_source_registry.retire_external(source_id);
                            clear_playback_ui();
                            continue;
                        }
                        // A protected or exact-local resolver has already
                        // handed media to the output at this point. If that
                        // load fails, keep the queue item but force the next
                        // Play through a new resolution instead of calling
                        // `play()` on an output that may never have accepted
                        // media.
                        if playback_session
                            .borrow_mut()
                            .mark_resolved_load_failed(event_generation)
                        {
                            active_output.borrow().stop();
                        }
                        // On error, restore the play icon (stop the spinner
                        // if we were buffering).
                        buffering_tracker.invalidate();
                        play_btn.set_child(Option::<&gtk::Widget>::None);
                        play_btn.set_icon_name("media-playback-start-symbolic");
                        if let Some(ref mut ctrl) = *media_ctrl.borrow_mut() {
                            ctrl.update_playback(false);
                        }
                    }
                }
            }
        });
    }

    // ── Apply persisted preferences (column visibility, order, browser) ─
    {
        let cfg = app_config.borrow();
        preferences::apply_column_visibility(&column_view, &cfg.visible_columns);
        preferences::apply_column_order(&column_view, &cfg.column_order);
        preferences::update_browser_visibility(&browser_widget, &cfg.browser_views);
    }

    // ── Persist column order on drag-and-drop reorder ────────────────
    {
        let config = app_config.clone();
        let cv = column_view.clone();
        let active_source_key = active_source_key.clone();
        column_view
            .columns()
            .connect_items_changed(move |_list, _pos, _removed, _added| {
                // Skip persistence while in radio mode — the renamed
                // Artist→Country / Album→State-Province columns would
                // corrupt the saved column order (issue #38).
                if super::radio::is_radio_backend(&active_source_key.borrow()) {
                    return;
                }
                let order = preferences::read_column_order(&cv);
                if !order.is_empty() {
                    let mut cfg = config.borrow_mut();
                    cfg.column_order = order;
                    preferences::save_config(&cfg);
                }
            });
    }

    // ── "Open With" pending-files action ────────────────────────────
    //
    // The OS file-open handler in main.rs queues paths in
    // `super::open_files`.  We expose a stateless application-level
    // GAction `app.play-pending-files` that drains the queue and plays
    // the file(s) on the active output.  The action is registered on
    // the GApplication (not the window) so the file-open handler can
    // look it up via `app.lookup_action`.
    {
        let active_output = active_output.clone();
        let media_ctrl = media_ctrl.clone();
        let album_art = hb.album_art.clone();
        let title_label = hb.title_label.clone();
        let artist_label = hb.artist_label.clone();
        let sm = sort_model.clone();
        let active_source_key = active_source_key.clone();
        let playback_session = playback_session.clone();
        let cv = column_view.clone();
        let playback_rt = rt_handle.clone();
        let playback_config = app_config.clone();
        let playback_source_registry = source_registry.clone();

        let play_pending = gtk::gio::SimpleAction::new("play-pending-files", None);
        play_pending.connect_activate(move |_, _| {
            let delivery = super::open_files::drain();
            if delivery.is_empty() {
                return;
            }

            let generation = delivery.generation();
            let registry = playback_source_registry.clone();
            let admission = playback_rt.spawn_blocking(move || {
                super::open_files::admit_first_accepted_audio(delivery, registry)
            });

            let active_output = active_output.clone();
            let media_ctrl = media_ctrl.clone();
            let album_art = album_art.clone();
            let title_label = title_label.clone();
            let artist_label = artist_label.clone();
            let sm = sm.clone();
            let active_source_key = active_source_key.clone();
            let playback_session = playback_session.clone();
            let cv = cv.clone();
            let playback_rt = playback_rt.clone();
            let playback_config = playback_config.clone();
            let playback_source_registry = playback_source_registry.clone();
            glib::MainContext::default().spawn_local(async move {
                match admission.await {
                    Ok(Some(pending)) if super::open_files::is_current(generation) => {
                        let ctx = super::playback::PlaybackContext {
                            model: sm,
                            active_source_key,
                            active_output,
                            album_art,
                            title_label,
                            artist_label,
                            media_ctrl,
                            session: playback_session,
                            app_config: playback_config,
                            rt_handle: playback_rt,
                            column_view: cv,
                            source_registry: playback_source_registry,
                        };
                        if super::playback::play_external_session(pending.session(), &ctx) {
                            pending.commit();
                        }
                    }
                    Ok(Some(_) | None) => {}
                    Err(_) => {
                        // The worker owns no log-safe path or backend detail.
                        warn!("External media admission worker stopped unexpectedly");
                    }
                }
            });
        });
        app.add_action(&play_pending);

        // Drain any paths that arrived before the window was built
        // (the typical case on first-launch Open With).
        play_pending.activate(None);
    }

    // ── Wire preferences action to the window ────────────────────────
    {
        let win = window.clone();
        let cv = column_view.clone();
        let bw = browser_widget.clone();
        let cfg = app_config.clone();
        let bs = browser_state.clone();
        let master_for_pref = master_tracks.clone();
        let prefs_action = gtk::gio::SimpleAction::new("show-preferences", None);
        prefs_action.connect_activate(move |_, _| {
            let bw_for_cb = bw.clone();
            let bs_for_cb = bs.clone();
            let master_for_cb = master_for_pref.clone();
            let on_aa_change: std::rc::Rc<dyn Fn(bool)> = std::rc::Rc::new(move |enabled: bool| {
                // Refresh the browser snapshot so the album-artist
                // grouping change takes effect against the latest
                // library state, not just whatever was loaded when
                // the browser was first built.
                let tracks = master_for_cb.borrow().clone();
                browser::rebuild_browser_data(&bw_for_cb, &bs_for_cb, &tracks);
                browser::set_album_artist_grouping(&bw_for_cb, &bs_for_cb, enabled);
            });
            preferences::show_preferences(&win, &cv, &bw, &cfg, on_aa_change);
        });
        window.add_action(&prefs_action);
    }

    // ── Ctrl+F: focus browser search entry ───────────────────────────
    {
        let bw = browser_widget.clone();
        let search_action = gtk::gio::SimpleAction::new("focus-search", None);
        search_action.connect_activate(move |_, _| {
            // The browser_widget is a vertical Box: SearchEntry on top,
            // panes_box below.  Find the SearchEntry (first child).
            if let Some(first) = bw.first_child() {
                if let Some(entry) = first.downcast_ref::<gtk::SearchEntry>() {
                    bw.set_visible(true);
                    entry.grab_focus();
                }
            }
        });
        window.add_action(&search_action);
    }
    app.set_accels_for_action("win.focus-search", &["<primary>f"]);

    // ── Handle playlist context menu actions ─────────────────────────
    super::playlist_actions::setup_playlist_actions(
        &WindowState {
            window: window.clone(),
            rt_handle: rt_handle.clone(),
            engine_tx: engine_tx.clone(),
            source_registry: source_registry.clone(),
            remote_provenance: remote_provenance.clone(),
            track_store: track_store.clone(),
            master_tracks: master_tracks.clone(),
            source_tracks: source_tracks.clone(),
            active_source_key: active_source_key.clone(),
            source_navigation: source_navigation.clone(),
            near_me_consent_request: near_me_consent_request.clone(),
            sidebar_store: sidebar_store_for_events.clone(),
            sidebar_selection: sidebar_sel_for_events.clone(),
            browser_widget: browser_widget.clone(),
            browser_state: browser_state.clone(),
            status_label: status_label.clone(),
            column_view: column_view.clone(),
            sort_model: sort_model.clone(),
            app_config: app_config.clone(),
            pending_connection: pending_connection_for_events.clone(),
            pre_connect_selection: pre_connect_selection_for_events.clone(),
        },
        playlist_action_rx,
    );

    // ── Receive LibraryEvents on GTK main thread ─────────────────────
    setup_library_events(
        engine_rx,
        rt_handle.clone(),
        track_store,
        status_label,
        master_tracks,
        source_tracks,
        active_source_key,
        source_navigation,
        &browser_widget,
        browser_state,
        &column_view,
        sidebar_store_for_events,
        sidebar_sel_for_events,
        scan_spinner,
        pending_connection_for_events,
        playback_session,
        root_trust_prompts,
        app_config,
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Helpers (kept in window.rs — used by multiple extracted modules)
// ═══════════════════════════════════════════════════════════════════════

/// Replace the visible tracklist, browser, and master track list with a
/// new set of tracks (e.g., when switching sidebar sources).
pub fn display_tracks(
    objects: &[TrackObject],
    track_store: &gtk::gio::ListStore,
    master_tracks: &RefCell<Vec<TrackObject>>,
    browser_widget: &gtk::Box,
    browser_state: &browser::BrowserState,
    status_label: &gtk::Label,
    column_view: &gtk::ColumnView,
) {
    // Use splice() to replace all items in a single operation.
    // This emits one `items-changed` signal instead of N individual
    // signals, which is dramatically faster for large libraries
    // (thousands of tracks) and prevents multi-second UI freezes.
    track_store.splice(0, track_store.n_items(), objects);

    tracklist::update_status(status_label, objects);
    browser::rebuild_browser_data(browser_widget, browser_state, objects);
    *master_tracks.borrow_mut() = objects.to_vec();
    column_view.scroll_to(0, None, gtk::ListScrollFlags::NONE, None);
}

/// Re-resolve queued library items from committed library state.
///
/// The playback queue is an immutable snapshot of identities, so it survives
/// sorting, filtering, and navigation — but a filesystem rename changes where a
/// track lives without changing which track it is. Only the library can say
/// where it moved to, and only the queue knows it is still holding it.
fn refresh_playback_queue(session: &Rc<RefCell<PlaybackSession>>, objects: &[TrackObject]) {
    let updates = {
        let session = session.borrow();
        let queue_ids = session.library_track_ids();
        if queue_ids.is_empty() {
            return;
        }

        // FullSync can contain tens of thousands of tracks. Scan the snapshot,
        // but clone refresh metadata only for the usually small set the queue
        // owns.
        let mut updates = HashMap::with_capacity(queue_ids.len());
        for track in objects {
            let Ok(track_id) = crate::architecture::TrackId::new(track.track_id()) else {
                continue;
            };
            if queue_ids.contains(&track_id) {
                updates.insert(track_id, QueueTrackRefresh::from_track(track));
            }
        }
        updates
    };
    if updates.is_empty() {
        return;
    }

    let refreshed = session.borrow_mut().refresh_library_tracks(&updates);
    if refreshed > 0 {
        info!(
            refreshed,
            "Re-resolved playback queue items after library change"
        );
    }
}

/// Retarget the rows of an already-open playlist after committed local changes.
/// The visible store and `master_tracks` share these GObject instances, so an
/// in-place URI overlay is immediately used by the next click without changing
/// playlist order, duplicates, or selection identity.
fn refresh_active_playlist_uris(
    active_source_key: &Rc<RefCell<String>>,
    master_tracks: &Rc<RefCell<Vec<TrackObject>>>,
    committed_local_rows: &[TrackObject],
) {
    if !active_source_key
        .borrow()
        .starts_with(PLAYLIST_SOURCE_PREFIX)
    {
        return;
    }

    let rows = master_tracks.borrow();
    let refreshed = refresh_projected_library_uris(&rows, committed_local_rows);
    if refreshed > 0 {
        info!(refreshed, "Refreshed active playlist paths");
    }
}

/// Spawn the library event receiver loop on the GTK main thread.
#[allow(clippy::too_many_arguments)]
fn setup_library_events(
    engine_rx: async_channel::Receiver<LibraryEvent>,
    rt_handle: tokio::runtime::Handle,
    track_store: gtk::gio::ListStore,
    status_label: gtk::Label,
    master_tracks: Rc<RefCell<Vec<TrackObject>>>,
    source_tracks: Rc<RefCell<HashMap<String, Vec<TrackObject>>>>,
    active_source_key: Rc<RefCell<String>>,
    source_navigation: Rc<RefCell<SourceNavigation>>,
    browser_widget: &gtk::Box,
    browser_state: browser::BrowserState,
    column_view: &gtk::ColumnView,
    sidebar_store: gtk::gio::ListStore,
    sidebar_selection: gtk::SingleSelection,
    scan_spinner: gtk::Spinner,
    pending_connection: Rc<RefCell<Option<PendingConnection>>>,
    playback_session: Rc<RefCell<PlaybackSession>>,
    root_trust_prompts: root_trust::RootTrustPromptController,
    app_config: Rc<RefCell<preferences::AppConfig>>,
) {
    let browser_widget = browser_widget.clone();
    let column_view = column_view.clone();

    // ── Debounce browser rebuilds for TrackUpserted / TrackRemoved ──
    // During initial scan, dozens of upsert events fire in quick
    // succession.  Instead of rebuilding the 3-pane browser on every
    // single event, we defer the rebuild by 500 ms.  If another event
    // arrives within that window the previous timer is invalidated.
    let browser_rebuild_gen: Rc<Cell<u32>> = Rc::new(Cell::new(0));

    glib::MainContext::default().spawn_local(async move {
        while let Ok(event) = engine_rx.recv().await {
            match event {
                LibraryEvent::FullSync(tracks) => {
                    info!(count = tracks.len(), "Received full library sync");

                    let objects: Vec<TrackObject> =
                        tracks.iter().map(arch_track_to_object).collect();

                    refresh_active_playlist_uris(&active_source_key, &master_tracks, &objects);

                    // A bulk change — a renamed album, a reconciliation — can
                    // move the files behind tracks the queue is holding. The
                    // queue owns identities, not rows, so it re-resolves them
                    // from the snapshot rather than being rebuilt from the view.
                    refresh_playback_queue(&playback_session, &objects);

                    // Store per-source.
                    source_tracks
                        .borrow_mut()
                        .insert("local".to_string(), objects.clone());

                    // Display only if local is the active source.
                    if *active_source_key.borrow() == "local" {
                        display_tracks(
                            &objects,
                            &track_store,
                            &master_tracks,
                            &browser_widget,
                            &browser_state,
                            &status_label,
                            &column_view,
                        );
                    }
                }

                LibraryEvent::TrackUpserted(track) => {
                    let obj = arch_track_to_object(&track);
                    let uri = obj.uri();

                    refresh_active_playlist_uris(
                        &active_source_key,
                        &master_tracks,
                        std::slice::from_ref(&obj),
                    );

                    // A single-file rename keeps the track's identity and moves
                    // its path, so a queue holding it must follow the track, not
                    // the path it was captured at.
                    refresh_playback_queue(&playback_session, std::slice::from_ref(&obj));

                    // Update source_tracks["local"].
                    {
                        let mut st = source_tracks.borrow_mut();
                        let local = st.entry("local".to_string()).or_default();
                        // Replace existing (by URI) or append.
                        if let Some(pos) = local.iter().position(|t| t.uri() == uri) {
                            local[pos] = obj.clone();
                        } else {
                            local.push(obj.clone());
                        }
                    }

                    // If local is the active source, update the visible tracklist.
                    if *active_source_key.borrow() == "local" {
                        // Check if already in the store (update) or new (append).
                        let mut found = false;
                        for i in 0..track_store.n_items() {
                            if let Some(existing) =
                                track_store.item(i).and_downcast_ref::<TrackObject>()
                            {
                                if existing.uri() == uri {
                                    track_store.remove(i);
                                    track_store.insert(i, &obj);
                                    found = true;
                                    break;
                                }
                            }
                        }
                        if !found {
                            track_store.append(&obj);
                        }

                        // Update master tracks immediately.
                        let st = source_tracks.borrow();
                        let local_tracks = st.get("local").cloned().unwrap_or_default();
                        *master_tracks.borrow_mut() = local_tracks.clone();

                        // Debounce browser rebuild + status update (500 ms).
                        // The tracklist store is already up-to-date above;
                        // only the 3-pane browser and status bar are deferred.
                        let gen = browser_rebuild_gen.get().wrapping_add(1);
                        browser_rebuild_gen.set(gen);

                        let gen_rc = browser_rebuild_gen.clone();
                        let source_tracks = source_tracks.clone();
                        let browser_widget = browser_widget.clone();
                        let browser_state = browser_state.clone();
                        let status_label = status_label.clone();
                        let active_source_key = active_source_key.clone();
                        let source_navigation = source_navigation.clone();
                        let navigation_request = source_navigation.borrow().latest_request("local");
                        let pending_connection = pending_connection.clone();

                        glib::timeout_add_local_once(Duration::from_millis(500), move || {
                            let Some(navigation_request) = navigation_request else {
                                return;
                            };
                            let pending_request = pending_connection
                                .borrow()
                                .as_ref()
                                .map(|pending| pending.request().clone());
                            let may_refresh = source_navigation.borrow().may_refresh_visible(
                                "local",
                                &navigation_request,
                                pending_request.as_ref(),
                            );
                            if gen_rc.get() != gen
                                || *active_source_key.borrow() != "local"
                                || !may_refresh
                            {
                                return; // Superseded by a newer event.
                            }
                            let st = source_tracks.borrow();
                            let local_tracks = st.get("local").cloned().unwrap_or_default();
                            tracklist::update_status(&status_label, &local_tracks);
                            browser::rebuild_browser_data(
                                &browser_widget,
                                &browser_state,
                                &local_tracks,
                            );
                        });
                    }
                }

                LibraryEvent::TrackRemoved(path) => {
                    // Build the file:// URI for comparison.
                    let removed_uri = url::Url::from_file_path(&path)
                        .map(|u| u.to_string())
                        .unwrap_or_default();

                    // Remove from source_tracks["local"].
                    {
                        let mut st = source_tracks.borrow_mut();
                        if let Some(local) = st.get_mut("local") {
                            local.retain(|t| t.uri() != removed_uri);
                        }
                    }

                    // If local is the active source, remove from visible tracklist.
                    if *active_source_key.borrow() == "local" {
                        for i in 0..track_store.n_items() {
                            if let Some(existing) =
                                track_store.item(i).and_downcast_ref::<TrackObject>()
                            {
                                if existing.uri() == removed_uri {
                                    track_store.remove(i);
                                    break;
                                }
                            }
                        }

                        // Update master tracks immediately.
                        let st = source_tracks.borrow();
                        let local_tracks = st.get("local").cloned().unwrap_or_default();
                        *master_tracks.borrow_mut() = local_tracks.clone();

                        // Debounce browser rebuild + status update (500 ms).
                        let gen = browser_rebuild_gen.get().wrapping_add(1);
                        browser_rebuild_gen.set(gen);

                        let gen_rc = browser_rebuild_gen.clone();
                        let source_tracks = source_tracks.clone();
                        let browser_widget = browser_widget.clone();
                        let browser_state = browser_state.clone();
                        let status_label = status_label.clone();
                        let active_source_key = active_source_key.clone();
                        let source_navigation = source_navigation.clone();
                        let navigation_request = source_navigation.borrow().latest_request("local");
                        let pending_connection = pending_connection.clone();

                        glib::timeout_add_local_once(Duration::from_millis(500), move || {
                            let Some(navigation_request) = navigation_request else {
                                return;
                            };
                            let pending_request = pending_connection
                                .borrow()
                                .as_ref()
                                .map(|pending| pending.request().clone());
                            let may_refresh = source_navigation.borrow().may_refresh_visible(
                                "local",
                                &navigation_request,
                                pending_request.as_ref(),
                            );
                            if gen_rc.get() != gen
                                || *active_source_key.borrow() != "local"
                                || !may_refresh
                            {
                                return; // Superseded by a newer event.
                            }
                            let st = source_tracks.borrow();
                            let local_tracks = st.get("local").cloned().unwrap_or_default();
                            tracklist::update_status(&status_label, &local_tracks);
                            browser::rebuild_browser_data(
                                &browser_widget,
                                &browser_state,
                                &local_tracks,
                            );
                        });
                    }
                }

                LibraryEvent::ScanProgress(done, total) => {
                    if done % 500 == 0 || done == total {
                        info!(done, total, "Scan progress");
                    }
                }

                LibraryEvent::ScanComplete => {
                    info!("Library scan complete");
                    scan_spinner.set_spinning(false);
                    scan_spinner.set_visible(false);
                }

                LibraryEvent::PlaylistProjectionsInvalidated => {
                    let active_key = active_source_key.borrow().clone();

                    // Any local mutation can change a live smart playlist, and
                    // reconciliation can remint/relink regular-playlist track
                    // IDs. Retire pre-settlement requests before clearing the
                    // cache so a late query cannot put stale rows back.
                    source_navigation
                        .borrow_mut()
                        .invalidate_prefix(PLAYLIST_SOURCE_PREFIX);
                    source_tracks
                        .borrow_mut()
                        .retain(|key, _| !key.starts_with(PLAYLIST_SOURCE_PREFIX));

                    if let Some(playlist_id) = active_key
                        .strip_prefix(PLAYLIST_SOURCE_PREFIX)
                        .map(str::to_string)
                    {
                        // The old rows may hold orphaned/reminted IDs. Do not
                        // leave them actionable while the settled projection
                        // is loading.
                        display_tracks(
                            &[],
                            &track_store,
                            &master_tracks,
                            &browser_widget,
                            &browser_state,
                            &status_label,
                            &column_view,
                        );

                        // `active_source_key` names the visible rows, while
                        // SourceNavigation names the user's latest intent.
                        // During remote authentication those intentionally
                        // differ. Never let background playlist maintenance
                        // supersede that newer remote intent.
                        if source_navigation.borrow().is_key(&active_key) {
                            let request = source_navigation.borrow_mut().select(active_key.clone());
                            super::source_connect::load_playlist_source(
                                rt_handle.clone(),
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
                        }
                    }
                }

                LibraryEvent::PlaylistsLoaded(playlists) => {
                    info!(count = playlists.len(), "Populating sidebar with playlists");
                    let active_key = active_source_key.borrow().clone();
                    let active_playlist_id = source_navigation
                        .borrow()
                        .is_key(&active_key)
                        .then(|| {
                            active_key
                                .strip_prefix(PLAYLIST_SOURCE_PREFIX)
                                .map(str::to_string)
                        })
                        .flatten();

                    // Find the "Playlists" header position in sidebar.
                    let mut playlist_header_pos = None;
                    let n = sidebar_store.n_items();
                    for i in 0..n {
                        if let Some(src) = sidebar_store.item(i).and_downcast_ref::<SourceObject>()
                        {
                            if src.is_header() && src.name() == "Playlists" {
                                playlist_header_pos = Some(i);
                                break;
                            }
                        }
                    }

                    if let Some(header_pos) = playlist_header_pos {
                        // Remove old playlist entries (between Playlists header
                        // and the next header).
                        let insert_pos = header_pos + 1;
                        while insert_pos < sidebar_store.n_items() {
                            if let Some(src) = sidebar_store
                                .item(insert_pos)
                                .and_downcast_ref::<SourceObject>()
                            {
                                if src.is_header() {
                                    break; // Hit next section header.
                                }
                                let bt = src.backend_type();
                                if bt == "playlist" || bt == "smart-playlist" {
                                    sidebar_store.remove(insert_pos);
                                } else {
                                    break;
                                }
                            } else {
                                break;
                            }
                        }

                        // Insert new playlist entries.
                        let mut active_position = None;
                        for (idx, (id, name, is_smart)) in playlists.iter().enumerate() {
                            let src = SourceObject::playlist(name, id, *is_smart);
                            let position = insert_pos + idx as u32;
                            sidebar_store.insert(position, &src);
                            if active_playlist_id.as_deref() == Some(id.as_str()) {
                                active_position = Some(position);
                            }
                        }

                        // Rebuilding the rows invalidates GtkSingleSelection's
                        // selected object. Restore the row that corresponds to
                        // the still-active playlist so sidebar and content do
                        // not diverge during watcher fallback scans.
                        if let Some(position) = active_position {
                            sidebar_selection.set_selected(position);
                        }
                    }
                }

                LibraryEvent::RootTrustRequired(requests) => {
                    root_trust_prompts.enqueue(requests);
                }

                LibraryEvent::RootTrustFinished {
                    request_id,
                    path,
                    reason,
                    outcome,
                } => {
                    root_trust_prompts.handle_finished(request_id, path, reason, outcome);
                }

                LibraryEvent::RootReauthorizationFinished {
                    request_id,
                    old_path,
                    new_path,
                    outcome,
                    message,
                } => {
                    if outcome.committed() {
                        let old_path = old_path.to_string_lossy();
                        let new_path = new_path.to_string_lossy();
                        let mut config = app_config.borrow_mut();
                        let mut candidate = config.clone();
                        if preferences::complete_root_reauthorization(
                            &mut candidate,
                            &request_id,
                            old_path.as_ref(),
                            new_path.as_ref(),
                        ) && preferences::save_config(&candidate)
                        {
                            *config = candidate;
                            info!(
                                %request_id,
                                old_path = %old_path,
                                new_path = %new_path,
                                ?outcome,
                                "Committed library root reauthorization to config"
                            );
                        } else {
                            warn!(
                                %request_id,
                                old_path = %old_path,
                                new_path = %new_path,
                                ?outcome,
                                "Database relocation committed but config cleanup did not; durable receipt will retry"
                            );
                        }
                    } else {
                        if outcome == RootReauthorizationOutcome::Rejected {
                            let old_key = old_path.to_string_lossy();
                            let new_key = new_path.to_string_lossy();
                            let mut config = app_config.borrow_mut();
                            let mut candidate = config.clone();
                            if preferences::reject_root_reauthorization(
                                &mut candidate,
                                &request_id,
                                old_key.as_ref(),
                                new_key.as_ref(),
                            ) && preferences::save_config(&candidate)
                            {
                                *config = candidate;
                            }
                        }
                        warn!(
                            %request_id,
                            old_path = %old_path.display(),
                            new_path = %new_path.display(),
                            ?outcome,
                            detail = message.as_deref().unwrap_or("validation failed"),
                            "Library root reauthorization did not commit"
                        );
                        let status = match outcome {
                            RootReauthorizationOutcome::Inconsistent => rust_i18n::t!(
                                "preferences.reauthorization_inconsistent_status"
                            ),
                            _ => rust_i18n::t!("preferences.reauthorization_failed_status"),
                        };
                        status_label.set_text(status.as_ref());
                    }
                }

                LibraryEvent::Error(msg) => {
                    tracing::error!(error = %msg, "Library engine error");
                    scan_spinner.set_spinning(false);
                    scan_spinner.set_visible(false);
                }

            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn plan_remote_publication_selection(
    pending: Option<&PendingConnection>,
    navigation: &SourceNavigation,
    source_id: crate::architecture::SourceId,
    generation: u64,
    source_key: &str,
    accepted: AcceptedRemotePublication,
    selected_before_accept: u32,
    projection_active_after_accept: bool,
) -> RemotePublicationSelection {
    let Some(pending) = pending.filter(|pending| {
        pending.matches_lifecycle(source_id, generation) && pending.source_key() == source_key
    }) else {
        return RemotePublicationSelection::Inactive;
    };

    // A row rebind performed by acceptance can synchronously activate and
    // render the source after the pending guard is cleared. Do not activate it
    // a second time merely because the original request is no longer current.
    if accepted.rebound
        && selected_before_accept == accepted.index
        && projection_active_after_accept
    {
        return RemotePublicationSelection::AlreadyActivated;
    }
    if !pending.may_auto_select(source_key, navigation) {
        return RemotePublicationSelection::Inactive;
    }
    if selected_before_accept == accepted.index {
        // The reducer may already have rebound connected/spinner state while
        // the pending guard intentionally suppressed that selection signal.
        // Force one post-publication signal now that catalogue and guard state
        // are authoritative.
        RemotePublicationSelection::Reactivate(accepted.index)
    } else {
        RemotePublicationSelection::Select(accepted.index)
    }
}

#[allow(clippy::too_many_arguments)]
fn publish_remote_library(
    source_id: crate::architecture::SourceId,
    generation: u64,
    objects: Vec<TrackObject>,
    source_tracks: &Rc<RefCell<HashMap<String, Vec<TrackObject>>>>,
    sidebar_store: &gtk::gio::ListStore,
    pending_connection: &Rc<RefCell<Option<PendingConnection>>>,
    sidebar_selection: &gtk::SingleSelection,
    active_source_key: &Rc<RefCell<String>>,
    source_navigation: &Rc<RefCell<SourceNavigation>>,
    track_store: &gtk::gio::ListStore,
    master_tracks: &Rc<RefCell<Vec<TrackObject>>>,
    browser_widget: &gtk::Box,
    browser_state: &browser::BrowserState,
    status_label: &gtk::Label,
    column_view: &gtk::ColumnView,
) {
    let source_key = source_id.to_string();
    source_tracks
        .borrow_mut()
        .insert(source_key.clone(), objects.clone());

    let pending_intent = pending_connection.borrow().clone();
    let should_clear_pending = pending_intent
        .as_ref()
        .is_some_and(|pending| pending.matches_lifecycle(source_id, generation));
    if should_clear_pending {
        // Clear before any programmatic selection: the selection handler
        // otherwise treats this completed connection as still pending.
        *pending_connection.borrow_mut() = None;
    }

    let selected_before_accept = sidebar_selection.selected();
    let accepted = accept_remote_publication(
        sidebar_store,
        Some(sidebar_selection),
        source_id,
        generation,
    );
    let projection_active_after_accept =
        *active_source_key.borrow() == source_key && source_navigation.borrow().is_key(&source_key);
    let selection_action = accepted.map_or(RemotePublicationSelection::Inactive, |accepted| {
        plan_remote_publication_selection(
            pending_intent.as_ref(),
            &source_navigation.borrow(),
            source_id,
            generation,
            &source_key,
            accepted,
            selected_before_accept,
            projection_active_after_accept,
        )
    });
    selection_action.apply(|index| sidebar_selection.set_selected(index));
    let auto_selected = selection_action.activates();

    if !auto_selected
        && *active_source_key.borrow() == source_key
        && source_navigation.borrow().is_key(&source_key)
    {
        display_tracks(
            &objects,
            track_store,
            master_tracks,
            browser_widget,
            browser_state,
            status_label,
            column_view,
        );
    }
}

/// Apply the sidebar state transition for an accepted remote publication.
///
/// A repeated Add, environment reconnect, or discovered-to-saved promotion
/// can submit another connection for a row whose previous session is still
/// marked connected. The accepted replacement publication still completes
/// that operation, so it must always clear the transient spinner even though
/// the durable connected state does not need to change.
fn accept_remote_publication(
    sidebar_store: &gtk::gio::ListStore,
    sidebar_selection: Option<&gtk::SingleSelection>,
    source_id: crate::architecture::SourceId,
    generation: u64,
) -> Option<AcceptedRemotePublication> {
    for index in 0..sidebar_store.n_items() {
        let Some(source) = sidebar_store.item(index).and_downcast::<SourceObject>() else {
            continue;
        };
        if source.source_id() != Some(source_id) {
            continue;
        }

        let mut changed = if !source.connected() {
            source.set_connected(true);
            true
        } else {
            false
        };
        // A predecessor catalogue can remain authoritative while replacement
        // generation G2 is pending. Publishing G1 must not clear G2's spinner.
        changed |= source.clear_connecting_generation(generation);

        if changed {
            if let Some(selection) = sidebar_selection {
                rebind_sidebar_source(sidebar_store, selection, index, &source, true);
            } else {
                // Headless model tests do not initialize a GTK selection model.
                sidebar_store.remove(index);
                sidebar_store.insert(index, &source);
            }
        }
        return Some(AcceptedRemotePublication {
            index,
            rebound: changed,
        });
    }
    None
}

/// Retire the transient spinner for the exact environment-configured owner
/// whose newest connection attempt failed.
///
/// The background task emits this transition only after its registry attempt
/// proves it is still latest. Keeping the lookup source-scoped prevents one
/// failed endpoint from disturbing another row or its retained session.
#[cfg(test)]
fn clear_failed_remote_connection(
    sidebar_store: &gtk::gio::ListStore,
    source_id: crate::architecture::SourceId,
    generation: u64,
) -> Option<u32> {
    for index in 0..sidebar_store.n_items() {
        let Some(source) = sidebar_store.item(index).and_downcast::<SourceObject>() else {
            continue;
        };
        if source.source_id() != Some(source_id)
            || !source.connecting()
            || !source.clear_connecting_generation(generation)
        {
            continue;
        }

        // SourceObject fields are plain GTK-side state. Reinsert the same
        // object so a bound sidebar row immediately drops its spinner.
        sidebar_store.remove(index);
        sidebar_store.insert(index, &source);
        return Some(index);
    }
    None
}

/// Convert an architecture `Track` to a UI `TrackObject`.
pub fn arch_track_to_object(t: &crate::architecture::models::Track) -> TrackObject {
    // Build playable URI: prefer stream_url, fall back to file:// from file_path.
    let uri = t
        .stream_url
        .as_ref()
        .map(|u| u.to_string())
        .or_else(|| {
            t.file_path
                .as_ref()
                .and_then(|p| url::Url::from_file_path(p).ok().map(|u| u.to_string()))
        })
        .unwrap_or_default();

    track_to_object(t, &uri, t.cover_art_url.as_ref().map(url::Url::as_str))
}

/// Convert a remote track into a pathless row bound to one adopted session.
fn arch_remote_track_to_object(
    track: &crate::architecture::models::Track,
    session_epoch: u64,
) -> TrackObject {
    let object = track_to_object(track, "", None);
    object.set_source_session_epoch(session_epoch);
    object
}

fn track_to_object(
    t: &crate::architecture::models::Track,
    uri: &str,
    artwork_reference: Option<&str>,
) -> TrackObject {
    let obj = TrackObject::new(
        t.track_number.unwrap_or(0),
        &t.title,
        t.duration_secs.unwrap_or(0),
        &t.artist_name,
        &t.album_title,
        t.genre.as_deref().unwrap_or("Unknown"),
        t.composer.as_deref().unwrap_or(""),
        t.year.unwrap_or(0),
        &t.date_modified
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
        t.bitrate_kbps.unwrap_or(0),
        t.sample_rate_hz.unwrap_or(0),
        t.play_count.unwrap_or(0),
        t.format.as_deref().unwrap_or(""),
        uri,
    );

    if let Some(native_track_id) = &t.native_track_id {
        obj.set_track_id(native_track_id.as_str());
    } else {
        // A missing/invalid native identity is deliberately unplayable. Do
        // not substitute the compatibility UUID: doing so silently routes a
        // different identity through remote and playlist queues.
        obj.set_track_id("");
    }

    if let Some(artwork_reference) = artwork_reference {
        obj.set_cover_art_url(artwork_reference);
    }

    // Propagate album artist for browser grouping.
    if let Some(ref aa) = t.album_artist_name {
        obj.set_album_artist(aa);
    }

    // Propagate disc number (shown in the Properties dialog).
    obj.set_disc_number(t.disc_number.unwrap_or(0));

    obj
}

// ── Sidebar category management ─────────────────────────────────────

/// The fixed ordering of sidebar category headers.
const CATEGORY_ORDER: &[&str] = &[
    "Local",
    "DAAP",
    "Subsonic",
    "Jellyfin",
    "Plex",
    "Internet Radio",
];

/// Map a backend type string to its sidebar category header name.
pub fn category_for_backend(backend_type: &str) -> &'static str {
    match backend_type {
        "subsonic" => "Subsonic",
        "jellyfin" => "Jellyfin",
        "plex" => "Plex",
        "daap" => "DAAP",
        _ => "Subsonic", // fallback
    }
}

/// Ensure the category header for `backend_type` exists in a `Vec<SourceObject>`
/// (used during initial source list construction before the ListStore is built).
fn ensure_category_header_vec(sources: &mut Vec<SourceObject>, backend_type: &str) {
    let category = category_for_backend(backend_type);
    let already_exists = sources
        .iter()
        .any(|s| s.is_header() && s.name() == category);
    if !already_exists {
        sources.push(SourceObject::header(category));
    }
}

/// Ensure the category header for `backend_type` exists in the sidebar
/// `ListStore`. Returns the index at which a new source should be inserted
/// (right after the last item in that category, or right after the header
/// if the category is empty).
pub fn ensure_category_header_store(store: &gtk::gio::ListStore, backend_type: &str) -> u32 {
    let category = category_for_backend(backend_type);
    let cat_order = CATEGORY_ORDER
        .iter()
        .position(|&c| c == category)
        .unwrap_or(CATEGORY_ORDER.len());

    // Check if the header already exists.
    for i in 0..store.n_items() {
        if let Some(src) = store.item(i).and_downcast_ref::<SourceObject>() {
            if src.is_header() && src.name() == category {
                // Header exists — find the end of this category
                // (next header or end of list).
                let mut insert_pos = i + 1;
                while insert_pos < store.n_items() {
                    if let Some(next) = store.item(insert_pos).and_downcast_ref::<SourceObject>() {
                        if next.is_header() {
                            break;
                        }
                    }
                    insert_pos += 1;
                }
                return insert_pos;
            }
        }
    }

    // Header doesn't exist — find the correct insertion point based on
    // CATEGORY_ORDER. Insert before the first header that comes after
    // this category in the ordering.
    let mut insert_at = store.n_items(); // default: end of list
    for i in 0..store.n_items() {
        if let Some(src) = store.item(i).and_downcast_ref::<SourceObject>() {
            if src.is_header() {
                let other_order = CATEGORY_ORDER
                    .iter()
                    .position(|&c| c == src.name().as_str())
                    .unwrap_or(CATEGORY_ORDER.len());
                if other_order > cat_order {
                    insert_at = i;
                    break;
                }
            }
        }
    }

    // Insert the header.
    let header = SourceObject::header(category);
    store.insert(insert_at, &header);
    insert_at + 1 // return position right after the new header
}

/// Remove a category header from the store if it has no remaining
/// non-header children (i.e., the category is now empty).
pub fn remove_empty_category_header(store: &gtk::gio::ListStore, category: &str) {
    for i in 0..store.n_items() {
        if let Some(src) = store.item(i).and_downcast_ref::<SourceObject>() {
            if src.is_header() && src.name() == category {
                // Check if the next item is another header or end of list.
                let next_is_header_or_end = if i + 1 >= store.n_items() {
                    true
                } else {
                    store
                        .item(i + 1)
                        .and_downcast_ref::<SourceObject>()
                        .is_some_and(|s| s.is_header())
                };
                if next_is_header_or_end {
                    store.remove(i);
                }
                return;
            }
        }
    }
}

// ── Popover scrollbar fix ───────────────────────────────────────────

#[cfg(test)]
mod identity_tests {
    use super::*;
    use crate::architecture::SourceId;

    fn disconnected_visible_snapshot(
        revision: u64,
    ) -> crate::source_lifecycle::LifecycleSnapshot<crate::source_registry::AcceptedView> {
        crate::source_lifecycle::LifecycleSnapshot {
            revision,
            state: crate::source_lifecycle::SourceState::Dormant,
            session_epoch: None,
            provenance: crate::source_lifecycle::ProvenanceSet::default(),
            visibility: crate::source_lifecycle::SourceVisibility::Visible,
            retention: crate::source_lifecycle::Retention::Retained,
            catalogue: None,
            views: HashMap::new(),
            failure: None,
            refresh_failures: HashMap::new(),
            pending_connect: None,
            pending_refreshes: HashMap::new(),
            pending_retirements: 0,
        }
    }

    #[test]
    fn saved_and_environment_startup_share_the_persisted_source_owner() {
        let persisted = SourceId::random();
        let mut sources = vec![
            SourceObject::header("Subsonic"),
            SourceObject::manual(
                "Saved",
                "subsonic",
                "HTTPS://MUSIC.EXAMPLE.TEST:443/base/",
                persisted,
            ),
        ];

        let connection_attempt = upsert_environment_source(
            &mut sources,
            "Subsonic (env)",
            "subsonic",
            "https://music.example.test/base",
        );

        let owners: Vec<_> = sources
            .iter()
            .filter(|source| {
                super::super::server_dialogs::same_remote_endpoint(
                    &source.backend_type(),
                    &source.server_url(),
                    "subsonic",
                    "https://music.example.test/base",
                )
            })
            .collect();
        assert_eq!(owners.len(), 1);
        assert_eq!(connection_attempt.source_id, persisted);
        assert_eq!(owners[0].source_id(), Some(persisted));
        assert!(owners[0].manually_added());
        assert!(owners[0].connecting());
    }

    #[test]
    fn accepted_reconnect_clears_connecting_on_an_already_connected_owner() {
        let persisted = SourceId::random();
        let saved = SourceObject::manual(
            "Saved",
            "subsonic",
            "HTTPS://MUSIC.EXAMPLE.TEST:443/base/",
            persisted,
        );
        saved.set_connected(true);
        let mut sources = vec![SourceObject::header("Subsonic"), saved.clone()];

        let connection_attempt = upsert_environment_source(
            &mut sources,
            "Subsonic (env)",
            "subsonic",
            "https://music.example.test/base",
        );

        assert_eq!(connection_attempt.source_id, persisted);
        assert!(saved.connected());
        assert!(saved.connecting());
        saved.set_connecting_generation(42);

        let store = gtk::gio::ListStore::new::<SourceObject>();
        for source in sources {
            store.append(&source);
        }
        let accepted_index = accept_remote_publication(&store, None, persisted, 42);

        assert_eq!(
            accepted_index,
            Some(AcceptedRemotePublication {
                index: 1,
                rebound: true,
            })
        );
        assert!(saved.connected());
        assert!(!saved.connecting());
    }

    #[test]
    fn failed_environment_reconnect_clears_only_the_exact_owner_spinner() {
        let persisted = SourceId::random();
        let other_id = SourceId::random();
        let saved = SourceObject::manual(
            "Saved",
            "subsonic",
            "https://music.example.test/base",
            persisted,
        );
        saved.set_connected(true);
        saved.set_connecting_generation(41);
        let other =
            SourceObject::manual("Other", "subsonic", "https://other.example.test", other_id);
        other.set_connecting_generation(81);

        let store = gtk::gio::ListStore::new::<SourceObject>();
        store.append(&SourceObject::header("Subsonic"));
        store.append(&saved);
        store.append(&other);

        // Retry B takes over the same row after failure A was queued but
        // before GTK receives it. Failure A must not clear retry B's spinner.
        saved.set_connecting_generation(42);
        assert_eq!(clear_failed_remote_connection(&store, persisted, 41), None);
        assert!(saved.connecting());
        assert_eq!(
            clear_failed_remote_connection(&store, persisted, 42),
            Some(1)
        );
        assert!(
            saved.connected(),
            "a failed reconnect keeps the prior session"
        );
        assert!(!saved.connecting());
        assert!(other.connecting());
        assert_eq!(clear_failed_remote_connection(&store, persisted, 42), None);
        saved.set_connecting(true);
        assert_eq!(
            clear_failed_remote_connection(&store, persisted, 42),
            None,
            "a generic/manual retry invalidates the environment token"
        );
        assert!(saved.connecting());
        saved.set_connecting(false);
        assert_eq!(clear_failed_remote_connection(&store, other_id, 82), None);
        assert!(other.connecting());
        assert_eq!(
            clear_failed_remote_connection(&store, other_id, 81),
            Some(2)
        );
    }

    #[test]
    fn predecessor_publication_does_not_clear_replacement_spinner() {
        let source_id = SourceId::random();
        let source =
            SourceObject::manual("Saved", "subsonic", "https://music.example.test", source_id);
        source.set_connected(true);
        source.set_connecting_generation(2);
        let store = gtk::gio::ListStore::new::<SourceObject>();
        store.append(&source);

        assert_eq!(
            accept_remote_publication(&store, None, source_id, 1),
            Some(AcceptedRemotePublication {
                index: 0,
                rebound: false,
            })
        );
        assert!(source.connected());
        assert!(source.connecting());
        assert_eq!(source.connecting_generation(), Some(2));

        assert_eq!(
            accept_remote_publication(&store, None, source_id, 2),
            Some(AcceptedRemotePublication {
                index: 0,
                rebound: true,
            })
        );
        assert!(!source.connecting());
    }

    #[test]
    fn fallback_matching_uses_stable_source_key_not_row_position() {
        let local = SourceObject::source("Local", "local", "folder-music-symbolic");
        let remote = SourceObject::manual(
            "Remote",
            "subsonic",
            "https://music.example.test",
            SourceId::random(),
        );

        assert!(source_matches_navigation_key(&local, "local"));
        assert!(source_matches_navigation_key(
            &remote,
            &remote.source_id().expect("remote identity").to_string()
        ));
        assert!(!source_matches_navigation_key(&remote, "local"));
    }

    #[test]
    fn retained_passwordless_disconnect_falls_back_before_row_rebind_can_reconnect() {
        let source_id = SourceId::random();
        let source_key = source_id.to_string();
        let mut reducer = SourceReducerState::default();
        reducer.published_catalogues.insert(source_id, (41, 7));
        let baseline = crate::source_lifecycle::LifecycleBaseline {
            revision: 9,
            shutting_down: false,
            sources: vec![(source_id, disconnected_visible_snapshot(9))],
        };

        let plan =
            SourceBaselinePlan::new(&reducer, &baseline, &source_key, false, &HashSet::new());
        assert_eq!(plan.clear_projections, vec![source_id]);

        let mut navigation = SourceNavigation::new(source_key.clone());
        let mut active_source_key = source_key.clone();
        assert!(apply_local_navigation_fallback(
            &mut navigation,
            &mut active_source_key,
            &source_key,
        ));
        assert_eq!(active_source_key, "local");
        assert!(navigation.is_key("local"));

        // Model the stable-key restoration that follows connected=false row
        // rebind. Because fallback was part of the authoritative baseline
        // phase, it chooses Local and never re-enters passwordless DAAP.
        let local = SourceObject::source("Local", "local", "folder-music-symbolic");
        let remote = SourceObject::discovered("mini", "daap", "http://mini.local:3689");
        remote.set_requires_password(false);
        remote.set_connected(false);
        let restored = [&local, &remote]
            .into_iter()
            .find(|source| source_matches_navigation_key(source, &active_source_key))
            .expect("visible fallback row");
        let passwordless_reconnects = usize::from(
            restored.backend_type() == "daap"
                && !restored.connected()
                && !restored.requires_password(),
        );
        assert_eq!(restored.backend_type(), "local");
        assert_eq!(passwordless_reconnects, 0);
    }

    #[test]
    fn hidden_external_registration_is_not_a_published_projection_to_clear() {
        let external_id = SourceId::external();
        let mut external = disconnected_visible_snapshot(11);
        external.visibility = crate::source_lifecycle::SourceVisibility::Hidden;
        external.retention = crate::source_lifecycle::Retention::Ephemeral;
        external.session_epoch = Some(29);
        let baseline = crate::source_lifecycle::LifecycleBaseline {
            revision: 11,
            shutting_down: false,
            sources: vec![(external_id, external)],
        };

        let plan = SourceBaselinePlan::new(
            &SourceReducerState::default(),
            &baseline,
            "local",
            false,
            &HashSet::new(),
        );

        assert!(plan.hidden_sources.contains(&external_id));
        assert!(plan.clear_projections.is_empty());
        assert!(!plan.clear_radio_projection);
    }

    #[test]
    fn hidden_pre_catalogue_source_clears_its_owned_sidebar_projection() {
        let source_id = SourceId::random();
        let mut hidden = disconnected_visible_snapshot(12);
        hidden.visibility = crate::source_lifecycle::SourceVisibility::Hidden;
        hidden.retention = crate::source_lifecycle::Retention::Ephemeral;
        let baseline = crate::source_lifecycle::LifecycleBaseline {
            revision: 12,
            shutting_down: false,
            sources: vec![(source_id, hidden)],
        };
        let ui_owned_sources = HashSet::from([source_id]);

        let plan = SourceBaselinePlan::new(
            &SourceReducerState::default(),
            &baseline,
            &source_id.to_string(),
            false,
            &ui_owned_sources,
        );

        assert_eq!(plan.clear_projections, vec![source_id]);
    }

    #[test]
    fn selected_radio_view_without_a_publication_is_invalidated_when_its_source_is_lost() {
        let radio_id = SourceId::radio_browser();
        let reducer = SourceReducerState::default();
        let baseline = crate::source_lifecycle::LifecycleBaseline {
            revision: 3,
            shutting_down: false,
            sources: vec![(radio_id, disconnected_visible_snapshot(3))],
        };

        let plan = SourceBaselinePlan::new(
            &reducer,
            &baseline,
            super::super::radio::TOP_VOTE_SOURCE_KEY,
            false,
            &HashSet::new(),
        );

        assert!(plan.clear_radio_projection);
        assert!(reducer.published_views.is_empty());
        assert_eq!(plan.radio_session_epoch, None);
    }

    #[test]
    fn exact_current_near_me_consent_prerequisite_is_not_misclassified_as_source_loss() {
        let radio_id = SourceId::radio_browser();
        let reducer = SourceReducerState::default();
        let baseline = crate::source_lifecycle::LifecycleBaseline {
            revision: 3,
            shutting_down: false,
            sources: vec![(radio_id, disconnected_visible_snapshot(3))],
        };
        let mut navigation = SourceNavigation::new("local");
        let stale = navigation.select(super::super::radio::NEARME_SOURCE_KEY);
        let current = navigation.select(super::super::radio::NEARME_SOURCE_KEY);

        assert!(!current_near_me_prerequisite(
            super::super::radio::NEARME_SOURCE_KEY,
            &navigation,
            Some(&stale),
        ));
        assert!(current_near_me_prerequisite(
            super::super::radio::NEARME_SOURCE_KEY,
            &navigation,
            Some(&current),
        ));
        let plan = SourceBaselinePlan::new(
            &reducer,
            &baseline,
            super::super::radio::NEARME_SOURCE_KEY,
            true,
            &HashSet::new(),
        );
        assert!(!plan.clear_radio_projection);

        navigation.select("local");
        assert!(!current_near_me_prerequisite(
            super::super::radio::NEARME_SOURCE_KEY,
            &navigation,
            Some(&current),
        ));
    }

    #[test]
    fn only_the_selected_exact_radio_intent_may_surface_a_setup_failure() {
        let selected = SourceNavigation::new(super::super::radio::TOP_CLICK_SOURCE_KEY);
        assert!(selected_radio_projection_owns_status(
            super::super::radio::TOP_CLICK_SOURCE_KEY,
            &selected,
        ));
        assert!(!selected_radio_projection_owns_status("local", &selected));

        let superseded = SourceNavigation::new("local");
        assert!(!selected_radio_projection_owns_status(
            super::super::radio::TOP_CLICK_SOURCE_KEY,
            &superseded,
        ));
    }

    #[test]
    fn accepted_selected_passwordless_generation_reactivates_once_without_reconnect() {
        let remote = SourceObject::discovered("mini", "daap", "http://mini.local:3689");
        let source_id = remote.source_id().expect("stable source");
        let source_key = source_id.to_string();
        remote.set_requires_password(false);
        remote.set_connecting_generation(41);

        let store = gtk::gio::ListStore::new::<SourceObject>();
        store.append(&SourceObject::source(
            "Local",
            "local",
            "folder-music-symbolic",
        ));
        store.append(&remote);

        let mut navigation = SourceNavigation::new("local");
        let request = navigation.select(source_key.clone());
        let mut pending = PendingConnection::new(source_key.clone(), request);
        assert!(pending.bind_lifecycle(source_id, 41, ConnectionIntentKind::PasswordlessDaap,));

        // The baseline reducer updates the row before catalogue publication;
        // its keep-selected rebind is suppressed by the still-live pending
        // guard. Acceptance therefore observes no remaining row mutation.
        remote.set_connected(true);
        assert!(remote.clear_connecting_generation(41));
        let accepted =
            accept_remote_publication(&store, None, source_id, 41).expect("accepted source row");
        assert_eq!(
            accepted,
            AcceptedRemotePublication {
                index: 1,
                rebound: false,
            }
        );
        assert!(remote.connected());
        assert!(!remote.connecting());

        let action = plan_remote_publication_selection(
            Some(&pending),
            &navigation,
            source_id,
            41,
            &source_key,
            accepted,
            1,
            false,
        );
        assert_eq!(action, RemotePublicationSelection::Reactivate(1));

        let mut active_source_key = "local".to_string();
        let mut selected = 1;
        let mut activations = 0;
        let mut passwordless_connects = 1; // the admitted generation G41
        action.apply(|position| {
            selected = position;
            if position == gtk::INVALID_LIST_POSITION {
                return;
            }
            let selected_source = store
                .item(position)
                .and_downcast::<SourceObject>()
                .expect("selected source");
            if selected_source.connected() {
                active_source_key = source_key.clone();
                navigation.select(source_key.clone());
                activations += 1;
            } else if selected_source.backend_type() == "daap"
                && !selected_source.requires_password()
            {
                passwordless_connects += 1;
            }
        });

        assert_eq!(selected, 1);
        assert_eq!(activations, 1);
        assert_eq!(passwordless_connects, 1);
        assert_eq!(active_source_key, source_key);

        assert_eq!(
            plan_remote_publication_selection(
                Some(&pending),
                &navigation,
                source_id,
                42,
                &source_key,
                accepted,
                1,
                false,
            ),
            RemotePublicationSelection::Inactive,
            "a stale lifecycle generation cannot reactivate"
        );
        assert_eq!(
            plan_remote_publication_selection(
                Some(&pending),
                &navigation,
                source_id,
                41,
                &source_key,
                accepted,
                1,
                false,
            ),
            RemotePublicationSelection::Inactive,
            "the reactivation superseded the deferred request exactly once"
        );
    }
}
