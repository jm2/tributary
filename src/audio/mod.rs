//! Audio playback engine and output abstraction.
//!
//! This module provides:
//!
//! - A non-blocking GStreamer [`Player`] that wraps a `playbin3` pipeline.
//! - An [`AudioOutput`](output::AudioOutput) trait for abstracting over
//!   different playback destinations (local speakers, MPD, AirPlay, etc.).
//! - [`LocalOutput`](local_output::LocalOutput) — wraps [`Player`] for
//!   local speaker output.
//! - [`MpdOutput`](mpd_output::MpdOutput) — sends commands to an MPD
//!   server over TCP.
//!
//! # Threading model
//!
//! The GStreamer pipeline runs its own internal threads for decoding and
//! output.  All public [`Player`] methods are designed to be called from
//! the **GTK main thread**.  Pipeline bus messages and the position
//! polling timer are dispatched through `glib` main-loop callbacks, so
//! they also execute on the main thread without blocking it.
//!
//! The caller receives events by consuming the [`async_channel::Receiver`]
//! inside a `glib::MainContext::default().spawn_local()` loop, identical
//! to the pattern used by [`LibraryEngine`](crate::local::engine::LibraryEngine).

pub mod airplay_output;
pub mod cast_http_server;
pub mod chromecast_output;
pub mod equalizer;
mod gstreamer_media;
pub mod local_output;
#[cfg(any(target_os = "macos", test))]
mod macos_audio;
pub mod mpd_output;
pub mod output;
#[cfg(any(target_os = "windows", target_os = "macos"))]
mod runtime_probe;
#[cfg(target_os = "macos")]
#[allow(clippy::redundant_pub_crate)]
pub(crate) use runtime_probe::run_packaged_audio_runtime_probe;
#[cfg(target_os = "windows")]
#[allow(clippy::redundant_pub_crate)]
pub(crate) use runtime_probe::run_packaged_audio_runtime_probe as run_packaged_windows_runtime_probe;
#[cfg(test)]
pub mod test_support;
#[cfg(any(target_os = "windows", test))]
mod windows_audio;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gst::prelude::*;
use gstreamer as gst;
use gtk::glib;
use tracing::{debug, error, info, warn};
use url::{Host, Url};

use self::equalizer::{EqSettings, Preset};
use self::gstreamer_media::{GstreamerMediaProxy, GstreamerMediaTicket};
use crate::architecture::media::ResolvedHttpRequest;
use crate::local::resolver::ResolvedLocalMedia;

/// `souphttpsrc`'s default blocking-I/O deadline is 15 seconds. Protected
/// playback gives the app-owned proxy a shorter upstream startup budget, then
/// leaves this larger downstream budget for the proxy's deterministic 502/504
/// response to reach GStreamer.
const PROTECTED_LOOPBACK_TIMEOUT_SECONDS: u32 = 30;

/// GLib's proxy-resolver sentinel for an explicitly direct connection.
///
/// An empty `souphttpsrc` proxy is not sufficient: with libsoup3 it restores
/// the system resolver and can send even a 127.0.0.1 request to an ambient
/// proxy. `direct://` installs a dedicated resolver that never leaves the
/// machine for this one validated Tributary ticket.
const DIRECT_PROXY_SENTINEL: &str = "direct://";

/// How often the main-context poll re-checks a parked limiter edit for
/// its published outcome (refinery R1). Each tick is one non-blocking
/// receive; the interval trades adoption latency for idle wakeups and
/// must stay far below any human-noticeable UI lag.
const PENDING_CLIP_SWAP_POLL_MS: u64 = 10;

// ── Events ──────────────────────────────────────────────────────────────

/// Monotonic identity of the playback load that owns a [`PlayerEvent`].
///
/// Outputs capture this value when a URI is loaded (or an asynchronous command
/// is started). The UI accepts an event only while the corresponding playback
/// session generation is still current, so delayed EOS/state/error events from
/// a superseded track or output cannot mutate the new session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct PlayerEventGeneration(u64);

impl PlayerEventGeneration {
    pub(crate) fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }

    pub(crate) fn from_raw(value: u64) -> Self {
        Self(value)
    }

    pub(crate) fn as_raw(self) -> u64 {
        self.0
    }
}

/// Events emitted by an output, delivered on the GTK main thread.
#[derive(Debug, Clone)]
pub enum PlayerEvent {
    /// The pipeline transitioned to a new coarse state.
    StateChanged {
        generation: PlayerEventGeneration,
        state: PlayerState,
    },
    /// Periodic position tick (values in milliseconds).
    PositionChanged {
        generation: PlayerEventGeneration,
        position_ms: u64,
        duration_ms: u64,
    },
    /// The current stream reached its natural end.
    TrackEnded { generation: PlayerEventGeneration },
    /// A pipeline error occurred.
    Error {
        generation: PlayerEventGeneration,
        message: String,
    },
}

impl PlayerEvent {
    pub fn state(generation: PlayerEventGeneration, state: PlayerState) -> Self {
        Self::StateChanged { generation, state }
    }

    pub fn position(generation: PlayerEventGeneration, position_ms: u64, duration_ms: u64) -> Self {
        Self::PositionChanged {
            generation,
            position_ms,
            duration_ms,
        }
    }

    pub fn ended(generation: PlayerEventGeneration) -> Self {
        Self::TrackEnded { generation }
    }

    pub fn error(generation: PlayerEventGeneration, message: impl Into<String>) -> Self {
        Self::Error {
            generation,
            message: message.into(),
        }
    }

    pub fn generation(&self) -> PlayerEventGeneration {
        match self {
            Self::StateChanged { generation, .. }
            | Self::PositionChanged { generation, .. }
            | Self::TrackEnded { generation }
            | Self::Error { generation, .. } => *generation,
        }
    }
}

/// Coarse playback state visible to the rest of the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
    Stopped,
    Buffering,
    Playing,
    Paused,
}

// ── Player ──────────────────────────────────────────────────────────────

/// Live equalizer engine state owned by the local player.
///
/// `settings` mirrors the persisted contract state; `chain` is present
/// exactly while an equalizer bin is installed at `playbin3.audio-filter`
/// (i.e. only when the equalizer is enabled on the local output).
/// `save_generation` implements the trailing-edge 750 ms debounce: every
/// change re-arms the timer and only the newest generation writes. Every
/// change-spell writes, including one whose result is exactly the
/// fresh-install default state — the contract has no default-state
/// suppression. `persistence_suppressed` latches a transient unreadable
/// `equalizer.cfg`: the debounced writer and the shutdown flush stay
/// blocked until a subsequent read of the file succeeds and reconciles
/// the in-memory state with disk, so a valid on-disk file can never be
/// overwritten with defaults. `retired` records an error-driven rollback
/// honored by [`Player::ensure_equalizer_installed`] until the user
/// changes the setting.
///
/// The equalizer module's aggregate settings-changed listener type (see
/// the `settings_applied` slot below).
type EqSettingsAppliedListener = Rc<dyn Fn(&EqSettings)>;

#[derive(Default)]
struct EqEngineState {
    settings: EqSettings,
    chain: Option<equalizer::EqChain>,
    save_generation: u64,
    persistence_suppressed: bool,
    retired: bool,
    /// Whether the main-context poll adopting a parked limiter edit's
    /// completion is armed (refinery R1). Arming is idempotent: the
    /// repeating timer clears the flag when the transaction settles or
    /// dies with the chain.
    clip_swap_poll_armed: bool,
    /// Monotonic generation of user-driven applies (panel edits and
    /// reload loads funnel through
    /// [`Player::apply_equalizer_settings`]), bumped at the top of every
    /// apply. Refinery round 4 (PR 220 A1): a limiter edit parked under
    /// its blocking probe is compared against this generation when its
    /// completion is adopted — a completion from an older generation is
    /// stale and must never be recorded over a newer user action.
    apply_generation: u64,
    /// The [`Self::apply_generation`] value under which the currently
    /// parked limiter edit was issued; `None` while no edit is parked.
    /// Recorded when the toggle parks and consumed at adoption.
    pending_edit_generation: Option<u64>,
    /// The open equalizer panel's display-resync closure (refinery
    /// round 3, PR 220): registered by the panel build path through
    /// [`AudioOutput::connect_equalizer_resync`], invoked by the poll's
    /// reconciliation when — and only when — it lands a recorded value
    /// that differs from the previously recorded one, so an already-open
    /// panel re-reads the recorded state instead of keeping the walk-back
    /// it displayed while the edit was parked. Single slot: the
    /// preferences dialog is rebuilt per presentation, so a fresh panel
    /// supersedes (and drops, widgets included) any stale registration.
    panel_resync: Option<Rc<dyn Fn()>>,
    /// The equalizer module's aggregate settings-changed listener
    /// (contract: *Live-reconfiguration boundary*): registered by the UI,
    /// invoked exactly once per applied gain delta — a single-property
    /// write or one probe-delivered batch — carrying the applied
    /// `EqSettings` snapshot, so the UI re-renders from a single snapshot
    /// instead of chasing per-property `notify` emissions. Single slot,
    /// replaced on re-registration like `panel_resync`.
    settings_applied: Option<EqSettingsAppliedListener>,
}

/// Whether an equalizer state change may reach disk right now. The only
/// gate is the transient-read latch: persistence of *any* state —
/// including the fresh-install default — is a contract requirement for
/// every change-spell once the file is readable again.
fn equalizer_persistence_allowed(state: &EqEngineState) -> bool {
    !state.persistence_suppressed
}

/// What one non-blocking poll tick of the parked limiter edit's
/// completion discovered.
enum PendingClipSwapPoll {
    /// The transaction is still parked on the streaming thread.
    StillParked,
    /// The chain vanished: the transaction died with it and the poll
    /// retires itself.
    ChainRetired,
    /// The transaction settled: `installed` is the adopted outcome and
    /// `requested` is the protection peeked from the parked request
    /// before adoption (`None` only if the request vanished with a
    /// concurrently-completed transaction).
    Adopted(bool, Option<equalizer::ClipProtection>),
}

/// Run one tick of the main-context poll for a parked limiter edit's
/// completion (refinery R1). Non-blocking: `None` from
/// `poll_pending_limiter_edit` keeps the transaction parked and the UI
/// running; a settled transaction falls through to reconciliation.
/// Extracted from the arming method to keep it within its method-length
/// budget (Codacy, PR 220 head cea20fe); behavior is unchanged.
fn poll_pending_clip_swap_tick(state_rc: &Rc<RefCell<EqEngineState>>) -> PendingClipSwapPoll {
    let mut state = state_rc.borrow_mut();
    let Some(chain) = state.chain.as_mut() else {
        // The chain is gone (the bin was retired while the edit was in
        // flight): the transaction died with it, and so does the poll.
        state.clip_swap_poll_armed = false;
        return PendingClipSwapPoll::ChainRetired;
    };
    let requested = chain.pending_limiter_edit_request();
    match chain.poll_pending_limiter_edit() {
        None => PendingClipSwapPoll::StillParked,
        Some(installed) => PendingClipSwapPoll::Adopted(installed, requested),
    }
}

/// Reconcile the recorded clip protection with a settled limiter edit and
/// persist the truth (the same recorded-vs-installed discipline the
/// synchronous path applies). Extracted from the arming method to keep it
/// within its method-length budget (Codacy, PR 220 head cea20fe). When
/// the reconciled value differs from the previously recorded one, the
/// registered panel resync is notified so an already-open panel re-reads
/// the recorded state (refinery round 3, PR 220).
///
/// Refinery round 4 (PR 220 A1): a completion adopted from a *stale*
/// apply generation is superseded — a newer user action (a serialized
/// apply that loaded a whole `EqSettings` snapshot) ran while the edit
/// was parked under its blocking probe, and recording the settled truth
/// over it would silently discard that newer decision. Instead the
/// stale truth is recorded **memory-only** (the recorded field must
/// never claim a topology the chain does not carry), persistence and
/// panel resync are skipped, and the protection the newer apply loaded
/// is returned to the caller so it can re-issue that loaded policy
/// against the settled chain. Returns `Some(loaded)` when the caller
/// must re-issue the loaded policy; `None` otherwise (normal adoption,
/// superseded-but-no-op, or no chain to reconcile).
fn reconcile_adopted_clip_swap(
    state_rc: &Rc<RefCell<EqEngineState>>,
    installed: bool,
    requested: Option<equalizer::ClipProtection>,
) -> Option<equalizer::ClipProtection> {
    let mut state = state_rc.borrow_mut();
    state.clip_swap_poll_armed = false;
    // Reconcile the topology-dependent recorded field against the
    // settled graph (the same recorded-vs-installed discipline the
    // synchronous path applies): a successful edit records the requested
    // protection; a rollback or wedge records what the adopted chain
    // actually carries.
    let chain_truth = state.chain.as_ref().map(|chain| {
        if chain.clip_protection_installed() {
            equalizer::ClipProtection::Soft
        } else {
            equalizer::ClipProtection::Off
        }
    });
    let Some(chain_truth) = chain_truth else {
        // The chain vanished between adoption and reconciliation (an
        // error retirement ran first on this main context): the retire
        // path owns the recorded state now — nothing to reconcile.
        return None;
    };
    // Stale-completion check (refinery round 4, PR 220 A1): the parked
    // edit's generation must match the current apply generation, or a
    // newer user action superseded it while the probe held the graph.
    // A completion recorded without a generation (the test fixture's
    // `complete_pending_edit` injection path) never counts as stale, so
    // the existing round-3 resync regressions keep their meaning.
    if state
        .pending_edit_generation
        .take()
        .is_some_and(|generation| generation != state.apply_generation)
    {
        // Superseded: record the settled chain truth memory-only so the
        // recorded field never lies about the topology, but do not
        // persist it and do not resync the panel — this value is about
        // to be superseded again by the re-issue.
        let loaded = state.settings.clip_protection;
        if loaded == chain_truth {
            // The loaded policy already matches the settled chain: the
            // newer apply's decision stands, nothing to re-issue.
            return None;
        }
        state.settings.clip_protection = chain_truth;
        return Some(loaded);
    }
    let previous = state.settings.clip_protection;
    state.settings.clip_protection = if installed {
        // The requested toggle is installed; the request was peeked from
        // the parked transaction before adoption, so it is present here.
        requested.unwrap_or(chain_truth)
    } else {
        // Rollback or wedge: what the chain carries is truth.
        chain_truth
    };
    drop(state);
    // Persist the reconciled truth through the trailing-edge debounce
    // (the same schedule the apply path uses).
    schedule_eq_save_for(state_rc);
    notify_panel_resync(state_rc, previous);
    None
}

/// Re-issue the clip protection a superseded apply loaded, against the
/// now-settled chain (refinery round 4, PR 220 A1). Runs after a stale
/// completion was adopted and the chain truth recorded memory-only:
/// the newer user decision must win the topology, not be silently
/// dropped by the settled state.
///
/// Gates mirror the apply path: only an enabled equalizer with a live
/// chain re-issues, and only when the loaded policy differs from the
/// installed truth — a no-op request would just churn the toggle path.
///
/// On success the loaded value is recorded, persisted (the apply path's
/// trailing-edge schedule) and the panel resync is notified — the panel
/// displayed the walk-back and must follow the final decision. On a
/// re-park (the re-issued edit parks under its own probe) the fresh
/// completion is adopted under the *current* generation and reconciles
/// normally; on a deferred seam the recorded chain truth already stands
/// and describes the graph truthfully.
fn reissue_loaded_clip_policy(
    playbin: &gst::Element,
    state_rc: &Rc<RefCell<EqEngineState>>,
    seams: &EqTestSeamsHandle,
    loaded: equalizer::ClipProtection,
) {
    let installed = {
        let state = state_rc.borrow();
        if !state.settings.enabled {
            return;
        }
        let Some(chain) = state.chain.as_ref() else {
            return;
        };
        if chain.clip_protection_installed() {
            equalizer::ClipProtection::Soft
        } else {
            equalizer::ClipProtection::Off
        }
    };
    if installed == loaded {
        return;
    }
    let previous = installed;
    if attempt_clip_protection_edit(playbin, state_rc, seams, loaded) {
        state_rc.borrow_mut().settings.clip_protection = loaded;
        schedule_eq_save_for(state_rc);
        notify_panel_resync(state_rc, previous);
    }
    // `false`: the re-issue parked again (its completion carries the
    // current generation and reconciles normally) or degraded (the
    // recorded chain truth stands). Either way nothing more to do.
}

/// Whether the pipeline is settled in `Playing`. Only a confirmed
/// `Playing` pipeline can be audibly interrupted by a topology edit, so
/// only then is the non-pausing dynamic probe used; a stopped or
/// unsettled pipeline is left to the seam (which edits directly when
/// not playing and defers on a transition in flight). Module-scope free
/// function (refinery round 4, PR 220 A1): the main-context adoption
/// closure cannot capture the `Player` by value, so the toggle
/// machinery runs over `(playbin, eq_state, eq_test_seams)` and the
/// `Player` methods delegate to it. The playing query is part of the
/// test seams for the same reason.
fn pipeline_confirmed_playing(playbin: &gst::Element, seams: &EqTestSeamsHandle) -> bool {
    #[cfg(test)]
    {
        let seams = seams.borrow();
        if let Some(playing) = seams.playing {
            return playing;
        }
    }
    let _ = seams;
    settled_zero_state(playbin.state(gst::ClockTime::ZERO)) == Some(true)
}

/// Attempt the clip-protection toggle through the dynamic blocking
/// pad-probe edit. `Some(installed)` when the edit reached a validated
/// topology, `None` when no chain is installed or the probe could not
/// engage so the caller falls back to the pause/relink seam.
fn dynamic_clip_swap_attempt(
    state_rc: &Rc<RefCell<EqEngineState>>,
    seams: &EqTestSeamsHandle,
    protection: equalizer::ClipProtection,
) -> Option<bool> {
    #[cfg(test)]
    if let Some(hook) = seams.borrow_mut().dynamic.take() {
        return hook();
    }
    let _ = seams;
    let mut state = state_rc.borrow_mut();
    let chain = state.chain.as_mut()?;
    chain.swap_clip_protection_under_block_probe(protection)
}

/// Run one pipeline-topology edit through the pause → edit → resume
/// seam. A running pipeline is paused and given a bounded window to
/// settle; the edit runs **only once the state query confirms the
/// pipeline reached `Paused`** — a slow or blocked transition never
/// leaves the edit running against live data flow. A pipeline that
/// misses the window gets its pending pause cancelled (back to
/// `Playing`) and the edit is deferred: the caller reports failure
/// and the next apply, or the next URI load's install seam, retries.
/// A NULL pipeline (idle player) skips straight to the edit.
///
/// A zero-timeout query that finds a *transition in flight* is also
/// a defer: its `current` member reports the transition's *origin*
/// state (e.g. `Paused` with `Playing` pending), which would read as
/// "not playing" and run the edit without ever confirming a settled
/// state — exactly the async-transition race the bounded window
/// exists to prevent.
fn suspend_pipeline_and_edit<F: FnOnce() -> bool>(
    playbin: &gst::Element,
    seams: &EqTestSeamsHandle,
    edit: F,
) -> bool {
    #[cfg(test)]
    if let Some(seam) = seams.borrow_mut().seam.take() {
        // Test hook: hand the edit to the injected seam outcome so
        // the deferred/confirmed caller discipline is exercisable
        // without a live pipeline.
        let mut edit: Option<F> = Some(edit);
        let mut run = move || {
            let taken = edit.take().expect("edit invoked at most once");
            taken()
        };
        return seam(&mut run);
    }
    let _ = seams;
    let Some(was_playing) = settled_zero_state(playbin.state(gst::ClockTime::ZERO)) else {
        warn!(
            "Pipeline state query found a transition in flight; \
             equalizer topology edit deferred"
        );
        return false;
    };
    if was_playing {
        if let Err(error) = playbin.set_state(gst::State::Paused) {
            warn!(
                error = %error,
                "Pipeline pause request failed; equalizer topology edit deferred"
            );
            return false;
        }
        // Bounded settle — a topology edit must never wedge the UI.
        let (_, settled, _) = playbin.state(gst::ClockTime::from_seconds(1));
        if settled != gst::State::Paused {
            let _ = playbin.set_state(gst::State::Playing);
            warn!(
                settled_state = ?settled,
                "Pipeline did not confirm PAUSED within the settle window; \
                 equalizer topology edit deferred"
            );
            return false;
        }
    }
    let result = edit();
    if was_playing {
        let _ = playbin.set_state(gst::State::Playing);
    }
    result
}

/// Attempt one clip-protection topology edit end to end (the shared
/// core of [`Player::toggle_clip_protection`], refinery round 4, PR 220
/// A1): dynamic blocking-pad-probe edit while the pipeline is confirmed
/// `Playing`, park when the probe engaged and outlived its engagement
/// window (the completion is adopted on the main context), and the
/// pause/relink seam otherwise. Returns `true` when a validated
/// topology carries `protection`; `false` when the edit was parked,
/// deferred, or the surgery degraded — the caller then records what the
/// installed chain actually carries.
///
/// On park, the current [`EqEngineState::apply_generation`] is recorded
/// with the transaction (`pending_edit_generation`), so the adoption
/// can recognize — and refuse to apply — a completion from an
/// apply that has since been superseded.
fn attempt_clip_protection_edit(
    playbin: &gst::Element,
    state_rc: &Rc<RefCell<EqEngineState>>,
    seams: &EqTestSeamsHandle,
    protection: equalizer::ClipProtection,
) -> bool {
    if pipeline_confirmed_playing(playbin, seams) {
        if let Some(installed) = dynamic_clip_swap_attempt(state_rc, seams, protection) {
            info!(
                clip_protection = ?protection,
                installed,
                "Clip protection element toggled under a blocking pad probe"
            );
            if installed {
                return true;
            }
            // The dynamic re-link failed and the surgery restored the
            // pre-edit layout: retry through the pause/relink seam.
        } else {
            let parked = state_rc
                .borrow()
                .chain
                .as_ref()
                .is_some_and(|chain| chain.has_pending_limiter_edit());
            if parked {
                // The edit engaged under its blocking probe and outlived
                // the engagement window (refinery R1): the caller-side
                // wait returned within the bounded window instead of
                // blocking, the transaction is parked, and the outcome is
                // adopted on the main context. No fallback may run — the
                // pause/relink seam would pause and edit a graph whose
                // topology is mid-surgery and unvalidated. The recorded
                // state reflects the pre-edit truth (the parked
                // transaction reports it), and the completion reconciles
                // the settled truth and persists it.
                info!(
                    clip_protection = ?protection,
                    "Clip protection edit engaged under its blocking probe; \
                     the completion is adopted asynchronously on the main context"
                );
                // Stamp the apply generation this edit was issued under
                // (refinery round 4, PR 220 A1). The chain gate means at
                // most one transaction can be parked, so the slot is
                // free here; assigning unconditionally is self-healing.
                {
                    let mut state = state_rc.borrow_mut();
                    state.pending_edit_generation = Some(state.apply_generation);
                }
                arm_pending_clip_swap_completion(playbin, state_rc, seams);
                return false;
            }
        }
    }
    suspend_pipeline_and_edit(playbin, seams, || {
        let mut state = state_rc.borrow_mut();
        let Some(chain) = state.chain.as_mut() else {
            return false;
        };
        let installed = chain.set_clip_protection(protection);
        info!(
            clip_protection = ?protection,
            installed,
            "Clip protection element toggled via the pause/relink seam"
        );
        installed
    })
}

/// Arm the main-context poll that adopts a parked limiter edit's
/// completion (refinery R1). The poll never blocks: every tick does
/// one non-blocking receive and otherwise yields, so the UI keeps
/// running for the callback's whole — possibly unbounded — surgery.
/// On adoption the recorded clip protection is reconciled to the
/// settled truth and persisted through the ordinary trailing-edge
/// debounce, exactly like a user-driven apply. A superseded completion
/// (stale apply generation, refinery round 4 A1) instead re-issues the
/// loaded policy via [`reissue_loaded_clip_policy`].
fn arm_pending_clip_swap_completion(
    playbin: &gst::Element,
    state_rc: &Rc<RefCell<EqEngineState>>,
    seams: &EqTestSeamsHandle,
) {
    {
        let mut state = state_rc.borrow_mut();
        if state.clip_swap_poll_armed {
            return;
        }
        state.clip_swap_poll_armed = true;
    }
    let playbin = playbin.clone();
    let seams = Rc::clone(seams);
    let poll_state = Rc::clone(state_rc);
    glib::timeout_add_local(
        Duration::from_millis(PENDING_CLIP_SWAP_POLL_MS),
        move || match poll_pending_clip_swap_tick(&poll_state) {
            PendingClipSwapPoll::StillParked => glib::ControlFlow::Continue,
            PendingClipSwapPoll::ChainRetired => glib::ControlFlow::Break,
            PendingClipSwapPoll::Adopted(installed, requested) => {
                if let Some(loaded) = reconcile_adopted_clip_swap(&poll_state, installed, requested)
                {
                    reissue_loaded_clip_policy(&playbin, &poll_state, &seams, loaded);
                }
                glib::ControlFlow::Break
            }
        },
    );
}

/// Notify the registered panel resync after a reconciliation moved the
/// recorded clip protection off `previous` (refinery round 3, PR 220):
/// the open panel displayed the walk-back while the edit was parked, so
/// it must re-read the recorded state once the settled truth lands. An
/// adoption that records the already-recorded value — a rollback
/// restoring the pre-edit layout — notifies nothing, the mirror image of
/// the panel's apply handlers' echo-safety skip. The closure runs here
/// on the main context with no engine borrow held, so it can re-read the
/// recorded state through the output.
fn notify_panel_resync(state_rc: &Rc<RefCell<EqEngineState>>, previous: equalizer::ClipProtection) {
    let (recorded, resync) = {
        let state = state_rc.borrow();
        (state.settings.clip_protection, state.panel_resync.clone())
    };
    if recorded == previous {
        return;
    }
    if let Some(resync) = resync {
        resync();
    }
}

/// The shared body of the trailing-edge debounced equalizer persistence:
/// re-arm the save timer and let only the newest generation write.
/// Extracted from the `Player` method so the main-context poll's
/// reconciliation can share it (Codacy, PR 220 head cea20fe); behavior is
/// unchanged.
fn schedule_eq_save_for(state_rc: &Rc<RefCell<EqEngineState>>) {
    let next_generation = state_rc.borrow().save_generation.wrapping_add(1);
    state_rc.borrow_mut().save_generation = next_generation;
    let state = Rc::clone(state_rc);
    glib::timeout_add_local_once(
        Duration::from_millis(equalizer::SAVE_DEBOUNCE_MS),
        move || {
            let state = state.borrow_mut();
            if state.save_generation != next_generation {
                return;
            }
            if !equalizer_persistence_allowed(&state) {
                return;
            }
            let _ = equalizer::save_equalizer_settings_to_disk(&state.settings);
        },
    );
}

/// Classify a zero-timeout `state()` query. Returns `Some(was_playing)`
/// when the pipeline reports a *settled* state (`Success` or `NoPreroll`
/// with no pending target), and `None` when a state transition is in
/// flight: an `Async` return reports the transition's *origin* state in
/// `current` and its target in `pending`, so acting on `current` alone
/// would run a topology edit against a pipeline whose data flow is
/// about to change — `current = Paused, pending = Playing` reads as
/// "not playing" and skips the pause entirely. The pending member of a
/// zero-timeout query must therefore never be discarded.
fn settled_zero_state(
    query: (
        Result<gst::StateChangeSuccess, gst::StateChangeError>,
        gst::State,
        gst::State,
    ),
) -> Option<bool> {
    let (result, current, pending) = query;
    let settled = matches!(
        result,
        Ok(gst::StateChangeSuccess::Success | gst::StateChangeSuccess::NoPreroll)
    );
    if !settled || pending != gst::State::VoidPending {
        return None;
    }
    Some(current == gst::State::Playing)
}

/// Seam hook: consumes one topology edit closure and reports the seam
/// outcome (`true` confirmed / `false` deferred), letting the
/// caller-discipline tests drive the pipeline-suspension path without a
/// live pipeline. Production never populates it.
type EqSeamHook = Box<dyn FnOnce(&mut dyn FnMut() -> bool) -> bool>;

/// Seam hook: consumes one dynamic clip-protection attempt and reports
/// its outcome (`Some(installed)` reached a validated topology, `None`
/// probe could not engage), letting the caller-discipline tests drive the
/// dynamic path without a live pipeline. Production never populates it.
type EqDynamicHook = Box<dyn FnOnce() -> Option<bool>>;

/// The caller-discipline test seams shared by the clip-toggle code paths.
/// Production leaves every slot empty, so the shared free-function paths
/// compile once and the overrides fall through to the real pipeline
/// queries and edits. Tests populate slots through
/// `Player::eq_test_seams`.
#[derive(Default)]
#[cfg_attr(not(test), allow(dead_code))] // slots are read by test-only paths
struct EqTestSeams {
    /// Overrides the settled-`Playing` query so the dynamic probe path is
    /// exercisable on a bare test pipeline.
    playing: Option<bool>,
    /// Overrides the dynamic blocking-pad-probe attempt.
    dynamic: Option<EqDynamicHook>,
    /// Overrides `with_pipeline_suspended` so the deferred/confirmed seam
    /// outcomes are exercisable deterministically.
    seam: Option<EqSeamHook>,
}

/// Shared handle to the test seams (the clip-toggle machinery runs from
/// both the `Player` methods and the main-context adoption closure, so
/// both sides need cheap clones).
type EqTestSeamsHandle = Rc<RefCell<EqTestSeams>>;

/// GStreamer playback engine.
///
/// Wraps a `playbin3` (with `playbin` fallback) and exposes a safe,
/// main-thread-only control surface.  State updates are pushed through
/// the [`async_channel::Receiver`] returned by [`Player::new`].
pub struct Player {
    /// Retains the CoreAudio default-output listener and the explicitly
    /// configured `osxaudiosink`. `Player::drop` retires it before taking the
    /// pipeline to `NULL`.
    #[cfg(target_os = "macos")]
    macos_audio_route: Option<macos_audio::MacosAudioRoute>,
    playbin: gst::Element,
    volume: Rc<Cell<f64>>,
    /// Allows at most one warning-triggered sink reconnect until a new load
    /// or an observed system-device change establishes a fresh boundary.
    sink_recovery_claimed: Rc<Cell<bool>>,
    event_tx: async_channel::Sender<PlayerEvent>,
    /// App-owned exact-origin fetch boundary for authenticated media. The
    /// pipeline receives only a dedicated loopback ticket, never the backend
    /// URL carrying the user's credential.
    media_proxy: Arc<GstreamerMediaProxy>,
    /// Generation assigned by the playback session before each URI load.
    event_generation: Rc<Cell<PlayerEventGeneration>>,
    /// Holds the latest volume awaiting a debounced disk write, or `None`
    /// when no write is scheduled.  Keeps slider-drag volume changes off
    /// the main-thread hot path (see [`Player::save_volume_debounced`]).
    volume_save_pending: Rc<Cell<Option<f64>>>,
    /// Equalizer contract state: persisted settings mirror plus the
    /// installed filter-bin handles and the debounce generation.
    eq_state: Rc<RefCell<EqEngineState>>,
    /// The watch is replaced on every URI load. Each watch captures that
    /// load's generation, so even an already-queued message from the previous
    /// pipeline incarnation remains identifiable as stale.
    bus_watch: RefCell<Option<gst::bus::BusWatchGuard>>,
    /// Retains the Windows device monitor and its bus watch for the lifetime
    /// of the local playback pipeline.
    #[cfg(target_os = "windows")]
    _windows_audio_route: Option<windows_audio::WindowsAudioRoute>,
    /// Test-only seam overrides for the equalizer caller-discipline
    /// paths (pipeline suspension, dynamic blocking-pad-probe, settled
    /// `Playing` query): when a slot is set, the shared toggle path
    /// delegates to it instead of the real pipeline, so the deferred and
    /// parked behaviors are exercisable deterministically in tests.
    /// Production never populates it.
    eq_test_seams: EqTestSeamsHandle,
    /// Test-only sink for the shutdown flush: when set, the flush
    /// records the settings it would persist here instead of touching
    /// the real user config path, so the close-drain behavior is
    /// observable without a test ever writing the user's equalizer.cfg
    /// (test discipline: tests never persist).
    #[cfg(test)]
    eq_flush_sink: RefCell<Option<Rc<RefCell<Vec<EqSettings>>>>>,
}

impl Player {
    /// Return a clone of the event sender.
    ///
    /// Used to give `MpdOutput` (or other non-GStreamer outputs) a sender
    /// that feeds into the **same** `player_rx` event loop, so position
    /// ticks, state changes, and errors from any output are handled
    /// uniformly by the single `PlayerEvent` consumer in `window.rs`.
    pub fn event_sender(&self) -> async_channel::Sender<PlayerEvent> {
        self.event_tx.clone()
    }

    /// Initialise GStreamer, build the pipeline, and start the bus watch
    /// and position polling timer.
    ///
    /// Returns the player and a receiver.  The caller must consume the
    /// receiver on the GTK main thread via:
    /// ```ignore
    /// glib::MainContext::default().spawn_local(async move {
    ///     while let Ok(event) = player_rx.recv().await {
    ///         // handle PlayerEvent …
    ///     }
    /// });
    /// ```
    pub fn new(
        rt_handle: tokio::runtime::Handle,
    ) -> anyhow::Result<(Self, async_channel::Receiver<PlayerEvent>)> {
        gst::init()?;
        info!("GStreamer {}", gst::version_string());

        // Prefer playbin3 (auto-plugging, modern); fall back to playbin.
        let playbin = gst::ElementFactory::make("playbin3")
            .build()
            .or_else(|_| {
                warn!("playbin3 unavailable, falling back to playbin");
                gst::ElementFactory::make("playbin").build()
            })
            .map_err(|e| anyhow::anyhow!("Failed to create playbin element: {e}"))?;

        // Protected remote media is deliberately handed to GStreamer as an
        // opaque loopback ticket. Configure the HTTP source before it opens so
        // an ambient system proxy can never receive that ticket.
        Self::install_loopback_http_source_policy(&playbin);

        #[cfg(target_os = "macos")]
        let macos_audio_route = macos_audio::MacosAudioRoute::install(&playbin);

        let volume = Rc::new(Cell::new(load_saved_volume().unwrap_or(1.0)));
        let sink_recovery_claimed = Rc::new(Cell::new(false));

        // Load the persisted equalizer state. A malformed file has already
        // been replaced with the default state (atomic replace) and reports
        // a single bounded diagnostic through the shared loader. A file
        // that exists but cannot be read leaves persistence suppressed
        // until a successful read reconciles state with disk.
        let (eq_settings, eq_load_status) = equalizer::load_settings_with_status();
        let eq_state = Rc::new(RefCell::new(EqEngineState {
            settings: eq_settings,
            chain: None,
            save_generation: 0,
            persistence_suppressed: eq_load_status == equalizer::EqLoadStatus::TransientReadFailure,
            retired: false,
            clip_swap_poll_armed: false,
            apply_generation: 0,
            pending_edit_generation: None,
            panel_resync: None,
            settings_applied: None,
        }));

        #[cfg(target_os = "windows")]
        let windows_audio_route = windows_audio::WindowsAudioRoute::install(
            &playbin,
            Rc::clone(&volume),
            Rc::clone(&sink_recovery_claimed),
        );

        playbin.set_property("volume", slider_to_pipeline(volume.get()));

        let (event_tx, event_rx) = async_channel::unbounded();

        let event_generation = Rc::new(Cell::new(PlayerEventGeneration::default()));
        Self::start_position_timer(&playbin, &event_tx, Rc::clone(&event_generation));

        let player = Self {
            #[cfg(target_os = "macos")]
            macos_audio_route,
            playbin,
            volume,
            sink_recovery_claimed,
            event_tx,
            media_proxy: Arc::new(GstreamerMediaProxy::new(Some(rt_handle))),
            event_generation,
            volume_save_pending: Rc::new(Cell::new(None)),
            eq_state,
            bus_watch: RefCell::new(None),
            #[cfg(target_os = "windows")]
            _windows_audio_route: windows_audio_route,
            eq_test_seams: Rc::new(RefCell::new(EqTestSeams::default())),
            #[cfg(test)]
            eq_flush_sink: RefCell::new(None),
        };

        Ok((player, event_rx))
    }

    // ── Playback controls ───────────────────────────────────────────

    /// Load a URI (e.g. `file:///path/to/song.flac`) and start playback.
    ///
    /// Immediately emits [`PlayerState::Buffering`] so the UI can show a
    /// spinner while the pipeline transitions to `Playing`.
    pub fn load_uri(&self, uri: &str) {
        tracing::debug!("Loading track");
        let generation = self.begin_load();
        let prepared = self.media_proxy.prepare(uri);
        self.finish_load(generation, prepared);
    }

    /// Load one backend-resolved authenticated request through an app-owned
    /// loopback ticket. The typed request is never eligible for direct
    /// GStreamer playback.
    pub fn load_resolved(&self, request: ResolvedHttpRequest) {
        tracing::debug!("Loading resolved track");
        let generation = self.begin_load();
        let prepared = self.media_proxy.prepare_resolved(request);
        self.finish_load(generation, prepared);
    }

    /// Load an exact local-library file through an app-owned handle-backed
    /// loopback ticket. The GStreamer source never reopens the database path.
    pub fn load_local(&self, media: ResolvedLocalMedia) {
        tracing::debug!("Loading authorized local track");
        let generation = self.begin_load();
        let prepared = self.media_proxy.prepare_local(media);
        self.finish_load(generation, prepared);
    }

    fn begin_load(&self) -> PlayerEventGeneration {
        // Remove the previous generation's watch before driving that pipeline
        // to NULL. Flush the bus during teardown as well: otherwise a queued
        // EOS from the old URI could be consumed by the newly attached watch
        // and inherit the new generation despite originating from the old
        // pipeline incarnation.
        self.bus_watch.borrow_mut().take();
        if let Some(bus) = self.playbin.bus() {
            bus.set_flushing(true);
        }
        let _ = self.playbin.set_state(gst::State::Null);
        // Retiring the pipeline state does not clear playbin's URI property.
        // If preparation of the replacement media then fails, a later Play
        // must not be able to restart the previous track under the new queue
        // item's metadata.
        self.playbin.set_property("uri", "");

        self.event_generation.get()
    }

    fn finish_load(
        &self,
        generation: PlayerEventGeneration,
        prepared: Result<gstreamer_media::PreparedGstreamerMedia, &'static str>,
    ) {
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(failure) => {
                error!(error = %failure, "Audio media preparation failed");
                self.emit_load_failure(generation, failure.to_string());
                return;
            }
        };
        self.playbin.set_property("uri", prepared.uri());
        // Re-apply volume — the NULL transition resets it to 1.0.
        self.playbin
            .set_property("volume", slider_to_pipeline(self.volume.get()));
        // The equalizer bin persists across URI transitions; only a
        // genuinely missing chain (first enable) is rebuilt here,
        // before the pipeline leaves NULL. An error-retired chain stays
        // retired until the user changes the equalizer setting.
        self.ensure_equalizer_installed();
        self.sink_recovery_claimed.set(false);
        if let Some(bus) = self.playbin.bus() {
            bus.set_flushing(false);
        }

        // Signal buffering immediately — the bus watch will send
        // `Playing` once the pipeline actually reaches that state.
        let ticket = prepared.ticket();
        match Self::attach_bus_watch(
            &self.playbin,
            &self.event_tx,
            generation,
            Arc::clone(&self.media_proxy),
            ticket.clone(),
            Rc::clone(&self.volume),
            Rc::clone(&self.sink_recovery_claimed),
            Rc::clone(&self.eq_state),
        ) {
            Ok(watch) => *self.bus_watch.borrow_mut() = Some(watch),
            Err(error) => {
                if let Some(ticket) = ticket.as_ref() {
                    self.media_proxy.revoke_if_current(ticket);
                }
                if let Some(bus) = self.playbin.bus() {
                    bus.set_flushing(true);
                }
                let _ = self
                    .event_tx
                    .try_send(PlayerEvent::error(generation, error.to_string()));
                let _ = self
                    .event_tx
                    .try_send(PlayerEvent::state(generation, PlayerState::Stopped));
                return;
            }
        }

        if let Err(e) = self
            .event_tx
            .try_send(PlayerEvent::state(generation, PlayerState::Buffering))
        {
            warn!(error = %e, "dropped Buffering event — UI consumer may be stalled");
        }

        if self.playbin.set_state(gst::State::Playing).is_err() {
            self.bus_watch.borrow_mut().take();
            if let Some(bus) = self.playbin.bus() {
                bus.set_flushing(true);
            }
            let _ = self.playbin.set_state(gst::State::Null);
            if let Some(ticket) = ticket.as_ref() {
                self.media_proxy.revoke_if_current(ticket);
            }
            error!("Audio pipeline failed to start");
            let _ = self.event_tx.try_send(PlayerEvent::error(
                generation,
                "Audio playback failed to start",
            ));
            let _ = self
                .event_tx
                .try_send(PlayerEvent::state(generation, PlayerState::Stopped));
        }
    }

    /// Resume playback from a paused state.
    pub fn play(&self) {
        debug!("play");
        let _ = self.playbin.set_state(gst::State::Playing);
    }

    /// Pause playback.
    pub fn pause(&self) {
        debug!("pause");
        let _ = self.playbin.set_state(gst::State::Paused);
    }

    /// Stop playback and reset the pipeline to NULL.
    pub fn stop(&self) {
        debug!("stop");
        self.bus_watch.borrow_mut().take();
        if let Some(bus) = self.playbin.bus() {
            // Leave the idle bus flushing until the next load; the explicit
            // scoped Stopped event below is the only stop notification needed.
            bus.set_flushing(true);
        }
        let _ = self.playbin.set_state(gst::State::Null);
        self.media_proxy.revoke();
        let generation = self.event_generation.get();
        if let Err(e) = self
            .event_tx
            .try_send(PlayerEvent::state(generation, PlayerState::Stopped))
        {
            warn!(error = %e, "dropped Stopped event — UI consumer may be stalled");
        }
    }

    /// Toggle between Playing ↔ Paused.
    pub fn toggle_play_pause(&self) {
        // Non-blocking state query (zero timeout).
        let (_, current, _) = self.playbin.state(gst::ClockTime::ZERO);
        match current {
            gst::State::Playing => self.pause(),
            gst::State::Paused => self.play(),
            _ => {}
        }
    }

    /// Seek to an absolute position (milliseconds from start).
    pub fn seek_to(&self, position_ms: u64) {
        debug!(position_ms, "seek");
        let _ = self.playbin.seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            gst::ClockTime::from_mseconds(position_ms),
        );
    }

    /// Associate subsequently emitted events with a playback-session load.
    pub fn set_event_generation(&self, generation: PlayerEventGeneration) {
        self.event_generation.set(generation);
    }

    // ── Volume ──────────────────────────────────────────────────────

    /// Set pipeline volume (clamped to 0.0 – 1.0, linear).
    /// Set volume from a linear slider position (0.0 – 1.0).
    /// Internally applies a cubic curve for perceptually linear loudness.
    pub fn set_volume(&self, level: f64) {
        self.volume.set(level.clamp(0.0, 1.0));
        self.playbin
            .set_property("volume", slider_to_pipeline(self.volume.get()));
        self.save_volume_debounced();
        debug!(volume = self.volume.get(), "Volume set");
    }

    /// Persist the current volume off the GTK main-thread hot path.
    ///
    /// The volume adjustment fires `set_volume` on every tick of a slider
    /// drag; writing the volume file synchronously on each tick would do
    /// many redundant blocking disk writes on the main thread.  Instead we
    /// coalesce them: record the latest value and, if no write is already
    /// scheduled, queue a single delayed flush that persists whatever value
    /// the slider has settled on.
    fn save_volume_debounced(&self) {
        let already_scheduled = self.volume_save_pending.get().is_some();
        self.volume_save_pending.set(Some(self.volume.get()));
        if already_scheduled {
            return;
        }
        let pending = Rc::clone(&self.volume_save_pending);
        glib::timeout_add_local_once(Duration::from_millis(750), move || {
            if let Some(level) = pending.take() {
                save_volume(level);
            }
        });
    }

    /// Current pipeline volume (0.0 – 1.0).
    pub fn volume(&self) -> f64 {
        self.volume.get()
    }

    // ── Equalizer (docs/equalizer.md contract) ──────────────────────

    /// Current in-memory equalizer state (mirrors the persisted file).
    pub fn equalizer_settings(&self) -> EqSettings {
        self.eq_state.borrow().settings
    }

    /// Force a re-read of `equalizer.cfg` — the settings UI's
    /// escape hatch from a malformed or transiently unreadable on-disk
    /// file, and the contract's "subsequent successful read" that ends a
    /// persistence suppression. Returns the state now in effect and, if
    /// the pipeline is live, re-applies it. A read that fails again
    /// keeps the suppression armed.
    pub fn reload_equalizer_settings(&self) -> EqSettings {
        let (settings, status) = equalizer::load_settings_with_status();
        self.eq_state.borrow_mut().persistence_suppressed =
            status == equalizer::EqLoadStatus::TransientReadFailure;
        self.apply_equalizer_settings(settings);
        settings
    }

    /// Apply a new equalizer state per the live-reconfiguration boundary:
    ///
    /// - `Enabled` and `Clip protection` toggles change the bin topology,
    ///   so they run through the pause → surgery → resume seam, which
    ///   edits only after the pipeline has confirmed `Paused`.
    /// - Band, preamp, and preset changes are buffer-boundary
    ///   property-write transactions (idle pad probe on the running
    ///   bin's sink ghost pad) on the installed chain.
    /// - The new state is persisted through the trailing-edge debounce;
    ///   every change-spell writes, including one whose result is exactly
    ///   the fresh-install default state.
    ///
    /// **Recorded-vs-installed discipline:** the recorded settings start
    /// as the user's choice and are walked back to the *installed*
    /// topology wherever the seam defers or the surgery fails, so the
    /// persisted state never describes a bin that is not actually there.
    /// A rolled-back record also makes the *next* apply retry the edit
    /// (the toggles still differ), which is what keeps a deferred
    /// uninstall/install from stranding a bin.
    ///
    /// A configuration update never emits a `Buffering` event: GObject
    /// property writes produce no pipeline event, and any `Buffering`
    /// observed on the bus originates from the upstream decoder.
    pub fn apply_equalizer_settings(&self, next: EqSettings) {
        // Stale-completion baseline (refinery round 4, PR 220 A1): every
        // user-driven apply — panel edit and reload load alike — bumps
        // the generation, so a limiter edit parked under an earlier
        // apply is recognized as superseded when its completion is
        // adopted on the main context.
        self.eq_state.borrow_mut().apply_generation += 1;

        let current = self.eq_state.borrow().settings;
        let enabled_changed = current.enabled != next.enabled;
        let clip_changed = next.enabled && current.clip_protection != next.clip_protection;

        let mut effective = next;
        // A freshly built chain already carries the requested gains
        // (`EqChain::build` stamps them at construction), so the gain
        // delivery below is skipped for this apply — pushing the same
        // values again would run a redundant probe batch over a chain
        // that was just stamped (refinery round 4, PR 220 A2).
        let mut fresh_install = false;

        if enabled_changed {
            if next.enabled {
                let installed = self.install_equalizer_bin(&next);
                fresh_install = installed;
                if !installed {
                    // Deferred install: no chain was recorded and no bin
                    // entered the pipeline. Keep `enabled` at its
                    // previous value so the recorded state stays
                    // truthful — band edits have no chain to land on —
                    // and the next apply retries the enable.
                    effective.enabled = current.enabled;
                }
            } else if !self.uninstall_equalizer_bin() {
                // Deferred uninstall: the bin is still attached *with
                // its chain handle retained*. Keep `enabled` recorded —
                // persisting `false` here would make
                // `ensure_equalizer_installed` skip both the rebuild and
                // the removal, stranding an audio-processing bin until
                // restart — so the next apply retries the removal.
                effective.enabled = current.enabled;
                // The retained chain also still carries its limiter, so
                // a compound disable that also requested a clip change
                // must reconcile this field too. Recording the requested
                // value would persist a topology the graph never adopted
                // (or falsely promise protection that is absent), and
                // the retry — an `enabled_changed` apply, with
                // `clip_changed` forced false by the disabled request —
                // would land the requested state without ever touching
                // the limiter. Recording what the retained chain
                // actually carries keeps the persisted and user-visible
                // state truthful in both clip directions and lets the
                // retry complete the compound operation cleanly.
                if let Some(installed) = self.installed_clip_protection() {
                    effective.clip_protection = installed;
                }
            }
        } else if clip_changed && !self.toggle_clip_protection(next.clip_protection) {
            // The toggle was deferred (the edit never ran, so the chain
            // carries the previous protection) or the surgery failed
            // (the chain degraded to the no-limiter layout): record what
            // the installed chain *actually* carries instead of assuming
            // either endpoint — the persisted and user-visible state
            // stays truthful, and the next apply re-attempts cleanly.
            effective.clip_protection = self
                .installed_clip_protection()
                .unwrap_or(current.clip_protection);
        }

        // Refinery round 4 (PR 220 A2): deliver the gain delta per the
        // live-reconfiguration boundary — one changed property is one
        // direct write, several are one probe-delivered batch. The
        // fresh-install skip is the only skip: a deferred uninstall that
        // retained its chain still runs, because the retained chain
        // carries the *old* gains and the diff against the previously
        // recorded settings must land on it.
        let gains_applied =
            effective.enabled && !fresh_install && self.push_band_transaction(&current, &effective);

        {
            let mut state = self.eq_state.borrow_mut();
            state.settings = effective;
            // A user-driven apply clears an error retirement: the
            // operator explicitly chose this configuration, so the next
            // load must honor it again.
            state.retired = false;
        }
        self.schedule_equalizer_save();
        if gains_applied {
            // The aggregate settings-changed event (contract:
            // *Live-reconfiguration boundary*): exactly one emission per
            // delivered delta, carrying the applied snapshot.
            self.emit_equalizer_settings_applied();
        }
    }

    /// The clip-protection policy the installed chain actually carries,
    /// or `None` when no chain is installed.
    fn installed_clip_protection(&self) -> Option<equalizer::ClipProtection> {
        self.eq_state.borrow().chain.as_ref().map(|chain| {
            if chain.clip_protection_installed() {
                equalizer::ClipProtection::Soft
            } else {
                equalizer::ClipProtection::Off
            }
        })
    }

    /// Register the open equalizer panel's display-resync closure
    /// (refinery round 3, PR 220). The panel build path calls this
    /// through the [`AudioOutput`](crate::audio::output::AudioOutput)
    /// seam; the parked limiter edit's main-context poll invokes the
    /// closure when its reconciliation lands a recorded clip-protection
    /// value that differs from the previously recorded one, so the open
    /// panel re-reads the recorded state instead of keeping the walk-back
    /// it displayed while the edit was parked. Registration replaces any
    /// previous closure: the preferences dialog is rebuilt per
    /// presentation, so the newest open panel supersedes a stale one
    /// (whose widget references drop with the replaced closure).
    pub fn connect_equalizer_resync(&self, on_resync: Rc<dyn Fn()>) {
        self.eq_state.borrow_mut().panel_resync = Some(on_resync);
    }

    /// Register the equalizer module's aggregate settings-changed
    /// listener (refinery round 4, PR 220 A2; contract:
    /// *Live-reconfiguration boundary*). The listener is invoked exactly
    /// once per applied gain delta — a single-property direct write or
    /// one probe-delivered batch — with the applied `EqSettings`
    /// snapshot, so the UI re-renders from one coherent snapshot instead
    /// of chasing per-property GObject `notify` emissions. Registration
    /// replaces any previous listener, mirroring
    /// [`Player::connect_equalizer_resync`].
    // UI wiring is a deliberate follow-up (refinery round 4 scope covers
    // the engine-side event); the tests are the current consumer.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn connect_equalizer_settings_applied(&self, on_applied: EqSettingsAppliedListener) {
        self.eq_state.borrow_mut().settings_applied = Some(on_applied);
    }

    /// Invoke the registered settings-changed listener with the applied
    /// snapshot. Runs on the caller's (GTK main) thread with no engine
    /// borrow held, so the listener may re-read the recorded state
    /// through the output. No listener registered — the equalizer panel
    /// is closed — is the ordinary idle case.
    fn emit_equalizer_settings_applied(&self) {
        let (applied, listener) = {
            let state = self.eq_state.borrow();
            (state.settings, state.settings_applied.clone())
        };
        if let Some(listener) = listener {
            listener(&applied);
        }
    }

    /// Limiter-only topology change inside the installed bin (clip
    /// protection toggle while the equalizer stays enabled). Thin
    /// delegate to [`attempt_clip_protection_edit`] — the shared core
    /// also runs from the main-context adoption poll's re-issue (the
    /// closure cannot capture the `Player` by value, so the toggle
    /// machinery lives at module scope over `(playbin, eq_state,
    /// eq_test_seams)`).
    fn toggle_clip_protection(&self, protection: equalizer::ClipProtection) -> bool {
        attempt_clip_protection_edit(
            &self.playbin,
            &self.eq_state,
            &self.eq_test_seams,
            protection,
        )
    }

    /// Deliver the gain delta between the previously recorded settings
    /// and `next` onto the installed chain (refinery round 4, PR 220
    /// A2): one changed property is a direct single-property write, two
    /// or more are one probe-delivered batch (contract:
    /// *Live-reconfiguration boundary*). Returns whether any property
    /// was written.
    fn push_band_transaction(&self, previous: &EqSettings, next: &EqSettings) -> bool {
        let state = self.eq_state.borrow();
        match state.chain.as_ref() {
            Some(chain) => chain.apply_gain_delta(previous, next),
            None => false,
        }
    }

    /// Install a freshly built equalizer bin via the pause/relink seam,
    /// or directly when the pipeline is not running. Construction or
    /// negotiation failure degrades to the passthrough layout with a
    /// single informational diagnostic — never a half-inserted chain.
    ///
    /// Returns `true` when the attempt reached a confirmed topology:
    /// the bin installed (chain retained), or a build failure degraded
    /// to the documented passthrough (the next URI load retries the
    /// build, and an equalizer-originated pipeline error retires the
    /// chain). Returns `false` only when the seam deferred: no chain
    /// was recorded, the pipeline is untouched, and the caller must
    /// keep `enabled` at its previous value so the next apply retries.
    fn install_equalizer_bin(&self, settings: &EqSettings) -> bool {
        match equalizer::EqChain::build(settings) {
            Ok(chain) => {
                let bin = chain.bin.clone();
                let installed = self.with_pipeline_suspended(|| {
                    self.playbin.set_property("audio-filter", Some(&bin));
                    true
                });
                if installed {
                    self.eq_state.borrow_mut().chain = Some(chain);
                    info!(
                        enabled = settings.enabled,
                        preset = Preset::key(settings.preset),
                        "Equalizer chain installed into local pipeline"
                    );
                }
                installed
            }
            Err(error) => {
                self.eq_state.borrow_mut().chain = None;
                self.playbin
                    .set_property("audio-filter", Option::<&gst::Element>::None);
                info!(
                    error = %error,
                    "Equalizer unavailable; local output remains passthrough"
                );
                true
            }
        }
    }

    /// Remove the installed equalizer bin via the pause/relink seam.
    /// Persisted settings stay on disk untouched.
    ///
    /// The chain handle is cleared **inside the confirmed edit** — a
    /// deferred seam therefore leaves both the bin attached and its
    /// handle retained, and the caller keeps `enabled` recorded, so the
    /// removal is retried by the next apply instead of stranding an
    /// audio-processing bin whose rebuild path (`enabled = false` +
    /// `chain = None`) has been erased. Returns `true` when the
    /// pipeline is confirmed passthrough, `false` when the edit was
    /// deferred.
    fn uninstall_equalizer_bin(&self) -> bool {
        if self.eq_state.borrow().chain.is_none() {
            // Nothing recorded as installed: the pipeline is already in
            // the passthrough layout, so there is nothing to confirm.
            return true;
        }
        if self
            .eq_state
            .borrow()
            .chain
            .as_ref()
            .is_some_and(|chain| chain.has_pending_limiter_edit())
        {
            // A limiter edit is still in flight under its blocking probe
            // (refinery R1): pausing the pipeline and detaching the bin
            // now would be a conflicting edit over an in-flight,
            // unvalidated graph. Defer under the same discipline as any
            // other deferred uninstall — the recorded state keeps
            // `enabled`, and the next apply retries the removal.
            warn!(
                "Clip protection edit still in flight under its blocking probe; \
                 equalizer uninstall deferred"
            );
            return false;
        }
        let preset = Preset::key(self.eq_state.borrow().settings.preset);
        self.with_pipeline_suspended(|| {
            let mut state = self.eq_state.borrow_mut();
            let had_chain = state.chain.take().is_some();
            self.playbin
                .set_property("audio-filter", Option::<&gst::Element>::None);
            drop(state);
            if had_chain {
                info!(
                    enabled = false,
                    preset, "Equalizer chain removed from local pipeline"
                );
            }
            true
        })
    }

    /// Guarantee an installed bin before a fresh pipeline spins up (URI
    /// load). The bin persists across URI transitions — gapless album
    /// navigation keeps the same chain, and its property state is *not*
    /// re-applied automatically on each new URI. An error-retired chain
    /// is not rebuilt: the rollback must outlive the load that triggered
    /// it, until the user changes the equalizer setting.
    fn ensure_equalizer_installed(&self) {
        let (settings, needs_install) = {
            let state = self.eq_state.borrow();
            (
                state.settings,
                state.settings.enabled && state.chain.is_none() && !state.retired,
            )
        };
        if needs_install {
            self.install_equalizer_bin(&settings);
        }
    }

    /// Run one pipeline-topology edit through the pause → edit → resume
    /// seam. A running pipeline is paused and given a bounded window to
    /// settle; the edit runs **only once the state query confirms the
    /// pipeline reached `Paused`** — a slow or blocked transition never
    /// leaves the edit running against live data flow. A pipeline that
    /// misses the window gets its pending pause cancelled (back to
    /// `Playing`) and the edit is deferred: the caller reports failure
    /// and the next apply, or the next URI load's install seam, retries.
    /// A NULL pipeline (idle player) skips straight to the edit.
    ///
    /// A zero-timeout query that finds a *transition in flight* is also
    /// a defer: its `current` member reports the transition's *origin*
    /// state (e.g. `Paused` with `Playing` pending), which would read as
    /// "not playing" and run the edit without ever confirming a settled
    /// state — exactly the async-transition race the bounded window
    /// exists to prevent.
    fn with_pipeline_suspended<F: FnOnce() -> bool>(&self, edit: F) -> bool {
        suspend_pipeline_and_edit(&self.playbin, &self.eq_test_seams, edit)
    }

    /// Persist the equalizer state on the trailing edge of a change
    /// spell: every change re-arms the 750 ms timer, and only the newest
    /// generation actually writes. Every change-spell writes — including
    /// one whose result is exactly the fresh-install default state, so a
    /// user-visible reset can never be resurrected from a stale file.
    /// A transient unreadable `equalizer.cfg` suppresses the write until
    /// a subsequent read succeeds (see [`equalizer_persistence_allowed`]).
    fn schedule_equalizer_save(&self) {
        schedule_eq_save_for(&self.eq_state);
    }

    /// Shutdown flush: synchronously write the current state before the
    /// pipeline is retired, so quitting with a pending debounce never
    /// loses the last change. Suppressed while the on-disk file is
    /// unreadable, for the same reason the debounced writer is.
    ///
    /// Called from two places: the `Player::drop` fallback and — the
    /// authoritative path — the UI's normal close-request drain, via
    /// [`crate::audio::output::AudioOutput::flush_equalizer_for_shutdown`],
    /// while the output is still alive. The drain call matters because
    /// the process exits through `std::process::exit` after the GTK main
    /// loop unwinds, so `Drop` is not guaranteed to run at all.
    pub fn flush_equalizer_for_shutdown(&self) {
        let state = self.eq_state.borrow_mut();
        if !equalizer_persistence_allowed(&state) {
            return;
        }
        #[cfg(test)]
        if let Some(sink) = self.eq_flush_sink.borrow_mut().as_ref() {
            sink.borrow_mut().push(state.settings);
            return;
        }
        let _ = equalizer::save_equalizer_settings_to_disk(&state.settings);
    }

    // ── State / position queries ────────────────────────────────────

    /// Non-blocking query of the current playback state.
    ///
    /// Reachable only through `LocalOutput::state` (the trait impl),
    /// which itself currently has no production caller — the UI
    /// follows state via `PlayerEvent::StateChanged` instead. Keeping
    /// the method as part of `Player`'s API surface for future
    /// on-demand queries.
    #[allow(dead_code)]
    pub fn state(&self) -> PlayerState {
        let (_, current, _) = self.playbin.state(gst::ClockTime::ZERO);
        match current {
            gst::State::Playing => PlayerState::Playing,
            gst::State::Paused => PlayerState::Paused,
            _ => PlayerState::Stopped,
        }
    }

    /// Current playback position in milliseconds, or `None` if
    /// the pipeline is not in a queryable state.
    pub fn position_ms(&self) -> Option<u64> {
        self.playbin
            .query_position::<gst::ClockTime>()
            .map(|t| t.mseconds())
    }

    // ── Internal: bus watch ─────────────────────────────────────────

    /// Watch the pipeline bus for EOS, Error, and StateChanged messages.
    ///
    /// The watch callback runs on the glib main loop (main thread).
    #[allow(clippy::too_many_arguments)] // mirrors the Player field set the watch captures
    fn attach_bus_watch(
        playbin: &gst::Element,
        event_tx: &async_channel::Sender<PlayerEvent>,
        generation: PlayerEventGeneration,
        media_proxy: Arc<GstreamerMediaProxy>,
        media_ticket: Option<Arc<GstreamerMediaTicket>>,
        volume: Rc<Cell<f64>>,
        sink_recovery_claimed: Rc<Cell<bool>>,
        eq_state: Rc<RefCell<EqEngineState>>,
    ) -> anyhow::Result<gst::bus::BusWatchGuard> {
        let bus = playbin
            .bus()
            .ok_or_else(|| anyhow::anyhow!("playbin has no bus"))?;

        let tx = event_tx.clone();
        let playbin_name = playbin.name();
        let started_at = Instant::now();
        let playbin_for_eq = playbin.downgrade();
        #[cfg(any(target_os = "windows", test))]
        let playbin_for_recovery = playbin.downgrade();
        #[cfg(not(any(target_os = "windows", test)))]
        let _ = (&volume, &sink_recovery_claimed);

        bus.add_watch_local(move |_bus, msg| {
            use gst::MessageView;

            #[cfg(any(target_os = "windows", test))]
            if playbin_for_recovery.upgrade().is_some_and(|playbin| {
                windows_audio::recover_warning(msg, &playbin, volume.get(), &sink_recovery_claimed)
            }) {
                return glib::ControlFlow::Continue;
            }

            match msg.view() {
                MessageView::Eos(_) => {
                    if let Some(ticket) = media_ticket.as_ref() {
                        media_proxy.revoke_if_current(ticket);
                    }
                    info!("End of stream");
                    if let Err(e) = tx.try_send(PlayerEvent::ended(generation)) {
                        warn!(error = %e, "dropped TrackEnded event — UI consumer may be stalled");
                    }
                }

                MessageView::Error(pipeline_error) => {
                    if let Some(ticket) = media_ticket.as_ref() {
                        media_proxy.revoke_if_current(ticket);
                    }
                    // Only equalizer-originated GStreamer failures retire
                    // the equalizer chain (contract: *Filter graph*): an
                    // error whose source lies inside `eq-bin` — such as a
                    // non-PCM source that cannot deliver the pinned F32LE
                    // stereo caps — rolls the bin back so every subsequent
                    // load stays in the passthrough layout instead of
                    // rebuilding the same failing bin. Errors from the
                    // decoder, demuxer, network, or audio sink are unrelated
                    // playback failures: the bin stays installed and the
                    // equalizer state is untouched. The retirement is
                    // honored by `ensure_equalizer_installed` until the
                    // user changes the equalizer setting (or restarts).
                    if eq_bin_originated(msg) {
                        if let Some(playbin) = playbin_for_eq.upgrade() {
                            let mut state = eq_state.borrow_mut();
                            state.chain = None;
                            state.retired = true;
                            playbin.set_property("audio-filter", Option::<&gst::Element>::None);
                            info!(
                                "Equalizer chain retired after an equalizer-originated \
                                 pipeline error; passthrough restored for subsequent loads"
                            );
                        }
                    }
                    // GStreamer error/debug strings can retain the complete
                    // authenticated source URI. Record only closed categories
                    // and numeric codes; never inspect message/debug/details.
                    let error_value = pipeline_error.error();
                    let source_category = pipeline_error_source_category(msg);
                    let elapsed_ms =
                        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
                    error!(
                        protected = media_ticket.is_some(),
                        domain = pipeline_error_domain(&error_value),
                        code = error_value.code(),
                        source_category = source_category.as_str(),
                        elapsed_ms,
                        "Audio pipeline error"
                    );
                    if let Err(e) =
                        tx.try_send(PlayerEvent::error(generation, source_category.ui_message()))
                    {
                        warn!(error = %e, "dropped Error event — UI consumer may be stalled");
                    }
                    return glib::ControlFlow::Break;
                }

                MessageView::StateChanged(sc) => {
                    // Only react to the playbin's own transitions,
                    // not those of child elements (decoders, sinks, …).
                    let is_playbin = msg.src().is_some_and(|src| src.name() == playbin_name);

                    if is_playbin {
                        let new_state = match sc.current() {
                            gst::State::Playing => PlayerState::Playing,
                            gst::State::Paused => PlayerState::Paused,
                            _ => PlayerState::Stopped,
                        };
                        debug!(
                            old = ?sc.old(),
                            new = ?sc.current(),
                            pending = ?sc.pending(),
                            "Pipeline state changed"
                        );
                        let _ = tx.try_send(PlayerEvent::state(generation, new_state));
                    }
                }

                MessageView::Buffering(buffering) => {
                    let percent = buffering.percent();
                    debug!(percent, "Buffering");
                    if percent < 100 {
                        let _ = tx.try_send(PlayerEvent::state(generation, PlayerState::Buffering));
                    }
                    // When buffering reaches 100%, GStreamer will emit a
                    // StateChanged → Playing message, so we don't need to
                    // send Playing here.
                }

                _ => {}
            }

            glib::ControlFlow::Continue
        })
        .map_err(|e| anyhow::anyhow!("Failed to add bus watch: {e}"))
    }

    /// Publish one coherent terminal sequence for a URI rejected before it can
    /// reach GStreamer. The supplied message is already fixed and URL-free.
    fn emit_load_failure(&self, generation: PlayerEventGeneration, message: String) {
        let _ = self
            .event_tx
            .try_send(PlayerEvent::state(generation, PlayerState::Buffering));
        let _ = self
            .event_tx
            .try_send(PlayerEvent::error(generation, message));
        let _ = self
            .event_tx
            .try_send(PlayerEvent::state(generation, PlayerState::Stopped));
    }

    // ── Internal: position polling ──────────────────────────────────

    /// Start a 500 ms timer that queries the pipeline position while
    /// playing and sends [`PlayerEvent::PositionChanged`].
    ///
    /// The timer self-cancels when the playbin is dropped (weak ref).
    fn start_position_timer(
        playbin: &gst::Element,
        event_tx: &async_channel::Sender<PlayerEvent>,
        event_generation: Rc<Cell<PlayerEventGeneration>>,
    ) {
        let playbin_weak = playbin.downgrade();
        let tx = event_tx.clone();

        glib::timeout_add_local(Duration::from_millis(500), move || {
            let Some(playbin) = playbin_weak.upgrade() else {
                return glib::ControlFlow::Break;
            };

            // Non-blocking check — only query when actually playing.
            let (_, state, _) = playbin.state(gst::ClockTime::ZERO);
            if state == gst::State::Playing {
                if let Some(pos) = playbin.query_position::<gst::ClockTime>() {
                    // Duration may be unknown for live streams (radio).
                    // Send 0 for duration_ms so the UI can still update
                    // the elapsed time label and clear the buffering spinner.
                    let dur = playbin
                        .query_duration::<gst::ClockTime>()
                        .map(|d| d.mseconds())
                        .unwrap_or(0);
                    let _ = tx.try_send(PlayerEvent::position(
                        event_generation.get(),
                        pos.mseconds(),
                        dur,
                    ));
                }
            }

            glib::ControlFlow::Continue
        });
    }

    // ── Internal: Windows plugin path ───────────────────────────────

    /// Force Tributary's own loopback media tickets to stay off ambient HTTP
    /// proxies. The callback is emitted on a GStreamer streaming thread, so it
    /// intentionally captures no GTK/Rc state.
    pub(super) fn install_loopback_http_source_policy(playbin: &gst::Element) {
        playbin.connect("source-setup", false, |args| {
            let source = args.get(1)?.get::<gst::Element>().ok()?;
            let location = source
                .find_property("location")
                .and_then(|_| source.property_value("location").get::<String>().ok());

            if !location
                .as_deref()
                .is_some_and(is_protected_loopback_ticket_uri)
            {
                return None;
            }

            if configure_protected_loopback_source(&source) {
                debug!("Protected loopback HTTP source forced to direct routing");
            } else {
                // A protected ticket must never fall back to a system proxy.
                // Publish a fixed bus error, then lock the source in NULL so
                // its parent cannot open the URI or wait indefinitely.
                gst::element_error!(
                    source,
                    gst::ResourceError::Settings,
                    ("Protected loopback routing unavailable")
                );
                source.set_locked_state(true);
                let _ = source.set_state(gst::State::Null);
                error!("Protected loopback HTTP source could not enforce direct routing");
            }

            None
        });
    }
}

/// Recognize only opaque HTTP tickets created by Tributary's dedicated local
/// media proxy. Ordinary loopback web/radio URLs keep their normal source
/// behavior, and non-loopback media may continue to use the user's proxy.
fn is_protected_loopback_ticket_uri(candidate: &str) -> bool {
    let Ok(url) = Url::parse(candidate) else {
        return false;
    };
    let loopback = matches!(
        url.host(),
        Some(Host::Ipv4(address)) if address.is_loopback()
    ) || matches!(
        url.host(),
        Some(Host::Ipv6(address)) if address.is_loopback()
    );
    let Some(route) = url.path().strip_prefix("/cast/") else {
        return false;
    };
    let (ticket_id, valid_extension) = match route.split_once('.') {
        Some((id, extension)) => (
            id,
            !extension.contains('.')
                && cast_http_server::PROTECTED_TICKET_AUDIO_EXTENSIONS.contains(&extension),
        ),
        None => (route, true),
    };
    let canonical_ticket_id = uuid::Uuid::parse_str(ticket_id)
        .is_ok_and(|ticket| ticket.hyphenated().to_string() == ticket_id);

    url.scheme() == "http"
        && loopback
        && url.port().is_some_and(|port| port != 0)
        && url.username().is_empty()
        && url.password().is_none()
        && !route.is_empty()
        && !route.contains('/')
        && canonical_ticket_id
        && valid_extension
        && url.query().is_none()
        && url.fragment().is_none()
}

/// Apply and verify the source properties that keep a protected ticket local.
/// The round-trip check makes an older or alternate HTTP plugin fail closed
/// instead of silently accepting a property value it cannot enforce.
fn configure_protected_loopback_source(source: &gst::Element) -> bool {
    let is_soup_http = source
        .factory()
        .is_some_and(|factory| factory.name() == "souphttpsrc");
    let required = ["proxy", "retries", "timeout"];
    if !is_soup_http
        || required
            .iter()
            .any(|property| source.find_property(property).is_none())
    {
        return false;
    }

    source.set_property("proxy", DIRECT_PROXY_SENTINEL);
    source.set_property("retries", 0_i32);
    source.set_property("timeout", PROTECTED_LOOPBACK_TIMEOUT_SECONDS);

    source
        .property_value("proxy")
        .get::<String>()
        .is_ok_and(|proxy| proxy.starts_with("direct:"))
        && source.property::<i32>("retries") == 0
        && source.property::<u32>("timeout") == PROTECTED_LOOPBACK_TIMEOUT_SECONDS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PipelineErrorSourceCategory {
    Network,
    Decoder,
    AudioOutput,
    Pipeline,
}

impl PipelineErrorSourceCategory {
    fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network-source",
            Self::Decoder => "decoder",
            Self::AudioOutput => "audio-sink",
            Self::Pipeline => "pipeline",
        }
    }

    fn ui_message(self) -> String {
        let locale = rust_i18n::locale();
        self.ui_message_for_locale(&locale)
    }

    fn ui_message_for_locale(self, locale: &str) -> String {
        match self {
            Self::Network => {
                rust_i18n::t!("errors.playback.network_request_failed", locale = locale)
            }
            Self::Decoder => rust_i18n::t!("errors.playback.decoder_failed", locale = locale),
            Self::AudioOutput => {
                rust_i18n::t!("errors.playback.audio_output_failed", locale = locale)
            }
            Self::Pipeline => rust_i18n::t!("errors.playback.playback_failed", locale = locale),
        }
        .into_owned()
    }
}

fn pipeline_error_source_category(message: &gst::MessageRef) -> PipelineErrorSourceCategory {
    let Some(element) = message
        .src()
        .and_then(|source| source.downcast_ref::<gst::Element>())
    else {
        return PipelineErrorSourceCategory::Pipeline;
    };
    let Some(klass) = element
        .factory()
        .and_then(|factory| factory.metadata("klass").map(str::to_owned))
    else {
        return PipelineErrorSourceCategory::Pipeline;
    };

    pipeline_error_source_category_from_klass(&klass)
}

/// Whether a bus message originates inside the equalizer filter bin:
/// the message source — an element or a pad — is `eq-bin` itself or an
/// object parented into it. This is the contract's only retirement
/// trigger; the source walk needs no message text, so the privacy rule
/// (never inspect error/debug strings) is preserved.
fn eq_bin_originated(message: &gst::MessageRef) -> bool {
    const EQ_BIN_NAME: &str = "eq-bin";
    const MAX_SOURCE_DEPTH: usize = 12;
    // The walk owns each step: `parent()` hands back a fresh owned
    // reference, so the loop carries `Option<gst::Object>` instead of
    // a borrow into the previous step's handle.
    let mut origin = message.src().cloned();
    for _ in 0..MAX_SOURCE_DEPTH {
        let Some(object) = origin else {
            return false;
        };
        if object.name() == EQ_BIN_NAME {
            return true;
        }
        origin = object.parent();
    }
    false
}

fn pipeline_error_source_category_from_klass(klass: &str) -> PipelineErrorSourceCategory {
    if klass.contains("Network") && klass.contains("Source") {
        PipelineErrorSourceCategory::Network
    } else if klass.contains("Decoder") || klass.contains("Demuxer") || klass.contains("Parser") {
        PipelineErrorSourceCategory::Decoder
    } else if klass.contains("Audio") && klass.contains("Sink") {
        PipelineErrorSourceCategory::AudioOutput
    } else {
        PipelineErrorSourceCategory::Pipeline
    }
}

/// Map GStreamer's quark to a closed category. The underlying error message is
/// deliberately never read because it may retain the authenticated URI.
fn pipeline_error_domain(error: &glib::Error) -> &'static str {
    use glib::error::ErrorDomain;

    let domain = error.domain();
    if domain == gst::CoreError::domain() {
        "core"
    } else if domain == gst::LibraryError::domain() {
        "library"
    } else if domain == gst::ResourceError::domain() {
        "resource"
    } else if domain == gst::StreamError::domain() {
        "stream"
    } else {
        "other"
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        info!("Shutting down GStreamer pipeline");
        // Shutdown flush: persist the current equalizer state even if
        // the debounce timer is still armed, so quitting never loses
        // the last change. A transient unreadable file keeps this
        // suppressed, exactly like the debounced writer.
        self.flush_equalizer_for_shutdown();
        #[cfg(target_os = "macos")]
        drop(self.macos_audio_route.take());
        let _ = self.playbin.set_state(gst::State::Null);
    }
}

// ── Volume curve ────────────────────────────────────────────────────────

/// Convert a linear slider position (0.0–1.0) to a GStreamer pipeline
/// volume using a cubic curve.  This makes the quiet half of the slider
/// far more usable — without it, most of the perceptible range is
/// crammed into the top 20% of travel.
fn slider_to_pipeline(slider: f64) -> f64 {
    slider * slider * slider
}

// ── Volume persistence ──────────────────────────────────────────────────

/// Path to the volume state file: `<data_dir>/tributary/volume`
fn volume_path() -> Option<std::path::PathBuf> {
    crate::paths::data_dir().map(|d| d.join("tributary").join("volume"))
}

fn load_saved_volume() -> Option<f64> {
    let path = volume_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let v: f64 = text.trim().parse().ok()?;
    if (0.0..=1.0).contains(&v) {
        Some(v)
    } else {
        None
    }
}

fn save_volume(level: f64) {
    if let Some(path) = volume_path() {
        // Ensure the parent directory exists (may not on first launch
        // if the DB hasn't been initialised yet).
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, format!("{level:.3}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    const PROXY_BYPASS_CHILD: &str = "TRIBUTARY_PROXY_BYPASS_CHILD";
    const PROXY_BYPASS_CHILD_VALUE: &str = "tributary-proxy-bypass-child-v1";

    fn serve_one_test_request(
        listener: std::net::TcpListener,
        response: &'static [u8],
        observed: std::sync::mpsc::Sender<bool>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        timeout: Option<Duration>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            use std::io::{Read, Write};

            listener
                .set_nonblocking(true)
                .expect("set test listener nonblocking");
            let deadline = timeout.map(|timeout| Instant::now() + timeout);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut request = [0_u8; 4096];
                        let _ = stream.read(&mut request);
                        let _ = stream.write_all(response);
                        let _ = stream.flush();
                        let _ = observed.send(true);
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if stop.load(std::sync::atomic::Ordering::Acquire) {
                            break;
                        }
                        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            let _ = observed.send(false);
        })
    }

    fn run_proxy_bypass_child() {
        let target = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("bind target listener");
        let target_addr = target.local_addr().expect("target listener address");
        let ticket = format!("http://{target_addr}/cast/550e8400-e29b-41d4-a716-446655440000.flac");

        gst::init().expect("GStreamer init");
        let source = gst::ElementFactory::make("souphttpsrc")
            .build()
            .expect("packaged souphttpsrc");
        let sink = gst::ElementFactory::make("fakesink")
            .build()
            .expect("GStreamer fakesink");
        let playbin = gst::ElementFactory::make("playbin3")
            .build()
            .or_else(|_| gst::ElementFactory::make("playbin").build())
            .expect("GStreamer playbin");

        source.set_property("location", &ticket);
        Player::install_loopback_http_source_policy(&playbin);
        playbin.emit_by_name::<()>("source-setup", &[&source]);
        assert!(source.property::<String>("proxy").starts_with("direct:"));
        // Keep a broken-policy child bounded independently of the production
        // 30-second downstream budget.
        source.set_property("timeout", 2_u32);

        let pipeline = gst::Pipeline::new();
        pipeline
            .add_many([&source, &sink])
            .expect("assemble proxy bypass pipeline");
        source.link(&sink).expect("link proxy bypass pipeline");

        // Start the bounded observation window only after process startup,
        // GStreamer initialization, and plugin discovery have completed.
        // Those operations can exceed several seconds on a cold Windows host.
        let (target_tx, target_rx) = std::sync::mpsc::channel();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let target_thread = serve_one_test_request(
            target,
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\ntest",
            target_tx,
            std::sync::Arc::clone(&stop),
            Some(Duration::from_secs(8)),
        );

        pipeline
            .set_state(gst::State::Playing)
            .expect("start proxy bypass pipeline");
        let bus = pipeline.bus().expect("proxy bypass pipeline bus");
        // The target observation, not the terminal message kind, proves the
        // request reached the intended fixture. The parent process separately
        // proves that the poisoned proxy was never contacted.
        // Some packaged source/plugin combinations report a downstream error
        // after the complete HTTP body has already reached `fakesink`; treating
        // that as proxy use made this security regression flaky on Windows.
        let _terminal = bus
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("proxy bypass pipeline reaches a terminal state");
        let _ = pipeline.set_state(gst::State::Null);

        // Do not cancel the target listener until it has recorded the route (or
        // exhausted its own deadline). This avoids racing an accepted request
        // during Windows process/thread teardown.
        let target_observed = target_rx
            .recv_timeout(Duration::from_secs(9))
            .expect("target listener result");
        stop.store(true, std::sync::atomic::Ordering::Release);
        target_thread.join().expect("target listener thread");

        assert!(
            target_observed,
            "the loopback media fixture was not reached"
        );
    }

    // ── slider_to_pipeline tests ────────────────────────────────────

    #[test]
    fn test_slider_to_pipeline_zero() {
        assert!((slider_to_pipeline(0.0) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_slider_to_pipeline_one() {
        assert!((slider_to_pipeline(1.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_slider_to_pipeline_half() {
        // 0.5^3 = 0.125
        assert!((slider_to_pipeline(0.5) - 0.125).abs() < 1e-10);
    }

    #[test]
    fn test_slider_to_pipeline_monotonic() {
        // The cubic curve should be monotonically increasing.
        let mut prev = slider_to_pipeline(0.0);
        for i in 1..=100 {
            let val = slider_to_pipeline(i as f64 / 100.0);
            assert!(val >= prev, "slider_to_pipeline should be monotonic");
            prev = val;
        }
    }

    // ── Volume persistence helpers ──────────────────────────────────

    #[test]
    fn test_volume_path_returns_some() {
        // On any system with a data directory, this should return Some.
        // (May fail in extremely minimal CI environments.)
        let path = volume_path();
        if let Some(p) = path {
            assert!(p.to_string_lossy().contains("tributary"));
            assert!(p.to_string_lossy().contains("volume"));
        }
    }

    // ── Cross-platform user-state isolation (tr-cy381) ──────────────

    /// Child marker for [`production_state_resolution_ignores_an_external_user_state_file`].
    const USER_STATE_ISOLATION_CHILD: &str = "TRIBUTARY_USER_STATE_ISOLATION_CHILD";
    const USER_STATE_ISOLATION_CHILD_VALUE: &str = "tributary-user-state-isolation-child-v1";
    const USER_STATE_ISOLATION_TEST: &str =
        "audio::tests::production_state_resolution_ignores_an_external_user_state_file";

    /// Seed `<root>/tributary/volume` with `value` and return its path.
    fn seed_user_state_volume(root: &std::path::Path, value: &str) -> std::path::PathBuf {
        let volume = root.join("tributary").join("volume");
        std::fs::create_dir_all(volume.parent().expect("state parent")).expect("create state dir");
        std::fs::write(&volume, value).expect("seed volume");
        volume
    }

    /// Spawn this test executable as the isolated user-state child, pointing
    /// the test-scoped sandbox redirect at `sandbox` and `HOME`/`XDG_*` at the
    /// distinct `external` tree, then capture its output.
    fn run_user_state_isolation_child(
        sandbox: &std::path::Path,
        external: &std::path::Path,
    ) -> std::process::Output {
        std::process::Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                USER_STATE_ISOLATION_TEST,
                "--nocapture",
                "--test-threads=1",
            ])
            .env(USER_STATE_ISOLATION_CHILD, USER_STATE_ISOLATION_CHILD_VALUE)
            .env(crate::paths::TEST_USER_STATE_DIR_ENV, sandbox)
            .env("HOME", external)
            .env("XDG_DATA_HOME", external)
            .env("XDG_CONFIG_HOME", external)
            .env("XDG_CACHE_HOME", external)
            .env("XDG_STATE_HOME", external)
            .output()
            .expect("run isolated user-state child")
    }

    /// Behavioral regression for tr-cy381: run the *production* user-state
    /// resolution ([`volume_path`] + [`load_saved_volume`]) in a separate
    /// test process and prove it reads the sandbox, not an external
    /// user-state file seeded behind `HOME`/`XDG_*`.
    ///
    /// The redirect (`TRIBUTARY_TEST_USER_STATE_DIR`) is honored before
    /// `dirs::data_dir()`, so the same assertion holds on Windows, where
    /// `dirs` resolves known folders through `SHGetKnownFolderPath` and
    /// ignores `HOME`/`XDG_*`. A `HOME`/`XDG`-only sandbox would fail this
    /// test on Windows by reading the external seed.
    #[test]
    fn production_state_resolution_ignores_an_external_user_state_file() {
        if std::env::var(USER_STATE_ISOLATION_CHILD).as_deref()
            == Ok(USER_STATE_ISOLATION_CHILD_VALUE)
        {
            let path = volume_path().expect("production volume path");
            let level = load_saved_volume().unwrap_or(1.0);
            println!("USER_STATE_PROBE path={} level={level}", path.display());
            return;
        }

        let sandbox = tempfile::tempdir().expect("sandbox root");
        let sandbox_volume = seed_user_state_volume(sandbox.path(), "0.250");

        // A distinct external tree the platform resolver would select if the
        // test-scoped redirect were absent (Linux/macOS via XDG/HOME).
        let external = tempfile::tempdir().expect("external root");
        seed_user_state_volume(external.path(), "0.750");

        let output = run_user_state_isolation_child(sandbox.path(), external.path());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "isolated user-state child failed: stdout={stdout} stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains(&format!("path={}", sandbox_volume.display())),
            "child resolved production state outside the sandbox: stdout={stdout}"
        );
        assert!(
            stdout.contains("level=0.25"),
            "child did not read the sandbox persistence value: stdout={stdout}"
        );
        assert!(
            !stdout.contains("0.75"),
            "child read an external user-state file: stdout={stdout}"
        );
    }

    #[test]
    fn only_opaque_tributary_loopback_tickets_receive_direct_routing() {
        let ticket = "550e8400-e29b-41d4-a716-446655440000.flac";
        assert!(is_protected_loopback_ticket_uri(&format!(
            "http://127.0.0.1:53123/cast/{ticket}"
        )));
        assert!(is_protected_loopback_ticket_uri(&format!(
            "http://[::1]:53123/cast/{ticket}"
        )));

        for rejected in [
            format!("https://127.0.0.1:53123/cast/{ticket}"),
            format!("http://192.168.1.5:53123/cast/{ticket}"),
            format!("http://127.0.0.1:53123/radio/{ticket}"),
            "http://127.0.0.1:53123/cast/not-a-ticket".to_string(),
            format!("http://127.0.0.1:53123/cast/{ticket}.exe"),
            format!("http://127.0.0.1:53123/cast/{ticket}.flac.exe"),
            format!("http://127.0.0.1:53123/cast/{ticket}.FLAC"),
            "http://127.0.0.1:53123/cast/550e8400e29b41d4a716446655440000.flac".to_string(),
            format!("http://user@127.0.0.1:53123/cast/{ticket}"),
            format!("http://127.0.0.1:53123/cast/{ticket}?forward=1"),
            format!("http://127.0.0.1:53123/cast/{ticket}#fragment"),
        ] {
            assert!(
                !is_protected_loopback_ticket_uri(&rejected),
                "non-ticket URI must retain normal proxy policy"
            );
        }
    }

    #[test]
    fn soup_source_policy_installs_and_verifies_a_direct_resolver() {
        gst::init().expect("GStreamer init");
        let Ok(source) = gst::ElementFactory::make("souphttpsrc").build() else {
            // Minimal development hosts may omit gst-plugins-good. Packaged
            // builds require it, and CI's package jobs exercise that contract.
            return;
        };

        source.set_property("proxy", "http://proxy.invalid:8080");
        source.set_property("retries", 3_i32);
        source.set_property("timeout", 15_u32);
        assert!(configure_protected_loopback_source(&source));
        assert!(source.property::<String>("proxy").starts_with("direct:"));
        assert_eq!(source.property::<i32>("retries"), 0);
        assert_eq!(
            source.property::<u32>("timeout"),
            PROTECTED_LOOPBACK_TIMEOUT_SECONDS
        );
    }

    #[test]
    fn source_setup_signal_overrides_a_poisoned_ticket_proxy_before_open() {
        gst::init().expect("GStreamer init");
        let Ok(playbin) = gst::ElementFactory::make("playbin3")
            .build()
            .or_else(|_| gst::ElementFactory::make("playbin").build())
        else {
            return;
        };
        let Ok(source) = gst::ElementFactory::make("souphttpsrc").build() else {
            return;
        };
        Player::install_loopback_http_source_policy(&playbin);
        source.set_property(
            "location",
            "http://127.0.0.1:54321/cast/550e8400-e29b-41d4-a716-446655440000.flac",
        );
        source.set_property("proxy", "http://192.0.2.1:3128");

        playbin.emit_by_name::<()>("source-setup", &[&source]);

        assert!(source.property::<String>("proxy").starts_with("direct:"));
        assert_eq!(source.property::<i32>("retries"), 0);
        assert_eq!(
            source.property::<u32>("timeout"),
            PROTECTED_LOOPBACK_TIMEOUT_SECONDS
        );
    }

    #[test]
    fn protected_loopback_source_bypasses_a_poisoned_ambient_proxy() {
        if std::env::var(PROXY_BYPASS_CHILD).as_deref() == Ok(PROXY_BYPASS_CHILD_VALUE) {
            run_proxy_bypass_child();
            return;
        }

        let poison = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("bind poison proxy listener");
        let poison_addr = poison.local_addr().expect("poison listener address");
        let proxy = format!("http://{poison_addr}");
        let (poison_tx, poison_rx) = std::sync::mpsc::channel();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // The proxy fixture is stop-driven rather than deadline-driven so cold
        // child startup and plugin discovery cannot make it disappear early.
        let poison_thread = serve_one_test_request(
            poison,
            b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            poison_tx,
            std::sync::Arc::clone(&stop),
            None,
        );

        let output =
            std::process::Command::new(std::env::current_exe().expect("current test executable"))
                .args([
                    "--exact",
                    "audio::tests::protected_loopback_source_bypasses_a_poisoned_ambient_proxy",
                    "--nocapture",
                ])
                .env(PROXY_BYPASS_CHILD, PROXY_BYPASS_CHILD_VALUE)
                .env("http_proxy", &proxy)
                .env("HTTP_PROXY", &proxy)
                .env_remove("no_proxy")
                .env_remove("NO_PROXY")
                .output()
                .expect("run isolated proxy bypass child");
        stop.store(true, std::sync::atomic::Ordering::Release);
        let poison_observed = poison_rx
            .recv_timeout(Duration::from_secs(9))
            .expect("poison listener result");
        poison_thread.join().expect("poison listener thread");

        assert!(
            !poison_observed,
            "the opaque loopback ticket reached the ambient proxy"
        );
        assert!(
            output.status.success(),
            "isolated GStreamer child failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn pipeline_diagnostics_use_closed_categories_and_fixed_ui_text() {
        assert_eq!(
            pipeline_error_source_category_from_klass("Source/Network"),
            PipelineErrorSourceCategory::Network
        );
        assert_eq!(
            pipeline_error_source_category_from_klass("Codec/Decoder/Audio"),
            PipelineErrorSourceCategory::Decoder
        );
        assert_eq!(
            pipeline_error_source_category_from_klass("Sink/Audio"),
            PipelineErrorSourceCategory::AudioOutput
        );
        assert_eq!(
            pipeline_error_source_category_from_klass("Generic/Bin"),
            PipelineErrorSourceCategory::Pipeline
        );

        let secret = "https://music.invalid/stream?token=must-not-escape";
        let error = glib::Error::new(gst::ResourceError::OpenRead, secret);
        assert_eq!(pipeline_error_domain(&error), "resource");
        for category in [
            PipelineErrorSourceCategory::Network,
            PipelineErrorSourceCategory::Decoder,
            PipelineErrorSourceCategory::AudioOutput,
            PipelineErrorSourceCategory::Pipeline,
        ] {
            assert!(!category.as_str().contains(secret));
            assert!(!category.ui_message().contains(secret));
        }
    }

    #[test]
    fn pipeline_error_messages_are_localized_for_every_catalog() {
        for category in [
            PipelineErrorSourceCategory::Network,
            PipelineErrorSourceCategory::Decoder,
            PipelineErrorSourceCategory::AudioOutput,
            PipelineErrorSourceCategory::Pipeline,
        ] {
            let english = category.ui_message_for_locale("en");
            assert!(!english.is_empty());

            for locale in rust_i18n::available_locales!() {
                let localized = category.ui_message_for_locale(&locale);
                assert!(!localized.is_empty(), "{locale} is empty for {category:?}");
                if locale != "en" {
                    assert_ne!(
                        localized, english,
                        "{locale} must not fall back to English for {category:?}"
                    );
                }
            }
        }
    }

    /// Build one error bus message whose source is the given object.
    fn error_message_from(source: &impl glib::object::IsA<gst::Object>) -> gst::Message {
        gst::message::Error::builder(gst::StreamError::Failed, "test failure")
            .src(source)
            .build()
    }

    /// Regression (contract acceptance 19): only errors originating
    /// inside `eq-bin` may retire the equalizer chain. Decoder, demuxer,
    /// network, and sink errors — whose sources live anywhere else — are
    /// unrelated playback failures and must not classify as equalizer
    /// failures, so the bin stays installed and the state untouched.
    #[test]
    fn only_errors_originating_inside_eq_bin_classify_as_equalizer_failures() {
        gst::init().expect("GStreamer init");
        let Ok(child) = gst::ElementFactory::make("fakesink").build() else {
            // fakesink ships with gst-plugins-base; skip only on a
            // broken development host.
            return;
        };
        let eq_bin = gst::Bin::with_name("eq-bin");
        eq_bin.add(&child).expect("add child to eq-bin fixture");

        // A child element inside the bin is equalizer-originated…
        assert!(eq_bin_originated(&error_message_from(&child)));
        // …as is one of its pads (pad → element → eq-bin walk)…
        let pad = child.static_pad("sink").expect("fakesink has a sink pad");
        assert!(eq_bin_originated(&error_message_from(&pad)));
        // …and so is a grandchild nested one bin deeper.
        let sub_bin = gst::Bin::new();
        let grandchild = gst::ElementFactory::make("fakesink")
            .build()
            .expect("fakesink available");
        sub_bin.add(&grandchild).expect("add grandchild");
        eq_bin.add(&sub_bin).expect("nest sub-bin");
        assert!(eq_bin_originated(&error_message_from(&grandchild)));

        // The bin itself posting is equalizer-originated.
        assert!(eq_bin_originated(&error_message_from(&eq_bin)));

        // An unrelated element (the usual decoder/sink/network case)
        // and a source-less message are not equalizer failures.
        let outsider = gst::ElementFactory::make("fakesink")
            .build()
            .expect("fakesink available");
        assert!(!eq_bin_originated(&error_message_from(&outsider)));
        let sourceless =
            gst::message::Error::builder(gst::StreamError::Failed, "no source").build();
        assert!(!eq_bin_originated(&sourceless));
    }

    /// Regression (contract acceptance 18, persistence half): a
    /// transient unreadable `equalizer.cfg` suppresses every persistence
    /// path — the debounced writer and the shutdown flush — until a
    /// subsequent read succeeds. Once readable, every change-spell
    /// writes, including one whose result is exactly the fresh-install
    /// default state: the contract has no default-state suppression.
    #[test]
    fn transient_unreadable_config_suppresses_every_persistence_path() {
        let mut state = EqEngineState::default();
        assert!(equalizer_persistence_allowed(&state));

        state.persistence_suppressed = true;
        assert!(!equalizer_persistence_allowed(&state));

        state.persistence_suppressed = false;
        state.settings = EqSettings::default();
        assert!(
            equalizer_persistence_allowed(&state),
            "a readable file must accept default-state writes"
        );
    }

    // ── Equalizer caller state discipline ───────────────────────────
    //
    // Regressions for the deferred/failed topology-edit paths: the
    // recorded settings must always describe the *installed* topology,
    // and every deferred edit must be retried by the next apply.

    /// Builds a bare `Player` around the given playbin and equalizer
    /// state. Only the equalizer surface is exercised; the event
    /// channel, media proxy, and bus watch are inert stand-ins.
    fn eq_test_player(playbin: gst::Element, eq_state: EqEngineState) -> Player {
        Player {
            #[cfg(target_os = "macos")]
            macos_audio_route: None,
            playbin,
            volume: Rc::new(Cell::new(1.0)),
            sink_recovery_claimed: Rc::new(Cell::new(false)),
            event_tx: async_channel::unbounded().0,
            media_proxy: Arc::new(GstreamerMediaProxy::new(None)),
            event_generation: Rc::new(Cell::new(PlayerEventGeneration(0))),
            volume_save_pending: Rc::new(Cell::new(None)),
            eq_state: Rc::new(RefCell::new(eq_state)),
            bus_watch: RefCell::new(None),
            #[cfg(target_os = "windows")]
            _windows_audio_route: None,
            eq_test_seams: Rc::new(RefCell::new(EqTestSeams::default())),
            eq_flush_sink: RefCell::new(None),
        }
    }

    fn eq_state_with(chain: Option<equalizer::EqChain>, settings: EqSettings) -> EqEngineState {
        EqEngineState {
            settings,
            chain,
            save_generation: 0,
            // Test players must never persist: the debounced writer and
            // the Drop shutdown flush both route through the real user
            // config path, and a test that applied `enabled = true`
            // would otherwise leave that file armed — later tests (the
            // protected-stream EOS child reads the same path) would then
            // run the equalizer install seam inside their playback
            // pipelines. Test discipline assertions are in-memory only;
            // on-disk persistence is the config module's tested concern.
            persistence_suppressed: true,
            retired: false,
            clip_swap_poll_armed: false,
            apply_generation: 0,
            pending_edit_generation: None,
            panel_resync: None,
            settings_applied: None,
        }
    }

    /// Whether the host provides the playbin and the plugins the
    /// equalizer bin needs, loading them exactly once per process.
    /// Minimal development hosts may omit gst-plugins-good; packaged
    /// builds require them (see the chain.rs tests for the contract).
    fn eq_engine_plugins_available() -> bool {
        static EQ_ENGINE_PLUGINS: OnceLock<bool> = OnceLock::new();
        *EQ_ENGINE_PLUGINS.get_or_init(|| {
            gst::init().is_ok()
                && gst::ElementFactory::make("playbin3")
                    .build()
                    .or_else(|_| gst::ElementFactory::make("playbin").build())
                    .is_ok()
                && gst::ElementFactory::make("equalizer-10bands")
                    .build()
                    .is_ok()
                && gst::ElementFactory::make("rglimiter").build().is_ok()
        })
    }

    fn eq_enabled_settings(clip: equalizer::ClipProtection) -> EqSettings {
        EqSettings {
            enabled: true,
            preset: equalizer::Preset::Flat,
            preamp_db: 0.0,
            bands_db: [0.0; 10],
            clip_protection: clip,
        }
    }

    fn eq_test_playbin() -> gst::Element {
        gst::ElementFactory::make("playbin3")
            .build()
            .or_else(|_| gst::ElementFactory::make("playbin").build())
            .expect("playbin for eq caller-discipline test")
    }

    fn installed_audio_filter(playbin: &gst::Element) -> Option<gst::Element> {
        playbin.property::<Option<gst::Element>>("audio-filter")
    }

    /// Serializes every caller-discipline test that arms glib timers on
    /// the global default main context (`apply_serialized`) or drives
    /// its dispatch loop (the pending-completion adoption test): the
    /// context can be owned by only one thread at a time, and parallel
    /// acquire/attach otherwise races.
    static CONTEXT_DRIVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Apply serialized across the test threads: the trailing-edge
    /// save attaches a glib timeout source to the *global* default main
    /// context (production does this once on the main GTK thread),
    /// and parallel attaches race the context acquire. Only these
    /// caller-discipline tests touch the context, so one test-local
    /// mutex around `apply` is enough to keep them deterministic.
    fn apply_serialized(player: &Player, next: EqSettings) {
        let _guard = CONTEXT_DRIVE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        player.apply_equalizer_settings(next);
    }

    /// Regression (review finding r3985265728 CRITICAL): a deferred
    /// disable must not strand the bin. The chain handle is cleared
    /// only inside the confirmed edit, and `enabled` stays recorded, so
    /// the persisted state stays truthful and the next apply retries
    /// the removal — the bin can never become unremovable-until-restart.
    #[test]
    fn deferred_uninstall_keeps_chain_and_enabled_and_a_later_apply_retries() {
        if !eq_engine_plugins_available() {
            return;
        }
        let playbin = eq_test_playbin();
        let settings = eq_enabled_settings(equalizer::ClipProtection::Off);
        let chain = equalizer::EqChain::build(&settings).expect("chain builds");
        // Seed the installed topology: a previously confirmed install
        // left the bin attached at `audio-filter`.
        playbin.set_property("audio-filter", Some(&chain.bin));
        let player = eq_test_player(playbin.clone(), eq_state_with(Some(chain), settings));

        let next = EqSettings {
            enabled: false,
            ..settings
        };
        // Defer the seam: the edit closure never runs.
        player.eq_test_seams.borrow_mut().seam = Some(Box::new(|_edit| false));
        apply_serialized(&player, next);

        {
            let state = player.eq_state.borrow();
            assert!(
                state.settings.enabled,
                "a deferred uninstall must keep `enabled` recorded so a later apply retries"
            );
            assert!(
                state.chain.is_some(),
                "the chain handle must survive a deferred uninstall"
            );
        }
        assert!(
            installed_audio_filter(&playbin).is_some(),
            "the bin must still be attached after a deferred uninstall"
        );

        // Retry with the real seam (a NULL playbin confirms directly).
        apply_serialized(&player, next);
        {
            let state = player.eq_state.borrow();
            assert!(!state.settings.enabled);
            assert!(
                state.chain.is_none(),
                "the retried removal must clear the chain handle"
            );
        }
        assert!(
            installed_audio_filter(&playbin).is_none(),
            "the retried removal must detach the bin"
        );
    }

    /// Regression (refinery R2, PR 220 audit): a compound reload that
    /// disables the equalizer AND requests a clip change while the pause
    /// seam defers must reconcile every topology-dependent field against
    /// the retained chain, not only `enabled`. With an installed Soft
    /// limiter, recording the requested Off persisted a topology the
    /// retained graph never adopted; the retry is an `enabled_changed`
    /// apply (`clip_changed` is forced false by the disabled request),
    /// so nothing but the recorded truth can drive the limiter back onto
    /// the removal path.
    #[test]
    fn compound_deferred_disable_records_the_retained_soft_limiter_and_retries() {
        if !eq_engine_plugins_available() {
            return;
        }
        let playbin = eq_test_playbin();
        let settings = eq_enabled_settings(equalizer::ClipProtection::Soft);
        let chain = equalizer::EqChain::build(&settings).expect("chain builds");
        playbin.set_property("audio-filter", Some(&chain.bin));
        let player = eq_test_player(playbin.clone(), eq_state_with(Some(chain), settings));

        let next = EqSettings {
            enabled: false,
            clip_protection: equalizer::ClipProtection::Off,
            ..settings
        };
        player.eq_test_seams.borrow_mut().seam = Some(Box::new(|_edit| false));
        apply_serialized(&player, next);

        {
            let state = player.eq_state.borrow();
            assert!(
                state.settings.enabled,
                "a deferred uninstall must keep `enabled` recorded"
            );
            assert_eq!(
                state.settings.clip_protection,
                equalizer::ClipProtection::Soft,
                "the compound defer must record the retained Soft limiter, not the requested Off"
            );
            let chain = state.chain.as_ref().expect("chain retained on defer");
            assert!(chain.clip_protection_installed());
        }
        assert!(installed_audio_filter(&playbin).is_some());

        // The retry (real seam) completes the compound operation: the bin
        // is removed and the requested Disabled/Off state is recorded.
        apply_serialized(&player, next);
        {
            let state = player.eq_state.borrow();
            assert!(!state.settings.enabled);
            assert_eq!(
                state.settings.clip_protection,
                equalizer::ClipProtection::Off
            );
            assert!(state.chain.is_none());
        }
        assert!(installed_audio_filter(&playbin).is_none());
    }

    /// Regression (refinery R2, inverse clip direction): with a retained
    /// chain carrying no limiter, a compound deferred disable must record
    /// Off rather than the requested Soft — the requested value would
    /// falsely promise clip protection the installed graph does not
    /// provide.
    #[test]
    fn compound_deferred_disable_records_the_retained_off_layout_and_retries() {
        if !eq_engine_plugins_available() {
            return;
        }
        let playbin = eq_test_playbin();
        let settings = eq_enabled_settings(equalizer::ClipProtection::Off);
        let chain = equalizer::EqChain::build(&settings).expect("chain builds");
        playbin.set_property("audio-filter", Some(&chain.bin));
        let player = eq_test_player(playbin.clone(), eq_state_with(Some(chain), settings));

        let next = EqSettings {
            enabled: false,
            clip_protection: equalizer::ClipProtection::Soft,
            ..settings
        };
        player.eq_test_seams.borrow_mut().seam = Some(Box::new(|_edit| false));
        apply_serialized(&player, next);

        {
            let state = player.eq_state.borrow();
            assert!(state.settings.enabled);
            assert_eq!(
                state.settings.clip_protection,
                equalizer::ClipProtection::Off,
                "the compound defer must record the retained no-limiter layout, not the requested Soft"
            );
            let chain = state.chain.as_ref().expect("chain retained on defer");
            assert!(!chain.clip_protection_installed());
        }
        assert!(installed_audio_filter(&playbin).is_some());

        apply_serialized(&player, next);
        {
            let state = player.eq_state.borrow();
            assert!(!state.settings.enabled);
            assert_eq!(
                state.settings.clip_protection,
                equalizer::ClipProtection::Soft
            );
            assert!(state.chain.is_none());
        }
        assert!(installed_audio_filter(&playbin).is_none());
    }

    /// Parked-transaction fixture: a playing player whose installed chain
    /// carries a pending (engaged, unpublished) limiter edit for
    /// `requested`, as if its surgery had engaged and outlived the
    /// caller's engagement window. Extracted so the transaction tests
    /// stay within their method-length budgets (Codacy, PR 220 head
    /// cea20fe); behavior is unchanged.
    fn eq_player_with_pending_clip_edit(requested: equalizer::ClipProtection) -> Player {
        let playbin = eq_test_playbin();
        let settings = eq_enabled_settings(equalizer::ClipProtection::Soft);
        let chain = equalizer::EqChain::build(&settings).expect("chain builds");
        playbin.set_property("audio-filter", Some(&chain.bin));
        let player = eq_test_player(playbin, eq_state_with(Some(chain), settings));
        player.eq_test_seams.borrow_mut().playing = Some(true);
        // Park a removal transaction as if its surgery had engaged and
        // outlived the caller's engagement window.
        player
            .eq_state
            .borrow_mut()
            .chain
            .as_mut()
            .expect("chain installed")
            .inject_pending_limiter_edit(requested);
        player
    }

    /// Publish `installed` as the outcome of the player's parked limiter
    /// edit, if one is parked. Extracted so the transaction tests stay
    /// within their method-length budgets (Codacy, PR 220 head cea20fe);
    /// behavior is unchanged.
    fn complete_pending_edit(player: &Player, installed: bool) -> bool {
        player
            .eq_state
            .borrow_mut()
            .chain
            .as_mut()
            .expect("chain retained")
            .complete_pending_limiter_edit(installed)
    }

    /// Drive the global default main context — under `CONTEXT_DRIVE_LOCK`,
    /// exactly as the UI loop would — until `done` observes the settled
    /// condition on the player's EQ state, or the bounded deadline
    /// passes. Returns whether the condition was observed. Extracted so
    /// the transaction tests stay within their method-length budgets
    /// (Codacy, PR 220 head cea20fe); behavior is unchanged.
    fn drive_context_until_eq_state(
        player: &Player,
        done: impl Fn(&EqEngineState) -> bool,
    ) -> bool {
        use std::time::{Duration, Instant};

        let context = glib::MainContext::default();
        let _guard = CONTEXT_DRIVE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        context
            .with_thread_default(|| {
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    context.iteration(false);
                    if done(&player.eq_state.borrow()) {
                        return true;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                false
            })
            .unwrap_or(false)
    }

    /// Regression (refinery R1, PR 220 audit): a clip-protection toggle
    /// refused because a transaction is parked must record the PRE-EDIT
    /// truth, never the unconfirmed result, and skip the pause/relink
    /// fallback over the in-flight graph; when the parked surgery then
    /// publishes its outcome, the main-context poll adopts it and
    /// reconciles the recorded state to the settled truth through the
    /// trailing-edge debounce. No pause/relink fallback may run over the
    /// in-flight graph.
    #[test]
    fn pending_clip_swap_records_pre_edit_truth_then_reconciles_on_completion() {
        use equalizer::ClipProtection;

        if !eq_engine_plugins_available() {
            return;
        }
        let player = eq_player_with_pending_clip_edit(ClipProtection::Off);

        // The user requests the same toggle. The pending guard must
        // refuse the dynamic edit, skip the pause/relink fallback
        // entirely, and record the pre-edit truth while in flight.
        apply_serialized(&player, eq_enabled_settings(ClipProtection::Off));
        {
            let state = player.eq_state.borrow();
            assert!(state.settings.enabled);
            assert_eq!(
                state.settings.clip_protection,
                ClipProtection::Soft,
                "the recorded protection must stay at the pre-edit truth while the edit is parked"
            );
            let chain = state.chain.as_ref().expect("chain retained");
            assert!(chain.has_pending_limiter_edit());
            assert!(
                chain.clip_protection_installed(),
                "the parked transaction reports the pre-edit truth: the limiter is still routed"
            );
            assert!(!chain.topology_wedged());
        }

        // The callback finishes its surgery and publishes the outcome;
        // the main-context poll adopts it and reconciles the recorded
        // state.
        let accepted = complete_pending_edit(&player, true);
        assert!(accepted, "the parked transaction accepted its publication");
        let adopted = drive_context_until_eq_state(&player, |state| {
            state.settings.clip_protection == ClipProtection::Off
        });
        assert!(adopted, "the parked completion was never adopted");
        {
            let state = player.eq_state.borrow();
            assert_eq!(
                state.settings.clip_protection,
                ClipProtection::Off,
                "the settled truth is reconciled on the main context"
            );
            let chain = state.chain.as_ref().expect("chain retained");
            assert!(!chain.has_pending_limiter_edit());
            assert!(
                !chain.clip_protection_installed(),
                "the adopted removal left the graph without the limiter"
            );
            assert!(!chain.topology_wedged());
        }
    }

    /// Regression (refinery round 3, PR 220): when the main-context poll
    /// adopts a parked limiter edit whose settled truth differs from the
    /// previously recorded clip protection, the panel-registered resync
    /// must fire exactly once, so an already-open panel re-reads the
    /// recorded state instead of keeping the walk-back it displayed while
    /// the edit was parked.
    #[test]
    fn adopted_clip_swap_notifies_the_panel_resync_when_the_recorded_value_changes() {
        use equalizer::ClipProtection;

        if !eq_engine_plugins_available() {
            return;
        }
        let player = eq_player_with_pending_clip_edit(ClipProtection::Off);
        // The user's Off toggle parks across the engagement window: the
        // recorded state stays at the pre-edit Soft (the walk-back the
        // open panel displays).
        apply_serialized(&player, eq_enabled_settings(ClipProtection::Off));
        let resyncs = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let hits = std::rc::Rc::clone(&resyncs);
        player.connect_equalizer_resync(std::rc::Rc::new(move || {
            hits.set(hits.get() + 1);
        }));

        // The parked surgery succeeds: the recorded value moves Soft →
        // Off on the main context, and the resync fires for it.
        let accepted = complete_pending_edit(&player, true);
        assert!(accepted, "the parked transaction accepted its publication");
        let adopted = drive_context_until_eq_state(&player, |state| {
            state.settings.clip_protection == ClipProtection::Off
        });
        assert!(adopted, "the parked completion was never adopted");
        assert_eq!(
            resyncs.get(),
            1,
            "the moved recorded value must notify the panel resync exactly once"
        );
    }

    /// Regression (refinery round 3, PR 220), the echo-safety control:
    /// an adoption landing the already-recorded value — a rollback
    /// restoring the pre-edit layout — must notify nothing. The panel
    /// already displays that value; a notification would be the mirror
    /// of the apply handlers' echo-safety violation.
    #[test]
    fn adopted_clip_swap_stays_silent_when_the_recorded_value_is_unchanged() {
        use equalizer::ClipProtection;

        if !eq_engine_plugins_available() {
            return;
        }
        let player = eq_player_with_pending_clip_edit(ClipProtection::Off);
        apply_serialized(&player, eq_enabled_settings(ClipProtection::Off));
        let resyncs = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let hits = std::rc::Rc::clone(&resyncs);
        player.connect_equalizer_resync(std::rc::Rc::new(move || {
            hits.set(hits.get() + 1);
        }));

        // The parked surgery restores the pre-edit layout: the chain
        // keeps its limiter, so the reconciled Soft equals the recorded
        // Soft and the poll must stay silent.
        let accepted = complete_pending_edit(&player, false);
        assert!(accepted, "the parked transaction accepted its publication");
        let adopted = drive_context_until_eq_state(&player, |state| !state.clip_swap_poll_armed);
        assert!(adopted, "the parked completion was never adopted");
        {
            let state = player.eq_state.borrow();
            assert_eq!(
                state.settings.clip_protection,
                ClipProtection::Soft,
                "the rollback must leave the recorded pre-edit truth in place"
            );
        }
        assert_eq!(
            resyncs.get(),
            0,
            "an unchanged recorded value must not notify the panel resync"
        );
    }

    /// Regression (refinery round 4, PR 220 A2 — *Live-reconfiguration
    /// boundary*): applying a gain snapshot that changes a single band
    /// writes exactly that band property — once — and fires the module's
    /// aggregate settings-changed event exactly once, carrying a
    /// snapshot equal to the recorded state. No other property on the
    /// chain is rewritten, so a per-property panel listener observes one
    /// precise change instead of an eleven-property storm.
    #[test]
    fn single_band_apply_fires_one_property_write_and_one_settings_applied_event() {
        use std::cell::Cell;
        use std::rc::Rc;

        if !eq_engine_plugins_available() {
            return;
        }
        let playbin = eq_test_playbin();
        let settings = eq_enabled_settings(equalizer::ClipProtection::Off);
        let chain = equalizer::EqChain::build(&settings).expect("chain builds");
        playbin.set_property("audio-filter", Some(&chain.bin));
        let player = eq_test_player(playbin, eq_state_with(Some(chain), settings));

        let eq = player
            .eq_state
            .borrow()
            .chain
            .as_ref()
            .expect("chain installed")
            .bin
            .by_name("eq")
            .expect("the eq stage is a named child of the bin");
        let band_counts: Vec<_> = (0..10)
            .map(|index| {
                let count = Rc::new(Cell::new(0u32));
                let hits = Rc::clone(&count);
                eq.connect_notify_local(Some(&format!("band{index}")), move |_, _| {
                    hits.set(hits.get() + 1);
                });
                count
            })
            .collect();

        let applications = Rc::new(Cell::new(0u32));
        let snapshots: Rc<std::cell::RefCell<Vec<EqSettings>>> =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        player.connect_equalizer_settings_applied({
            let applications = Rc::clone(&applications);
            let snapshots = Rc::clone(&snapshots);
            Rc::new(move |applied| {
                applications.set(applications.get() + 1);
                snapshots.borrow_mut().push(*applied);
            })
        });

        let mut next = settings;
        next.bands_db[4] = -4.0;
        apply_serialized(&player, next);

        assert_eq!(
            applications.get(),
            1,
            "exactly one aggregate settings-changed event per applied gain delta"
        );
        assert_eq!(snapshots.borrow().len(), 1);
        {
            let state = player.eq_state.borrow();
            assert_eq!(snapshots.borrow()[0], state.settings);
            assert!((state.settings.bands_db[4] - (-4.0)).abs() < 1e-9);
        }
        assert_eq!(
            band_counts[4].get(),
            1,
            "band4 must be written exactly once"
        );
        for (index, count) in band_counts.iter().enumerate() {
            if index != 4 {
                assert_eq!(count.get(), 0, "band{index} must not be rewritten");
            }
        }
    }

    /// Regression (refinery round 4, PR 220 A2): a multi-property gain
    /// edit fires the aggregate settings-changed event exactly once —
    /// one event per user action, not one per written property. The
    /// batch itself is a full write-set (every gain property rewritten
    /// inside the one probe-delivered transaction), so the per-property
    /// assertion is one write each within that single batch.
    #[test]
    fn multi_band_apply_fires_the_settings_applied_event_exactly_once() {
        use std::cell::Cell;
        use std::rc::Rc;

        if !eq_engine_plugins_available() {
            return;
        }
        let playbin = eq_test_playbin();
        let settings = eq_enabled_settings(equalizer::ClipProtection::Off);
        let chain = equalizer::EqChain::build(&settings).expect("chain builds");
        playbin.set_property("audio-filter", Some(&chain.bin));
        let player = eq_test_player(playbin, eq_state_with(Some(chain), settings));

        let eq = player
            .eq_state
            .borrow()
            .chain
            .as_ref()
            .expect("chain installed")
            .bin
            .by_name("eq")
            .expect("the eq stage is a named child of the bin");
        let band_counts: Vec<_> = (0..10)
            .map(|index| {
                let count = Rc::new(Cell::new(0u32));
                let hits = Rc::clone(&count);
                eq.connect_notify_local(Some(&format!("band{index}")), move |_, _| {
                    hits.set(hits.get() + 1);
                });
                count
            })
            .collect();

        let applications = Rc::new(Cell::new(0u32));
        player.connect_equalizer_settings_applied({
            let applications = Rc::clone(&applications);
            Rc::new(move |_| {
                applications.set(applications.get() + 1);
            })
        });

        let mut next = settings;
        next.preamp_db = -6.0;
        next.bands_db[1] = -2.0;
        next.bands_db[5] = 4.0;
        next.bands_db[9] = 1.0;
        apply_serialized(&player, next);

        assert_eq!(
            applications.get(),
            1,
            "the whole batch must be one settings-changed event"
        );
        for (index, count) in band_counts.iter().enumerate() {
            assert_eq!(
                count.get(),
                1,
                "band{index} must be written exactly once within the single batch"
            );
        }
    }

    /// Regression (refinery round 4, PR 220 A1): a limiter edit parked
    /// under its blocking probe is STALE when its completion is adopted
    /// after a newer user apply. The adoption must supersede the stale
    /// outcome — recording the chain truth in memory only, persisting
    /// nothing, notifying nothing — and, because the loaded clip policy
    /// differs from that truth, reissue the loaded policy so the user's
    /// latest decision wins end to end: recorded state, persistence
    /// schedule, panel resync, and installed topology.
    #[test]
    fn stale_parked_clip_swap_completion_supersedes_and_reissues_the_loaded_policy() {
        use equalizer::ClipProtection;

        if !eq_engine_plugins_available() {
            return;
        }
        let player = eq_player_with_pending_clip_edit(ClipProtection::Off);

        // Apply #1: the user requests Off; the edit parks across the
        // engagement window (the chain refuses a second topology edit),
        // the pre-edit Soft stays recorded, and the parked edit's
        // generation is stamped for staleness detection.
        apply_serialized(&player, eq_enabled_settings(ClipProtection::Off));
        {
            let state = player.eq_state.borrow();
            assert_eq!(
                state.settings.clip_protection,
                ClipProtection::Soft,
                "the pre-edit truth stays recorded while the edit is parked"
            );
            assert!(state
                .chain
                .as_ref()
                .expect("chain retained")
                .has_pending_limiter_edit());
        }

        let resyncs = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let hits = std::rc::Rc::clone(&resyncs);
        player.connect_equalizer_resync(std::rc::Rc::new(move || {
            hits.set(hits.get() + 1);
        }));

        // Apply #2: a newer user action loads a snapshot with the clip
        // policy unchanged (Soft) — no new toggle, but the parked edit's
        // completion is now a stale outcome from an older generation.
        apply_serialized(&player, eq_enabled_settings(ClipProtection::Soft));

        // The stale completion settles: the parked Off surgery reports
        // the limiter removed. The adoption must not record Off.
        let accepted = complete_pending_edit(&player, true);
        assert!(accepted, "the parked transaction accepted its publication");
        let settled = drive_context_until_eq_state(&player, |state| {
            !state.clip_swap_poll_armed
                && !state
                    .chain
                    .as_ref()
                    .map(|chain| chain.has_pending_limiter_edit())
                    .unwrap_or(true)
        });
        assert!(
            settled,
            "the stale completion was never adopted and reissued"
        );
        {
            let state = player.eq_state.borrow();
            assert_eq!(
                state.settings.clip_protection,
                ClipProtection::Soft,
                "the loaded policy must win: the stale Off outcome is superseded and reissued"
            );
            let chain = state.chain.as_ref().expect("chain retained");
            assert!(
                chain.clip_protection_installed(),
                "the reissued Soft policy must leave the limiter installed"
            );
            assert!(!chain.topology_wedged());
            assert!(!chain.has_pending_limiter_edit());
        }
        assert_eq!(
            resyncs.get(),
            1,
            "the adoption supersedes silently and the reissue notifies the panel exactly once"
        );
    }

    /// Regression (refinery round 4, PR 220 A1, persisted-state side):
    /// the stale adoption of scenario one must leave the FULL reloaded
    /// snapshot in place — preamp and band gains included — with the
    /// stale outcome never persisting over any reloaded field.
    #[test]
    fn stale_parked_clip_swap_completion_preserves_the_reloaded_settings() {
        use equalizer::ClipProtection;

        if !eq_engine_plugins_available() {
            return;
        }
        let player = eq_player_with_pending_clip_edit(ClipProtection::Off);
        apply_serialized(&player, eq_enabled_settings(ClipProtection::Off));

        let reloaded = EqSettings {
            preamp_db: -6.0,
            bands_db: {
                let mut bands = [0.0; 10];
                bands[3] = 2.0;
                bands
            },
            ..eq_enabled_settings(ClipProtection::Soft)
        };
        apply_serialized(&player, reloaded);

        let accepted = complete_pending_edit(&player, true);
        assert!(accepted, "the parked transaction accepted its publication");
        let settled = drive_context_until_eq_state(&player, |state| {
            !state.clip_swap_poll_armed
                && !state
                    .chain
                    .as_ref()
                    .map(|chain| chain.has_pending_limiter_edit())
                    .unwrap_or(true)
        });
        assert!(
            settled,
            "the stale completion was never adopted and reissued"
        );
        let state = player.eq_state.borrow();
        assert_eq!(
            state.settings, reloaded,
            "the stale adoption must preserve every reloaded field, gains included"
        );
        assert!(
            state
                .chain
                .as_ref()
                .expect("chain retained")
                .clip_protection_installed(),
            "the reissued Soft policy must leave the limiter installed"
        );
    }

    /// Regression (refinery R1, PR 220 audit): while a limiter edit is
    /// parked under its blocking probe, disabling the equalizer must
    /// DEFER the uninstall — pausing the pipeline and detaching the bin
    /// now would be a conflicting edit over an in-flight, unvalidated
    /// graph. The bin stays recorded as installed at the pre-edit truth,
    /// and a later apply retries the removal.
    #[test]
    fn pending_clip_swap_defers_the_equalizer_uninstall() {
        if !eq_engine_plugins_available() {
            return;
        }
        let playbin = eq_test_playbin();
        let settings = eq_enabled_settings(equalizer::ClipProtection::Soft);
        let chain = equalizer::EqChain::build(&settings).expect("chain builds");
        playbin.set_property("audio-filter", Some(&chain.bin));
        let player = eq_test_player(playbin.clone(), eq_state_with(Some(chain), settings));
        player.eq_test_seams.borrow_mut().playing = Some(true);

        player
            .eq_state
            .borrow_mut()
            .chain
            .as_mut()
            .expect("chain installed")
            .inject_pending_limiter_edit(equalizer::ClipProtection::Off);

        let next = EqSettings {
            enabled: false,
            clip_protection: equalizer::ClipProtection::Off,
            ..settings
        };
        apply_serialized(&player, next);

        let state = player.eq_state.borrow();
        assert!(
            state.settings.enabled,
            "the uninstall is deferred: the bin stays recorded as installed"
        );
        assert_eq!(
            state.settings.clip_protection,
            equalizer::ClipProtection::Soft,
            "the deferred uninstall records the pre-edit truth, not the requested Off"
        );
        let chain = state.chain.as_ref().expect("chain retained on defer");
        assert!(chain.has_pending_limiter_edit());
        assert!(chain.clip_protection_installed());
        assert!(installed_audio_filter(&playbin).is_some());
    }

    /// Regression (refinery R3, PR 220 audit): the normal close-request
    /// drain flushes the pending equalizer persistence through the
    /// [`AudioOutput`](crate::audio::output::AudioOutput) trait while the
    /// output is still alive — never relying on `Drop`, which
    /// `std::process::exit` after the GTK main loop unwinds does not run.
    /// The flush sink records what would hit the real user config path,
    /// so no test ever writes the user's `equalizer.cfg`.
    #[test]
    fn local_output_shutdown_flush_persists_pending_state_while_alive() {
        use crate::audio::output::AudioOutput;

        if !eq_engine_plugins_available() {
            return;
        }
        let settings = eq_enabled_settings(equalizer::ClipProtection::Off);
        // `eq_state_with` suppresses persistence for caller-discipline
        // tests; the flush must pass the latch to reach the sink.
        let mut state = eq_state_with(None, settings);
        state.persistence_suppressed = false;
        let player = eq_test_player(eq_test_playbin(), state);
        let sink = Rc::new(RefCell::new(Vec::new()));
        *player.eq_flush_sink.borrow_mut() = Some(Rc::clone(&sink));

        // A user edit whose debounced write is still armed: the flush must
        // persist exactly the current in-memory state.
        player.eq_state.borrow_mut().settings.preamp_db = -6.0;

        let output = local_output::LocalOutput::new(player);
        AudioOutput::flush_equalizer_for_shutdown(&output);

        let flushed = sink.borrow();
        assert_eq!(
            flushed.len(),
            1,
            "the drain flush must persist the pending state exactly once"
        );
        assert!(
            (flushed[0].preamp_db - -6.0).abs() < f64::EPSILON,
            "the flush must persist the current in-memory preamp, got {}",
            flushed[0].preamp_db
        );
    }

    /// The shutdown flush honors the transient-read-failure latch, exactly
    /// like the debounced writer: a suppressed output must never overwrite
    /// the on-disk bytes (contract: *Persistence*, transient read failure).
    #[test]
    fn local_output_shutdown_flush_is_suppressed_after_a_transient_read_failure() {
        use crate::audio::output::AudioOutput;

        if !eq_engine_plugins_available() {
            return;
        }
        let settings = eq_enabled_settings(equalizer::ClipProtection::Off);
        let mut state = eq_state_with(None, settings);
        state.persistence_suppressed = true;
        let player = eq_test_player(eq_test_playbin(), state);
        let sink = Rc::new(RefCell::new(Vec::new()));
        *player.eq_flush_sink.borrow_mut() = Some(Rc::clone(&sink));

        let output = local_output::LocalOutput::new(player);
        AudioOutput::flush_equalizer_for_shutdown(&output);

        assert!(
            sink.borrow().is_empty(),
            "a persistence-suppressed output must not flush"
        );
    }

    /// Regression (review finding r3983308690 P2): a deferred enable
    /// must not persist `enabled = true` while no chain exists — band
    /// edits would be passthrough while the UI shows enabled. The
    /// previous (disabled) record stays, and the next apply retries.
    #[test]
    fn deferred_install_keeps_disabled_record_and_a_later_apply_retries() {
        if !eq_engine_plugins_available() {
            return;
        }
        let playbin = eq_test_playbin();
        let settings = EqSettings {
            enabled: false,
            ..eq_enabled_settings(equalizer::ClipProtection::Off)
        };
        let player = eq_test_player(playbin.clone(), eq_state_with(None, settings));

        let next = EqSettings {
            enabled: true,
            ..settings
        };
        player.eq_test_seams.borrow_mut().seam = Some(Box::new(|_edit| false));
        apply_serialized(&player, next);

        {
            let state = player.eq_state.borrow();
            assert!(
                !state.settings.enabled,
                "a deferred install must keep the previous disabled record"
            );
            assert!(state.chain.is_none());
        }
        assert!(installed_audio_filter(&playbin).is_none());

        // Retry with the real seam.
        apply_serialized(&player, next);
        {
            let state = player.eq_state.borrow();
            assert!(state.settings.enabled);
            assert!(state.chain.is_some());
        }
        assert!(installed_audio_filter(&playbin).is_some());
    }

    /// Regression (review findings r3983308675 P1 / r3985265725 Major,
    /// deferred arm): when the clip-protection toggle is deferred, the
    /// recorded protection must be what the installed chain carries
    /// (Soft), not the requested value — the previous code's rollback
    /// write was immediately overwritten by the unconditional settings
    /// assignment, so the persisted state lied and the retry never
    /// fired (`clip_changed` went false).
    #[test]
    fn deferred_clip_toggle_records_the_installed_protection_not_the_request() {
        if !eq_engine_plugins_available() {
            return;
        }
        let playbin = eq_test_playbin();
        let settings = eq_enabled_settings(equalizer::ClipProtection::Soft);
        let chain = equalizer::EqChain::build(&settings).expect("chain builds");
        let player = eq_test_player(playbin.clone(), eq_state_with(Some(chain), settings));

        let next = EqSettings {
            clip_protection: equalizer::ClipProtection::Off,
            ..settings
        };
        player.eq_test_seams.borrow_mut().seam = Some(Box::new(|_edit| false));
        apply_serialized(&player, next);

        {
            let state = player.eq_state.borrow();
            assert_eq!(
                state.settings.clip_protection,
                equalizer::ClipProtection::Soft,
                "a deferred toggle must record the installed Soft limiter"
            );
            assert!(state.settings.enabled);
            let chain = state.chain.as_ref().expect("chain retained on defer");
            assert!(chain.clip_protection_installed());
        }

        // The recorded Soft keeps `clip_changed` true, so the next apply
        // retries the toggle and confirms the removal.
        apply_serialized(&player, next);
        {
            let state = player.eq_state.borrow();
            assert_eq!(
                state.settings.clip_protection,
                equalizer::ClipProtection::Off
            );
            let chain = state.chain.as_ref().expect("chain stays installed");
            assert!(!chain.clip_protection_installed());
        }
    }

    /// Regression (review findings r3983308675 P1 / r3985265725 Major,
    /// failed-surgery arm): when the limiter surgery fails and the
    /// chain degrades to the no-limiter layout, the recorded protection
    /// must be the degraded `Off`, not the requested `Soft`. The limiter
    /// insertion here fails on a duplicate `clipper` element name inside
    /// the bin (`bin.add` rejects duplicates), which is the same
    /// degraded-false path a failed removal reports.
    #[test]
    fn failed_clip_surgery_records_the_degraded_layout() {
        if !eq_engine_plugins_available() {
            return;
        }
        let playbin = eq_test_playbin();
        let settings = eq_enabled_settings(equalizer::ClipProtection::Off);
        let chain = equalizer::EqChain::build(&settings).expect("chain builds");
        // Plant the name collision that fails the insertion.
        let impostor = gst::ElementFactory::make("fakesink")
            .name("clipper")
            .build()
            .expect("fakesink for the name collision");
        chain.bin.add(&impostor).expect("plant the name collision");
        let player = eq_test_player(playbin.clone(), eq_state_with(Some(chain), settings));

        let next = EqSettings {
            clip_protection: equalizer::ClipProtection::Soft,
            ..settings
        };
        // Let the edit run against the chain; the surgery itself fails.
        player.eq_test_seams.borrow_mut().seam = Some(Box::new(|edit| edit()));
        apply_serialized(&player, next);

        let state = player.eq_state.borrow();
        assert_eq!(
            state.settings.clip_protection,
            equalizer::ClipProtection::Off,
            "a failed surgery must record the degraded no-limiter layout"
        );
        let chain = state.chain.as_ref().expect("chain stays installed");
        assert!(!chain.clip_protection_installed());
    }

    /// Regression (operator F1, PR #220, caller arm): when a limiter
    /// removal fails at the direct relink but the limiter path is
    /// restored, the chain keeps the owned handle, so the recorded
    /// (persisted) protection must stay the installed `Soft` — not the
    /// requested `Off` — and the next apply must retry the removal. The
    /// pre-fix code dropped the handle, recorded `Off` while the limiter
    /// stayed in the bin, and invited a second `clipper` on the next
    /// enable.
    #[test]
    fn failed_limiter_removal_records_installed_soft_and_retries() {
        if !eq_engine_plugins_available() {
            return;
        }
        let playbin = eq_test_playbin();
        let settings = eq_enabled_settings(equalizer::ClipProtection::Soft);
        let mut chain = equalizer::EqChain::build(&settings).expect("chain builds");
        // Refuse the direct relink once at the real surgery boundary.
        chain
            .inject_limiter_remove_fault(equalizer::chain::LimiterRemoveFault::DirectRelinkBlocked);
        let player = eq_test_player(playbin.clone(), eq_state_with(Some(chain), settings));

        let next = EqSettings {
            clip_protection: equalizer::ClipProtection::Off,
            ..settings
        };
        // Let the edit run against the chain; the surgery fails once.
        player.eq_test_seams.borrow_mut().seam = Some(Box::new(|edit| edit()));
        apply_serialized(&player, next);

        {
            let state = player.eq_state.borrow();
            assert_eq!(
                state.settings.clip_protection,
                equalizer::ClipProtection::Soft,
                "a failed removal that stays routed must record the installed Soft limiter"
            );
            let chain = state.chain.as_ref().expect("chain stays installed");
            assert!(
                chain.clip_protection_installed(),
                "the owned limiter handle must survive the failed removal"
            );
        }

        // The injected fault fired once: the next apply retries and lands Off.
        player.eq_test_seams.borrow_mut().seam = Some(Box::new(|edit| edit()));
        apply_serialized(&player, next);
        {
            let state = player.eq_state.borrow();
            assert_eq!(
                state.settings.clip_protection,
                equalizer::ClipProtection::Off,
                "the retried removal must record the now-installed Off"
            );
            let chain = state.chain.as_ref().expect("chain stays installed");
            assert!(!chain.clip_protection_installed());
        }
    }

    /// Regression (review finding G, PR #220): an ordinary clip-protection
    /// toggle on a playing pipeline is delivered by the dynamic blocking
    /// pad probe and must never touch the pause/relink seam — the toggle
    /// cannot interrupt playback. The seam hook is set to panic, so any
    /// pause-path use fails the test loudly instead of silently pausing.
    #[test]
    fn ordinary_clip_toggle_does_not_use_the_pause_seam() {
        if !eq_engine_plugins_available() {
            return;
        }
        let playbin = eq_test_playbin();
        let settings = eq_enabled_settings(equalizer::ClipProtection::Soft);
        let chain = equalizer::EqChain::build(&settings).expect("chain builds");
        playbin.set_property("audio-filter", Some(&chain.bin));
        let player = eq_test_player(playbin.clone(), eq_state_with(Some(chain), settings));
        // A playing pipeline is exactly the case the probe path exists for.
        player.eq_test_seams.borrow_mut().playing = Some(true);
        player.eq_test_seams.borrow_mut().seam = Some(Box::new(|_edit| {
            panic!("the pause/relink seam must not run for an ordinary live toggle")
        }));

        let next = EqSettings {
            clip_protection: equalizer::ClipProtection::Off,
            ..settings
        };
        apply_serialized(&player, next);

        {
            let state = player.eq_state.borrow();
            assert_eq!(
                state.settings.clip_protection,
                equalizer::ClipProtection::Off,
                "the dynamic probe edit must land the requested removal"
            );
            let chain = state.chain.as_ref().expect("chain stays installed");
            assert!(!chain.clip_protection_installed());
        }
        assert!(
            player.eq_test_seams.borrow().seam.is_some(),
            "the pause/relink seam must remain unused after a dynamic edit"
        );
    }

    /// Regression (review finding G, PR #220): the pause/relink seam is the
    /// failed-dynamic-re-link fallback. When the dynamic probe edit reports
    /// a failed re-link (the surgery restored the pre-edit layout), the
    /// seam retries the edit and lands the removal. If the seam did not run
    /// as the fallback, the requested toggle would never be reached.
    #[test]
    fn clip_toggle_falls_back_to_the_pause_seam_after_a_failed_dynamic_relink() {
        if !eq_engine_plugins_available() {
            return;
        }
        let playbin = eq_test_playbin();
        let settings = eq_enabled_settings(equalizer::ClipProtection::Soft);
        let chain = equalizer::EqChain::build(&settings).expect("chain builds");
        playbin.set_property("audio-filter", Some(&chain.bin));
        let player = eq_test_player(playbin.clone(), eq_state_with(Some(chain), settings));
        player.eq_test_seams.borrow_mut().playing = Some(true);
        // The dynamic probe edit reports a failed re-link: the surgery
        // restored the pre-edit layout, so the toggle is unmet.
        player.eq_test_seams.borrow_mut().dynamic = Some(Box::new(|| Some(false)));
        // The seam then retries and lands the removal.
        player.eq_test_seams.borrow_mut().seam = Some(Box::new(|edit| edit()));

        let next = EqSettings {
            clip_protection: equalizer::ClipProtection::Off,
            ..settings
        };
        apply_serialized(&player, next);

        {
            let state = player.eq_state.borrow();
            assert_eq!(
                state.settings.clip_protection,
                equalizer::ClipProtection::Off,
                "the seam fallback must land the removal after the dynamic re-link failed"
            );
            let chain = state.chain.as_ref().expect("chain stays installed");
            assert!(!chain.clip_protection_installed());
        }
        assert!(
            player.eq_test_seams.borrow().seam.is_none(),
            "the pause/relink seam must have been exercised as the fallback"
        );
    }

    /// Regression (review finding r3985258424 P2): the pending member of
    /// a zero-timeout state query must never be discarded — a query
    /// that finds a transition in flight reports the *origin* state in
    /// `current` and must classify as unsettled, deferring the edit.
    #[test]
    fn zero_timeout_queries_with_a_transition_in_flight_are_unsettled() {
        use gst::StateChangeSuccess as Scs;

        let settled = |success| Ok::<gst::StateChangeSuccess, gst::StateChangeError>(success);
        // A settled, pending-free query confirms the reported state:
        // only a confirmed Playing state reads as "was playing".
        let confirms = |state: gst::State, success: gst::StateChangeSuccess| {
            assert_eq!(
                settled_zero_state((settled(success), state, gst::State::VoidPending)),
                Some(state == gst::State::Playing)
            );
        };
        confirms(gst::State::Playing, Scs::Success);
        confirms(gst::State::Paused, Scs::Success);
        confirms(gst::State::Null, Scs::Success);
        // NoPreroll (live pipelines) reports a settled, confirmed state.
        confirms(gst::State::Paused, Scs::NoPreroll);
        // The discarded-pending hazard: origin `Paused`, target `Playing`
        // in flight reads as "not playing" but is not settled.
        assert_eq!(
            settled_zero_state((settled(Scs::Async), gst::State::Paused, gst::State::Playing)),
            None
        );
        assert_eq!(
            settled_zero_state((settled(Scs::Async), gst::State::Null, gst::State::Paused)),
            None
        );
        // A failed state query is not a settled confirmation either.
        assert_eq!(
            settled_zero_state((
                Err(gst::StateChangeError),
                gst::State::Null,
                gst::State::VoidPending
            )),
            None
        );
    }

    /// Regression (review finding r3985258424 P2, live half): a real
    /// pipeline stuck mid-transition (data flow into the sink is
    /// blocked, so the PLAYING transition can never preroll) reports
    /// `Async` with a pending target from the zero-timeout query and
    /// must classify as unsettled.
    #[test]
    fn a_pipeline_mid_transition_is_not_settled() {
        gst::init().expect("GStreamer init");
        let Ok(src) = gst::ElementFactory::make("audiotestsrc").build() else {
            return;
        };
        let Ok(sink) = gst::ElementFactory::make("fakesink").build() else {
            return;
        };
        let pipeline = gst::Pipeline::new();
        pipeline.add_many([&src, &sink]).expect("assemble fixture");
        src.link(&sink).expect("link fixture");

        // Block data flow at the sink pad so the PLAYING transition can
        // never complete its preroll — the pipeline stays mid-transition.
        let sink_pad = sink.static_pad("sink").expect("fakesink sink pad");
        sink_pad.add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_pad, _info| {
            gst::PadProbeReturn::Ok
        });

        pipeline
            .set_state(gst::State::Playing)
            .expect("request PLAYING");
        assert_eq!(
            settled_zero_state(pipeline.state(gst::ClockTime::ZERO)),
            None,
            "a pipeline stuck mid-transition must classify as unsettled"
        );

        let _ = pipeline.set_state(gst::State::Null);
    }
}
