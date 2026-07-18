//! Output selector row-click handling (local, MPD, AirPlay, Chromecast).
//!
//! Extracted from `window.rs` — handles switching the active audio output
//! when the user clicks a row in the output selector popover.

use std::cell::RefCell;
use std::net::SocketAddr;
use std::rc::Rc;

use adw::prelude::*;
use tracing::{info, warn};

use crate::audio::airplay_output::AirPlayOutput;
use crate::audio::chromecast_output::ChromecastOutput;
use crate::audio::mpd_output::{MpdControlMode, MpdOutput};
use crate::audio::output::{AudioOutput, OutputType};
use crate::audio::PlayerEvent;

use super::output_dialogs::load_saved_outputs;
use super::playback::{stop_owned_playback, PlaybackSession};

/// Stable identity for a selectable output endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputTarget {
    Local,
    Mpd {
        host: String,
        port: u16,
        exclusive_control: bool,
    },
    AirPlay {
        host: String,
        port: u16,
    },
    Chromecast {
        address: SocketAddr,
    },
}

fn output_change_required(active: &OutputTarget, requested: &OutputTarget) -> bool {
    active != requested
}

enum OutputActivation {
    Local,
    Remote(Box<dyn AudioOutput>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputSelectionOutcome {
    Reselected,
    Changed,
    Unavailable,
}

fn output_type_for_target(target: &OutputTarget) -> OutputType {
    match target {
        OutputTarget::Local => OutputType::Local,
        OutputTarget::Mpd { .. } => OutputType::Mpd,
        OutputTarget::AirPlay { .. } => OutputType::AirPlay,
        OutputTarget::Chromecast { .. } => OutputType::Chromecast,
    }
}

/// Atomically apply the non-widget portion of an output selection.
///
/// A reselect returns before touching the session or either output slot. A
/// real change first validates both the replacement and the local-output
/// parking invariant, then clears the queue generation before stopping the
/// old output. Consequently, even a synchronously delivered terminal event
/// from that Stop is stale. Switching away from Local parks the exact output;
/// switching between remote endpoints retains it; switching back restores it.
fn apply_output_selection(
    active_target: &mut OutputTarget,
    requested: OutputTarget,
    session: &mut PlaybackSession,
    active_output: &mut Box<dyn AudioOutput>,
    parked_local: &mut Option<Box<dyn AudioOutput>>,
    activation: OutputActivation,
) -> OutputSelectionOutcome {
    if !output_change_required(active_target, &requested) {
        return OutputSelectionOutcome::Reselected;
    }

    let slots_are_consistent = active_output.output_type() == output_type_for_target(active_target)
        && match active_target {
            OutputTarget::Local => parked_local.is_none(),
            _ => parked_local
                .as_ref()
                .is_some_and(|output| output.output_type() == OutputType::Local),
        };
    if !slots_are_consistent {
        return OutputSelectionOutcome::Unavailable;
    }

    let activation_is_valid = match (&requested, &activation) {
        (OutputTarget::Local, OutputActivation::Local) => parked_local
            .as_ref()
            .is_some_and(|output| output.output_type() == OutputType::Local),
        (OutputTarget::Local, OutputActivation::Remote(_)) | (_, OutputActivation::Local) => false,
        (target, OutputActivation::Remote(output)) => {
            output.output_type() == output_type_for_target(target)
        }
    };
    if !activation_is_valid {
        return OutputSelectionOutcome::Unavailable;
    }

    stop_owned_playback(session, active_output.as_ref());

    match activation {
        OutputActivation::Local => {
            let Some(local) = parked_local.take() else {
                unreachable!("local activation was validated above");
            };
            *active_output = local;
        }
        OutputActivation::Remote(remote) => {
            let previous = std::mem::replace(active_output, remote);
            if matches!(active_target, OutputTarget::Local) {
                *parked_local = Some(previous);
            }
        }
    }

    *active_target = requested;
    OutputSelectionOutcome::Changed
}

/// Wire the output selector popover: switching between local, MPD,
/// AirPlay, and Chromecast outputs.
///
/// This function does not use `WindowState` because it operates on audio
/// output state only, not track/sidebar state.
#[allow(clippy::too_many_arguments)]
pub fn setup_output_selector(
    output_list: &gtk::ListBox,
    output_button: &gtk::MenuButton,
    active_output: &Rc<RefCell<Box<dyn AudioOutput>>>,
    parked_local: &Rc<RefCell<Option<Box<dyn AudioOutput>>>>,
    active_target: &Rc<RefCell<OutputTarget>>,
    playback_session: &Rc<RefCell<PlaybackSession>>,
    clear_playback_ui: Rc<dyn Fn()>,
    event_sender: &async_channel::Sender<PlayerEvent>,
    volume_scale: &gtk::Scale,
    rt_handle: &tokio::runtime::Handle,
) {
    let active_output = active_output.clone();
    let parked_local = parked_local.clone();
    let active_target = active_target.clone();
    let playback_session = playback_session.clone();
    let event_sender = event_sender.clone();
    let volume_scale = volume_scale.clone();
    let output_button = output_button.clone();
    // Real runtime handle for embedded media servers used by remote outputs.
    let rt_handle = rt_handle.clone();

    output_list.connect_row_activated(move |list_box, activated_row| {
        let idx = activated_row.index();

        let Some(requested_target) = target_for_row(list_box, activated_row) else {
            return;
        };

        // Selecting the already-active endpoint must not stop or otherwise
        // perturb playback.
        if !output_change_required(&active_target.borrow(), &requested_target) {
            update_checkmarks(list_box, idx);
            if let Some(popover) = output_button.popover() {
                popover.popdown();
            }
            return;
        }

        // Output changes deliberately clear rather than implicitly transfer a
        // session. This avoids leaving a queue cursor attached to an output
        // that has no media loaded.
        let previous_output_type = active_output.borrow().output_type();
        info!(
            ?previous_output_type,
            ?requested_target,
            "Changing audio output"
        );

        let row_name = output_row_name(activated_row);
        let (activation, supports_volume) = match &requested_target {
            OutputTarget::Local => (OutputActivation::Local, true),
            OutputTarget::Chromecast { address } => {
                let output = ChromecastOutput::new(
                    &row_name,
                    *address,
                    event_sender.clone(),
                    volume_scale.value(),
                )
                .with_runtime(rt_handle.clone());
                (OutputActivation::Remote(Box::new(output)), true)
            }
            OutputTarget::AirPlay { host, port } => {
                let output = AirPlayOutput::new(
                    &row_name,
                    host,
                    *port,
                    event_sender.clone(),
                    volume_scale.value(),
                )
                .with_runtime(rt_handle.clone());
                let supports_volume = output.supports_volume();
                (OutputActivation::Remote(Box::new(output)), supports_volume)
            }
            OutputTarget::Mpd {
                host,
                port,
                exclusive_control,
            } => {
                let output = MpdOutput::new(
                    &row_name,
                    host,
                    *port,
                    MpdControlMode::from(*exclusive_control),
                    event_sender.clone(),
                )
                .with_runtime(rt_handle.clone());
                (OutputActivation::Remote(Box::new(output)), false)
            }
        };

        let outcome = {
            let mut target = active_target.borrow_mut();
            let mut session = playback_session.borrow_mut();
            let mut output = active_output.borrow_mut();
            let mut parked = parked_local.borrow_mut();
            apply_output_selection(
                &mut target,
                requested_target,
                &mut session,
                &mut output,
                &mut parked,
                activation,
            )
        };
        if outcome != OutputSelectionOutcome::Changed {
            warn!(?outcome, "Output selection could not be committed");
            return;
        }

        clear_playback_ui();
        volume_scale.set_sensitive(supports_volume);
        info!(output = %row_name, "Audio output changed");

        // ── Update checkmark visibility on all rows ───────────
        update_checkmarks(list_box, idx);

        // Close the popover after selection.
        if let Some(popover) = output_button.popover() {
            popover.popdown();
        }
    });
}

fn target_for_row(
    list_box: &gtk::ListBox,
    activated_row: &gtk::ListBoxRow,
) -> Option<OutputTarget> {
    let idx = activated_row.index();
    if idx == 0 {
        return Some(OutputTarget::Local);
    }

    let icon = row_icon_name(activated_row);
    let host_port = activated_row.widget_name().to_string();
    if icon == "video-display-symbolic" {
        let address = host_port.parse().ok()?;
        return Some(OutputTarget::Chromecast { address });
    }
    if icon == "network-wireless-symbolic" {
        let (host, port) = parse_host_port(&host_port, 7000);
        return Some(OutputTarget::AirPlay { host, port });
    }

    let saved = load_saved_outputs();
    let mpd_idx = mpd_index_before_row(list_box, idx);
    saved.get(mpd_idx).map(|entry| OutputTarget::Mpd {
        host: entry.host.clone(),
        port: entry.port,
        exclusive_control: entry.exclusive_control,
    })
}

fn row_icon_name(row: &gtk::ListBoxRow) -> gtk::glib::GString {
    row.first_child()
        .and_then(|inner| inner.downcast::<gtk::Box>().ok())
        .and_then(|row_box| {
            row_box
                .first_child()
                .and_then(|image| image.downcast::<gtk::Image>().ok())
        })
        .and_then(|icon| icon.icon_name())
        .unwrap_or_default()
}

fn output_row_name(row: &gtk::ListBoxRow) -> String {
    row.first_child()
        .and_then(|inner| inner.downcast::<gtk::Box>().ok())
        .and_then(|row_box| {
            row_box
                .first_child()
                .and_then(|icon| icon.next_sibling())
                .and_then(|l| l.downcast::<gtk::Label>().ok())
        })
        .map(|l| l.text().to_string())
        .unwrap_or_default()
}

/// Count MPD rows before `idx`, excluding local and discovered receiver rows.
fn mpd_index_before_row(list_box: &gtk::ListBox, idx: i32) -> usize {
    let mut mpd_idx = 0usize;
    let mut child = list_box.first_child();
    let mut row_count = 0i32;
    while let Some(c) = child {
        if row_count > 0 && row_count < idx {
            // Check if this row is an MPD row.
            let is_mpd = c
                .first_child()
                .and_then(|inner| inner.downcast::<gtk::Box>().ok())
                .and_then(|rb| {
                    rb.first_child()
                        .and_then(|i| i.downcast::<gtk::Image>().ok())
                })
                .and_then(|icon| icon.icon_name())
                .is_some_and(|n| n == "network-server-symbolic");
            if is_mpd {
                mpd_idx += 1;
            }
        }
        row_count += 1;
        child = c.next_sibling();
    }
    mpd_idx
}

/// Parse "host:port" from a widget name string, with a default port fallback.
fn parse_host_port(host_port: &str, default_port: u16) -> (String, u16) {
    if let Some(colon) = host_port.rfind(':') {
        let h = &host_port[..colon];
        let p = host_port[colon + 1..]
            .parse::<u16>()
            .unwrap_or(default_port);
        (h.to_string(), p)
    } else {
        (host_port.to_string(), default_port)
    }
}

/// Update checkmark visibility on all output selector rows.
fn update_checkmarks(list_box: &gtk::ListBox, active_idx: i32) {
    let mut row_idx = 0i32;
    let mut child = list_box.first_child();
    while let Some(c) = child {
        // Each row is a gtk::ListBoxRow wrapping our gtk::Box.
        if let Some(row_box) = c
            .first_child()
            .and_then(|inner| inner.downcast::<gtk::Box>().ok())
        {
            // The checkmark is the last child Image with widget name "output-check".
            let mut box_child = row_box.first_child();
            while let Some(bc) = box_child {
                if let Some(img) = bc.downcast_ref::<gtk::Image>() {
                    if img.widget_name() == "output-check" {
                        img.set_visible(row_idx == active_idx);
                    }
                }
                box_child = bc.next_sibling();
            }
        }
        row_idx += 1;
        child = c.next_sibling();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::audio::PlayerEventGeneration;

    #[derive(Debug, Default)]
    struct FakeOutputState {
        generations: Vec<PlayerEventGeneration>,
        loads: Vec<String>,
        stops: usize,
        drops: usize,
    }

    struct FakeOutput {
        name: String,
        output_type: OutputType,
        state: Rc<RefCell<FakeOutputState>>,
        reject_loads: Cell<usize>,
        volume: f64,
    }

    impl FakeOutput {
        fn boxed(
            name: &str,
            output_type: OutputType,
            reject_loads: usize,
        ) -> (Box<dyn AudioOutput>, Rc<RefCell<FakeOutputState>>) {
            let state = Rc::new(RefCell::new(FakeOutputState::default()));
            (
                Box::new(Self {
                    name: name.to_string(),
                    output_type,
                    state: Rc::clone(&state),
                    reject_loads: Cell::new(reject_loads),
                    volume: 0.5,
                }),
                state,
            )
        }
    }

    impl Drop for FakeOutput {
        fn drop(&mut self) {
            self.state.borrow_mut().drops += 1;
        }
    }

    impl AudioOutput for FakeOutput {
        fn name(&self) -> &str {
            &self.name
        }

        fn output_type(&self) -> OutputType {
            self.output_type
        }

        fn supports_volume(&self) -> bool {
            self.output_type != OutputType::Mpd
        }

        fn load_uri(&self, uri: &str) -> bool {
            self.state.borrow_mut().loads.push(uri.to_string());
            let remaining = self.reject_loads.get();
            if remaining == 0 {
                true
            } else {
                self.reject_loads.set(remaining - 1);
                false
            }
        }

        fn load_resolved(&self, _request: crate::architecture::media::ResolvedHttpRequest) -> bool {
            false
        }

        fn load_local(&self, _media: crate::local::resolver::ResolvedLocalMedia) -> bool {
            false
        }

        fn set_event_generation(&self, generation: PlayerEventGeneration) {
            self.state.borrow_mut().generations.push(generation);
        }

        fn play(&self) {}

        fn pause(&self) {}

        fn stop(&self) {
            self.state.borrow_mut().stops += 1;
        }

        fn toggle_play_pause(&self) {}

        fn seek_to(&self, _position_ms: u64) {}

        fn set_volume(&mut self, level: f64) {
            self.volume = level;
        }

        fn volume(&self) -> f64 {
            self.volume
        }

        fn state(&self) -> crate::audio::PlayerState {
            crate::audio::PlayerState::Stopped
        }

        fn position_ms(&self) -> Option<u64> {
            None
        }
    }

    #[test]
    fn reselecting_current_output_is_a_no_op() {
        let current = OutputTarget::Local;
        assert!(!output_change_required(&current, &OutputTarget::Local));

        let current = OutputTarget::Mpd {
            host: "music.local".to_string(),
            port: 6600,
            exclusive_control: true,
        };
        assert!(!output_change_required(&current, &current));
    }

    #[test]
    fn approving_the_same_mpd_endpoint_is_a_real_output_change() {
        let unconfirmed = OutputTarget::Mpd {
            host: "music.local".to_string(),
            port: 6600,
            exclusive_control: false,
        };
        let exclusive = OutputTarget::Mpd {
            host: "music.local".to_string(),
            port: 6600,
            exclusive_control: true,
        };
        assert!(output_change_required(&unconfirmed, &exclusive));
    }

    #[test]
    fn endpoint_identity_distinguishes_real_output_changes() {
        let first = OutputTarget::Chromecast {
            address: "192.0.2.10:8009".parse().unwrap(),
        };
        let second = OutputTarget::Chromecast {
            address: "192.0.2.11:8009".parse().unwrap(),
        };
        assert!(output_change_required(&first, &second));
        assert!(output_change_required(&first, &OutputTarget::Local));
    }

    #[test]
    fn output_slots_preserve_reselect_retry_generation_and_local_restore_semantics() {
        let (mut active_output, local_state) = FakeOutput::boxed("local", OutputType::Local, 0);
        let mut parked_local = None;
        let mut active_target = OutputTarget::Local;
        let queue_item = super::super::playback::QueueItem::external(
            "file:///tmp/example.flac".to_string(),
            "Example".to_string(),
            "Artist".to_string(),
            "Album".to_string(),
        );
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![queue_item], 0));
        let local_load = session
            .load_current_direct(active_output.as_ref())
            .expect("local queue item reaches the active output");
        assert!(local_load.accepted);
        let local_event = PlayerEvent::ended(local_load.generation);
        let local_identity = session.current_identity().cloned();

        assert_eq!(
            apply_output_selection(
                &mut active_target,
                OutputTarget::Local,
                &mut session,
                &mut active_output,
                &mut parked_local,
                // The early identity gate must not inspect or consume an
                // activation for the already-active endpoint.
                OutputActivation::Local,
            ),
            OutputSelectionOutcome::Reselected
        );
        assert_eq!(session.current_identity(), local_identity.as_ref());
        assert!(session.accepts_event_generation(local_event.generation()));
        assert_eq!(local_state.borrow().stops, 0);
        assert!(parked_local.is_none());

        let remote_target = OutputTarget::Mpd {
            host: "music.local".to_string(),
            port: 6600,
            exclusive_control: true,
        };
        let (remote, remote_state) = FakeOutput::boxed("mpd", OutputType::Mpd, 1);
        assert_eq!(
            apply_output_selection(
                &mut active_target,
                remote_target.clone(),
                &mut session,
                &mut active_output,
                &mut parked_local,
                OutputActivation::Remote(remote),
            ),
            OutputSelectionOutcome::Changed
        );
        assert_eq!(active_target, remote_target);
        assert_eq!(active_output.name(), "mpd");
        assert_eq!(local_state.borrow().stops, 1);
        assert!(parked_local.is_some());
        assert!(!session.has_current());
        assert!(!session.accepts_event_generation(local_event.generation()));

        let remote_item = super::super::playback::QueueItem::external(
            "https://radio.invalid/live".to_string(),
            "Station".to_string(),
            "Remote".to_string(),
            "Live".to_string(),
        );
        assert!(session.replace_queue(vec![remote_item], 0));
        let rejected = session
            .load_current_direct(active_output.as_ref())
            .expect("remote output receives the current item");
        assert!(!rejected.accepted);
        assert!(session.accepts_event_generation(rejected.generation));

        let retry = session
            .load_current_direct(active_output.as_ref())
            .expect("retry keeps the same queue item");
        assert!(retry.accepted);
        assert_ne!(retry.generation, rejected.generation);
        assert!(!session.accepts_event_generation(rejected.generation));
        assert!(session.accepts_event_generation(retry.generation));
        assert_eq!(
            remote_state.borrow().loads,
            ["https://radio.invalid/live", "https://radio.invalid/live"]
        );

        let cast_target = OutputTarget::Chromecast {
            address: "192.0.2.10:8009".parse().unwrap(),
        };
        let (cast, cast_state) = FakeOutput::boxed("cast", OutputType::Chromecast, 0);
        assert_eq!(
            apply_output_selection(
                &mut active_target,
                cast_target.clone(),
                &mut session,
                &mut active_output,
                &mut parked_local,
                OutputActivation::Remote(cast),
            ),
            OutputSelectionOutcome::Changed
        );
        assert_eq!(active_target, cast_target);
        assert_eq!(active_output.name(), "cast");
        assert!(parked_local.is_some());
        assert_eq!(remote_state.borrow().stops, 1);
        assert_eq!(remote_state.borrow().drops, 1);
        assert!(!session.has_current());
        assert!(!session.accepts_event_generation(retry.generation));

        let cast_item = super::super::playback::QueueItem::external(
            "file:///tmp/cast.flac".to_string(),
            "Cast".to_string(),
            "Artist".to_string(),
            "Album".to_string(),
        );
        assert!(session.replace_queue(vec![cast_item], 0));
        let cast_load = session
            .load_current_direct(active_output.as_ref())
            .expect("replacement output receives a fresh queue item");
        assert!(cast_load.accepted);

        assert_eq!(
            apply_output_selection(
                &mut active_target,
                OutputTarget::Local,
                &mut session,
                &mut active_output,
                &mut parked_local,
                OutputActivation::Local,
            ),
            OutputSelectionOutcome::Changed
        );
        assert_eq!(active_target, OutputTarget::Local);
        assert_eq!(active_output.name(), "local");
        assert!(parked_local.is_none());
        assert_eq!(cast_state.borrow().stops, 1);
        assert_eq!(cast_state.borrow().drops, 1);
        assert!(!session.has_current());
        assert!(!session.accepts_event_generation(cast_load.generation));

        assert_eq!(
            apply_output_selection(
                &mut active_target,
                OutputTarget::Local,
                &mut session,
                &mut active_output,
                &mut parked_local,
                OutputActivation::Local,
            ),
            OutputSelectionOutcome::Reselected
        );
        assert_eq!(local_state.borrow().stops, 1);
    }

    #[test]
    fn an_invalid_replacement_preserves_the_current_output_and_session() {
        let (mut active_output, local_state) = FakeOutput::boxed("local", OutputType::Local, 0);
        let mut parked_local = None;
        let mut active_target = OutputTarget::Local;
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![super::super::playback::QueueItem::external(
                "file:///tmp/example.flac".to_string(),
                "Example".to_string(),
                "Artist".to_string(),
                "Album".to_string(),
            )],
            0,
        ));
        let current = session.current_identity().cloned();
        let (wrong_type, wrong_state) = FakeOutput::boxed("wrong", OutputType::AirPlay, 0);

        assert_eq!(
            apply_output_selection(
                &mut active_target,
                OutputTarget::Mpd {
                    host: "music.local".to_string(),
                    port: 6600,
                    exclusive_control: true,
                },
                &mut session,
                &mut active_output,
                &mut parked_local,
                OutputActivation::Remote(wrong_type),
            ),
            OutputSelectionOutcome::Unavailable
        );
        assert_eq!(active_target, OutputTarget::Local);
        assert_eq!(active_output.name(), "local");
        assert_eq!(session.current_identity(), current.as_ref());
        assert!(parked_local.is_none());
        assert_eq!(local_state.borrow().stops, 0);
        assert_eq!(wrong_state.borrow().drops, 1);

        let remote_target = OutputTarget::Mpd {
            host: "music.local".to_string(),
            port: 6600,
            exclusive_control: true,
        };
        let (mut active_output, remote_state) = FakeOutput::boxed("mpd", OutputType::Mpd, 0);
        let (wrong_parked, wrong_parked_state) =
            FakeOutput::boxed("wrong parked", OutputType::AirPlay, 0);
        let mut parked_local = Some(wrong_parked);
        let mut active_target = remote_target.clone();
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(
            vec![super::super::playback::QueueItem::external(
                "https://radio.invalid/live".to_string(),
                "Station".to_string(),
                "Remote".to_string(),
                "Live".to_string(),
            )],
            0,
        ));
        let load = session
            .load_current_direct(active_output.as_ref())
            .expect("remote queue item reaches the active output");
        let current = session.current_identity().cloned();

        assert_eq!(
            apply_output_selection(
                &mut active_target,
                OutputTarget::Local,
                &mut session,
                &mut active_output,
                &mut parked_local,
                OutputActivation::Local,
            ),
            OutputSelectionOutcome::Unavailable
        );
        assert_eq!(active_target, remote_target);
        assert_eq!(active_output.name(), "mpd");
        assert_eq!(session.current_identity(), current.as_ref());
        assert!(session.accepts_event_generation(load.generation));
        assert_eq!(remote_state.borrow().stops, 0);
        assert_eq!(parked_local.as_ref().unwrap().name(), "wrong parked");
        assert_eq!(wrong_parked_state.borrow().drops, 0);
    }
}
