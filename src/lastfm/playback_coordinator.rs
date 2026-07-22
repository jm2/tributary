//! Process-lifetime coordination boundary for Last.fm playback evidence.
//!
//! The coordinator is deliberately dormant until the complete consent,
//! credential, runtime, and source-policy activation boundary exists. Startup
//! nevertheless claims its unique process owner now, so later activation
//! cannot accidentally grow a second playback-evidence owner. Window code
//! receives only an epoch-bound cloneable binding plus the retained,
//! uncloneable owner used for terminal shutdown.
#![allow(clippy::redundant_pub_crate)] // Explicit crate-internal authority boundary.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::audio::{PlayerEvent, PlayerEventGeneration};
use crate::source_registry::SourceRegistry;

use super::playback_owner::{LastFmAcceptedOutputLoad, LastFmOutputIntent};

static PROCESS_OWNER_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Fixed category returned when process ownership has already been consumed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum LastFmPlaybackCoordinatorClaimError {
    #[error("Last.fm playback coordinator ownership is unavailable")]
    AlreadyClaimed,
}

/// Fixed category returned when a window cannot bind the coordinator.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum LastFmPlaybackCoordinatorBindError {
    #[error("Last.fm playback coordinator has shut down")]
    Shutdown,
    #[error("Last.fm playback coordinator state is unavailable")]
    Poisoned,
    #[error("Last.fm playback coordinator window identity is exhausted")]
    WindowEpochExhausted,
}

/// Fixed, content-free failure of one coordinator ingress operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum LastFmPlaybackCoordinatorFailure {
    #[error("Last.fm playback coordinator state is unavailable")]
    Poisoned,
}

/// Content-free disposition of one coordinator operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "coordinator disposition must be handled or deliberately ignored"]
pub(crate) enum LastFmPlaybackCoordinatorOutcome {
    /// The process boundary exists but user/runtime activation does not.
    Dormant,
    /// A state transition was applied.
    Applied,
    /// The caller belongs to a superseded window epoch.
    StaleWindow,
    /// Process-lifetime shutdown has closed all ingress.
    Shutdown,
    /// The state mutex failed closed and terminally shut down the coordinator.
    Failed(LastFmPlaybackCoordinatorFailure),
}

/// Typed reason for unconditional retirement of the active occurrence.
///
/// These categories contain no source identity, metadata, generation, output
/// locator, or error text. Application shutdown is intentionally a distinct
/// owner operation because it closes the complete coordinator, not merely the
/// current occurrence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LastFmPlaybackRetirement {
    Stop,
    SourceRetirement,
    OutputReplacement,
    QueueAbandoned,
    Terminal,
}

enum LastFmPlaybackCoordinatorState {
    Dormant { window_epoch: u64 },
    Shutdown,
}

struct LastFmPlaybackCoordinatorCore {
    state: Mutex<LastFmPlaybackCoordinatorState>,
}

impl LastFmPlaybackCoordinatorCore {
    fn new() -> Self {
        Self {
            state: Mutex::new(LastFmPlaybackCoordinatorState::Dormant { window_epoch: 0 }),
        }
    }

    fn lock_state(
        &self,
    ) -> Result<MutexGuard<'_, LastFmPlaybackCoordinatorState>, LastFmPlaybackCoordinatorFailure>
    {
        match self.state.lock() {
            Ok(state) => Ok(state),
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                *state = LastFmPlaybackCoordinatorState::Shutdown;
                // Clear poison while the recovery guard is still held. A
                // concurrent binding can then observe only the terminal
                // Shutdown state, never a second transient Poisoned result.
                self.state.clear_poison();
                drop(state);
                Err(LastFmPlaybackCoordinatorFailure::Poisoned)
            }
        }
    }

    fn binding_outcome(&self, binding_epoch: u64) -> LastFmPlaybackCoordinatorOutcome {
        let Ok(state) = self.lock_state() else {
            return LastFmPlaybackCoordinatorOutcome::Failed(
                LastFmPlaybackCoordinatorFailure::Poisoned,
            );
        };
        match *state {
            LastFmPlaybackCoordinatorState::Dormant { window_epoch }
                if window_epoch == binding_epoch =>
            {
                LastFmPlaybackCoordinatorOutcome::Dormant
            }
            LastFmPlaybackCoordinatorState::Dormant { .. } => {
                LastFmPlaybackCoordinatorOutcome::StaleWindow
            }
            LastFmPlaybackCoordinatorState::Shutdown => LastFmPlaybackCoordinatorOutcome::Shutdown,
        }
    }

    fn shutdown(&self) -> LastFmPlaybackCoordinatorOutcome {
        let Ok(mut state) = self.lock_state() else {
            return LastFmPlaybackCoordinatorOutcome::Failed(
                LastFmPlaybackCoordinatorFailure::Poisoned,
            );
        };
        match *state {
            LastFmPlaybackCoordinatorState::Dormant { .. } => {
                *state = LastFmPlaybackCoordinatorState::Shutdown;
                LastFmPlaybackCoordinatorOutcome::Applied
            }
            LastFmPlaybackCoordinatorState::Shutdown => LastFmPlaybackCoordinatorOutcome::Shutdown,
        }
    }

    fn force_shutdown(&self) {
        match self.state.lock() {
            Ok(mut state) => *state = LastFmPlaybackCoordinatorState::Shutdown,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                *state = LastFmPlaybackCoordinatorState::Shutdown;
                self.state.clear_poison();
                drop(state);
            }
        }
    }
}

/// Unique, non-cloneable process owner of Last.fm playback coordination.
///
/// Dropping this value terminally closes its bindings. The process-global
/// claim remains consumed after drop, so neither a failed first window build
/// nor a later application activation can recreate ownership.
pub(crate) struct LastFmPlaybackCoordinatorOwner {
    core: Arc<LastFmPlaybackCoordinatorCore>,
    next_window_epoch: u64,
    shutdown_started: bool,
}

impl LastFmPlaybackCoordinatorOwner {
    /// Claim the sole coordinator owner for this process.
    pub(crate) fn claim_process() -> Result<Self, LastFmPlaybackCoordinatorClaimError> {
        PROCESS_OWNER_CLAIMED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| LastFmPlaybackCoordinatorClaimError::AlreadyClaimed)?;
        Ok(Self {
            core: Arc::new(LastFmPlaybackCoordinatorCore::new()),
            next_window_epoch: 0,
            shutdown_started: false,
        })
    }

    /// Bind one window environment after its source registry exists.
    ///
    /// Rebinding advances a checked epoch and makes every older binding inert.
    /// The registry is retained only by the returned binding; dormant and
    /// shutdown coordinator states retain no source or remote policy.
    pub(crate) fn bind_window(
        &mut self,
        source_registry: SourceRegistry,
    ) -> Result<LastFmPlaybackCoordinatorBinding, LastFmPlaybackCoordinatorBindError> {
        if self.shutdown_started {
            return Err(LastFmPlaybackCoordinatorBindError::Shutdown);
        }
        let Ok(mut state) = self.core.lock_state() else {
            self.shutdown_started = true;
            return Err(LastFmPlaybackCoordinatorBindError::Poisoned);
        };
        match *state {
            LastFmPlaybackCoordinatorState::Dormant { .. } => {}
            LastFmPlaybackCoordinatorState::Shutdown => {
                self.shutdown_started = true;
                return Err(LastFmPlaybackCoordinatorBindError::Shutdown);
            }
        }
        let Some(window_epoch) = self.next_window_epoch.checked_add(1) else {
            *state = LastFmPlaybackCoordinatorState::Shutdown;
            self.shutdown_started = true;
            return Err(LastFmPlaybackCoordinatorBindError::WindowEpochExhausted);
        };
        self.next_window_epoch = window_epoch;
        *state = LastFmPlaybackCoordinatorState::Dormant { window_epoch };
        drop(state);
        Ok(LastFmPlaybackCoordinatorBinding {
            core: Arc::clone(&self.core),
            _source_registry: source_registry,
            window_epoch,
        })
    }

    /// Terminally close process-lifetime coordinator admission.
    pub(crate) fn shutdown(&mut self) -> LastFmPlaybackCoordinatorOutcome {
        self.shutdown_started = true;
        self.core.shutdown()
    }

    #[cfg(test)]
    #[allow(clippy::redundant_pub_crate)]
    pub(crate) fn isolated_for_test() -> Self {
        Self {
            core: Arc::new(LastFmPlaybackCoordinatorCore::new()),
            next_window_epoch: 0,
            shutdown_started: false,
        }
    }
}

impl Drop for LastFmPlaybackCoordinatorOwner {
    fn drop(&mut self) {
        self.shutdown_started = true;
        self.core.force_shutdown();
    }
}

impl fmt::Debug for LastFmPlaybackCoordinatorOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmPlaybackCoordinatorOwner(<redacted>)")
    }
}

/// Cloneable, epoch-bound window ingress for Last.fm playback coordination.
///
/// The source registry is the future active environment for synchronous
/// attribution revalidation. It is never copied into dormant/shutdown state,
/// and no method holds the coordinator mutex while invoking caller code.
#[derive(Clone)]
pub(crate) struct LastFmPlaybackCoordinatorBinding {
    core: Arc<LastFmPlaybackCoordinatorCore>,
    _source_registry: SourceRegistry,
    window_epoch: u64,
}

impl LastFmPlaybackCoordinatorBinding {
    /// Consume an output transition intent before Stop/Load touches output.
    pub(crate) fn observe_output_intent(
        &self,
        intent: LastFmOutputIntent,
    ) -> LastFmPlaybackCoordinatorOutcome {
        let outcome = self.core.binding_outcome(self.window_epoch);
        drop(intent);
        outcome
    }

    /// Consume one accepted generation without constructing metadata while
    /// the coordinator is dormant, stale, failed, or shut down.
    ///
    /// The metadata-bearing extractor and the metadata-free exact discard are
    /// separate closures by design. This dormant implementation always drops
    /// `build` uncalled and then invokes `discard` exactly once. The state lock
    /// is released before either closure is dropped or invoked.
    pub(crate) fn accept_output_load_lazy<Build, Discard>(
        &self,
        _generation: PlayerEventGeneration,
        build: Build,
        discard: Discard,
    ) -> LastFmPlaybackCoordinatorOutcome
    where
        Build: FnOnce() -> Option<LastFmAcceptedOutputLoad>,
        Discard: FnOnce(),
    {
        let outcome = self.core.binding_outcome(self.window_epoch);
        drop(build);
        discard();
        outcome
    }

    /// Observe an already generation-gated output event.
    pub(crate) fn observe_event(&self, _event: &PlayerEvent) -> LastFmPlaybackCoordinatorOutcome {
        self.core.binding_outcome(self.window_epoch)
    }

    /// Re-anchor evidence after a seek, restart, or same-output resume.
    pub(crate) fn observe_discontinuity(
        &self,
        _generation: PlayerEventGeneration,
    ) -> LastFmPlaybackCoordinatorOutcome {
        self.core.binding_outcome(self.window_epoch)
    }

    /// Revalidate the active occurrence against this window's registry.
    pub(crate) fn revalidate_active_authority(&self) -> LastFmPlaybackCoordinatorOutcome {
        self.core.binding_outcome(self.window_epoch)
    }

    /// Unconditionally retire the active occurrence for a typed global cause.
    pub(crate) fn retire(
        &self,
        _reason: LastFmPlaybackRetirement,
    ) -> LastFmPlaybackCoordinatorOutcome {
        self.core.binding_outcome(self.window_epoch)
    }
}

impl fmt::Debug for LastFmPlaybackCoordinatorBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmPlaybackCoordinatorBinding(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use super::*;

    fn registry() -> (tokio::runtime::Runtime, SourceRegistry) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build coordinator test runtime");
        let registry = SourceRegistry::new(runtime.handle().clone());
        (runtime, registry)
    }

    #[test]
    fn process_claim_is_exactly_once_even_after_owner_drop() {
        let owner = LastFmPlaybackCoordinatorOwner::claim_process()
            .expect("first process claim must succeed");
        drop(owner);
        assert!(matches!(
            LastFmPlaybackCoordinatorOwner::claim_process(),
            Err(LastFmPlaybackCoordinatorClaimError::AlreadyClaimed)
        ));
    }

    #[test]
    fn dormant_binding_never_invokes_metadata_extractor_and_discards_once() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind dormant owner");
        let discarded = Cell::new(0);
        let core = Arc::clone(&binding.core);

        let outcome = binding.accept_output_load_lazy(
            PlayerEventGeneration::from_raw(7),
            || -> Option<LastFmAcceptedOutputLoad> {
                panic!("dormant coordinator must not extract playback metadata")
            },
            || {
                assert!(
                    core.state.try_lock().is_ok(),
                    "coordinator lock must be released before exact discard"
                );
                discarded.set(discarded.get() + 1);
            },
        );

        assert_eq!(outcome, LastFmPlaybackCoordinatorOutcome::Dormant);
        assert_eq!(discarded.get(), 1);
    }

    #[test]
    fn rebinding_makes_old_window_inert_without_moving_registry_into_state() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let old = owner
            .bind_window(registry.clone())
            .expect("bind first window");
        let current = owner.bind_window(registry).expect("bind second window");

        assert_eq!(
            old.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::StaleWindow
        );
        assert_eq!(
            current.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Dormant
        );
    }

    #[test]
    fn stale_and_shutdown_bindings_still_discard_without_extracting() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let stale = owner
            .bind_window(registry.clone())
            .expect("bind first window");
        let current = owner.bind_window(registry).expect("bind second window");
        let stale_discarded = Cell::new(0);

        assert_eq!(
            stale.accept_output_load_lazy(
                PlayerEventGeneration::from_raw(3),
                || -> Option<LastFmAcceptedOutputLoad> {
                    panic!("stale binding must not extract playback metadata")
                },
                || stale_discarded.set(stale_discarded.get() + 1),
            ),
            LastFmPlaybackCoordinatorOutcome::StaleWindow
        );
        assert_eq!(stale_discarded.get(), 1);

        assert_eq!(owner.shutdown(), LastFmPlaybackCoordinatorOutcome::Applied);
        let shutdown_discarded = Cell::new(0);
        assert_eq!(
            current.accept_output_load_lazy(
                PlayerEventGeneration::from_raw(4),
                || -> Option<LastFmAcceptedOutputLoad> {
                    panic!("shutdown binding must not extract playback metadata")
                },
                || shutdown_discarded.set(shutdown_discarded.get() + 1),
            ),
            LastFmPlaybackCoordinatorOutcome::Shutdown
        );
        assert_eq!(shutdown_discarded.get(), 1);
    }

    #[test]
    fn window_epoch_exhaustion_terminally_closes_owner() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        owner.next_window_epoch = u64::MAX;

        assert!(matches!(
            owner.bind_window(registry),
            Err(LastFmPlaybackCoordinatorBindError::WindowEpochExhausted)
        ));
        assert_eq!(owner.shutdown(), LastFmPlaybackCoordinatorOutcome::Shutdown);
    }

    #[test]
    fn all_dormant_non_metadata_ingress_is_inert() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind dormant owner");
        let generation = PlayerEventGeneration::from_raw(11);
        let event = PlayerEvent::state(generation, crate::audio::PlayerState::Playing);
        let intent =
            LastFmOutputIntent::for_test(PlayerEventGeneration::from_raw(10), generation, None);

        assert_eq!(
            binding.observe_output_intent(intent),
            LastFmPlaybackCoordinatorOutcome::Dormant
        );
        assert_eq!(
            binding.observe_event(&event),
            LastFmPlaybackCoordinatorOutcome::Dormant
        );
        assert_eq!(
            binding.observe_discontinuity(generation),
            LastFmPlaybackCoordinatorOutcome::Dormant
        );
        for reason in [
            LastFmPlaybackRetirement::Stop,
            LastFmPlaybackRetirement::SourceRetirement,
            LastFmPlaybackRetirement::OutputReplacement,
            LastFmPlaybackRetirement::QueueAbandoned,
            LastFmPlaybackRetirement::Terminal,
        ] {
            assert_eq!(
                binding.retire(reason),
                LastFmPlaybackCoordinatorOutcome::Dormant
            );
        }
    }

    #[test]
    fn shutdown_and_owner_drop_make_every_binding_terminal() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner
            .bind_window(registry.clone())
            .expect("bind first owner");
        assert_eq!(owner.shutdown(), LastFmPlaybackCoordinatorOutcome::Applied);
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Shutdown
        );
        assert_eq!(owner.shutdown(), LastFmPlaybackCoordinatorOutcome::Shutdown);

        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind second owner");
        drop(owner);
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Shutdown
        );
    }

    #[test]
    fn poisoned_state_fails_once_then_remains_terminally_shutdown() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind dormant owner");
        let core = Arc::clone(&owner.core);
        let poisoned = catch_unwind(AssertUnwindSafe(|| {
            let _state = core.state.lock().expect("lock coordinator state");
            panic!("poison coordinator state for regression");
        }));
        assert!(poisoned.is_err());

        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Failed(LastFmPlaybackCoordinatorFailure::Poisoned)
        );
        assert_eq!(
            binding.revalidate_active_authority(),
            LastFmPlaybackCoordinatorOutcome::Shutdown
        );
    }

    #[test]
    fn diagnostics_are_fixed_and_redacted() {
        let (_runtime, registry) = registry();
        let mut owner = LastFmPlaybackCoordinatorOwner::isolated_for_test();
        let binding = owner.bind_window(registry).expect("bind dormant owner");

        assert_eq!(
            format!("{owner:?}"),
            "LastFmPlaybackCoordinatorOwner(<redacted>)"
        );
        assert_eq!(
            format!("{binding:?}"),
            "LastFmPlaybackCoordinatorBinding(<redacted>)"
        );
    }
}
