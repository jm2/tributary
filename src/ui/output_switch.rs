//! Output selector row-click handling (local, MPD, AirPlay, Chromecast).
//!
//! Extracted from `window.rs` — handles switching the active audio output
//! when the user clicks a row in the output selector popover.

use std::cell::RefCell;
use std::net::SocketAddr;
use std::rc::Rc;

use adw::prelude::*;
use tracing::info;

use crate::audio::airplay_output::AirPlayOutput;
use crate::audio::chromecast_output::ChromecastOutput;
use crate::audio::mpd_output::MpdOutput;
use crate::audio::output::AudioOutput;
use crate::audio::PlayerEvent;

use super::output_dialogs::load_saved_outputs;
use super::playback::PlaybackSession;

/// Stable identity for a selectable output endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputTarget {
    Local,
    Mpd { host: String, port: u16 },
    AirPlay { host: String, port: u16 },
    Chromecast { address: SocketAddr },
}

fn output_change_required(active: &OutputTarget, requested: &OutputTarget) -> bool {
    active != requested
}

fn prepare_output_change(
    active: &OutputTarget,
    requested: &OutputTarget,
    session: &mut PlaybackSession,
) -> bool {
    if !output_change_required(active, requested) {
        return false;
    }
    session.clear();
    true
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
        let should_change = prepare_output_change(
            &active_target.borrow(),
            &requested_target,
            &mut playback_session.borrow_mut(),
        );
        if !should_change {
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
        active_output.borrow().stop();
        clear_playback_ui();

        if idx == 0 {
            // ── Switch to "My Computer" (LocalOutput) ─────────
            // If the local output is parked, move it back.
            if let Some(local) = parked_local.borrow_mut().take() {
                *active_output.borrow_mut() = local;
                info!("Switched to local output (My Computer)");
            }
            // else: already local, no-op.

            volume_scale.set_sensitive(true);
        } else {
            // ── Determine if this is an MPD or AirPlay row ────
            // Check the icon on the activated row to distinguish
            // AirPlay (network-wireless-symbolic) from MPD
            // (network-server-symbolic).
            let row_icon_name = activated_row
                .first_child()
                .and_then(|inner| inner.downcast::<gtk::Box>().ok())
                .and_then(|row_box| {
                    row_box
                        .first_child()
                        .and_then(|i| i.downcast::<gtk::Image>().ok())
                })
                .and_then(|icon| icon.icon_name())
                .unwrap_or_default();

            let is_airplay = row_icon_name == "network-wireless-symbolic";
            let is_chromecast = row_icon_name == "video-display-symbolic";

            if is_chromecast {
                let OutputTarget::Chromecast { address } = &requested_target else {
                    return;
                };
                handle_chromecast_switch(
                    activated_row,
                    &active_output,
                    &parked_local,
                    &event_sender,
                    &volume_scale,
                    &rt_handle,
                    *address,
                );
            } else if is_airplay {
                handle_airplay_switch(
                    activated_row,
                    &active_output,
                    &parked_local,
                    &event_sender,
                    &volume_scale,
                    &rt_handle,
                );
            } else {
                handle_mpd_switch(
                    list_box,
                    idx,
                    &active_output,
                    &parked_local,
                    &event_sender,
                    &volume_scale,
                    &rt_handle,
                );
            }
        }

        *active_target.borrow_mut() = requested_target;

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

// ═══════════════════════════════════════════════════════════════════════
// Per-output-type handlers
// ═══════════════════════════════════════════════════════════════════════

/// Switch to a Chromecast output.
fn handle_chromecast_switch(
    activated_row: &gtk::ListBoxRow,
    active_output: &Rc<RefCell<Box<dyn AudioOutput>>>,
    parked_local: &Rc<RefCell<Option<Box<dyn AudioOutput>>>>,
    event_sender: &async_channel::Sender<PlayerEvent>,
    volume_scale: &gtk::Scale,
    rt_handle: &tokio::runtime::Handle,
    address: SocketAddr,
) {
    let cast_name = activated_row
        .first_child()
        .and_then(|inner| inner.downcast::<gtk::Box>().ok())
        .and_then(|row_box| {
            row_box
                .first_child()
                .and_then(|icon| icon.next_sibling())
                .and_then(|l| l.downcast::<gtk::Label>().ok())
        })
        .map(|l| l.text().to_string())
        .unwrap_or_default();

    park_local_if_needed(active_output, parked_local, event_sender);

    // Seed the new output with the current slider value so selecting a device
    // doesn't reset its effective volume to maximum (the slider stays
    // authoritative and the device starts at the user's chosen level).
    let chromecast = ChromecastOutput::new(
        &cast_name,
        address,
        event_sender.clone(),
        volume_scale.value(),
    )
    .with_runtime(rt_handle.clone());
    *active_output.borrow_mut() = Box::new(chromecast);
    info!(
        name = %cast_name,
        %address,
        "Switched to Chromecast output"
    );

    // Chromecast supports volume — keep slider enabled.
    volume_scale.set_sensitive(true);
}

/// Switch to an AirPlay output.
fn handle_airplay_switch(
    activated_row: &gtk::ListBoxRow,
    active_output: &Rc<RefCell<Box<dyn AudioOutput>>>,
    parked_local: &Rc<RefCell<Option<Box<dyn AudioOutput>>>>,
    event_sender: &async_channel::Sender<PlayerEvent>,
    volume_scale: &gtk::Scale,
    rt_handle: &tokio::runtime::Handle,
) {
    let airplay_name = activated_row
        .first_child()
        .and_then(|inner| inner.downcast::<gtk::Box>().ok())
        .and_then(|row_box| {
            row_box
                .first_child()
                .and_then(|icon| icon.next_sibling())
                .and_then(|l| l.downcast::<gtk::Label>().ok())
        })
        .map(|l| l.text().to_string())
        .unwrap_or_default();

    let host_port = activated_row.widget_name().to_string();
    let (host, port) = parse_host_port(&host_port, 7000);

    park_local_if_needed(active_output, parked_local, event_sender);

    // Seed the new output with the current slider value so selecting a device
    // doesn't reset its effective volume to maximum (0 dB) on first playback.
    let airplay = AirPlayOutput::new(
        &airplay_name,
        &host,
        port,
        event_sender.clone(),
        volume_scale.value(),
    )
    .with_runtime(rt_handle.clone());
    let supports_volume = airplay.supports_volume();
    *active_output.borrow_mut() = Box::new(airplay);
    info!(
        name = %airplay_name,
        host = %host,
        port,
        "Switched to AirPlay output"
    );

    volume_scale.set_sensitive(supports_volume);
}

/// Switch to an MPD output.
fn handle_mpd_switch(
    list_box: &gtk::ListBox,
    idx: i32,
    active_output: &Rc<RefCell<Box<dyn AudioOutput>>>,
    parked_local: &Rc<RefCell<Option<Box<dyn AudioOutput>>>>,
    event_sender: &async_channel::Sender<PlayerEvent>,
    volume_scale: &gtk::Scale,
    rt_handle: &tokio::runtime::Handle,
) {
    let saved = load_saved_outputs();
    let mpd_idx = mpd_index_before_row(list_box, idx);

    if let Some(entry) = saved.get(mpd_idx) {
        park_local_if_needed(active_output, parked_local, event_sender);

        let mpd = MpdOutput::new(&entry.name, &entry.host, entry.port, event_sender.clone())
            .with_runtime(rt_handle.clone());
        *active_output.borrow_mut() = Box::new(mpd);
        info!(
            name = %entry.name,
            host = %entry.host,
            port = entry.port,
            "Switched to MPD output"
        );

        volume_scale.set_sensitive(false);
    }
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

// ═══════════════════════════════════════════════════════════════════════
// Shared helpers
// ═══════════════════════════════════════════════════════════════════════

/// Park the local output (swap it out for a dummy) if it isn't already parked.
fn park_local_if_needed(
    active_output: &Rc<RefCell<Box<dyn AudioOutput>>>,
    parked_local: &Rc<RefCell<Option<Box<dyn AudioOutput>>>>,
    event_sender: &async_channel::Sender<PlayerEvent>,
) {
    if parked_local.borrow().is_none() {
        let dummy: Box<dyn AudioOutput> = Box::new(MpdOutput::new(
            "_dummy",
            "127.0.0.1",
            1,
            event_sender.clone(),
        ));
        let local = std::mem::replace(&mut *active_output.borrow_mut(), dummy);
        *parked_local.borrow_mut() = Some(local);
    }
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
    use super::*;

    #[test]
    fn reselecting_current_output_is_a_no_op() {
        let current = OutputTarget::Local;
        assert!(!output_change_required(&current, &OutputTarget::Local));

        let current = OutputTarget::Mpd {
            host: "music.local".to_string(),
            port: 6600,
        };
        assert!(!output_change_required(&current, &current));
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
    fn real_output_change_clears_but_reselection_preserves_session() {
        let queue_item = super::super::playback::QueueItem::external(
            "file:///tmp/example.flac".to_string(),
            "Example".to_string(),
            "Artist".to_string(),
            "Album".to_string(),
        );
        let mut session = PlaybackSession::default();
        assert!(session.replace_queue(vec![queue_item.clone()], 0));
        let event_from_current_output =
            PlayerEvent::ended(crate::audio::PlayerEventGeneration::default());
        assert!(session.accepts_event_generation(event_from_current_output.generation()));

        assert!(!prepare_output_change(
            &OutputTarget::Local,
            &OutputTarget::Local,
            &mut session,
        ));
        assert!(session.has_current());

        assert!(prepare_output_change(
            &OutputTarget::Local,
            &OutputTarget::AirPlay {
                host: "speaker.local".to_string(),
                port: 7000,
            },
            &mut session,
        ));
        assert!(!session.has_current());
        assert!(!session.accepts_event_generation(event_from_current_output.generation()));
    }
}
