//! Virtualized browser for server-owned playlists.
//!
//! The dialog retains only existing source identity, bounded display hints,
//! and broker-minted opaque tokens. Server-native playlist identity and exact
//! session authority never enter GTK properties, action targets, or logs.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::gio;
use gtk::glib;
use gtk::glib::subclass::prelude::*;

use crate::architecture::SourceId;
use crate::local::server_playlist_browser::{
    ServerPlaylistBrowseOutcome, ServerPlaylistBrowserActionOutcome,
    ServerPlaylistBrowserActionToken, ServerPlaylistBrowserEntry, ServerPlaylistBrowserHandle,
    ServerPlaylistBrowserRequestStatus, ServerPlaylistBrowserSessionToken, ServerPlaylistUiRuntime,
};
use crate::source_registry::{ServerPlaylistCapability, SourceRegistry};

use super::objects::SourceObject;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ServerPlaylistBrowserObject {
        pub name: RefCell<String>,
        pub owner: RefCell<Option<String>>,
        pub action_token: RefCell<Option<ServerPlaylistBrowserActionToken>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ServerPlaylistBrowserObject {
        const NAME: &'static str = "TributaryServerPlaylistBrowserObject";
        type Type = super::ServerPlaylistBrowserObject;
    }

    impl ObjectImpl for ServerPlaylistBrowserObject {}
}

glib::wrapper! {
    pub struct ServerPlaylistBrowserObject(ObjectSubclass<imp::ServerPlaylistBrowserObject>);
}

impl ServerPlaylistBrowserObject {
    fn new(entry: &ServerPlaylistBrowserEntry) -> Self {
        let object: Self = glib::Object::builder().build();
        let name = entry
            .name()
            .filter(|name| !name.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| rust_i18n::t!("server_playlists.browser_unnamed").into_owned());
        let owner = entry
            .owner()
            .filter(|owner| !owner.trim().is_empty())
            .map(|owner| {
                rust_i18n::t!("server_playlists.browser_owner", owner = owner).into_owned()
            });
        object.imp().name.replace(name);
        object.imp().owner.replace(owner);
        object
            .imp()
            .action_token
            .replace(Some(entry.action_token()));
        object
    }

    fn name(&self) -> String {
        self.imp().name.borrow().clone()
    }

    fn owner(&self) -> Option<String> {
        self.imp().owner.borrow().clone()
    }

    fn action_token(&self) -> Option<ServerPlaylistBrowserActionToken> {
        self.imp().action_token.borrow().clone()
    }

    fn revoke_token(&self, token: &ServerPlaylistBrowserActionToken) {
        let mut current = self.imp().action_token.borrow_mut();
        if current.as_ref() == Some(token) {
            current.take();
        }
    }
}

struct BrowserSourceChoice {
    source_id: SourceId,
    display_name: String,
    fallback_name: Arc<str>,
}

struct BrowserSessionLease {
    browser: ServerPlaylistBrowserHandle,
    token: ServerPlaylistBrowserSessionToken,
}

fn revoke_session(lease: BrowserSessionLease) {
    match lease.browser.close_session(lease.token.clone()) {
        ServerPlaylistBrowserRequestStatus::Queued | ServerPlaylistBrowserRequestStatus::Closed => {
        }
        ServerPlaylistBrowserRequestStatus::Busy => {
            glib::MainContext::default().spawn_local(async move {
                loop {
                    glib::timeout_future(Duration::from_millis(10)).await;
                    match lease.browser.close_session(lease.token.clone()) {
                        ServerPlaylistBrowserRequestStatus::Queued
                        | ServerPlaylistBrowserRequestStatus::Closed => break,
                        ServerPlaylistBrowserRequestStatus::Busy => {}
                    }
                }
            });
        }
    }
}

fn revoke_ready_browse(
    browser: &ServerPlaylistBrowserHandle,
    outcome: &ServerPlaylistBrowseOutcome,
) {
    if let ServerPlaylistBrowseOutcome::Ready(snapshot) = outcome {
        revoke_session(BrowserSessionLease {
            browser: browser.clone(),
            token: snapshot.session_token(),
        });
    }
}

fn advance_generation(generation: &Cell<Option<u64>>) -> Option<u64> {
    let next = generation.get()?.checked_add(1);
    generation.set(next);
    next
}

fn action_outcome_consumes_token(outcome: Option<ServerPlaylistBrowserActionOutcome>) -> bool {
    !matches!(outcome, Some(ServerPlaylistBrowserActionOutcome::Busy))
}

fn supports_server_playlist_browser(capability: Option<ServerPlaylistCapability>) -> bool {
    capability == Some(ServerPlaylistCapability::PullSnapshots)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrowserAction {
    ImportCopy,
    KeepSynced,
}

impl BrowserAction {
    const fn progress_key(self) -> &'static str {
        match self {
            Self::ImportCopy => "server_playlists.browser_importing",
            Self::KeepSynced => "server_playlists.browser_linking",
        }
    }
}

struct BrowserControllerInner {
    parent: glib::WeakRef<adw::ApplicationWindow>,
    sidebar_store: gio::ListStore,
    source_registry: SourceRegistry,
    tokio: tokio::runtime::Handle,
    runtime: RefCell<Option<ServerPlaylistUiRuntime>>,
    active_dialog: RefCell<Option<Rc<BrowserDialogState>>>,
}

/// Window-scoped owner for the server-playlist browser dialog.
#[derive(Clone)]
pub(super) struct ServerPlaylistBrowserController {
    inner: Rc<BrowserControllerInner>,
}

impl ServerPlaylistBrowserController {
    pub(super) fn new(
        parent: &adw::ApplicationWindow,
        sidebar_store: &gio::ListStore,
        source_registry: SourceRegistry,
        tokio: tokio::runtime::Handle,
    ) -> Self {
        Self {
            inner: Rc::new(BrowserControllerInner {
                parent: parent.downgrade(),
                sidebar_store: sidebar_store.clone(),
                source_registry,
                tokio,
                runtime: RefCell::new(None),
                active_dialog: RefCell::new(None),
            }),
        }
    }

    pub(super) fn set_runtime(&self, runtime: ServerPlaylistUiRuntime) {
        self.inner.runtime.replace(Some(runtime));
        let active_dialog = self.inner.active_dialog.borrow().clone();
        if let Some(dialog) = active_dialog {
            dialog.refresh_sources(&self.inner);
        }
    }

    pub(super) fn show(&self) {
        let Some(parent) = self.inner.parent.upgrade() else {
            return;
        };
        let active_dialog = self.inner.active_dialog.borrow().clone();
        if let Some(dialog) = active_dialog {
            dialog.dialog.present(Some(&parent));
            return;
        }

        let dialog = BrowserDialogState::new(&self.inner);
        self.inner.active_dialog.replace(Some(Rc::clone(&dialog)));
        dialog.dialog.present(Some(&parent));
        dialog.refresh_sources(&self.inner);
        dialog.source_dropdown.grab_focus();
    }

    pub(super) fn source_lifecycle_changed(&self) {
        let active_dialog = self.inner.active_dialog.borrow().clone();
        if let Some(dialog) = active_dialog {
            if dialog.action_running.get() {
                dialog.sources_stale.set(true);
            } else {
                dialog.refresh_sources(&self.inner);
            }
        }
    }

    pub(super) fn close_dialog(&self) {
        let active_dialog = self.inner.active_dialog.borrow().clone();
        if let Some(dialog) = active_dialog {
            dialog.dialog.close();
        }
    }
}

struct BrowserDialogState {
    dialog: adw::Dialog,
    source_dropdown: gtk::DropDown,
    sources: RefCell<Vec<BrowserSourceChoice>>,
    updating_sources: Cell<bool>,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    list_view: gtk::ListView,
    spinner: gtk::Spinner,
    status: gtk::Label,
    reload_button: gtk::Button,
    import_button: gtk::Button,
    keep_synced_button: gtk::Button,
    session: RefCell<Option<BrowserSessionLease>>,
    browse_generation: Cell<Option<u64>>,
    action_generation: Cell<Option<u64>>,
    action_running: Cell<bool>,
    sources_stale: Cell<bool>,
}

impl BrowserDialogState {
    fn new(controller: &Rc<BrowserControllerInner>) -> Rc<Self> {
        let dialog = adw::Dialog::builder()
            .title(rust_i18n::t!("server_playlists.browser_title").as_ref())
            .content_width(620)
            .content_height(520)
            .build();
        let source_dropdown = gtk::DropDown::builder().hexpand(true).build();
        let store = gio::ListStore::new::<ServerPlaylistBrowserObject>();
        let selection = gtk::SingleSelection::new(Some(store.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        let factory = playlist_factory();
        let list_view = gtk::ListView::builder()
            .model(&selection)
            .factory(&factory)
            .single_click_activate(false)
            .vexpand(true)
            .build();
        let list_accessible =
            rust_i18n::t!("server_playlists.browser_list_accessible_label").into_owned();
        list_view.update_property(&[gtk::accessible::Property::Label(&list_accessible)]);

        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);
        spinner.set_accessible_role(gtk::AccessibleRole::Presentation);
        let status = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .build();
        status.set_accessible_role(gtk::AccessibleRole::Status);
        let reload_button =
            gtk::Button::with_label(rust_i18n::t!("server_playlists.browser_reload").as_ref());
        let import_button =
            gtk::Button::with_label(rust_i18n::t!("server_playlists.action_import_copy").as_ref());
        import_button.add_css_class("suggested-action");
        let keep_synced_button =
            gtk::Button::with_label(rust_i18n::t!("server_playlists.action_keep_synced").as_ref());
        import_button.set_sensitive(false);
        keep_synced_button.set_sensitive(false);

        let state = Rc::new(Self {
            dialog,
            source_dropdown,
            sources: RefCell::new(Vec::new()),
            updating_sources: Cell::new(false),
            store,
            selection,
            list_view,
            spinner,
            status,
            reload_button,
            import_button,
            keep_synced_button,
            session: RefCell::new(None),
            browse_generation: Cell::new(Some(0)),
            action_generation: Cell::new(Some(0)),
            action_running: Cell::new(false),
            sources_stale: Cell::new(false),
        });
        state.build_content();
        state.connect_signals(controller);
        state
    }

    fn build_content(&self) {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&adw::HeaderBar::new());

        let source_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_start(16)
            .margin_end(16)
            .margin_top(12)
            .margin_bottom(8)
            .build();
        let source_label_text = rust_i18n::t!("server_playlists.browser_source").into_owned();
        let source_label = gtk::Label::builder()
            .label(&source_label_text)
            .halign(gtk::Align::Start)
            .build();
        source_label.set_mnemonic_widget(Some(&self.source_dropdown));
        self.source_dropdown
            .update_property(&[gtk::accessible::Property::Label(&source_label_text)]);
        source_row.append(&source_label);
        source_row.append(&self.source_dropdown);
        content.append(&source_row);

        let scrolled = gtk::ScrolledWindow::builder()
            .child(&self.list_view)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .build();
        content.append(&scrolled);

        let feedback = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_start(16)
            .margin_end(16)
            .margin_top(8)
            .margin_bottom(8)
            .build();
        feedback.append(&self.spinner);
        feedback.append(&self.status);
        feedback.append(&self.reload_button);
        content.append(&feedback);

        let actions = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .halign(gtk::Align::End)
            .margin_start(16)
            .margin_end(16)
            .margin_bottom(16)
            .build();
        actions.append(&self.import_button);
        actions.append(&self.keep_synced_button);
        content.append(&actions);
        self.dialog.set_child(Some(&content));
    }

    fn connect_signals(self: &Rc<Self>, controller: &Rc<BrowserControllerInner>) {
        let weak_state = Rc::downgrade(self);
        let weak_controller = Rc::downgrade(controller);
        self.source_dropdown.connect_selected_notify(move |_| {
            let (Some(state), Some(controller)) = (weak_state.upgrade(), weak_controller.upgrade())
            else {
                return;
            };
            if !state.updating_sources.get() {
                state.load_selected_source(&controller);
            }
        });

        let weak_state = Rc::downgrade(self);
        self.selection.connect_selection_changed(move |_, _, _| {
            if let Some(state) = weak_state.upgrade() {
                state.refresh_action_sensitivity();
            }
        });

        let weak_state = Rc::downgrade(self);
        let weak_controller = Rc::downgrade(controller);
        self.reload_button.connect_clicked(move |_| {
            if let (Some(state), Some(controller)) =
                (weak_state.upgrade(), weak_controller.upgrade())
            {
                state.focus_status();
                state.load_selected_source(&controller);
            }
        });

        let weak_state = Rc::downgrade(self);
        let weak_controller = Rc::downgrade(controller);
        self.import_button.connect_clicked(move |_| {
            if let (Some(state), Some(controller)) =
                (weak_state.upgrade(), weak_controller.upgrade())
            {
                state.start_action(&controller, BrowserAction::ImportCopy);
            }
        });

        let weak_state = Rc::downgrade(self);
        let weak_controller = Rc::downgrade(controller);
        self.keep_synced_button.connect_clicked(move |_| {
            if let (Some(state), Some(controller)) =
                (weak_state.upgrade(), weak_controller.upgrade())
            {
                state.start_action(&controller, BrowserAction::KeepSynced);
            }
        });

        let weak_state = Rc::downgrade(self);
        let weak_controller = Rc::downgrade(controller);
        self.dialog.connect_closed(move |_| {
            let (Some(state), Some(controller)) = (weak_state.upgrade(), weak_controller.upgrade())
            else {
                return;
            };
            state.invalidate_async_work();
            state.close_session();
            let mut active_dialog = controller.active_dialog.borrow_mut();
            if active_dialog
                .as_ref()
                .is_some_and(|active| Rc::ptr_eq(active, &state))
            {
                active_dialog.take();
            }
        });
    }

    fn collect_sources(controller: &BrowserControllerInner) -> Vec<BrowserSourceChoice> {
        let mut seen = HashSet::new();
        let mut sources = Vec::new();
        for position in 0..controller.sidebar_store.n_items() {
            let Some(source) = controller
                .sidebar_store
                .item(position)
                .and_downcast::<SourceObject>()
            else {
                continue;
            };
            let Some(source_id) = source.source_id() else {
                continue;
            };
            if !seen.insert(source_id)
                || !supports_server_playlist_browser(
                    controller
                        .source_registry
                        .current_server_playlist_capability(source_id),
                )
            {
                continue;
            }
            let name = source.name();
            sources.push(BrowserSourceChoice {
                source_id,
                display_name: name.clone(),
                fallback_name: Arc::from(name),
            });
        }
        sources
    }

    fn refresh_sources(self: &Rc<Self>, controller: &Rc<BrowserControllerInner>) {
        if self.action_running.get() {
            self.sources_stale.set(true);
            return;
        }
        self.sources_stale.set(false);
        self.preserve_focus_before_list_reset();
        let _ = advance_generation(&self.browse_generation);
        self.close_session();
        let old_source = {
            let sources = self.sources.borrow();
            sources
                .get(self.source_dropdown.selected() as usize)
                .map(|choice| choice.source_id)
        };
        let sources = Self::collect_sources(controller);
        let labels: Vec<&str> = sources
            .iter()
            .map(|source| source.display_name.as_str())
            .collect();
        if sources.is_empty() && self.source_dropdown.has_focus() {
            self.focus_status();
        }
        let model = gtk::StringList::new(&labels);
        let selected = old_source
            .and_then(|source_id| {
                sources
                    .iter()
                    .position(|choice| choice.source_id == source_id)
            })
            .unwrap_or(0);
        self.updating_sources.set(true);
        self.source_dropdown.set_model(Some(&model));
        self.source_dropdown
            .set_selected(u32::try_from(selected).unwrap_or(0));
        self.source_dropdown.set_sensitive(!sources.is_empty());
        self.sources.replace(sources);
        self.updating_sources.set(false);

        if self.sources.borrow().is_empty() {
            self.store.remove_all();
            self.stop_loading();
            self.set_status("server_playlists.browser_failed");
            self.reload_button.set_sensitive(true);
            self.refresh_action_sensitivity();
        } else {
            self.load_selected_source(controller);
        }
    }

    fn load_selected_source(self: &Rc<Self>, controller: &Rc<BrowserControllerInner>) {
        if self.action_running.get() {
            return;
        }
        self.close_session();
        let Some(generation) = advance_generation(&self.browse_generation) else {
            if self.source_dropdown.has_focus() {
                self.focus_status();
            }
            self.store.remove_all();
            self.stop_loading();
            self.set_status("server_playlists.browser_failed");
            self.source_dropdown.set_sensitive(false);
            self.reload_button.set_sensitive(false);
            self.refresh_action_sensitivity();
            return;
        };
        let selected = self.source_dropdown.selected() as usize;
        let (source_id, fallback_name) = {
            let sources = self.sources.borrow();
            let Some(choice) = sources.get(selected) else {
                self.store.remove_all();
                self.stop_loading();
                self.set_status("server_playlists.browser_failed");
                self.reload_button.set_sensitive(true);
                self.refresh_action_sensitivity();
                return;
            };
            (choice.source_id, Arc::clone(&choice.fallback_name))
        };
        let Some(runtime) = controller.runtime.borrow().clone() else {
            self.store.remove_all();
            self.stop_loading();
            self.set_status("server_playlists.browser_failed");
            self.reload_button.set_sensitive(true);
            self.refresh_action_sensitivity();
            return;
        };

        self.store.remove_all();
        self.selection.set_selected(gtk::INVALID_LIST_POSITION);
        self.spinner.set_visible(true);
        self.spinner.start();
        self.list_view
            .update_state(&[gtk::accessible::State::Busy(true)]);
        self.set_status("server_playlists.browser_loading");
        self.reload_button.set_sensitive(false);
        self.refresh_action_sensitivity();

        let browser = runtime.browser();
        let submission = browser.browse(source_id, fallback_name);
        let (result_tx, result_rx) = async_channel::bounded(1);
        controller.tokio.spawn(async move {
            let _ = result_tx.send(submission.completion().await).await;
        });
        let weak_state = Rc::downgrade(self);
        let browser_for_result = browser.clone();
        glib::MainContext::default().spawn_local(async move {
            let Ok(outcome) = result_rx.recv().await else {
                if let Some(state) = weak_state.upgrade() {
                    state.finish_browse_failure(generation, "server_playlists.browser_failed");
                }
                return;
            };
            let Some(state) = weak_state.upgrade() else {
                revoke_ready_browse(&browser_for_result, &outcome);
                return;
            };
            if state.browse_generation.get() != Some(generation) {
                revoke_ready_browse(&browser_for_result, &outcome);
                return;
            }
            let status_had_focus = state.status.has_focus();
            state.stop_loading();
            state.reload_button.set_sensitive(true);
            match outcome {
                ServerPlaylistBrowseOutcome::Ready(snapshot) => {
                    let old_session = state.session.replace(Some(BrowserSessionLease {
                        browser: browser_for_result,
                        token: snapshot.session_token(),
                    }));
                    if let Some(old_session) = old_session {
                        revoke_session(old_session);
                    }
                    for entry in snapshot.entries() {
                        state.store.append(&ServerPlaylistBrowserObject::new(entry));
                    }
                    if snapshot.entries().is_empty() {
                        state.set_status("server_playlists.browser_empty");
                    } else {
                        state.clear_status();
                        if status_had_focus {
                            state.list_view.grab_focus();
                        }
                    }
                }
                ServerPlaylistBrowseOutcome::Busy => {
                    state.set_status("server_playlists.browser_busy");
                }
                ServerPlaylistBrowseOutcome::Unsupported
                | ServerPlaylistBrowseOutcome::Unavailable
                | ServerPlaylistBrowseOutcome::Failed
                | ServerPlaylistBrowseOutcome::Superseded
                | ServerPlaylistBrowseOutcome::Closed
                | ServerPlaylistBrowseOutcome::Interrupted => {
                    state.set_status("server_playlists.browser_failed");
                }
            }
            state.refresh_action_sensitivity();
        });
    }

    fn finish_browse_failure(&self, generation: u64, key: &str) {
        if self.browse_generation.get() != Some(generation) {
            return;
        }
        self.stop_loading();
        self.reload_button.set_sensitive(true);
        self.set_status(key);
        self.refresh_action_sensitivity();
    }

    fn selected_object(&self) -> Option<ServerPlaylistBrowserObject> {
        self.selection
            .selected_item()?
            .downcast::<ServerPlaylistBrowserObject>()
            .ok()
    }

    fn refresh_action_sensitivity(&self) {
        let enabled = self.action_generation.get().is_some()
            && !self.action_running.get()
            && self
                .selected_object()
                .is_some_and(|item| item.action_token().is_some());
        self.import_button.set_sensitive(enabled);
        self.keep_synced_button.set_sensitive(enabled);
    }

    fn start_action(
        self: &Rc<Self>,
        controller: &Rc<BrowserControllerInner>,
        action: BrowserAction,
    ) {
        if self.action_running.get() {
            return;
        }
        let Some(item) = self.selected_object() else {
            return;
        };
        let Some(token) = item.action_token() else {
            return;
        };
        let Some(runtime) = controller.runtime.borrow().clone() else {
            self.set_status("server_playlists.browser_action_failed");
            return;
        };
        let Some(generation) = advance_generation(&self.action_generation) else {
            item.revoke_token(&token);
            self.set_status("server_playlists.browser_action_failed");
            self.focus_status();
            self.refresh_action_sensitivity();
            return;
        };
        let submission = match action {
            BrowserAction::ImportCopy => runtime.browser().import_copy(token.clone()),
            BrowserAction::KeepSynced => runtime.browser().keep_synced(token.clone()),
        };
        self.action_running.set(true);
        self.list_view.set_sensitive(false);
        self.list_view
            .update_state(&[gtk::accessible::State::Busy(true)]);
        self.source_dropdown.set_sensitive(false);
        self.reload_button.set_sensitive(false);
        self.set_status(action.progress_key());
        self.focus_status();
        self.refresh_action_sensitivity();

        let (result_tx, result_rx) = async_channel::bounded(1);
        controller.tokio.spawn(async move {
            let _ = result_tx.send(submission.completion().await).await;
        });
        let weak_state = Rc::downgrade(self);
        let weak_controller = Rc::downgrade(controller);
        glib::MainContext::default().spawn_local(async move {
            let outcome = result_rx.recv().await.ok();
            if action_outcome_consumes_token(outcome) {
                item.revoke_token(&token);
            }
            let (Some(state), Some(controller)) = (weak_state.upgrade(), weak_controller.upgrade())
            else {
                return;
            };
            if state.action_generation.get() != Some(generation) {
                return;
            }
            state.action_running.set(false);
            state.list_view.set_sensitive(true);
            state.list_view.reset_state(gtk::AccessibleState::Busy);
            state.reload_button.set_sensitive(true);
            let key = match outcome {
                Some(ServerPlaylistBrowserActionOutcome::Imported) => {
                    "server_playlists.browser_imported"
                }
                Some(ServerPlaylistBrowserActionOutcome::Linked) => {
                    "server_playlists.browser_linked"
                }
                Some(ServerPlaylistBrowserActionOutcome::AlreadyLinked) => {
                    "server_playlists.browser_already_linked"
                }
                Some(
                    ServerPlaylistBrowserActionOutcome::Rejected
                    | ServerPlaylistBrowserActionOutcome::Unavailable
                    | ServerPlaylistBrowserActionOutcome::Superseded,
                ) => "server_playlists.browser_reload_required",
                Some(ServerPlaylistBrowserActionOutcome::Busy) => "server_playlists.browser_busy",
                Some(
                    ServerPlaylistBrowserActionOutcome::Unsupported
                    | ServerPlaylistBrowserActionOutcome::Failed
                    | ServerPlaylistBrowserActionOutcome::Closed
                    | ServerPlaylistBrowserActionOutcome::Interrupted,
                )
                | None => "server_playlists.browser_action_failed",
            };
            state.set_status(key);
            let retry_focus = matches!(outcome, Some(ServerPlaylistBrowserActionOutcome::Busy))
                && state.status.has_focus();
            if state.sources_stale.replace(false) {
                state.refresh_sources(&controller);
            } else {
                state
                    .source_dropdown
                    .set_sensitive(!state.sources.borrow().is_empty());
                state.refresh_action_sensitivity();
                if retry_focus {
                    state.action_button(action).grab_focus();
                }
            }
        });
    }

    fn action_button(&self, action: BrowserAction) -> &gtk::Button {
        match action {
            BrowserAction::ImportCopy => &self.import_button,
            BrowserAction::KeepSynced => &self.keep_synced_button,
        }
    }

    fn preserve_focus_before_list_reset(&self) {
        let focus_is_in_list = self.list_view.has_focus()
            || adw::prelude::AdwDialogExt::focus(&self.dialog)
                .is_some_and(|focus| focus.is_ancestor(&self.list_view));
        if focus_is_in_list {
            self.focus_status();
        }
    }

    fn close_session(&self) {
        let Some(session) = self.session.borrow_mut().take() else {
            return;
        };
        revoke_session(session);
    }

    fn invalidate_async_work(&self) {
        let _ = advance_generation(&self.browse_generation);
        let _ = advance_generation(&self.action_generation);
    }

    fn stop_loading(&self) {
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.list_view.reset_state(gtk::AccessibleState::Busy);
    }

    fn set_status(&self, key: &str) {
        let value = rust_i18n::t!(key).into_owned();
        self.status.set_focusable(true);
        self.status.set_text(&value);
        self.status
            .update_property(&[gtk::accessible::Property::Label(&value)]);
    }

    fn clear_status(&self) {
        self.status.set_text("");
        self.status.set_focusable(false);
        self.status.reset_property(gtk::AccessibleProperty::Label);
    }

    fn focus_status(&self) {
        self.status.set_focusable(true);
        self.status.grab_focus();
    }
}

impl Drop for BrowserDialogState {
    fn drop(&mut self) {
        if let Some(session) = self.session.get_mut().take() {
            revoke_session(session);
        }
    }
}

fn playlist_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let title = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let owner = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .visible(false)
            .build();
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        row.append(&title);
        row.append(&owner);
        item.set_child(Some(&row));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(object) = item.item().and_downcast::<ServerPlaylistBrowserObject>() else {
            return;
        };
        let Some(row) = item.child().and_downcast::<gtk::Box>() else {
            return;
        };
        let Some(title) = row.first_child().and_downcast::<gtk::Label>() else {
            return;
        };
        let Some(owner) = title.next_sibling().and_downcast::<gtk::Label>() else {
            return;
        };
        let name = object.name();
        title.set_text(&name);
        let owner_text = object.owner();
        owner.set_text(owner_text.as_deref().unwrap_or_default());
        owner.set_visible(owner_text.is_some());
        let accessible =
            owner_text.map_or_else(|| name.clone(), |owner| format!("{name}, {owner}"));
        row.update_property(&[gtk::accessible::Property::Label(&accessible)]);
    });
    factory.connect_unbind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(row) = item.child().and_downcast::<gtk::Box>() else {
            return;
        };
        let Some(title) = row.first_child().and_downcast::<gtk::Label>() else {
            return;
        };
        let Some(owner) = title.next_sibling().and_downcast::<gtk::Label>() else {
            return;
        };
        title.set_text("");
        owner.set_text("");
        owner.set_visible(false);
        row.reset_property(gtk::AccessibleProperty::Label);
    });
    factory
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::{
        action_outcome_consumes_token, advance_generation, supports_server_playlist_browser,
        BrowserAction,
    };
    use crate::local::server_playlist_browser::ServerPlaylistBrowserActionOutcome;
    use crate::source_registry::ServerPlaylistCapability;

    #[test]
    fn action_progress_copy_is_fixed_and_content_free() {
        assert_eq!(
            BrowserAction::ImportCopy.progress_key(),
            "server_playlists.browser_importing"
        );
        assert_eq!(
            BrowserAction::KeepSynced.progress_key(),
            "server_playlists.browser_linking"
        );
    }

    #[test]
    fn capacity_busy_is_the_only_settlement_that_preserves_the_action_token() {
        assert!(!action_outcome_consumes_token(Some(
            ServerPlaylistBrowserActionOutcome::Busy
        )));

        for outcome in [
            ServerPlaylistBrowserActionOutcome::Imported,
            ServerPlaylistBrowserActionOutcome::Linked,
            ServerPlaylistBrowserActionOutcome::AlreadyLinked,
            ServerPlaylistBrowserActionOutcome::Rejected,
            ServerPlaylistBrowserActionOutcome::Unsupported,
            ServerPlaylistBrowserActionOutcome::Unavailable,
            ServerPlaylistBrowserActionOutcome::Failed,
            ServerPlaylistBrowserActionOutcome::Superseded,
            ServerPlaylistBrowserActionOutcome::Closed,
            ServerPlaylistBrowserActionOutcome::Interrupted,
        ] {
            assert!(action_outcome_consumes_token(Some(outcome)));
        }
        assert!(action_outcome_consumes_token(None));
    }

    #[test]
    fn ui_generations_fail_closed_at_exhaustion() {
        let generation = Cell::new(Some(u64::MAX - 1));
        assert_eq!(advance_generation(&generation), Some(u64::MAX));
        assert_eq!(advance_generation(&generation), None);
        assert_eq!(generation.get(), None);
        assert_eq!(advance_generation(&generation), None);
    }

    #[test]
    fn source_filter_exposes_only_pull_snapshot_capability() {
        assert!(supports_server_playlist_browser(Some(
            ServerPlaylistCapability::PullSnapshots
        )));
        assert!(!supports_server_playlist_browser(Some(
            ServerPlaylistCapability::Unsupported
        )));
        assert!(!supports_server_playlist_browser(None));
    }
}
