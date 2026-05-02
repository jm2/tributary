//! mDNS/DNS-SD discovery event handling for sidebar and output selector.
//!
//! Handles [`DiscoveryEvent`](crate::discovery::DiscoveryEvent)s from
//! the network discovery service — adds/removes servers in the sidebar
//! and AirPlay/Chromecast devices in the output selector.

use adw::prelude::*;
use gtk::glib;
use tracing::info;

use super::header_bar;
use super::objects::SourceObject;
use super::window;
use super::window_state::WindowState;

/// Wire mDNS/DNS-SD discovery: adds/removes servers in the sidebar
/// and AirPlay/Chromecast devices in the output selector.
pub fn setup_discovery(state: &WindowState, output_list: &gtk::ListBox) {
    let discovery_rx = crate::discovery::start_discovery();
    let store = state.sidebar_store.clone();
    let rt_handle = state.rt_handle.clone();
    let source_tracks = state.source_tracks.clone();
    let active_source_key = state.active_source_key.clone();
    let sidebar_selection = state.sidebar_selection.clone();
    let track_store = state.track_store.clone();
    let master_tracks = state.master_tracks.clone();
    let browser_widget = state.browser_widget.clone();
    let browser_state = state.browser_state.clone();
    let status_label = state.status_label.clone();
    let column_view = state.column_view.clone();
    let output_list = output_list.clone();

    glib::MainContext::default().spawn_local(async move {
        while let Ok(event) = discovery_rx.recv().await {
            match event {
                crate::discovery::DiscoveryEvent::Found(server) => {
                    // ── AirPlay devices go to the output selector, not sidebar ──
                    if server.service_type == "airplay" {
                        handle_airplay_found(&output_list, &server);
                        continue;
                    }

                    // ── AirPlay 2 (`_airplay._tcp`) — not yet supported ──
                    // The current output path uses GStreamer's `raopsink`
                    // and only speaks legacy RAOP, so an AirPlay 2-only
                    // receiver (HomePod, recent Apple TV) cannot actually
                    // play audio if selected.  Drop the event here until
                    // a sender-side AirPlay 2 implementation lands.  See
                    // the AirPlay 2 roadmap section in README.md.
                    if server.service_type == "airplay2" {
                        info!(
                            name = %server.name,
                            url = %server.url,
                            "AirPlay 2 receiver discovered — skipping (sender support not yet implemented)"
                        );
                        continue;
                    }

                    // ── Chromecast devices go to the output selector, not sidebar ──
                    if server.service_type == "chromecast" {
                        handle_chromecast_found(&output_list, &server);
                        continue;
                    }

                    // Dedup: check if this URL is already in the sidebar.
                    let already_exists = (0..store.n_items()).any(|i| {
                        store
                            .item(i)
                            .and_downcast_ref::<SourceObject>()
                            .is_some_and(|s| s.server_url() == server.url)
                    });
                    if already_exists {
                        continue;
                    }

                    info!(
                        name = %server.name,
                        url = %server.url,
                        backend = %server.service_type,
                        "Adding discovered server to sidebar"
                    );

                    // Insert under the correct category header.
                    let insert_pos =
                        window::ensure_category_header_store(&store, &server.service_type);
                    let src =
                        SourceObject::discovered(&server.name, &server.service_type, &server.url);

                    // Apply requires_password if already known from discovery.
                    if let Some(rp) = server.requires_password {
                        src.set_requires_password(rp);
                    }

                    store.insert(insert_pos, &src);

                    // For DAAP servers, probe whether a password is required
                    // in the background and update the sidebar item.
                    if server.service_type == "daap" && server.requires_password.is_none() {
                        probe_daap_password(&rt_handle, &store, &server.url);
                    }
                }

                crate::discovery::DiscoveryEvent::Lost { url, service_type } => {
                    // ── Chromecast devices: remove from output selector ──
                    if service_type == "chromecast" {
                        handle_chromecast_lost(&output_list, &url);
                        continue;
                    }

                    // ── AirPlay devices: remove from output selector ──
                    if service_type == "airplay" {
                        handle_airplay_lost(&output_list, &url);
                        continue;
                    }

                    // ── AirPlay 2: matched the Found drop above; nothing
                    //    was added to the popover, so nothing to remove.
                    if service_type == "airplay2" {
                        continue;
                    }

                    info!(
                        url = %url,
                        backend = %service_type,
                        "Removing lost server from sidebar"
                    );

                    // Find the sidebar entry for this URL.
                    for i in 0..store.n_items() {
                        if let Some(src) = store.item(i).and_downcast_ref::<SourceObject>() {
                            if src.server_url() == url {
                                // Never auto-remove manually-added servers.
                                if src.manually_added() {
                                    break;
                                }
                                // If connected and still the active source,
                                // switch to local before removing.
                                let was_active = *active_source_key.borrow() == url;

                                if src.connected() {
                                    // Remove from source_tracks map.
                                    source_tracks.borrow_mut().remove(&url);
                                }

                                // Remove the sidebar entry.
                                store.remove(i);

                                // Clean up empty category header.
                                let category = window::category_for_backend(&service_type);
                                window::remove_empty_category_header(&store, category);

                                // If this was the active source, switch to local.
                                if was_active {
                                    *active_source_key.borrow_mut() = "local".to_string();
                                    sidebar_selection.set_selected(1);

                                    let st = source_tracks.borrow();
                                    let local_tracks = st.get("local").cloned().unwrap_or_default();
                                    window::display_tracks(
                                        &local_tracks,
                                        &track_store,
                                        &master_tracks,
                                        &browser_widget,
                                        &browser_state,
                                        &status_label,
                                        &column_view,
                                    );
                                }

                                break;
                            }
                        }
                    }
                }
            }
        }
    });
}

// ═══════════════════════════════════════════════════════════════════════
// AirPlay / Chromecast handlers
// ═══════════════════════════════════════════════════════════════════════

/// Add a discovered AirPlay device to the output selector.
fn handle_airplay_found(output_list: &gtk::ListBox, server: &crate::discovery::DiscoveredServer) {
    let airplay_url = &server.url;
    let airplay_name = server.name.clone();

    // Dedup: check if this AirPlay device is already in outputs.
    if is_device_in_output_list(output_list, &airplay_name) {
        return;
    }

    info!(
        name = %airplay_name,
        url = %airplay_url,
        "AirPlay receiver discovered — adding to output selector"
    );
    let row = header_bar::build_output_row(&airplay_name, "network-wireless-symbolic", false);
    // Store the host:port on the row so the output selector can use it.
    if let Ok(parsed) = url::Url::parse(airplay_url) {
        let host = parsed.host_str().unwrap_or("").to_string();
        let port = parsed.port().unwrap_or(7000);
        row.set_widget_name(&format!("{host}:{port}"));
    }
    output_list.append(&row);
    propagate_widget_name(output_list);
}

/// Add a discovered Chromecast device to the output selector.
fn handle_chromecast_found(
    output_list: &gtk::ListBox,
    server: &crate::discovery::DiscoveredServer,
) {
    let cast_url = &server.url;
    let cast_name = server.name.clone();

    // Dedup: check if this Chromecast is already in outputs.
    if is_device_in_output_list(output_list, &cast_name) {
        return;
    }

    info!(
        name = %cast_name,
        url = %cast_url,
        "Chromecast device discovered — adding to output selector"
    );
    let row = header_bar::build_output_row(&cast_name, "video-display-symbolic", false);
    // Extract host:port from cast://host:port URL.
    let host_port = cast_url.strip_prefix("cast://").unwrap_or(cast_url);
    row.set_widget_name(host_port);
    output_list.append(&row);
    propagate_widget_name(output_list);
}

/// Remove a lost Chromecast from the output selector.
fn handle_chromecast_lost(output_list: &gtk::ListBox, url: &str) {
    info!(url = %url, "Chromecast device lost — removing from output selector");
    let lost_hp = url.strip_prefix("cast://").unwrap_or(url);

    let mut child = output_list.first_child();
    let mut row_idx = 0i32;
    while let Some(c) = child {
        let next = c.next_sibling();
        if row_idx > 0 {
            if let Some(row_box) = c
                .first_child()
                .and_then(|inner| inner.downcast::<gtk::Box>().ok())
            {
                if let Some(icon) = row_box
                    .first_child()
                    .and_then(|i| i.downcast::<gtk::Image>().ok())
                {
                    if icon
                        .icon_name()
                        .is_some_and(|n| n == "video-display-symbolic")
                    {
                        if let Some(list_row) = c.downcast_ref::<gtk::ListBoxRow>() {
                            // Match by widget name (host:port).
                            let row_hp = list_row.widget_name().to_string();
                            if row_hp == lost_hp {
                                output_list.remove(list_row);
                            }
                        }
                    }
                }
            }
        }
        row_idx += 1;
        child = next;
    }
}

/// Remove a lost AirPlay device from the output selector.
fn handle_airplay_lost(output_list: &gtk::ListBox, url: &str) {
    info!(url = %url, "AirPlay receiver lost — removing from output selector");

    let mut child = output_list.first_child();
    let mut row_idx = 0i32;
    while let Some(c) = child {
        let next = c.next_sibling();
        // Skip index 0 ("My Computer") — never remove it.
        if row_idx > 0 {
            if let Some(row_box) = c
                .first_child()
                .and_then(|inner| inner.downcast::<gtk::Box>().ok())
            {
                // Check the icon — AirPlay rows use "network-wireless-symbolic".
                if let Some(icon) = row_box
                    .first_child()
                    .and_then(|i| i.downcast::<gtk::Image>().ok())
                {
                    if icon
                        .icon_name()
                        .is_some_and(|n| n == "network-wireless-symbolic")
                    {
                        if let Some(list_row) = c.downcast_ref::<gtk::ListBoxRow>() {
                            output_list.remove(list_row);
                        }
                    }
                }
            }
        }
        row_idx += 1;
        child = next;
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════

/// Check if a device with the given name already exists in the output list.
fn is_device_in_output_list(output_list: &gtk::ListBox, name: &str) -> bool {
    let mut child = output_list.first_child();
    while let Some(c) = child {
        if let Some(row_box) = c
            .first_child()
            .and_then(|inner| inner.downcast::<gtk::Box>().ok())
        {
            if let Some(label) = row_box
                .first_child()
                .and_then(|icon| icon.next_sibling())
                .and_then(|l| l.downcast::<gtk::Label>().ok())
            {
                if label.text() == name {
                    return true;
                }
            }
        }
        child = c.next_sibling();
    }
    false
}

/// Propagate widget name from a ListBox child's inner Box to its wrapping ListBoxRow.
fn propagate_widget_name(output_list: &gtk::ListBox) {
    if let Some(last_row) = output_list.last_child() {
        if let Some(list_row) = last_row.downcast_ref::<gtk::ListBoxRow>() {
            if let Some(inner) = list_row.first_child() {
                let name = inner.widget_name().to_string();
                if !name.is_empty() && name != "GtkBox" {
                    list_row.set_widget_name(&name);
                }
            }
        }
    }
}

/// Probe whether a DAAP server requires a password, updating the sidebar item.
fn probe_daap_password(
    rt_handle: &tokio::runtime::Handle,
    store: &gtk::gio::ListStore,
    server_url: &str,
) {
    let probe_url = server_url.to_string();
    let store = store.clone();
    let (probe_tx, probe_rx) = async_channel::bounded::<Option<bool>>(1);

    rt_handle.spawn(async move {
        let result = crate::daap::client::DaapClient::probe_requires_password(&probe_url).await;
        let _ = probe_tx.send(result).await;
    });

    let probe_server_url = server_url.to_string();
    glib::MainContext::default().spawn_local(async move {
        if let Ok(Some(requires_pw)) = probe_rx.recv().await {
            // Find the source in the store and update it.
            for i in 0..store.n_items() {
                if let Some(src) = store.item(i).and_downcast_ref::<SourceObject>() {
                    if src.server_url() == probe_server_url && !src.connected() {
                        src.set_requires_password(requires_pw);
                        // Force rebind by remove + re-insert.
                        let src = src.clone();
                        store.remove(i);
                        store.insert(i, &src);
                        break;
                    }
                }
            }
        }
    });
}
