//! Visible recovery controls for linked server playlists.
//!
//! GTK is allowed to retain only the already-published local playlist ID.
//! Source and server-native playlist identity remain behind the headless
//! runtime. Every activation re-reads the current sidebar row, and every
//! asynchronous result is generation-gated before it may affect widgets.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::db::entities::server_playlist_link::{
    ServerPlaylistLocalState, ServerPlaylistRemoteState,
};
use crate::local::server_playlist_runtime::{
    ServerPlaylistLinkInspection, ServerPlaylistOperationOutcome, ServerPlaylistOperations,
};

use super::objects::{PlaylistSidebarKind, SourceObject};
use super::tracklist::{
    ServerPlaylistActivity, ServerPlaylistAvailability, ServerPlaylistScope,
    ServerPlaylistStatusShell, ServerPlaylistStatusState,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryAction {
    SyncNow,
    Retry,
    ReplaceLocalWithServer,
    Unlink,
    RemoveLocalCopy,
}

impl RecoveryAction {
    const ALL: [Self; 5] = [
        Self::SyncNow,
        Self::Retry,
        Self::ReplaceLocalWithServer,
        Self::Unlink,
        Self::RemoveLocalCopy,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::SyncNow => "server-playlist-sync-now",
            Self::Retry => "server-playlist-retry",
            Self::ReplaceLocalWithServer => "server-playlist-replace-local",
            Self::Unlink => "server-playlist-unlink",
            Self::RemoveLocalCopy => "server-playlist-remove-local-copy",
        }
    }

    const fn confirmation(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::ReplaceLocalWithServer => Some((
                "server_playlists.replace_heading",
                "server_playlists.replace_body",
            )),
            Self::Unlink => Some((
                "server_playlists.unlink_heading",
                "server_playlists.unlink_body",
            )),
            Self::RemoveLocalCopy => Some((
                "server_playlists.remove_heading",
                "server_playlists.remove_body",
            )),
            Self::SyncNow | Self::Retry => None,
        }
    }
}

#[derive(Clone)]
struct MirrorSelection {
    local_playlist_id: Arc<str>,
    conflict: bool,
    missing: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RecoveryActivity {
    #[default]
    Idle,
    Running,
    Failed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum InspectionState {
    #[default]
    Pending,
    Linked {
        available: bool,
    },
    NotLinked,
    Failed,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RecoveryPresentation {
    state: ServerPlaylistStatusState,
    enabled: [bool; 5],
}

impl RecoveryPresentation {
    fn hidden() -> Self {
        Self {
            state: ServerPlaylistStatusState::default(),
            enabled: [false; 5],
        }
    }

    fn for_mirror(
        conflict: bool,
        missing: bool,
        activity: RecoveryActivity,
        inspection: InspectionState,
    ) -> Self {
        let (available, inspected_link) = match inspection {
            InspectionState::Linked { available } => (available, true),
            InspectionState::Pending if activity != RecoveryActivity::Idle => (false, false),
            InspectionState::Failed => (false, false),
            InspectionState::Pending | InspectionState::NotLinked | InspectionState::Closed => {
                return Self::hidden()
            }
        };
        let inspection_failed = inspection == InspectionState::Failed;
        let state = ServerPlaylistStatusState {
            scope: ServerPlaylistScope::Linked,
            activity: match (activity, inspection_failed) {
                (RecoveryActivity::Idle, true) => ServerPlaylistActivity::Failed,
                (RecoveryActivity::Idle, false) => ServerPlaylistActivity::Idle,
                (RecoveryActivity::Running, _) => ServerPlaylistActivity::Running,
                (RecoveryActivity::Failed, _) => ServerPlaylistActivity::Failed,
            },
            conflict,
            missing,
            availability: if available {
                ServerPlaylistAvailability::Available
            } else {
                ServerPlaylistAvailability::Unavailable
            },
        };

        if activity == RecoveryActivity::Running || !inspected_link {
            return Self {
                state,
                enabled: [false; 5],
            };
        }

        let mut enabled = [false; 5];
        // Source-independent recovery remains available while disconnected.
        enabled[RecoveryAction::Unlink as usize] = true;
        enabled[RecoveryAction::RemoveLocalCopy as usize] = true;
        if available {
            if missing {
                enabled[RecoveryAction::Retry as usize] = true;
            } else if conflict {
                enabled[RecoveryAction::ReplaceLocalWithServer as usize] = true;
            } else if activity == RecoveryActivity::Failed {
                enabled[RecoveryAction::Retry as usize] = true;
            } else {
                enabled[RecoveryAction::SyncNow as usize] = true;
            }
        }
        Self { state, enabled }
    }

    fn enables(self, action: RecoveryAction) -> bool {
        self.enabled[action as usize]
    }
}

fn activation_snapshot_is_current(
    expected_generation: u64,
    current_generation: u64,
    expected_playlist_id: &str,
    current_playlist_id: Option<&str>,
) -> bool {
    expected_generation == current_generation && current_playlist_id == Some(expected_playlist_id)
}

const fn next_generation(current: u64) -> Option<u64> {
    current.checked_add(1)
}

struct OperationState {
    generation: u64,
    activity: RecoveryActivity,
}

fn settle_operation(
    states: &mut HashMap<Arc<str>, OperationState>,
    playlist_id: &Arc<str>,
    generation: u64,
    outcome: ServerPlaylistOperationOutcome,
) -> bool {
    if states.get(playlist_id).map(|state| state.generation) != Some(generation) {
        return false;
    }

    match outcome {
        ServerPlaylistOperationOutcome::Applied
        | ServerPlaylistOperationOutcome::Conflict
        | ServerPlaylistOperationOutcome::Missing
        | ServerPlaylistOperationOutcome::Unlinked
        | ServerPlaylistOperationOutcome::Removed
        | ServerPlaylistOperationOutcome::Superseded => {
            states.remove(playlist_id);
        }
        ServerPlaylistOperationOutcome::Rejected
        | ServerPlaylistOperationOutcome::Unavailable
        | ServerPlaylistOperationOutcome::Failed
        | ServerPlaylistOperationOutcome::Closed
        | ServerPlaylistOperationOutcome::Interrupted => {
            states
                .get_mut(playlist_id)
                .expect("generation was checked above")
                .activity = RecoveryActivity::Failed;
        }
    }
    true
}

struct RecoveryInner {
    window: adw::ApplicationWindow,
    selection: gtk::SingleSelection,
    shell: ServerPlaylistStatusShell,
    runtime: tokio::runtime::Handle,
    operations: RefCell<Option<ServerPlaylistOperations>>,
    actions: RefCell<Vec<(RecoveryAction, gio::SimpleAction)>>,
    inspection_generation: Cell<u64>,
    inspected: RefCell<Option<(Arc<str>, InspectionState)>>,
    operation_generation: Cell<u64>,
    operation_states: RefCell<HashMap<Arc<str>, OperationState>>,
    generation_exhausted: Cell<bool>,
}

/// Lifecycle-owned facade for the linked-playlist footer and its five
/// targetless window actions.
#[derive(Clone)]
pub(super) struct ServerPlaylistRecoveryController {
    inner: Rc<RecoveryInner>,
}

impl ServerPlaylistRecoveryController {
    pub(super) fn new(
        window: &adw::ApplicationWindow,
        selection: &gtk::SingleSelection,
        shell: ServerPlaylistStatusShell,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let controller = Self {
            inner: Rc::new(RecoveryInner {
                window: window.clone(),
                selection: selection.clone(),
                shell,
                runtime,
                operations: RefCell::new(None),
                actions: RefCell::new(Vec::new()),
                inspection_generation: Cell::new(0),
                inspected: RefCell::new(None),
                operation_generation: Cell::new(0),
                operation_states: RefCell::new(HashMap::new()),
                generation_exhausted: Cell::new(false),
            }),
        };

        for action in RecoveryAction::ALL {
            let simple_action = gio::SimpleAction::new(action.name(), None);
            simple_action.set_enabled(false);
            let weak_inner = Rc::downgrade(&controller.inner);
            simple_action.connect_activate(move |_, _| {
                let Some(inner) = weak_inner.upgrade() else {
                    return;
                };
                Self { inner }.activate(action);
            });
            window.add_action(&simple_action);
            controller
                .inner
                .actions
                .borrow_mut()
                .push((action, simple_action));
        }

        let weak_inner = Rc::downgrade(&controller.inner);
        selection.connect_selection_changed(move |_, _, _| {
            let Some(inner) = weak_inner.upgrade() else {
                return;
            };
            Self { inner }.refresh_inspection();
        });
        controller.render();
        controller
    }

    pub(super) fn set_operations(&self, operations: ServerPlaylistOperations) {
        self.inner.operations.replace(Some(operations));
        self.refresh_inspection();
    }

    /// Re-inspect after one authoritative full sidebar snapshot. Failed UI
    /// attempts are cleared, while admitted operations remain visibly busy
    /// until their own completion settles.
    pub(super) fn playlist_snapshot_changed(&self) {
        self.inner
            .operation_states
            .borrow_mut()
            .retain(|_, state| state.activity == RecoveryActivity::Running);
        self.refresh_inspection();
    }

    /// Source lifecycle invalidations can change exact-session availability
    /// without changing durable playlist state.
    pub(super) fn source_lifecycle_changed(&self) {
        self.refresh_inspection();
    }

    fn selected_mirror(&self) -> Option<MirrorSelection> {
        let source = self
            .inner
            .selection
            .selected_item()?
            .downcast::<SourceObject>()
            .ok()?;
        let PlaylistSidebarKind::PullMirror {
            local_state,
            remote_state,
        } = source.playlist_kind()?
        else {
            return None;
        };
        Some(MirrorSelection {
            local_playlist_id: Arc::from(source.playlist_id()),
            conflict: local_state == ServerPlaylistLocalState::Conflict,
            missing: remote_state == ServerPlaylistRemoteState::Missing,
        })
    }

    fn current_presentation(&self) -> RecoveryPresentation {
        if self.inner.generation_exhausted.get() {
            return RecoveryPresentation::hidden();
        }
        let Some(mirror) = self.selected_mirror() else {
            return RecoveryPresentation::hidden();
        };
        let activity = self
            .inner
            .operation_states
            .borrow()
            .get(&mirror.local_playlist_id)
            .map_or(RecoveryActivity::Idle, |state| state.activity);
        let inspection = self
            .inner
            .inspected
            .borrow()
            .as_ref()
            .filter(|(playlist_id, _)| playlist_id == &mirror.local_playlist_id)
            .map_or(InspectionState::Pending, |(_, state)| *state);
        RecoveryPresentation::for_mirror(mirror.conflict, mirror.missing, activity, inspection)
    }

    fn render(&self) {
        let presentation = self.current_presentation();
        self.inner
            .shell
            .render(presentation.state, rust_i18n::locale().as_ref());
        for (action, simple_action) in self.inner.actions.borrow().iter() {
            simple_action.set_enabled(presentation.enables(*action));
        }
    }

    fn refresh_inspection(&self) {
        if self.inner.generation_exhausted.get() {
            self.render();
            return;
        }
        let Some(generation) = next_generation(self.inner.inspection_generation.get()) else {
            self.exhaust_generations();
            return;
        };
        self.inner.inspection_generation.set(generation);
        self.inner.inspected.replace(None);
        self.render();

        let Some(mirror) = self.selected_mirror() else {
            return;
        };
        let Some(operations) = self.inner.operations.borrow().clone() else {
            return;
        };
        let playlist_id = Arc::clone(&mirror.local_playlist_id);
        let playlist_id_for_worker = Arc::clone(&playlist_id);
        let (result_tx, result_rx) = async_channel::bounded(1);
        self.inner.runtime.spawn(async move {
            let outcome = operations.inspect_link(playlist_id_for_worker).await;
            let _ = result_tx.send(outcome).await;
        });

        let weak_inner = Rc::downgrade(&self.inner);
        glib::MainContext::default().spawn_local(async move {
            let outcome = result_rx
                .recv()
                .await
                .unwrap_or(ServerPlaylistLinkInspection::Failed);
            let Some(inner) = weak_inner.upgrade() else {
                return;
            };
            let controller = Self { inner };
            if controller.inner.inspection_generation.get() != generation
                || controller
                    .selected_mirror()
                    .is_none_or(|selected| selected.local_playlist_id != playlist_id)
            {
                return;
            }
            let state = match outcome {
                ServerPlaylistLinkInspection::NotLinked => InspectionState::NotLinked,
                ServerPlaylistLinkInspection::Linked { available } => {
                    InspectionState::Linked { available }
                }
                ServerPlaylistLinkInspection::Failed => InspectionState::Failed,
                ServerPlaylistLinkInspection::Closed => InspectionState::Closed,
            };
            controller
                .inner
                .inspected
                .replace(Some((playlist_id, state)));
            controller.render();
        });
    }

    fn activate(&self, action: RecoveryAction) {
        let Some(mirror) = self.selected_mirror() else {
            return;
        };
        if !self.current_presentation().enables(action) {
            return;
        }
        let inspection_generation = self.inner.inspection_generation.get();
        if let Some((heading_key, body_key)) = action.confirmation() {
            let dialog = adw::AlertDialog::builder()
                .heading(rust_i18n::t!(heading_key).as_ref())
                .body(rust_i18n::t!(body_key).as_ref())
                .close_response("cancel")
                .default_response("cancel")
                .build();
            dialog.add_response("cancel", rust_i18n::t!("dialogs.cancel").as_ref());
            dialog.add_response("confirm", rust_i18n::t!(action_label_key(action)).as_ref());
            dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
            let weak_inner = Rc::downgrade(&self.inner);
            dialog.connect_response(None, move |_, response| {
                if response != "confirm" {
                    return;
                }
                let Some(inner) = weak_inner.upgrade() else {
                    return;
                };
                Self { inner }.start(
                    action,
                    Arc::clone(&mirror.local_playlist_id),
                    inspection_generation,
                );
            });
            dialog.present(Some(&self.inner.window));
        } else {
            self.start(action, mirror.local_playlist_id, inspection_generation);
        }
    }

    fn start(
        &self,
        action: RecoveryAction,
        expected_playlist_id: Arc<str>,
        expected_inspection_generation: u64,
    ) {
        let Some(current) = self.selected_mirror() else {
            return;
        };
        if !activation_snapshot_is_current(
            expected_inspection_generation,
            self.inner.inspection_generation.get(),
            expected_playlist_id.as_ref(),
            Some(current.local_playlist_id.as_ref()),
        ) || !self.current_presentation().enables(action)
        {
            return;
        }
        let Some(operations) = self.inner.operations.borrow().clone() else {
            return;
        };
        let Some(generation) = next_generation(self.inner.operation_generation.get()) else {
            self.exhaust_generations();
            return;
        };
        self.inner.operation_generation.set(generation);
        self.inner.operation_states.borrow_mut().insert(
            Arc::clone(&expected_playlist_id),
            OperationState {
                generation,
                activity: RecoveryActivity::Running,
            },
        );
        self.render();

        let submission = match action {
            RecoveryAction::SyncNow => operations.sync_now(Arc::clone(&expected_playlist_id)),
            RecoveryAction::Retry => operations.retry(Arc::clone(&expected_playlist_id)),
            RecoveryAction::ReplaceLocalWithServer => {
                operations.replace_local_with_server(Arc::clone(&expected_playlist_id))
            }
            RecoveryAction::Unlink => operations.unlink(Arc::clone(&expected_playlist_id)),
            RecoveryAction::RemoveLocalCopy => {
                operations.remove_local_copy(Arc::clone(&expected_playlist_id))
            }
        };
        let (result_tx, result_rx) = async_channel::bounded(1);
        self.inner.runtime.spawn(async move {
            let _ = result_tx.send(submission.completion().await).await;
        });

        let weak_inner = Rc::downgrade(&self.inner);
        glib::MainContext::default().spawn_local(async move {
            let outcome = result_rx
                .recv()
                .await
                .unwrap_or(ServerPlaylistOperationOutcome::Interrupted);
            let Some(inner) = weak_inner.upgrade() else {
                return;
            };
            let controller = Self { inner };
            let settled = settle_operation(
                &mut controller.inner.operation_states.borrow_mut(),
                &expected_playlist_id,
                generation,
                outcome,
            );
            if !settled {
                return;
            }
            controller.refresh_inspection();
        });
    }

    fn exhaust_generations(&self) {
        self.inner.generation_exhausted.set(true);
        self.inner.inspected.replace(None);
        self.inner.operation_states.borrow_mut().clear();
        self.render();
    }
}

const fn action_label_key(action: RecoveryAction) -> &'static str {
    match action {
        RecoveryAction::SyncNow => "server_playlists.action_sync_now",
        RecoveryAction::Retry => "server_playlists.action_retry",
        RecoveryAction::ReplaceLocalWithServer => {
            "server_playlists.action_replace_local_with_server"
        }
        RecoveryAction::Unlink => "server_playlists.action_unlink",
        RecoveryAction::RemoveLocalCopy => "server_playlists.action_remove_local_copy",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::{
        activation_snapshot_is_current, next_generation, settle_operation, InspectionState,
        OperationState, RecoveryAction, RecoveryActivity, RecoveryPresentation,
    };
    use crate::local::server_playlist_runtime::ServerPlaylistOperationOutcome;
    use crate::ui::tracklist::{
        ServerPlaylistActivity, ServerPlaylistAvailability, ServerPlaylistScope,
    };

    #[test]
    fn clean_available_mirror_exposes_only_current_safe_actions() {
        let plan = RecoveryPresentation::for_mirror(
            false,
            false,
            RecoveryActivity::Idle,
            InspectionState::Linked { available: true },
        );
        assert_eq!(plan.state.scope, ServerPlaylistScope::Linked);
        assert!(plan.enables(RecoveryAction::SyncNow));
        assert!(plan.enables(RecoveryAction::Unlink));
        assert!(plan.enables(RecoveryAction::RemoveLocalCopy));
        assert!(!plan.enables(RecoveryAction::Retry));
        assert!(!plan.enables(RecoveryAction::ReplaceLocalWithServer));
    }

    #[test]
    fn conflict_and_missing_choose_one_network_recovery() {
        let conflict = RecoveryPresentation::for_mirror(
            true,
            false,
            RecoveryActivity::Idle,
            InspectionState::Linked { available: true },
        );
        assert!(conflict.enables(RecoveryAction::ReplaceLocalWithServer));
        assert!(!conflict.enables(RecoveryAction::Retry));

        let conflict_missing = RecoveryPresentation::for_mirror(
            true,
            true,
            RecoveryActivity::Idle,
            InspectionState::Linked { available: true },
        );
        assert!(conflict_missing.enables(RecoveryAction::Retry));
        assert!(!conflict_missing.enables(RecoveryAction::ReplaceLocalWithServer));
    }

    #[test]
    fn offline_keeps_only_source_independent_recovery_enabled() {
        let plan = RecoveryPresentation::for_mirror(
            false,
            false,
            RecoveryActivity::Failed,
            InspectionState::Linked { available: false },
        );
        assert_eq!(
            plan.state.availability,
            ServerPlaylistAvailability::Unavailable
        );
        assert!(plan.enables(RecoveryAction::Unlink));
        assert!(plan.enables(RecoveryAction::RemoveLocalCopy));
        assert!(!plan.enables(RecoveryAction::SyncNow));
        assert!(!plan.enables(RecoveryAction::Retry));
        assert!(!plan.enables(RecoveryAction::ReplaceLocalWithServer));
    }

    #[test]
    fn pending_failed_and_closed_inspections_grant_no_action_authority() {
        let pending = RecoveryPresentation::for_mirror(
            false,
            false,
            RecoveryActivity::Idle,
            InspectionState::Pending,
        );
        assert_eq!(pending.state.scope, ServerPlaylistScope::Hidden);

        let failed = RecoveryPresentation::for_mirror(
            false,
            false,
            RecoveryActivity::Idle,
            InspectionState::Failed,
        );
        assert_eq!(failed.state.activity, ServerPlaylistActivity::Failed);

        let closed = RecoveryPresentation::for_mirror(
            false,
            false,
            RecoveryActivity::Idle,
            InspectionState::Closed,
        );
        assert_eq!(closed.state.scope, ServerPlaylistScope::Hidden);

        for plan in [pending, failed, closed] {
            assert!(RecoveryAction::ALL
                .into_iter()
                .all(|action| !plan.enables(action)));
        }
    }

    #[test]
    fn running_and_revoked_states_fail_closed() {
        let running = RecoveryPresentation::for_mirror(
            false,
            false,
            RecoveryActivity::Running,
            InspectionState::Linked { available: true },
        );
        assert_eq!(running.state.activity, ServerPlaylistActivity::Running);
        assert!(RecoveryAction::ALL
            .into_iter()
            .all(|action| !running.enables(action)));

        let revoked = RecoveryPresentation::for_mirror(
            false,
            false,
            RecoveryActivity::Idle,
            InspectionState::NotLinked,
        );
        assert_eq!(revoked.state.scope, ServerPlaylistScope::Hidden);
        assert!(RecoveryAction::ALL
            .into_iter()
            .all(|action| !revoked.enables(action)));
    }

    #[test]
    fn action_names_are_targetless_window_actions() {
        assert_eq!(RecoveryAction::SyncNow.name(), "server-playlist-sync-now");
        assert_eq!(RecoveryAction::Retry.name(), "server-playlist-retry");
        assert_eq!(
            RecoveryAction::ReplaceLocalWithServer.name(),
            "server-playlist-replace-local"
        );
        assert_eq!(RecoveryAction::Unlink.name(), "server-playlist-unlink");
        assert_eq!(
            RecoveryAction::RemoveLocalCopy.name(),
            "server-playlist-remove-local-copy"
        );
    }

    #[test]
    fn confirmation_snapshot_rejects_selection_aba_and_generation_changes() {
        assert!(activation_snapshot_is_current(
            7,
            7,
            "local-playlist-a",
            Some("local-playlist-a")
        ));
        assert!(!activation_snapshot_is_current(
            7,
            9,
            "local-playlist-a",
            Some("local-playlist-a")
        ));
        assert!(!activation_snapshot_is_current(
            7,
            7,
            "local-playlist-a",
            Some("local-playlist-b")
        ));
        assert!(!activation_snapshot_is_current(
            7,
            7,
            "local-playlist-a",
            None
        ));
    }

    #[test]
    fn stale_operation_completion_cannot_mutate_a_newer_generation() {
        let playlist_id = Arc::<str>::from("local-playlist-a");
        let mut states = HashMap::from([(
            Arc::clone(&playlist_id),
            OperationState {
                generation: 11,
                activity: RecoveryActivity::Running,
            },
        )]);

        assert!(!settle_operation(
            &mut states,
            &playlist_id,
            10,
            ServerPlaylistOperationOutcome::Applied,
        ));
        assert_eq!(
            states.get(&playlist_id).map(|state| state.activity),
            Some(RecoveryActivity::Running)
        );

        assert!(settle_operation(
            &mut states,
            &playlist_id,
            11,
            ServerPlaylistOperationOutcome::Interrupted,
        ));
        assert_eq!(
            states.get(&playlist_id).map(|state| state.activity),
            Some(RecoveryActivity::Failed)
        );

        states.insert(
            Arc::clone(&playlist_id),
            OperationState {
                generation: 12,
                activity: RecoveryActivity::Running,
            },
        );
        assert!(settle_operation(
            &mut states,
            &playlist_id,
            12,
            ServerPlaylistOperationOutcome::Superseded,
        ));
        assert!(!states.contains_key(&playlist_id));
    }

    #[test]
    fn generation_exhaustion_is_detected_instead_of_wrapping() {
        assert_eq!(next_generation(0), Some(1));
        assert_eq!(next_generation(u64::MAX - 1), Some(u64::MAX));
        assert_eq!(next_generation(u64::MAX), None);
    }
}
