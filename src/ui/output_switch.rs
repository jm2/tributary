//! Output selector row-click handling (local, MPD, AirPlay, Chromecast).
//!
//! Extracted from `window.rs` — handles switching the active audio output
//! when the user clicks a row in the output selector popover.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use tracing::info;

use crate::audio::airplay_output::AirPlayOutput;
use crate::audio::chromecast_output::ChromecastOutput;
use crate::audio::mpd_output::MpdOutput;
use crate::audio::output::AudioOutput;
use crate::audio::PlayerEvent;

use super::output_dialogs::load_saved_outputs;

/// Wire the output selector popover: switching between local, MPD,
/// AirPlay, and Chromecast outputs.
///
/// This function does not use `WindowState` because it operates on audio
/// output state only, not track/sidebar state.
pub fn setup_output_selector(
    output_list: &gtk::ListBox,
    output_button: &gtk::MenuButton,
    active_output: &Rc<RefCell<Box<dyn AudioOutput>>>,
    parked_local: &Rc<RefCell<Option<Box<dyn AudioOutput>>>>,
    event_sender: &async_channel::Sender<PlayerEvent>,
    volume_scale: &gtk::Scale,
) {
    let active_output = active_output.clone();
    let parked_local = parked_local.clone();
    let event_sender = event_sender.clone();
    let volume_scale = volume_scale.clone();
    let output_button = output_button.clone();

    output_list.connect_row_activated(move |list_box, activated_row| {
        let idx = activated_row.index();

        // ── Stop the current output before switching ──────────
        active_output.borrow().stop();

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
                handle_chromecast_switch(
                    activated_row,
                    &active_output,
                    &parked_local,
                    &event_sender,
                    &volume_scale,
                );
            } else if is_airplay {
                handle_airplay_switch(
                    activated_row,
                    &active_output,
                    &parked_local,
                    &event_sender,
                    &volume_scale,
                );
            } else {
                handle_mpd_switch(
                    list_box,
                    idx,
                    &active_output,
                    &parked_local,
                    &event_sender,
                    &volume_scale,
                );
            }
        }

        // ── Update checkmark visibility on all rows ───────────
        update_checkmarks(list_box, idx);

        // Close the popover after selection.
        if let Some(popover) = output_button.popover() {
            popover.popdown();
        }
    });
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

    let host_port = activated_row.widget_name().to_string();
    let (host, port) = parse_host_port(&host_port, 8009);

    park_local_if_needed(active_output, parked_local, event_sender);

    let chromecast = ChromecastOutput::new(&cast_name, &host, port, event_sender.clone());
    *active_output.borrow_mut() = Box::new(chromecast);
    info!(
        name = %cast_name,
        host = %host,
        port,
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

    let airplay = AirPlayOutput::new(&airplay_name, &host, port, event_sender.clone());
    *active_output.borrow_mut() = Box::new(airplay);
    info!(
        name = %airplay_name,
        host = %host,
        port,
        "Switched to AirPlay output"
    );

    volume_scale.set_sensitive(false);
}

/// Switch to an MPD output.
fn handle_mpd_switch(
    list_box: &gtk::ListBox,
    idx: i32,
    active_output: &Rc<RefCell<Box<dyn AudioOutput>>>,
    parked_local: &Rc<RefCell<Option<Box<dyn AudioOutput>>>>,
    event_sender: &async_channel::Sender<PlayerEvent>,
    volume_scale: &gtk::Scale,
) {
    let saved = load_saved_outputs();
    // Count non-AirPlay rows before this one (excluding
    // index 0 = "My Computer") to get the saved_idx.
    let mut mpd_idx = 0usize;
    let mut child = list_box.first_child();
    let mut row_count = 0i32;
    while let Some(c) = child {
        if row_count > 0 && row_count < idx {
            // Check if this row is NOT an AirPlay row.
            let is_ap = c
                .first_child()
                .and_then(|inner| inner.downcast::<gtk::Box>().ok())
                .and_then(|rb| {
                    rb.first_child()
                        .and_then(|i| i.downcast::<gtk::Image>().ok())
                })
                .and_then(|icon| icon.icon_name())
                .is_some_and(|n| n == "network-wireless-symbolic");
            if !is_ap {
                mpd_idx += 1;
            }
        }
        row_count += 1;
        child = c.next_sibling();
    }

    if let Some(entry) = saved.get(mpd_idx) {
        park_local_if_needed(active_output, parked_local, event_sender);

        let mpd = MpdOutput::new(&entry.name, &entry.host, entry.port, event_sender.clone());
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
