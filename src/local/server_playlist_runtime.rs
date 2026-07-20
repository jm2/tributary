//! Headless runtime for pull-synchronized server playlists.
//!
//! The runtime joins three independent authorities only at their narrowest
//! boundaries: durable playlist revision tickets, exact source-session read
//! receipts, and the coordinator's latest-request admission guard. It owns no
//! GTK objects and never exposes server-native identity to UI code.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt};
use sea_orm::DatabaseConnection;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::architecture::SourceId;
use crate::server_playlist_coordinator::{
    ServerPlaylistAdmissionGuard, ServerPlaylistCoordinatorHandle, ServerPlaylistOperationContext,
    ServerPlaylistOperationKey, ServerPlaylistRequestStamp, ServerPlaylistRequestStatus,
};
use crate::source_registry::{
    ServerPlaylistAbsenceEvidence, ServerPlaylistCommitAuthority, ServerPlaylistListing,
    ServerPlaylistPull, ServerPlaylistSelection, SourceRegistry,
};

use super::playlist_manager::PlaylistManager;
use super::playlist_manager::{
    ServerPlaylistMissingOutcome, ServerPlaylistPullOutcome, ServerPlaylistPullPolicy,
    ServerPlaylistRemoveOutcome, ServerPlaylistSyncPreparation, ServerPlaylistUnlinkOutcome,
};
use super::playlist_sidebar::{PlaylistSidebarRefresh, PlaylistSidebarRefreshRequest};

/// Maximum local operations in flight for one reconnect sweep.
const MAX_RECONNECT_FANOUT: usize = 8;

/// Content-free completion of one manually submitted local operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerPlaylistOperationOutcome {
    Applied,
    Conflict,
    Missing,
    Unlinked,
    Removed,
    Superseded,
    Rejected,
    Unavailable,
    Failed,
    Closed,
    Interrupted,
}

/// Immediate queue status plus a private, redacted completion receiver.
#[must_use = "manual server-playlist submissions should be observed"]
pub struct ServerPlaylistSubmission {
    status: ServerPlaylistRequestStatus,
    completion: oneshot::Receiver<ServerPlaylistOperationOutcome>,
}

impl ServerPlaylistSubmission {
    pub const fn status(&self) -> ServerPlaylistRequestStatus {
        self.status
    }

    pub async fn completion(self) -> ServerPlaylistOperationOutcome {
        if self.status == ServerPlaylistRequestStatus::Closed {
            return ServerPlaylistOperationOutcome::Closed;
        }
        self.completion
            .await
            .unwrap_or(ServerPlaylistOperationOutcome::Interrupted)
    }
}

impl fmt::Debug for ServerPlaylistSubmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistSubmission")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

struct ManualCompletion {
    sender: Option<oneshot::Sender<ServerPlaylistOperationOutcome>>,
    fallback: ServerPlaylistOperationOutcome,
}

impl ManualCompletion {
    fn new(sender: oneshot::Sender<ServerPlaylistOperationOutcome>) -> Self {
        Self {
            sender: Some(sender),
            fallback: ServerPlaylistOperationOutcome::Superseded,
        }
    }

    fn mark_started(&mut self) {
        self.fallback = ServerPlaylistOperationOutcome::Interrupted;
    }

    fn complete(mut self, outcome: ServerPlaylistOperationOutcome) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(outcome);
        }
    }
}

impl Drop for ManualCompletion {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(self.fallback);
        }
    }
}

/// Cloneable submission facade shared by reconnect recovery and future GTK
/// action-token wiring.
///
/// Local IDs are accepted only as operation inputs and never appear in Debug
/// output. Native server IDs remain behind persistence and registry types.
#[derive(Clone)]
pub struct ServerPlaylistOperations {
    database: DatabaseConnection,
    coordinator: ServerPlaylistCoordinatorHandle,
    source_registry: SourceRegistry,
    playlist_sidebar_refresh: PlaylistSidebarRefresh,
}

impl ServerPlaylistOperations {
    pub fn new(
        database: DatabaseConnection,
        coordinator: ServerPlaylistCoordinatorHandle,
        source_registry: SourceRegistry,
        playlist_sidebar_refresh: PlaylistSidebarRefresh,
    ) -> Self {
        Self {
            database,
            coordinator,
            source_registry,
            playlist_sidebar_refresh,
        }
    }

    /// Request a drift-safe pull of one linked local playlist.
    pub fn sync_now(&self, playlist_id: impl Into<Arc<str>>) -> ServerPlaylistSubmission {
        self.request_pull(playlist_id.into(), ServerPlaylistPullPolicy::RefuseDrift)
    }

    /// Retry uses the same drift-safe policy and latest-request lane as Sync
    /// Now. Presentation state, rather than a separate mutation contract,
    /// distinguishes the two user intents.
    pub fn retry(&self, playlist_id: impl Into<Arc<str>>) -> ServerPlaylistSubmission {
        self.request_pull(playlist_id.into(), ServerPlaylistPullPolicy::RefuseDrift)
    }

    /// Explicitly discard local drift in favor of the exact current server
    /// snapshot.
    pub fn replace_local_with_server(
        &self,
        playlist_id: impl Into<Arc<str>>,
    ) -> ServerPlaylistSubmission {
        self.request_pull(playlist_id.into(), ServerPlaylistPullPolicy::ReplaceLocal)
    }

    /// Detach one mirror without contacting its source.
    pub fn unlink(&self, playlist_id: impl Into<Arc<str>>) -> ServerPlaylistSubmission {
        let playlist_id = playlist_id.into();
        let operations = self.clone();
        self.submit_local(Arc::clone(&playlist_id), move |context| async move {
            operations.run_unlink(playlist_id, context).await
        })
    }

    /// Delete one linked local copy without contacting its source.
    pub fn remove_local_copy(&self, playlist_id: impl Into<Arc<str>>) -> ServerPlaylistSubmission {
        let playlist_id = playlist_id.into();
        let operations = self.clone();
        self.submit_local(Arc::clone(&playlist_id), move |context| async move {
            operations.run_remove(playlist_id, context).await
        })
    }

    fn request_pull(
        &self,
        playlist_id: Arc<str>,
        policy: ServerPlaylistPullPolicy,
    ) -> ServerPlaylistSubmission {
        let operations = self.clone();
        self.submit_local(Arc::clone(&playlist_id), move |context| async move {
            operations
                .run_manual_pull(playlist_id, policy, context)
                .await
        })
    }

    fn submit_local<Operation, OperationFuture>(
        &self,
        playlist_id: Arc<str>,
        operation: Operation,
    ) -> ServerPlaylistSubmission
    where
        Operation: FnOnce(ServerPlaylistOperationContext) -> OperationFuture + Send + 'static,
        OperationFuture: Future<Output = ServerPlaylistOperationOutcome> + Send + 'static,
    {
        let key = ServerPlaylistOperationKey::local_playlist(playlist_id);
        let (completion, receiver) = oneshot::channel();
        let mut completion = ManualCompletion::new(completion);
        let status = self.coordinator.request(key, move |context| async move {
            completion.mark_started();
            let outcome = operation(context).await;
            completion.complete(outcome);
        });
        ServerPlaylistSubmission {
            status,
            completion: receiver,
        }
    }

    fn schedule_reconnect(
        &self,
        source_id: SourceId,
        session_epoch: u64,
    ) -> ServerPlaylistRequestStatus {
        let Ok(stamp) = self.coordinator.reserve_request_stamp() else {
            return ServerPlaylistRequestStatus::Closed;
        };
        let fanout_stamp = stamp.clone();
        let operations = self.clone();
        self.coordinator.begin_if_not_newer(
            ServerPlaylistOperationKey::source(source_id),
            &stamp,
            move |context| async move {
                operations
                    .run_reconnect_sweep(source_id, session_epoch, fanout_stamp, context)
                    .await;
            },
        )
    }

    async fn run_manual_pull(
        &self,
        playlist_id: Arc<str>,
        policy: ServerPlaylistPullPolicy,
        context: ServerPlaylistOperationContext,
    ) -> ServerPlaylistOperationOutcome {
        if context.is_cancelled() {
            return ServerPlaylistOperationOutcome::Superseded;
        }
        let manager = PlaylistManager::new(self.database.clone());
        let preparation = match before_cancel(
            &context,
            manager.prepare_server_playlist_sync(playlist_id.as_ref()),
        )
        .await
        {
            None => return ServerPlaylistOperationOutcome::Superseded,
            Some(Err(_)) => return ServerPlaylistOperationOutcome::Failed,
            Some(Ok(None)) => return ServerPlaylistOperationOutcome::Superseded,
            Some(Ok(Some(preparation))) => preparation,
        };
        let source_id = preparation.ticket().source_id();
        let Some(session_epoch) = self
            .source_registry
            .snapshot(source_id)
            .and_then(|snapshot| snapshot.session_epoch)
        else {
            return ServerPlaylistOperationOutcome::Unavailable;
        };
        let listing = match before_cancel(
            &context,
            self.source_registry
                .list_server_playlists_for_session(source_id, session_epoch),
        )
        .await
        {
            None => return ServerPlaylistOperationOutcome::Superseded,
            Some(Err(_)) => return ServerPlaylistOperationOutcome::Unavailable,
            Some(Ok(listing)) => listing,
        };
        self.apply_listing(manager, preparation, listing, policy, context)
            .await
    }

    async fn run_unlink(
        &self,
        playlist_id: Arc<str>,
        context: ServerPlaylistOperationContext,
    ) -> ServerPlaylistOperationOutcome {
        if context.is_cancelled() {
            return ServerPlaylistOperationOutcome::Superseded;
        }
        let manager = PlaylistManager::new(self.database.clone());
        let preparation = match before_cancel(
            &context,
            manager.prepare_server_playlist_sync(playlist_id.as_ref()),
        )
        .await
        {
            None => return ServerPlaylistOperationOutcome::Superseded,
            Some(Err(_)) => return ServerPlaylistOperationOutcome::Failed,
            Some(Ok(None)) => return ServerPlaylistOperationOutcome::Superseded,
            Some(Ok(Some(preparation))) => preparation,
        };
        let outcome = manager
            .unlink_server_playlist_if_admitted(preparation.ticket().clone(), move || async move {
                context.admit().await.ok()
            })
            .await;
        if matches!(&outcome, Ok(ServerPlaylistUnlinkOutcome::Unlinked(_))) {
            self.request_sidebar_refresh();
        }
        match outcome {
            Ok(ServerPlaylistUnlinkOutcome::Unlinked(_)) => {
                ServerPlaylistOperationOutcome::Unlinked
            }
            Ok(ServerPlaylistUnlinkOutcome::Superseded) => {
                ServerPlaylistOperationOutcome::Superseded
            }
            Ok(ServerPlaylistUnlinkOutcome::Rejected) => ServerPlaylistOperationOutcome::Rejected,
            Err(_) => ServerPlaylistOperationOutcome::Failed,
        }
    }

    async fn run_remove(
        &self,
        playlist_id: Arc<str>,
        context: ServerPlaylistOperationContext,
    ) -> ServerPlaylistOperationOutcome {
        if context.is_cancelled() {
            return ServerPlaylistOperationOutcome::Superseded;
        }
        let manager = PlaylistManager::new(self.database.clone());
        let preparation = match before_cancel(
            &context,
            manager.prepare_server_playlist_sync(playlist_id.as_ref()),
        )
        .await
        {
            None => return ServerPlaylistOperationOutcome::Superseded,
            Some(Err(_)) => return ServerPlaylistOperationOutcome::Failed,
            Some(Ok(None)) => return ServerPlaylistOperationOutcome::Superseded,
            Some(Ok(Some(preparation))) => preparation,
        };
        let outcome = manager
            .remove_local_server_playlist_if_admitted(
                preparation.ticket().clone(),
                move || async move { context.admit().await.ok() },
            )
            .await;
        if matches!(&outcome, Ok(ServerPlaylistRemoveOutcome::Removed)) {
            self.request_sidebar_refresh();
        }
        match outcome {
            Ok(ServerPlaylistRemoveOutcome::Removed) => ServerPlaylistOperationOutcome::Removed,
            Ok(ServerPlaylistRemoveOutcome::Superseded) => {
                ServerPlaylistOperationOutcome::Superseded
            }
            Ok(ServerPlaylistRemoveOutcome::Rejected) => ServerPlaylistOperationOutcome::Rejected,
            Err(_) => ServerPlaylistOperationOutcome::Failed,
        }
    }

    async fn run_reconnect_sweep(
        &self,
        source_id: SourceId,
        session_epoch: u64,
        stamp: ServerPlaylistRequestStamp,
        context: ServerPlaylistOperationContext,
    ) {
        if context.is_cancelled() {
            return;
        }
        let database = self.database.clone();
        let manager = PlaylistManager::new(database.clone());
        let Some(Ok(links)) =
            before_cancel(&context, manager.list_server_playlist_links(source_id)).await
        else {
            return;
        };

        // Capture every exact durable revision before the single network
        // listing. A link changed after this point can only supersede its
        // ticket; completion never reloads a newer revision opportunistically.
        let mut preparations = Vec::with_capacity(links.len());
        for link in links {
            let Some(Ok(preparation)) = before_cancel(
                &context,
                manager.prepare_server_playlist_sync(&link.playlist_id),
            )
            .await
            else {
                return;
            };
            if let Some(preparation) = preparation {
                if preparation.ticket().source_id() == source_id {
                    preparations.push(preparation);
                }
            }
        }

        let Some(Ok(listing)) = before_cancel(
            &context,
            self.source_registry
                .list_server_playlists_for_session(source_id, session_epoch),
        )
        .await
        else {
            return;
        };

        let mut in_flight = FuturesUnordered::new();
        for preparation in preparations {
            if context.is_cancelled() {
                return;
            }
            while in_flight.len() >= MAX_RECONNECT_FANOUT {
                if !wait_for_fanout(&context, &mut in_flight).await {
                    return;
                }
            }
            let key = ServerPlaylistOperationKey::local_playlist(
                preparation.ticket().playlist_id().to_string(),
            );
            let (completion, completed) = oneshot::channel();
            if let Some(selection) = listing.select(preparation.ticket().native_playlist_id()) {
                let operations = self.clone();
                let database = database.clone();
                self.coordinator
                    .begin_if_not_newer(key, &stamp, move |local_context| async move {
                        operations
                            .apply_reconnect_selection(
                                database,
                                preparation,
                                selection,
                                local_context,
                            )
                            .await;
                        let _ = completion.send(());
                    });
            } else if let Some(evidence) =
                listing.prove_absent(preparation.ticket().native_playlist_id())
            {
                let operations = self.clone();
                let database = database.clone();
                self.coordinator
                    .begin_if_not_newer(key, &stamp, move |local_context| async move {
                        operations
                            .apply_absence(
                                PlaylistManager::new(database),
                                preparation,
                                evidence,
                                local_context,
                            )
                            .await;
                        let _ = completion.send(());
                    });
            }
            in_flight.push(completed);
        }
        while !in_flight.is_empty() {
            if !wait_for_fanout(&context, &mut in_flight).await {
                return;
            }
        }
    }

    async fn apply_reconnect_selection(
        &self,
        database: DatabaseConnection,
        preparation: ServerPlaylistSyncPreparation,
        selection: ServerPlaylistSelection,
        context: ServerPlaylistOperationContext,
    ) {
        let Some(Ok(pull)) = before_cancel(
            &context,
            self.source_registry.get_server_playlist(selection),
        )
        .await
        else {
            return;
        };
        let _ = self
            .apply_pull(
                PlaylistManager::new(database),
                preparation,
                pull,
                ServerPlaylistPullPolicy::RefuseDrift,
                context,
            )
            .await;
    }

    async fn apply_listing(
        &self,
        manager: PlaylistManager,
        preparation: ServerPlaylistSyncPreparation,
        listing: ServerPlaylistListing,
        policy: ServerPlaylistPullPolicy,
        context: ServerPlaylistOperationContext,
    ) -> ServerPlaylistOperationOutcome {
        if let Some(selection) = listing.select(preparation.ticket().native_playlist_id()) {
            let pull = match before_cancel(
                &context,
                self.source_registry.get_server_playlist(selection),
            )
            .await
            {
                None => return ServerPlaylistOperationOutcome::Superseded,
                Some(Err(_)) => return ServerPlaylistOperationOutcome::Unavailable,
                Some(Ok(pull)) => pull,
            };
            self.apply_pull(manager, preparation, pull, policy, context)
                .await
        } else if let Some(evidence) =
            listing.prove_absent(preparation.ticket().native_playlist_id())
        {
            self.apply_absence(manager, preparation, evidence, context)
                .await
        } else {
            ServerPlaylistOperationOutcome::Failed
        }
    }

    async fn apply_pull(
        &self,
        manager: PlaylistManager,
        preparation: ServerPlaylistSyncPreparation,
        pull: ServerPlaylistPull,
        policy: ServerPlaylistPullPolicy,
        context: ServerPlaylistOperationContext,
    ) -> ServerPlaylistOperationOutcome {
        let registry = &self.source_registry;
        let pull_ref = &pull;
        let outcome = manager
            .apply_server_playlist_pull_if_admitted(
                preparation.ticket().clone(),
                pull_ref,
                policy,
                move || admit_pull(context, registry, pull_ref),
            )
            .await;
        if matches!(
            &outcome,
            Ok(ServerPlaylistPullOutcome::Applied { .. } | ServerPlaylistPullOutcome::Conflict(_))
        ) {
            self.request_sidebar_refresh();
        }
        match outcome {
            Ok(ServerPlaylistPullOutcome::Applied { .. }) => {
                ServerPlaylistOperationOutcome::Applied
            }
            Ok(ServerPlaylistPullOutcome::Conflict(_)) => ServerPlaylistOperationOutcome::Conflict,
            Ok(ServerPlaylistPullOutcome::Superseded) => ServerPlaylistOperationOutcome::Superseded,
            Ok(ServerPlaylistPullOutcome::Rejected) => ServerPlaylistOperationOutcome::Rejected,
            Err(_) => ServerPlaylistOperationOutcome::Failed,
        }
    }

    async fn apply_absence(
        &self,
        manager: PlaylistManager,
        preparation: ServerPlaylistSyncPreparation,
        evidence: ServerPlaylistAbsenceEvidence,
        context: ServerPlaylistOperationContext,
    ) -> ServerPlaylistOperationOutcome {
        let registry = &self.source_registry;
        let evidence_ref = &evidence;
        let outcome = manager
            .mark_server_playlist_missing_if_admitted(
                preparation.ticket().clone(),
                evidence_ref,
                move || admit_absence(context, registry, evidence_ref),
            )
            .await;
        if matches!(&outcome, Ok(ServerPlaylistMissingOutcome::Marked(_))) {
            self.request_sidebar_refresh();
        }
        match outcome {
            Ok(ServerPlaylistMissingOutcome::Marked(_)) => ServerPlaylistOperationOutcome::Missing,
            Ok(ServerPlaylistMissingOutcome::Superseded) => {
                ServerPlaylistOperationOutcome::Superseded
            }
            Ok(ServerPlaylistMissingOutcome::Rejected) => ServerPlaylistOperationOutcome::Rejected,
            Err(_) => ServerPlaylistOperationOutcome::Failed,
        }
    }

    fn request_sidebar_refresh(&self) {
        match self.playlist_sidebar_refresh.request() {
            PlaylistSidebarRefreshRequest::Requested
            | PlaylistSidebarRefreshRequest::Coalesced
            | PlaylistSidebarRefreshRequest::Closed => (),
        }
    }
}

impl fmt::Debug for ServerPlaylistOperations {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistOperations")
            .field("closed", &self.coordinator.is_closed())
            .finish_non_exhaustive()
    }
}

async fn before_cancel<T>(
    context: &ServerPlaylistOperationContext,
    future: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        () = context.cancelled() => None,
        output = future => Some(output),
    }
}

async fn wait_for_fanout(
    context: &ServerPlaylistOperationContext,
    in_flight: &mut FuturesUnordered<oneshot::Receiver<()>>,
) -> bool {
    tokio::select! {
        biased;
        () = context.cancelled() => false,
        _ = in_flight.next() => true,
    }
}

async fn admit_pull(
    context: ServerPlaylistOperationContext,
    registry: &SourceRegistry,
    pull: &ServerPlaylistPull,
) -> Option<(ServerPlaylistCommitAuthority, ServerPlaylistAdmissionGuard)> {
    let guard = context.admit().await.ok()?;
    let authority = registry.acquire_server_playlist_pull_commit_authority(pull)?;
    Some((authority, guard))
}

async fn admit_absence(
    context: ServerPlaylistOperationContext,
    registry: &SourceRegistry,
    evidence: &ServerPlaylistAbsenceEvidence,
) -> Option<(ServerPlaylistCommitAuthority, ServerPlaylistAdmissionGuard)> {
    let guard = context.admit().await.ok()?;
    let authority = registry.acquire_server_playlist_absence_commit_authority(evidence)?;
    Some((authority, guard))
}

#[derive(Default)]
struct ObservedSessionEpochs {
    by_source: HashMap<SourceId, u64>,
}

impl ObservedSessionEpochs {
    fn take_new(
        &mut self,
        sessions: impl IntoIterator<Item = (SourceId, Option<u64>)>,
    ) -> Vec<(SourceId, u64)> {
        let mut newly_observed = Vec::new();
        for (source_id, session_epoch) in sessions {
            let Some(session_epoch) = session_epoch else {
                continue;
            };
            if self.by_source.get(&source_id).copied() == Some(session_epoch) {
                continue;
            }
            self.by_source.insert(source_id, session_epoch);
            newly_observed.push((source_id, session_epoch));
        }
        newly_observed
    }
}

/// Observe accepted source sessions and schedule at most one reconnect sweep
/// for each exact `(SourceId, session_epoch)`.
///
/// The receiver is subscribed before engine construction. The
/// subscribe/baseline/watch ordering below ensures an invalidation racing the
/// atomic baseline is either represented by that baseline or remains newer
/// than its revision and forces an immediate resnapshot.
pub async fn run_server_playlist_reconnect_observer(
    operations: ServerPlaylistOperations,
    mut invalidations: watch::Receiver<u64>,
    shutdown: CancellationToken,
) {
    let mut observed_sessions = ObservedSessionEpochs::default();
    let baseline = operations.source_registry.snapshot_all();
    let mut revision = baseline.revision;
    let mut shutting_down = baseline.shutting_down;
    if shutting_down || shutdown.is_cancelled() {
        return;
    }
    for (source_id, session_epoch) in observed_sessions.take_new(
        baseline
            .sources
            .into_iter()
            .map(|(source_id, snapshot)| (source_id, snapshot.session_epoch)),
    ) {
        if shutdown.is_cancelled() {
            return;
        }
        operations.schedule_reconnect(source_id, session_epoch);
    }

    while !shutting_down && !shutdown.is_cancelled() {
        let watched_revision = *invalidations.borrow_and_update();
        if watched_revision <= revision {
            tokio::select! {
                () = shutdown.cancelled() => return,
                changed = invalidations.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }

        let baseline = operations.source_registry.snapshot_all();
        revision = baseline.revision;
        shutting_down = baseline.shutting_down;
        if shutting_down || shutdown.is_cancelled() {
            return;
        }
        for (source_id, session_epoch) in observed_sessions.take_new(
            baseline
                .sources
                .into_iter()
                .map(|(source_id, snapshot)| (source_id, snapshot.session_epoch)),
        ) {
            if shutdown.is_cancelled() {
                return;
            }
            operations.schedule_reconnect(source_id, session_epoch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn manual_completion_distinguishes_supersession_from_started_interruption() {
        let (superseded_tx, superseded_rx) = oneshot::channel();
        drop(ManualCompletion::new(superseded_tx));
        assert_eq!(
            superseded_rx.await.expect("superseded completion"),
            ServerPlaylistOperationOutcome::Superseded
        );

        let (interrupted_tx, interrupted_rx) = oneshot::channel();
        let mut interrupted = ManualCompletion::new(interrupted_tx);
        interrupted.mark_started();
        drop(interrupted);
        assert_eq!(
            interrupted_rx.await.expect("interrupted completion"),
            ServerPlaylistOperationOutcome::Interrupted
        );

        let (applied_tx, applied_rx) = oneshot::channel();
        ManualCompletion::new(applied_tx).complete(ServerPlaylistOperationOutcome::Applied);
        assert_eq!(
            applied_rx.await.expect("explicit completion"),
            ServerPlaylistOperationOutcome::Applied
        );
    }

    #[tokio::test]
    async fn displaced_pending_manual_operation_reports_superseded_without_starting() {
        let (coordinator, shutdown) =
            crate::server_playlist_coordinator::spawn_server_playlist_coordinator();
        let key = ServerPlaylistOperationKey::local_playlist("pending-completion-local-id");

        let (first_admitted_tx, first_admitted_rx) = oneshot::channel();
        let (release_first_tx, release_first_rx) = oneshot::channel();
        coordinator.request(key.clone(), move |context| async move {
            let guard = context.admit().await.expect("first operation admitted");
            first_admitted_tx.send(()).expect("report first admission");
            release_first_rx.await.expect("release first operation");
            drop(guard);
        });
        first_admitted_rx.await.expect("first operation admitted");

        let (pending_tx, pending_rx) = oneshot::channel();
        let mut pending = ManualCompletion::new(pending_tx);
        coordinator.request(key.clone(), move |_| async move {
            pending.mark_started();
            pending.complete(ServerPlaylistOperationOutcome::Applied);
        });

        let (latest_started_tx, latest_started_rx) = oneshot::channel();
        coordinator.request(key, move |_| async move {
            latest_started_tx
                .send(())
                .expect("report latest operation start");
        });
        assert_eq!(
            pending_rx.await.expect("displaced pending completion"),
            ServerPlaylistOperationOutcome::Superseded
        );

        release_first_tx.send(()).expect("release first operation");
        latest_started_rx
            .await
            .expect("latest pending operation ran");
        shutdown.shutdown().await.expect("orderly shutdown");
    }

    #[test]
    fn reconnect_epochs_schedule_once_and_ignore_same_session_invalidations() {
        let first = SourceId::random();
        let second = SourceId::random();
        let mut observed = ObservedSessionEpochs::default();

        assert_eq!(
            observed.take_new([(first, Some(1)), (second, None)]),
            vec![(first, 1)]
        );
        assert!(observed
            .take_new([(first, Some(1)), (second, None)])
            .is_empty());
        assert_eq!(
            observed.take_new([(first, Some(2)), (second, Some(7))]),
            vec![(first, 2), (second, 7)]
        );
        assert!(observed
            .take_new([(first, None), (second, Some(7))])
            .is_empty());
    }

    #[test]
    fn reconnect_epoch_tracking_is_source_scoped() {
        let first = SourceId::random();
        let second = SourceId::random();
        let mut observed = ObservedSessionEpochs::default();

        assert_eq!(
            observed.take_new([(first, Some(9)), (second, Some(9))]),
            vec![(first, 9), (second, 9)]
        );
        assert_eq!(
            observed.take_new([(second, Some(10)), (first, Some(9))]),
            vec![(second, 10)]
        );
    }
}
