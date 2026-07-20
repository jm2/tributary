//! Headless, tokenized browser for pull-only server playlists.
//!
//! Presentation receives bounded hints and opaque, revocable action tokens.
//! Exact native identities, selections, source-session receipts, database
//! authority, and coordinator keys remain owned by the Tokio-side broker.

use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use sea_orm::DatabaseConnection;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::architecture::server_playlist::MAX_SERVER_PLAYLIST_HINT_BYTES;
use crate::architecture::{NativePlaylistId, SourceId, MAX_SERVER_PLAYLISTS_PER_LIST};
use crate::server_playlist_coordinator::{
    ServerPlaylistAdmissionError, ServerPlaylistAdmissionGuard, ServerPlaylistCoordinatorHandle,
    ServerPlaylistOperationContext, ServerPlaylistOperationKey,
};
use crate::source_registry::{
    ServerPlaylistCapability, ServerPlaylistCommitAuthority, ServerPlaylistError,
    ServerPlaylistListing, ServerPlaylistPull, ServerPlaylistSelection, SourceRegistry,
};

use super::playlist_manager::{
    PlaylistManager, ServerPlaylistCreateOutcome, ServerPlaylistImportOutcome,
};
use super::playlist_sidebar::{PlaylistSidebarRefresh, PlaylistSidebarRefreshRequest};
use super::server_playlist_runtime::{ServerPlaylistLinkInspection, ServerPlaylistOperations};

/// Maximum commands waiting for the single browser owner.
const SERVER_PLAYLIST_BROWSER_COMMAND_CAPACITY: usize = 32;
/// Maximum actions accepted by the browser but not yet settled.
pub const MAX_SERVER_PLAYLIST_BROWSER_ACTIONS: usize = 8;

struct BrowserSessionSeal(u8);

/// Revocable identity for one published browser snapshot.
///
/// This token has no serializable or textual representation and contains no
/// source, session, playlist, or native identity.
#[derive(Clone)]
pub struct ServerPlaylistBrowserSessionToken(Arc<BrowserSessionSeal>);

impl ServerPlaylistBrowserSessionToken {
    fn fresh() -> Self {
        Self(Arc::new(BrowserSessionSeal(0)))
    }
}

impl PartialEq for ServerPlaylistBrowserSessionToken {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ServerPlaylistBrowserSessionToken {}

impl Hash for ServerPlaylistBrowserSessionToken {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

impl fmt::Debug for ServerPlaylistBrowserSessionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = self.0 .0;
        formatter
            .debug_struct("ServerPlaylistBrowserSessionToken")
            .finish_non_exhaustive()
    }
}

struct BrowserActionSeal(u8);

/// One-shot authority for a single entry in the active browser snapshot.
///
/// Capacity rejection does not consume this token. Once an action is
/// admitted by the browser owner, the token is atomically removed before any
/// network or database work is scheduled.
#[derive(Clone)]
pub struct ServerPlaylistBrowserActionToken(Arc<BrowserActionSeal>);

impl ServerPlaylistBrowserActionToken {
    fn fresh() -> Self {
        Self(Arc::new(BrowserActionSeal(0)))
    }
}

impl PartialEq for ServerPlaylistBrowserActionToken {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ServerPlaylistBrowserActionToken {}

impl Hash for ServerPlaylistBrowserActionToken {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

impl fmt::Debug for ServerPlaylistBrowserActionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = self.0 .0;
        formatter
            .debug_struct("ServerPlaylistBrowserActionToken")
            .finish_non_exhaustive()
    }
}

/// Presentation-safe metadata for one server-owned playlist.
#[derive(Clone)]
pub struct ServerPlaylistBrowserEntry {
    action_token: ServerPlaylistBrowserActionToken,
    name: Option<String>,
    owner: Option<String>,
    advertised_track_count: Option<u64>,
}

impl ServerPlaylistBrowserEntry {
    pub fn action_token(&self) -> ServerPlaylistBrowserActionToken {
        self.action_token.clone()
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    pub const fn advertised_track_count(&self) -> Option<u64> {
        self.advertised_track_count
    }
}

impl fmt::Debug for ServerPlaylistBrowserEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistBrowserEntry")
            .field("name_byte_len", &self.name.as_ref().map(String::len))
            .field("owner_byte_len", &self.owner.as_ref().map(String::len))
            .field("advertised_track_count", &self.advertised_track_count)
            .finish_non_exhaustive()
    }
}

/// One complete browser publication. A newer browse or explicit close
/// revokes its session and every unused action token.
#[derive(Clone)]
pub struct ServerPlaylistBrowserSnapshot {
    session_token: ServerPlaylistBrowserSessionToken,
    entries: Vec<ServerPlaylistBrowserEntry>,
}

impl ServerPlaylistBrowserSnapshot {
    pub fn session_token(&self) -> ServerPlaylistBrowserSessionToken {
        self.session_token.clone()
    }

    pub fn entries(&self) -> &[ServerPlaylistBrowserEntry] {
        &self.entries
    }
}

impl fmt::Debug for ServerPlaylistBrowserSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistBrowserSnapshot")
            .field("entry_count", &self.entries.len())
            .finish_non_exhaustive()
    }
}

/// Immediate result of submitting work to the bounded browser lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerPlaylistBrowserRequestStatus {
    Queued,
    Busy,
    Closed,
}

/// Fixed, content-free result of a server-playlist listing.
pub enum ServerPlaylistBrowseOutcome {
    Ready(ServerPlaylistBrowserSnapshot),
    Unsupported,
    Unavailable,
    Failed,
    Superseded,
    Busy,
    Closed,
    Interrupted,
}

impl fmt::Debug for ServerPlaylistBrowseOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready(snapshot) => formatter.debug_tuple("Ready").field(snapshot).finish(),
            Self::Unsupported => formatter.write_str("Unsupported"),
            Self::Unavailable => formatter.write_str("Unavailable"),
            Self::Failed => formatter.write_str("Failed"),
            Self::Superseded => formatter.write_str("Superseded"),
            Self::Busy => formatter.write_str("Busy"),
            Self::Closed => formatter.write_str("Closed"),
            Self::Interrupted => formatter.write_str("Interrupted"),
        }
    }
}

/// Fixed, content-free result of Import Copy or Keep Synced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerPlaylistBrowserActionOutcome {
    Imported,
    Linked,
    AlreadyLinked,
    Rejected,
    Unsupported,
    Unavailable,
    Failed,
    Superseded,
    Busy,
    Closed,
    Interrupted,
}

/// Immediate browse status plus its eventual redacted result.
#[must_use = "server-playlist browse submissions should be observed"]
pub struct ServerPlaylistBrowseSubmission {
    status: ServerPlaylistBrowserRequestStatus,
    completion: oneshot::Receiver<ServerPlaylistBrowseOutcome>,
}

impl ServerPlaylistBrowseSubmission {
    pub const fn status(&self) -> ServerPlaylistBrowserRequestStatus {
        self.status
    }

    pub async fn completion(self) -> ServerPlaylistBrowseOutcome {
        match self.status {
            ServerPlaylistBrowserRequestStatus::Busy => ServerPlaylistBrowseOutcome::Busy,
            ServerPlaylistBrowserRequestStatus::Closed => ServerPlaylistBrowseOutcome::Closed,
            ServerPlaylistBrowserRequestStatus::Queued => self
                .completion
                .await
                .unwrap_or(ServerPlaylistBrowseOutcome::Interrupted),
        }
    }
}

impl fmt::Debug for ServerPlaylistBrowseSubmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistBrowseSubmission")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// Immediate action status plus its eventual redacted result.
#[must_use = "server-playlist browser actions should be observed"]
pub struct ServerPlaylistBrowserActionSubmission {
    status: ServerPlaylistBrowserRequestStatus,
    completion: oneshot::Receiver<ServerPlaylistBrowserActionOutcome>,
}

impl ServerPlaylistBrowserActionSubmission {
    pub const fn status(&self) -> ServerPlaylistBrowserRequestStatus {
        self.status
    }

    pub async fn completion(self) -> ServerPlaylistBrowserActionOutcome {
        match self.status {
            ServerPlaylistBrowserRequestStatus::Busy => ServerPlaylistBrowserActionOutcome::Busy,
            ServerPlaylistBrowserRequestStatus::Closed => {
                ServerPlaylistBrowserActionOutcome::Closed
            }
            ServerPlaylistBrowserRequestStatus::Queued => self
                .completion
                .await
                .unwrap_or(ServerPlaylistBrowserActionOutcome::Interrupted),
        }
    }
}

impl fmt::Debug for ServerPlaylistBrowserActionSubmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistBrowserActionSubmission")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
enum BrowserActionKind {
    ImportCopy,
    KeepSynced,
}

enum BrowserCommand {
    Browse {
        source_id: SourceId,
        fallback_name: Arc<str>,
        completion: oneshot::Sender<ServerPlaylistBrowseOutcome>,
    },
    Act {
        token: ServerPlaylistBrowserActionToken,
        kind: BrowserActionKind,
        completion: oneshot::Sender<ServerPlaylistBrowserActionOutcome>,
    },
    CloseSession {
        token: ServerPlaylistBrowserSessionToken,
    },
}

struct BrowserHandleInner {
    commands: async_channel::Sender<BrowserCommand>,
}

impl Drop for BrowserHandleInner {
    fn drop(&mut self) {
        self.commands.close();
    }
}

/// Cloneable, nonblocking presentation-side browser handle.
#[derive(Clone)]
pub struct ServerPlaylistBrowserHandle {
    inner: Arc<BrowserHandleInner>,
}

impl ServerPlaylistBrowserHandle {
    /// Start an independently cancellable latest-only listing. This lane does
    /// not use the reconnect coordinator's source key.
    pub fn browse(
        &self,
        source_id: SourceId,
        fallback_name: impl Into<Arc<str>>,
    ) -> ServerPlaylistBrowseSubmission {
        let (completion, receiver) = oneshot::channel();
        let command = BrowserCommand::Browse {
            source_id,
            fallback_name: fallback_name.into(),
            completion,
        };
        ServerPlaylistBrowseSubmission {
            status: self.try_submit(command),
            completion: receiver,
        }
    }

    pub fn import_copy(
        &self,
        token: ServerPlaylistBrowserActionToken,
    ) -> ServerPlaylistBrowserActionSubmission {
        self.submit_action(token, BrowserActionKind::ImportCopy)
    }

    pub fn keep_synced(
        &self,
        token: ServerPlaylistBrowserActionToken,
    ) -> ServerPlaylistBrowserActionSubmission {
        self.submit_action(token, BrowserActionKind::KeepSynced)
    }

    fn submit_action(
        &self,
        token: ServerPlaylistBrowserActionToken,
        kind: BrowserActionKind,
    ) -> ServerPlaylistBrowserActionSubmission {
        let (completion, receiver) = oneshot::channel();
        let command = BrowserCommand::Act {
            token,
            kind,
            completion,
        };
        ServerPlaylistBrowserActionSubmission {
            status: self.try_submit(command),
            completion: receiver,
        }
    }

    /// Revoke one exact published session and all of its unused action
    /// tokens. Already accepted actions continue to their ordered settlement.
    pub fn close_session(
        &self,
        token: ServerPlaylistBrowserSessionToken,
    ) -> ServerPlaylistBrowserRequestStatus {
        self.try_submit(BrowserCommand::CloseSession { token })
    }

    fn try_submit(&self, command: BrowserCommand) -> ServerPlaylistBrowserRequestStatus {
        match self.inner.commands.try_send(command) {
            Ok(()) => ServerPlaylistBrowserRequestStatus::Queued,
            Err(async_channel::TrySendError::Full(_)) => ServerPlaylistBrowserRequestStatus::Busy,
            Err(async_channel::TrySendError::Closed(_)) => {
                ServerPlaylistBrowserRequestStatus::Closed
            }
        }
    }

    /// Stop accepting new browser commands. Already queued commands are
    /// observed in owner order; accepted work reaches an orderly redacted
    /// settlement before the owner returns.
    pub fn close(&self) -> bool {
        self.inner.commands.close()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.commands.is_closed()
    }
}

impl fmt::Debug for ServerPlaylistBrowserHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistBrowserHandle")
            .field("closed", &self.is_closed())
            .finish()
    }
}

/// Single-consumer half of the browser command lane.
pub struct ServerPlaylistBrowserReceiver {
    commands: async_channel::Receiver<BrowserCommand>,
}

impl fmt::Debug for ServerPlaylistBrowserReceiver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistBrowserReceiver")
            .field("closed", &self.commands.is_closed())
            .field("pending", &self.commands.len())
            .finish_non_exhaustive()
    }
}

/// Construct the bounded presentation-to-owner browser lane.
pub fn server_playlist_browser_channel(
) -> (ServerPlaylistBrowserHandle, ServerPlaylistBrowserReceiver) {
    let (commands, receiver) = async_channel::bounded(SERVER_PLAYLIST_BROWSER_COMMAND_CAPACITY);
    (
        ServerPlaylistBrowserHandle {
            inner: Arc::new(BrowserHandleInner { commands }),
        },
        ServerPlaylistBrowserReceiver { commands: receiver },
    )
}

/// Cloneable facade for the GTK browser and recovery handoff.
///
/// The grouped value keeps browse/action tokens and durable-link recovery on
/// one redacted surface without granting presentation direct database,
/// coordinator, registry, or native-identity access.
#[derive(Clone)]
pub struct ServerPlaylistUiRuntime {
    operations: ServerPlaylistOperations,
    browser: ServerPlaylistBrowserHandle,
}

impl ServerPlaylistUiRuntime {
    pub const fn new(
        operations: ServerPlaylistOperations,
        browser: ServerPlaylistBrowserHandle,
    ) -> Self {
        Self {
            operations,
            browser,
        }
    }

    pub fn operations(&self) -> ServerPlaylistOperations {
        self.operations.clone()
    }

    pub fn browser(&self) -> ServerPlaylistBrowserHandle {
        self.browser.clone()
    }

    pub async fn inspect_link(&self, playlist_id: impl AsRef<str>) -> ServerPlaylistLinkInspection {
        self.operations.inspect_link(playlist_id).await
    }
}

impl fmt::Debug for ServerPlaylistUiRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerPlaylistUiRuntime")
            .field("operations", &self.operations)
            .field("browser", &self.browser)
            .finish()
    }
}

struct BrowserActionRecord {
    source_id: SourceId,
    native_id: NativePlaylistId,
    selection: ServerPlaylistSelection,
    fallback_name: Arc<str>,
}

struct BrowserActionServices {
    database: DatabaseConnection,
    source_registry: SourceRegistry,
    sidebar_refresh: PlaylistSidebarRefresh,
}

struct ActiveBrowserSession {
    generation: u64,
    token: ServerPlaylistBrowserSessionToken,
    actions: HashMap<ServerPlaylistBrowserActionToken, BrowserActionRecord>,
}

struct PendingBrowse {
    generation: u64,
    source_id: SourceId,
    session_epoch: u64,
    fallback_name: Arc<str>,
    completion: oneshot::Sender<ServerPlaylistBrowseOutcome>,
}

struct ActiveBrowse {
    generation: u64,
    cancellation: CancellationToken,
}

enum BrowseTaskResult {
    Listed(Result<ServerPlaylistListing, ServerPlaylistError>),
    Cancelled,
    Interrupted,
}

enum ActionTaskResult {
    Completed(ServerPlaylistBrowserActionOutcome),
    Dropped { started: bool },
}

enum InternalMessage {
    BrowseFinished {
        generation: u64,
        fallback_name: Arc<str>,
        completion: oneshot::Sender<ServerPlaylistBrowseOutcome>,
        result: BrowseTaskResult,
    },
    ActionFinished {
        action_id: u64,
        result: ActionTaskResult,
    },
}

struct BrowseTaskReporter {
    generation: u64,
    fallback_name: Option<Arc<str>>,
    completion: Option<oneshot::Sender<ServerPlaylistBrowseOutcome>>,
    internal: mpsc::UnboundedSender<InternalMessage>,
}

impl BrowseTaskReporter {
    fn complete(mut self, result: BrowseTaskResult) {
        self.send(result);
    }

    fn send(&mut self, result: BrowseTaskResult) {
        let (Some(fallback_name), Some(completion)) =
            (self.fallback_name.take(), self.completion.take())
        else {
            return;
        };
        let _ = self.internal.send(InternalMessage::BrowseFinished {
            generation: self.generation,
            fallback_name,
            completion,
            result,
        });
    }
}

impl Drop for BrowseTaskReporter {
    fn drop(&mut self) {
        self.send(BrowseTaskResult::Interrupted);
    }
}

struct ActionTaskReporter {
    action_id: u64,
    started: bool,
    completed: bool,
    internal: mpsc::UnboundedSender<InternalMessage>,
}

impl ActionTaskReporter {
    fn mark_started(&mut self) {
        self.started = true;
    }

    fn complete(mut self, outcome: ServerPlaylistBrowserActionOutcome) {
        self.completed = true;
        let _ = self.internal.send(InternalMessage::ActionFinished {
            action_id: self.action_id,
            result: ActionTaskResult::Completed(outcome),
        });
    }
}

impl Drop for ActionTaskReporter {
    fn drop(&mut self) {
        if !self.completed {
            let _ = self.internal.send(InternalMessage::ActionFinished {
                action_id: self.action_id,
                result: ActionTaskResult::Dropped {
                    started: self.started,
                },
            });
        }
    }
}

struct BrowserOwner {
    database: DatabaseConnection,
    coordinator: ServerPlaylistCoordinatorHandle,
    source_registry: SourceRegistry,
    sidebar_refresh: PlaylistSidebarRefresh,
    commands: async_channel::Receiver<BrowserCommand>,
    internal_tx: mpsc::UnboundedSender<InternalMessage>,
    internal_rx: mpsc::UnboundedReceiver<InternalMessage>,
    active_session: Option<ActiveBrowserSession>,
    browse_generation: u64,
    active_browse: Option<ActiveBrowse>,
    pending_browse: Option<PendingBrowse>,
    next_action_id: u64,
    action_completions: HashMap<u64, oneshot::Sender<ServerPlaylistBrowserActionOutcome>>,
    action_shutdown: CancellationToken,
    accepting: bool,
}

impl BrowserOwner {
    fn new(
        receiver: ServerPlaylistBrowserReceiver,
        database: DatabaseConnection,
        coordinator: ServerPlaylistCoordinatorHandle,
        source_registry: SourceRegistry,
        sidebar_refresh: PlaylistSidebarRefresh,
    ) -> Self {
        // This completion lane cannot block Drop reporters. Its producer set
        // is nevertheless structurally bounded to one listing plus the eight
        // action slots; newer listings replace one pending request without
        // spawning another task.
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        Self {
            database,
            coordinator,
            source_registry,
            sidebar_refresh,
            commands: receiver.commands,
            internal_tx,
            internal_rx,
            active_session: None,
            browse_generation: 0,
            active_browse: None,
            pending_browse: None,
            next_action_id: 0,
            action_completions: HashMap::new(),
            action_shutdown: CancellationToken::new(),
            accepting: true,
        }
    }

    async fn run(mut self, shutdown: CancellationToken) {
        loop {
            if !self.accepting && self.active_browse.is_none() && self.action_completions.is_empty()
            {
                break;
            }
            tokio::select! {
                biased;
                () = shutdown.cancelled(), if self.accepting => {
                    self.begin_shutdown(true);
                }
                command = self.commands.recv(), if self.accepting => {
                    match command {
                        Ok(command) => self.handle_command(command),
                        Err(_) => self.begin_shutdown(false),
                    }
                }
                message = self.internal_rx.recv() => {
                    if let Some(message) = message {
                        self.handle_internal(message);
                    } else {
                        self.begin_shutdown(true);
                    }
                }
            }
        }
        self.active_session = None;
    }

    fn handle_command(&mut self, command: BrowserCommand) {
        match command {
            BrowserCommand::Browse {
                source_id,
                fallback_name,
                completion,
            } => self.begin_browse(source_id, fallback_name, completion),
            BrowserCommand::Act {
                token,
                kind,
                completion,
            } => self.begin_action(token, kind, completion),
            BrowserCommand::CloseSession { token } => {
                if self
                    .active_session
                    .as_ref()
                    .is_some_and(|session| session.token == token)
                {
                    self.active_session = None;
                }
            }
        }
    }

    fn begin_browse(
        &mut self,
        source_id: SourceId,
        fallback_name: Arc<str>,
        completion: oneshot::Sender<ServerPlaylistBrowseOutcome>,
    ) {
        self.active_session = None;
        if let Some(active) = &self.active_browse {
            active.cancellation.cancel();
        }
        if let Some(pending) = self.pending_browse.take() {
            let _ = pending
                .completion
                .send(ServerPlaylistBrowseOutcome::Superseded);
        }
        let Some(generation) = self.browse_generation.checked_add(1) else {
            self.commands.close();
            self.begin_shutdown(true);
            let _ = completion.send(ServerPlaylistBrowseOutcome::Closed);
            return;
        };
        self.browse_generation = generation;

        if fallback_name.len() > MAX_SERVER_PLAYLIST_HINT_BYTES {
            let _ = completion.send(ServerPlaylistBrowseOutcome::Failed);
            return;
        }
        let Some((session_epoch, capability)) = self
            .source_registry
            .current_server_playlist_session(source_id)
        else {
            let _ = completion.send(ServerPlaylistBrowseOutcome::Unavailable);
            return;
        };
        if capability != ServerPlaylistCapability::PullSnapshots {
            let _ = completion.send(ServerPlaylistBrowseOutcome::Unsupported);
            return;
        }

        let pending = PendingBrowse {
            generation,
            source_id,
            session_epoch,
            fallback_name,
            completion,
        };
        if self.active_browse.is_some() {
            self.pending_browse = Some(pending);
        } else {
            self.start_browse(pending);
        }
    }

    fn start_browse(&mut self, pending: PendingBrowse) {
        let PendingBrowse {
            generation,
            source_id,
            session_epoch,
            fallback_name,
            completion,
        } = pending;
        let cancellation = CancellationToken::new();
        self.active_browse = Some(ActiveBrowse {
            generation,
            cancellation: cancellation.clone(),
        });
        let source_registry = self.source_registry.clone();
        let reporter = BrowseTaskReporter {
            generation,
            fallback_name: Some(fallback_name),
            completion: Some(completion),
            internal: self.internal_tx.clone(),
        };
        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                () = cancellation.cancelled() => BrowseTaskResult::Cancelled,
                result = source_registry.list_server_playlists_for_session(source_id, session_epoch) => {
                    BrowseTaskResult::Listed(result)
                }
            };
            reporter.complete(result);
        });
    }

    fn begin_action(
        &mut self,
        token: ServerPlaylistBrowserActionToken,
        kind: BrowserActionKind,
        completion: oneshot::Sender<ServerPlaylistBrowserActionOutcome>,
    ) {
        // Capacity is checked before touching the token table. A caller may
        // safely retry the same token after Busy.
        if self.action_completions.len() >= MAX_SERVER_PLAYLIST_BROWSER_ACTIONS {
            let _ = completion.send(ServerPlaylistBrowserActionOutcome::Busy);
            return;
        }
        let Some(action_id) = self.next_action_id.checked_add(1) else {
            self.commands.close();
            self.begin_shutdown(true);
            let _ = completion.send(ServerPlaylistBrowserActionOutcome::Closed);
            return;
        };
        let Some(record) = self
            .active_session
            .as_mut()
            .and_then(|session| session.actions.remove(&token))
        else {
            let _ = completion.send(ServerPlaylistBrowserActionOutcome::Rejected);
            return;
        };
        self.next_action_id = action_id;
        self.action_completions.insert(action_id, completion);

        let BrowserActionRecord {
            source_id,
            native_id,
            selection,
            fallback_name,
        } = record;
        let services = BrowserActionServices {
            database: self.database.clone(),
            source_registry: self.source_registry.clone(),
            sidebar_refresh: self.sidebar_refresh.clone(),
        };
        let action_shutdown = self.action_shutdown.clone();
        let mut reporter = ActionTaskReporter {
            action_id,
            started: false,
            completed: false,
            internal: self.internal_tx.clone(),
        };
        self.coordinator.request(
            ServerPlaylistOperationKey::remote_playlist(source_id, native_id),
            move |context| async move {
                reporter.mark_started();
                let outcome = run_browser_action(
                    services,
                    selection,
                    fallback_name,
                    kind,
                    context,
                    action_shutdown,
                )
                .await;
                reporter.complete(outcome);
            },
        );
        // A closed coordinator drops the rejected closure, whose reporter
        // sends an internal completion that this owner maps to Closed.
    }

    fn handle_internal(&mut self, message: InternalMessage) {
        match message {
            InternalMessage::BrowseFinished {
                generation,
                fallback_name,
                completion,
                result,
            } => {
                self.finish_browse(generation, fallback_name, completion, result);
                if self.accepting && self.active_browse.is_none() {
                    if let Some(pending) = self.pending_browse.take() {
                        self.start_browse(pending);
                    }
                }
            }
            InternalMessage::ActionFinished { action_id, result } => {
                let Some(completion) = self.action_completions.remove(&action_id) else {
                    return;
                };
                let outcome = match result {
                    ActionTaskResult::Completed(outcome) => outcome,
                    ActionTaskResult::Dropped { started } => {
                        if !self.accepting || self.coordinator.is_closed() {
                            ServerPlaylistBrowserActionOutcome::Closed
                        } else if started {
                            ServerPlaylistBrowserActionOutcome::Interrupted
                        } else {
                            ServerPlaylistBrowserActionOutcome::Superseded
                        }
                    }
                };
                let _ = completion.send(outcome);
            }
        }
    }

    fn finish_browse(
        &mut self,
        generation: u64,
        fallback_name: Arc<str>,
        completion: oneshot::Sender<ServerPlaylistBrowseOutcome>,
        result: BrowseTaskResult,
    ) {
        if self.active_browse.as_ref().map(|browse| browse.generation) != Some(generation) {
            let _ = completion.send(ServerPlaylistBrowseOutcome::Superseded);
            return;
        }
        self.active_browse = None;
        if !self.accepting {
            let _ = completion.send(ServerPlaylistBrowseOutcome::Closed);
            return;
        }
        if self.browse_generation != generation {
            let _ = completion.send(ServerPlaylistBrowseOutcome::Superseded);
            return;
        }
        let listing = match result {
            BrowseTaskResult::Listed(Ok(listing)) => listing,
            BrowseTaskResult::Listed(Err(ServerPlaylistError::UnsupportedSource)) => {
                let _ = completion.send(ServerPlaylistBrowseOutcome::Unsupported);
                return;
            }
            BrowseTaskResult::Listed(Err(ServerPlaylistError::Unavailable)) => {
                let _ = completion.send(ServerPlaylistBrowseOutcome::Unavailable);
                return;
            }
            BrowseTaskResult::Listed(Err(ServerPlaylistError::BackendFailure(_))) => {
                let _ = completion.send(ServerPlaylistBrowseOutcome::Failed);
                return;
            }
            BrowseTaskResult::Interrupted => {
                let _ = completion.send(ServerPlaylistBrowseOutcome::Interrupted);
                return;
            }
            BrowseTaskResult::Cancelled => {
                let _ = completion.send(ServerPlaylistBrowseOutcome::Superseded);
                return;
            }
        };
        if listing.playlists().len() > MAX_SERVER_PLAYLISTS_PER_LIST {
            let _ = completion.send(ServerPlaylistBrowseOutcome::Failed);
            return;
        }

        let session_token = ServerPlaylistBrowserSessionToken::fresh();
        let mut entries = Vec::with_capacity(listing.playlists().len());
        let mut actions = HashMap::with_capacity(listing.playlists().len());
        for summary in listing.playlists() {
            let Some(selection) = listing.select(summary.native_id()) else {
                let _ = completion.send(ServerPlaylistBrowseOutcome::Failed);
                return;
            };
            let action_token = ServerPlaylistBrowserActionToken::fresh();
            let name = summary.name().map(str::to_string);
            let owner = summary.owner().map(str::to_string);
            let action_fallback = Arc::<str>::from(
                summary
                    .name()
                    .unwrap_or_else(|| fallback_name.as_ref())
                    .to_string(),
            );
            entries.push(ServerPlaylistBrowserEntry {
                action_token: action_token.clone(),
                name,
                owner,
                advertised_track_count: summary.advertised_track_count(),
            });
            actions.insert(
                action_token,
                BrowserActionRecord {
                    source_id: listing.source_id(),
                    native_id: summary.native_id().clone(),
                    selection,
                    fallback_name: action_fallback,
                },
            );
        }

        self.active_session = Some(ActiveBrowserSession {
            generation,
            token: session_token.clone(),
            actions,
        });
        let outcome = ServerPlaylistBrowseOutcome::Ready(ServerPlaylistBrowserSnapshot {
            session_token,
            entries,
        });
        if completion.send(outcome).is_err()
            && self
                .active_session
                .as_ref()
                .is_some_and(|session| session.generation == generation)
        {
            self.active_session = None;
        }
    }

    fn begin_shutdown(&mut self, reject_pending: bool) {
        if !self.accepting {
            return;
        }
        self.accepting = false;
        self.commands.close();
        self.active_session = None;
        if let Some(active) = &self.active_browse {
            active.cancellation.cancel();
        }
        if let Some(pending) = self.pending_browse.take() {
            let _ = pending.completion.send(ServerPlaylistBrowseOutcome::Closed);
        }
        self.action_shutdown.cancel();
        if reject_pending {
            while let Ok(command) = self.commands.try_recv() {
                match command {
                    BrowserCommand::Browse { completion, .. } => {
                        let _ = completion.send(ServerPlaylistBrowseOutcome::Closed);
                    }
                    BrowserCommand::Act { completion, .. } => {
                        let _ = completion.send(ServerPlaylistBrowserActionOutcome::Closed);
                    }
                    BrowserCommand::CloseSession { .. } => {}
                }
            }
        }
    }
}

/// Run the single Tokio owner for one browser channel.
///
/// Shutdown revokes unused tokens, cancels read-only listing and pre-admission
/// action work, and drains every accepted action completion before returning.
pub async fn run_server_playlist_browser(
    receiver: ServerPlaylistBrowserReceiver,
    database: DatabaseConnection,
    coordinator: ServerPlaylistCoordinatorHandle,
    source_registry: SourceRegistry,
    sidebar_refresh: PlaylistSidebarRefresh,
    shutdown: CancellationToken,
) {
    BrowserOwner::new(
        receiver,
        database,
        coordinator,
        source_registry,
        sidebar_refresh,
    )
    .run(shutdown)
    .await;
}

const ADMISSION_UNSET: u8 = 0;
const ADMISSION_SUPERSEDED: u8 = 1;
const ADMISSION_CLOSED: u8 = 2;
const ADMISSION_UNAVAILABLE: u8 = 3;

async fn run_browser_action(
    services: BrowserActionServices,
    selection: ServerPlaylistSelection,
    fallback_name: Arc<str>,
    kind: BrowserActionKind,
    context: ServerPlaylistOperationContext,
    shutdown: CancellationToken,
) -> ServerPlaylistBrowserActionOutcome {
    let BrowserActionServices {
        database,
        source_registry,
        sidebar_refresh,
    } = services;
    let pull = tokio::select! {
        biased;
        () = shutdown.cancelled() => return ServerPlaylistBrowserActionOutcome::Closed,
        () = context.cancelled() => return ServerPlaylistBrowserActionOutcome::Superseded,
        result = source_registry.get_server_playlist(selection) => match result {
            Ok(pull) => pull,
            Err(ServerPlaylistError::UnsupportedSource) => {
                return ServerPlaylistBrowserActionOutcome::Unsupported;
            }
            Err(ServerPlaylistError::Unavailable) => {
                return ServerPlaylistBrowserActionOutcome::Unavailable;
            }
            Err(ServerPlaylistError::BackendFailure(_)) => {
                return ServerPlaylistBrowserActionOutcome::Failed;
            }
        },
    };

    let admission = Arc::new(AtomicU8::new(ADMISSION_UNSET));
    let manager = PlaylistManager::new(database);
    let registry = &source_registry;
    let pull_ref = &pull;
    let outcome = match kind {
        BrowserActionKind::ImportCopy => {
            let admission_for_commit = Arc::clone(&admission);
            manager
                .import_server_playlist_copy_if_admitted(
                    pull_ref,
                    fallback_name.as_ref(),
                    move || {
                        admit_browser_pull(
                            context,
                            registry,
                            pull_ref,
                            shutdown,
                            admission_for_commit,
                        )
                    },
                )
                .await
                .map(|outcome| match outcome {
                    ServerPlaylistImportOutcome::Committed(_) => {
                        ServerPlaylistBrowserActionOutcome::Imported
                    }
                    ServerPlaylistImportOutcome::Rejected => {
                        rejected_action_outcome(admission.load(Ordering::Acquire))
                    }
                })
        }
        BrowserActionKind::KeepSynced => {
            let admission_for_commit = Arc::clone(&admission);
            manager
                .create_server_playlist_mirror_if_admitted(
                    pull_ref,
                    fallback_name.as_ref(),
                    move || {
                        admit_browser_pull(
                            context,
                            registry,
                            pull_ref,
                            shutdown,
                            admission_for_commit,
                        )
                    },
                )
                .await
                .map(|outcome| match outcome {
                    ServerPlaylistCreateOutcome::Committed { .. } => {
                        ServerPlaylistBrowserActionOutcome::Linked
                    }
                    ServerPlaylistCreateOutcome::AlreadyLinked(_) => {
                        ServerPlaylistBrowserActionOutcome::AlreadyLinked
                    }
                    ServerPlaylistCreateOutcome::Rejected => {
                        rejected_action_outcome(admission.load(Ordering::Acquire))
                    }
                })
        }
    };
    match outcome {
        Ok(
            outcome @ (ServerPlaylistBrowserActionOutcome::Imported
            | ServerPlaylistBrowserActionOutcome::Linked),
        ) => {
            request_sidebar_refresh(&sidebar_refresh);
            outcome
        }
        Ok(outcome) => outcome,
        Err(_) => ServerPlaylistBrowserActionOutcome::Failed,
    }
}

async fn admit_browser_pull(
    context: ServerPlaylistOperationContext,
    registry: &SourceRegistry,
    pull: &ServerPlaylistPull,
    shutdown: CancellationToken,
    admission: Arc<AtomicU8>,
) -> Option<(ServerPlaylistCommitAuthority, ServerPlaylistAdmissionGuard)> {
    let guard = tokio::select! {
        biased;
        () = shutdown.cancelled() => {
            admission.store(ADMISSION_CLOSED, Ordering::Release);
            return None;
        }
        result = context.admit() => match result {
            Ok(guard) => guard,
            Err(ServerPlaylistAdmissionError::Superseded) => {
                admission.store(ADMISSION_SUPERSEDED, Ordering::Release);
                return None;
            }
            Err(ServerPlaylistAdmissionError::Closed) => {
                admission.store(ADMISSION_CLOSED, Ordering::Release);
                return None;
            }
        },
    };
    let Some(authority) = registry.acquire_server_playlist_pull_commit_authority(pull) else {
        admission.store(ADMISSION_UNAVAILABLE, Ordering::Release);
        return None;
    };
    Some((authority, guard))
}

fn rejected_action_outcome(admission: u8) -> ServerPlaylistBrowserActionOutcome {
    match admission {
        ADMISSION_SUPERSEDED => ServerPlaylistBrowserActionOutcome::Superseded,
        ADMISSION_CLOSED => ServerPlaylistBrowserActionOutcome::Closed,
        ADMISSION_UNAVAILABLE => ServerPlaylistBrowserActionOutcome::Unavailable,
        _ => ServerPlaylistBrowserActionOutcome::Rejected,
    }
}

fn request_sidebar_refresh(refresh: &PlaylistSidebarRefresh) {
    match refresh.request() {
        PlaylistSidebarRefreshRequest::Requested
        | PlaylistSidebarRefreshRequest::Coalesced
        | PlaylistSidebarRefreshRequest::Closed => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn opaque_tokens_use_identity_and_redacted_debug() {
        let session = ServerPlaylistBrowserSessionToken::fresh();
        let session_clone = session.clone();
        let other_session = ServerPlaylistBrowserSessionToken::fresh();
        assert_eq!(session, session_clone);
        assert_ne!(session, other_session);

        let action = ServerPlaylistBrowserActionToken::fresh();
        let action_clone = action.clone();
        let other_action = ServerPlaylistBrowserActionToken::fresh();
        assert_eq!(action, action_clone);
        assert_ne!(action, other_action);
        let tokens = HashSet::from([action, other_action]);
        assert_eq!(tokens.len(), 2);

        assert_eq!(
            format!("{session:?}"),
            "ServerPlaylistBrowserSessionToken { .. }"
        );
        assert_eq!(
            format!("{action_clone:?}"),
            "ServerPlaylistBrowserActionToken { .. }"
        );
    }

    #[test]
    fn entry_and_snapshot_debug_reveal_only_shape() {
        let entry = ServerPlaylistBrowserEntry {
            action_token: ServerPlaylistBrowserActionToken::fresh(),
            name: Some("private name".to_string()),
            owner: Some("private owner".to_string()),
            advertised_track_count: Some(7),
        };
        let snapshot = ServerPlaylistBrowserSnapshot {
            session_token: ServerPlaylistBrowserSessionToken::fresh(),
            entries: vec![entry.clone()],
        };
        let entry_debug = format!("{entry:?}");
        let snapshot_debug = format!("{snapshot:?}");
        assert!(!entry_debug.contains("private name"));
        assert!(!entry_debug.contains("private owner"));
        assert!(entry_debug.contains("name_byte_len"));
        assert_eq!(
            snapshot_debug,
            "ServerPlaylistBrowserSnapshot { entry_count: 1, .. }"
        );
    }
}
