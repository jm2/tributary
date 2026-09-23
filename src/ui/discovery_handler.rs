//! mDNS/DNS-SD discovery event handling for sidebar and output selector.
//!
//! Handles [`DiscoveryEvent`](crate::discovery::DiscoveryEvent)s from
//! the network discovery service — adds/removes servers in the sidebar
//! and AirPlay/Chromecast devices in the output selector.

use adw::prelude::*;
use gtk::glib;
use tracing::info;

use crate::architecture::AdvertisedHttpRoute;

use super::header_bar;
use super::objects::SourceObject;
use super::output_switch::{
    airplay_row_endpoint, encode_airplay_row_identity, row_icon_name, AIRPLAY_ROW_ICON,
    CHROMECAST_ROW_ICON,
};
use super::window;
use super::window_state::WindowState;

fn discovery_publisher(backend_type: &str, server_url: &str) -> Option<String> {
    let parsed = crate::http_security::parse_base_url(server_url).ok()?;
    let canonical = crate::architecture::identity::canonical_remote_base_url(&parsed).ok()?;
    Some(format!("discovery:{backend_type}:{canonical}"))
}

/// Wire mDNS/DNS-SD discovery: adds/removes servers in the sidebar
/// and AirPlay/Chromecast devices in the output selector.
pub fn setup_discovery(state: &WindowState, output_list: &gtk::ListBox) {
    let discovery_rx = crate::discovery::start_discovery();
    let store = state.sidebar_store.clone();
    let sidebar_selection = state.sidebar_selection.clone();
    let rt_handle = state.rt_handle.clone();
    let source_registry = state.source_registry.clone();
    let remote_provenance = state.remote_provenance.clone();
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
                            "AirPlay 2 receiver discovered — skipping (sender support not yet implemented)"
                        );
                        continue;
                    }

                    // ── Chromecast devices go to the output selector, not sidebar ──
                    if server.service_type == "chromecast" {
                        handle_chromecast_found(&output_list, &server);
                        continue;
                    }

                    // Treat every remote-library discovery event as
                    // unauthenticated input at the GTK boundary too. Current
                    // producers already construct or validate safe base URLs;
                    // this second check prevents a future producer from
                    // publishing user-info/query credentials to a row or log.
                    let parsed_url = match crate::http_security::parse_base_url(&server.url) {
                        Ok(url) => url,
                        Err(error) => {
                            tracing::warn!(error, "Ignoring discovered server with invalid URL");
                            continue;
                        }
                    };
                    let advertised_route = match server.advertised_route.clone() {
                        Some(route) if route.matches_origin(&parsed_url) => Some(route),
                        Some(_) => {
                            tracing::warn!(
                                "Ignoring advertised route that does not match its server origin"
                            );
                            None
                        }
                        None => None,
                    };
                    let Some(publisher) =
                        discovery_publisher(&server.service_type, &server.url)
                    else {
                        tracing::warn!("Ignoring discovered server without canonical publisher identity");
                        continue;
                    };

                    // Stable SourceId owns the logical source. Canonical
                    // `(backend, endpoint)` is only the discovery lookup key.
                    // An updated publication refreshes this row's ephemeral
                    // route; replacing or removing a prior advertised route
                    // revokes work that captured the withdrawn address.
                    let existing = remote_source_at(
                        &store,
                        &server.url,
                        &server.service_type,
                    )
                    .map(|(_, source)| source);
                    if let Some(source) = existing {
                        let Some(source_id) = source.source_id() else {
                            tracing::warn!("Ignoring discovered source without stable identity");
                            continue;
                        };
                        if !remote_provenance.ensure(
                            &source_registry,
                            source_id,
                            crate::source_lifecycle::SourceProvenance::Discovery,
                            publisher,
                        ) {
                            tracing::debug!("Ignoring discovery publication during shutdown");
                            continue;
                        }
                        let route_changed = reconcile_discovery_route(
                            &source,
                            advertised_route.clone(),
                            |source_id| {
                                // A pending constructor or active adapter may
                                // have captured the withdrawn address. Claims
                                // and the logical row remain; only exact
                                // lifecycle/session authority is revoked.
                                let _ = source_registry.disconnect(source_id);
                            },
                        );
                        if let Some(requires_password) = server.requires_password {
                            source.set_requires_password(requires_password);
                        }
                        if route_changed
                            && !source.connected()
                            && server.service_type == "daap"
                            && server.requires_password.is_none()
                        {
                            probe_daap_password(
                                &rt_handle,
                                &store,
                                &sidebar_selection,
                                &server.url,
                                advertised_route,
                            );
                        }
                        continue;
                    }

                    info!(
                        name = %server.name,
                        backend = %server.service_type,
                        "Adding discovered server to sidebar"
                    );

                    // Insert under the correct category header.
                    let insert_pos =
                        window::ensure_category_header_store(&store, &server.service_type);
                    let src =
                        SourceObject::discovered(&server.name, &server.service_type, &server.url);
                    let Some(source_id) = src.source_id() else {
                        tracing::warn!("Ignoring discovered source without stable identity");
                        continue;
                    };
                    if !remote_provenance.ensure(
                        &source_registry,
                        source_id,
                        crate::source_lifecycle::SourceProvenance::Discovery,
                        publisher,
                    ) {
                        tracing::debug!("Ignoring discovery publication during shutdown");
                        continue;
                    }
                    src.set_advertised_route(advertised_route.clone());

                    // Apply requires_password if already known from discovery.
                    if let Some(rp) = server.requires_password {
                        src.set_requires_password(rp);
                    }

                    store.insert(insert_pos, &src);

                    // For DAAP servers, probe whether a password is required
                    // in the background and update the sidebar item.
                    if server.service_type == "daap" && server.requires_password.is_none() {
                        probe_daap_password(
                            &rt_handle,
                            &store,
                            &sidebar_selection,
                            &server.url,
                            advertised_route,
                        );
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

                    // Endpoint lookup includes the backend protocol. A Lost
                    // event must retire only the exact row and stable owner
                    // claimed by that `(backend, canonical endpoint)` pair.
                    let Some((_, source)) =
                        remote_source_at(&store, &url, &service_type)
                    else {
                        tracing::debug!(
                            backend = %service_type,
                            "Ignoring lost server event that does not own a sidebar source"
                        );
                        continue;
                    };
                    let Some(source_id) = source.source_id() else {
                        tracing::warn!("Ignoring discovered source without stable identity");
                        continue;
                    };
                    let Some(publisher) = discovery_publisher(&service_type, &url) else {
                        tracing::warn!("Ignoring lost server without canonical publisher identity");
                        continue;
                    };
                    if !remote_provenance.release(
                        &source_registry,
                        source_id,
                        crate::source_lifecycle::SourceProvenance::Discovery,
                        &publisher,
                    ) {
                        tracing::debug!(
                            backend = %service_type,
                            "Ignoring lost event without a matching discovery claim"
                        );
                        continue;
                    }

                    let remaining_provenance = source_registry
                        .snapshot(source_id)
                        .map(|snapshot| snapshot.provenance)
                        .unwrap_or_default();

                    info!(
                        backend = %service_type,
                        "Handling lost server discovery event"
                    );
                    source.set_advertised_route(None);
                    // Every constructor and resolver created from this row may
                    // have captured the now-withdrawn advertised route. Revoke
                    // that route-bound ownership even when Saved/Environment
                    // keeps the logical row visible. The lifecycle reducer
                    // owns pending/cache/playback/navigation cleanup and row
                    // demotion/removal from the resulting baseline.
                    let _ = source_registry.disconnect(source_id);
                    tracing::debug!(
                        backend = %service_type,
                        retained = !remaining_provenance.is_empty(),
                        "Withdrawn discovery route handed to lifecycle disconnect"
                    );
                }
            }
        }
    });
}

// ═══════════════════════════════════════════════════════════════════════
// AirPlay / Chromecast handlers
// ═══════════════════════════════════════════════════════════════════════

/// Add a discovered AirPlay device to the output selector.
///
/// Rows are keyed by endpoint plus device identifier, never by display name,
/// and only against other AirPlay rows: receivers sharing a name, or a
/// Chromecast sharing the name or endpoint, each keep their own row.
fn handle_airplay_found(output_list: &gtk::ListBox, server: &crate::discovery::DiscoveredServer) {
    let Some(endpoint) = airplay_endpoint(&server.url) else {
        tracing::warn!(url = %server.url, "Ignoring AirPlay receiver without a host:port endpoint");
        return;
    };
    let identity = encode_airplay_row_identity(&endpoint, server.device_id.as_deref());
    if output_rows_with_icon(output_list, AIRPLAY_ROW_ICON)
        .iter()
        .any(|row| row.widget_name() == identity.as_str())
    {
        return;
    }

    info!(
        name = %server.name,
        url = %server.url,
        device_id = ?server.device_id,
        "AirPlay receiver discovered — adding to output selector"
    );
    let row = header_bar::build_output_row(&server.name, AIRPLAY_ROW_ICON, false);
    // The output selector resolves the row's target from this identity.
    row.set_widget_name(&identity);
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
    // Extract host:port from cast://host:port URL.
    let host_port = cast_url.strip_prefix("cast://").unwrap_or(cast_url);

    // Dedup by endpoint among Chromecast rows only, never by display name.
    if output_rows_with_icon(output_list, CHROMECAST_ROW_ICON)
        .iter()
        .any(|row| row.widget_name() == host_port)
    {
        return;
    }

    info!(
        name = %cast_name,
        url = %cast_url,
        "Chromecast device discovered — adding to output selector"
    );
    let row = header_bar::build_output_row(&cast_name, CHROMECAST_ROW_ICON, false);
    row.set_widget_name(host_port);
    output_list.append(&row);
    propagate_widget_name(output_list);
}

/// Remove a lost Chromecast from the output selector.
fn handle_chromecast_lost(output_list: &gtk::ListBox, url: &str) {
    info!(url = %url, "Chromecast device lost — removing from output selector");
    let lost_hp = url.strip_prefix("cast://").unwrap_or(url);

    for row in output_rows_with_icon(output_list, CHROMECAST_ROW_ICON) {
        if row.widget_name() == lost_hp {
            output_list.remove(&row);
        }
    }
}

/// Remove a lost AirPlay device from the output selector: only the AirPlay
/// rows at its endpoint, leaving every other receiver (and a Chromecast at
/// the same endpoint) in place.
fn handle_airplay_lost(output_list: &gtk::ListBox, url: &str) {
    info!(url = %url, "AirPlay receiver lost — removing from output selector");
    let Some(endpoint) = airplay_endpoint(url) else {
        return;
    };

    for row in output_rows_with_icon(output_list, AIRPLAY_ROW_ICON) {
        if airplay_row_endpoint(&row.widget_name()) == endpoint {
            output_list.remove(&row);
        }
    }
}

/// The `host:port` a discovered AirPlay URL (`http://host[:port]`) addresses.
fn airplay_endpoint(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str().filter(|host| !host.is_empty())?;
    let port = parsed.port_or_known_default().unwrap_or(7000);
    Some(format!("{host}:{port}"))
}

// ═══════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════

pub(super) fn remote_source_at(
    store: &gtk::gio::ListStore,
    server_url: &str,
    backend_type: &str,
) -> Option<(u32, SourceObject)> {
    (0..store.n_items()).find_map(|index| {
        store
            .item(index)
            .and_downcast::<SourceObject>()
            .filter(|source| {
                same_remote_server_url(&source.server_url(), server_url)
                    && source.backend_type() == backend_type
            })
            .map(|source| (index, source))
    })
}

fn same_remote_server_url(left: &str, right: &str) -> bool {
    crate::http_security::parse_base_url(left)
        .ok()
        .zip(crate::http_security::parse_base_url(right).ok())
        .and_then(|(left, right)| {
            crate::architecture::identity::canonical_remote_base_url(&left)
                .ok()
                .zip(crate::architecture::identity::canonical_remote_base_url(&right).ok())
        })
        .is_some_and(|(left, right)| left == right)
}

/// The output selector rows of one kind, identified by their icon. The local
/// row and saved MPD rows never carry a discovered receiver's icon.
fn output_rows_with_icon(output_list: &gtk::ListBox, icon: &str) -> Vec<gtk::ListBoxRow> {
    let mut rows = Vec::new();
    let mut child = output_list.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if let Ok(row) = widget.downcast::<gtk::ListBoxRow>() {
            if row_icon_name(&row) == icon {
                rows.push(row);
            }
        }
    }
    rows
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

fn accepts_daap_probe_result(
    source: &SourceObject,
    probe_server_url: &str,
    advertised_route: &Option<AdvertisedHttpRoute>,
) -> bool {
    source.backend_type() == "daap"
        && same_remote_server_url(&source.server_url(), probe_server_url)
        && source.advertised_route() == *advertised_route
        && !source.connected()
}

/// Replace the latest discovery route while revoking any operation or session
/// that could still be bound to a withdrawn advertised address. Adding the
/// first route does not withdraw the canonical endpoint used by existing
/// work; replacing or removing an existing route does.
fn reconcile_discovery_route(
    source: &SourceObject,
    advertised_route: Option<AdvertisedHttpRoute>,
    mut disconnect: impl FnMut(crate::architecture::SourceId),
) -> bool {
    let previous = source.advertised_route();
    let changed = previous != advertised_route;
    let withdrew_route = previous.is_some() && changed;
    source.set_advertised_route(advertised_route);
    if withdrew_route {
        if let Some(source_id) = source.source_id() {
            disconnect(source_id);
        }
    }
    changed
}

/// Probe whether a DAAP server requires a password, updating the sidebar item.
fn probe_daap_password(
    rt_handle: &tokio::runtime::Handle,
    store: &gtk::gio::ListStore,
    selection: &gtk::SingleSelection,
    server_url: &str,
    advertised_route: Option<AdvertisedHttpRoute>,
) {
    let probe_url = server_url.to_string();
    let route_for_probe = advertised_route.clone();
    let store = store.clone();
    let selection = selection.clone();
    let (probe_tx, probe_rx) = async_channel::bounded::<Option<bool>>(1);

    rt_handle.spawn(async move {
        let result = crate::daap::client::DaapClient::probe_requires_password_with_route(
            &probe_url,
            route_for_probe,
        )
        .await;
        let _ = probe_tx.send(result).await;
    });

    let probe_server_url = server_url.to_string();
    glib::MainContext::default().spawn_local(async move {
        if let Ok(Some(requires_pw)) = probe_rx.recv().await {
            // Find the source in the store and update it.
            for i in 0..store.n_items() {
                if let Some(src) = store.item(i).and_downcast_ref::<SourceObject>() {
                    if accepts_daap_probe_result(src, &probe_server_url, &advertised_route) {
                        src.set_requires_password(requires_pw);
                        let src = src.clone();
                        super::window::rebind_sidebar_source(&store, &selection, i, &src, true);
                        break;
                    }
                }
            }
        }
    });
}

/// Output-row contracts, run from the crate's single GTK-initializing test
/// (browser.rs `gtk_widget_contracts_hold_on_one_session`); never standalone
/// `#[test]`s. See `ui::widget_test_session`.
#[cfg(all(test, not(target_os = "macos")))]
pub mod widget_tests {
    use super::*;

    fn server(
        name: &str,
        url: &str,
        service_type: &str,
        device_id: Option<&str>,
    ) -> crate::discovery::DiscoveredServer {
        crate::discovery::DiscoveredServer {
            name: name.to_string(),
            url: url.to_string(),
            service_type: service_type.to_string(),
            requires_password: None,
            device_id: device_id.map(str::to_string),
            advertised_route: None,
        }
    }

    fn row_names(output_list: &gtk::ListBox) -> Vec<String> {
        let mut names = Vec::new();
        let mut child = output_list.first_child();
        while let Some(widget) = child {
            if let Some(row) = widget.downcast_ref::<gtk::ListBoxRow>() {
                names.push(row.widget_name().to_string());
            }
            child = widget.next_sibling();
        }
        names
    }

    /// The selector as discovery leaves it: the local row, two AirPlay
    /// receivers that share the display name "Den", and a Chromecast also
    /// named "Den" at the first receiver's endpoint. Republications of the
    /// same receivers add nothing.
    fn selector_with_same_named_receivers() -> gtk::ListBox {
        let output_list = gtk::ListBox::new();
        let local = header_bar::build_output_row("My Computer", "audio-speakers-symbolic", true);
        local.set_widget_name("My Computer");
        output_list.append(&local);
        propagate_widget_name(&output_list);

        let den_a = server(
            "Den",
            "http://10.0.0.5:7000",
            "airplay",
            Some("AABBCCDDEEFF"),
        );
        let den_b = server(
            "Den",
            "http://10.0.0.6:7000",
            "airplay",
            Some("112233445566"),
        );
        let den_cast = server("Den", "cast://10.0.0.5:7000", "chromecast", None);
        for _ in 0..2 {
            handle_airplay_found(&output_list, &den_a);
            handle_chromecast_found(&output_list, &den_cast);
            handle_airplay_found(&output_list, &den_b);
        }
        output_list
    }

    /// Rows are keyed by identity within their own kind: same-named AirPlay
    /// receivers and a same-named Chromecast at a shared endpoint all stay
    /// listed, and republication does not duplicate any of them.
    pub fn discovered_rows_are_keyed_by_identity_not_display_name() {
        let output_list = selector_with_same_named_receivers();
        assert_eq!(
            row_names(&output_list),
            [
                "My Computer",
                "10.0.0.5:7000|AABBCCDDEEFF",
                "10.0.0.5:7000",
                "10.0.0.6:7000|112233445566",
            ]
        );
    }

    /// Losing one AirPlay receiver removes only its row: the Chromecast at
    /// the same endpoint and the other same-named receiver survive.
    pub fn airplay_loss_removes_only_that_receivers_row() {
        let output_list = selector_with_same_named_receivers();

        handle_airplay_lost(&output_list, "http://10.0.0.5:7000");
        assert_eq!(
            row_names(&output_list),
            ["My Computer", "10.0.0.5:7000", "10.0.0.6:7000|112233445566"]
        );

        handle_airplay_lost(&output_list, "http://10.0.0.6:7000");
        handle_chromecast_lost(&output_list, "cast://10.0.0.5:7000");
        assert_eq!(row_names(&output_list), ["My Computer"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};

    fn advertised_route(address: SocketAddr) -> AdvertisedHttpRoute {
        AdvertisedHttpRoute::new(
            &url::Url::parse("http://mini.local:3689").expect("origin"),
            [address],
        )
        .expect("advertised route")
    }

    #[test]
    fn same_origin_rows_are_namespaced_by_backend_protocol() {
        let store = gtk::gio::ListStore::new::<SourceObject>();
        let subsonic = SourceObject::manual(
            "Subsonic",
            "subsonic",
            "http://mini.local:4533",
            crate::architecture::SourceId::random(),
        );
        let daap = SourceObject::manual(
            "DAAP",
            "daap",
            "http://mini.local:4533",
            crate::architecture::SourceId::random(),
        );
        store.append(&subsonic);
        store.append(&daap);

        assert!(remote_source_at(&store, "http://mini.local:4533", "subsonic").is_some());
        assert!(remote_source_at(&store, "http://mini.local:4533", "daap").is_some());
        assert_ne!(subsonic.source_id(), daap.source_id());
    }

    #[test]
    fn root_url_spelling_matches_discovery_without_changing_the_owned_key() {
        let store = gtk::gio::ListStore::new::<SourceObject>();
        let source = SourceObject::manual(
            "DAAP",
            "daap",
            "HTTP://MINI.LOCAL:80/",
            crate::architecture::SourceId::random(),
        );
        store.append(&source);

        let (_, owner) = remote_source_at(&store, "http://mini.local", "daap")
            .expect("canonical root URL owner");
        assert_eq!(owner.server_url(), "HTTP://MINI.LOCAL:80/");
        assert!(accepts_daap_probe_result(
            &owner,
            "http://mini.local",
            &None
        ));
        assert!(same_remote_server_url(
            "http://mini.local/base",
            "http://MINI.local:80/base"
        ));
        assert!(!same_remote_server_url(
            "http://mini.local/base",
            "http://mini.local/other"
        ));
    }

    #[test]
    fn active_route_replacement_revokes_session_but_preserves_row_and_new_route() {
        let source = SourceObject::discovered("mini", "daap", "http://mini.local:3689");
        source.set_connected(true);
        let source_id = source.source_id().expect("stable source");
        let old_route = advertised_route(SocketAddr::from((Ipv4Addr::LOCALHOST, 3_689)));
        let new_route = advertised_route(SocketAddr::from(([127, 0, 0, 2], 3_689)));
        source.set_advertised_route(Some(old_route));
        let mut disconnected = Vec::new();

        assert!(reconcile_discovery_route(
            &source,
            Some(new_route.clone()),
            |id| disconnected.push(id),
        ));

        assert_eq!(disconnected, vec![source_id]);
        assert_eq!(source.source_id(), Some(source_id));
        assert!(
            source.connected(),
            "GTK demotion belongs to the baseline reducer"
        );
        assert_eq!(source.advertised_route(), Some(new_route));
    }

    #[test]
    fn pending_route_reduction_cancels_captured_route_without_dropping_row() {
        let source = SourceObject::discovered("mini", "daap", "http://mini.local:3689");
        source.set_connecting_generation(41);
        let source_id = source.source_id().expect("stable source");
        let old_route = advertised_route(SocketAddr::from((Ipv4Addr::LOCALHOST, 3_689)));
        source.set_advertised_route(Some(old_route));
        let mut disconnected = Vec::new();

        assert!(reconcile_discovery_route(&source, None, |id| {
            disconnected.push(id);
        }));

        assert_eq!(disconnected, vec![source_id]);
        assert_eq!(source.source_id(), Some(source_id));
        assert_eq!(source.connecting_generation(), Some(41));
        assert_eq!(source.advertised_route(), None);
    }

    #[test]
    fn first_discovery_route_does_not_revoke_canonical_endpoint_work() {
        let source = SourceObject::discovered("mini", "daap", "http://mini.local:3689");
        source.set_connecting_generation(7);
        let route = advertised_route(SocketAddr::from((Ipv4Addr::LOCALHOST, 3_689)));
        let mut disconnects = 0;

        assert!(reconcile_discovery_route(
            &source,
            Some(route.clone()),
            |_| {
                disconnects += 1;
            }
        ));

        assert_eq!(disconnects, 0);
        assert_eq!(source.advertised_route(), Some(route));
    }
}
