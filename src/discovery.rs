//! Zero-config network discovery for Subsonic, Jellyfin, Plex, and DAAP servers.
//!
//! - **Subsonic:** mDNS browse for `_subsonic._tcp.local.`
//! - **Plex:** mDNS browse for `_plexmediasvr._tcp.local.`
//! - **DAAP:** mDNS browse for `_daap._tcp.local.`
//! - **Jellyfin:** UDP broadcast `"Who is JellyfinServer?"` to `255.255.255.255:7359`
//!
//! All discovered servers are streamed to the GTK main thread via a
//! single [`async_channel`].

use std::collections::HashSet;
use std::net::UdpSocket;
use std::time::Duration;

use tracing::{debug, info, warn};

/// A server found via network discovery.
#[derive(Debug, Clone)]
pub struct DiscoveredServer {
    /// Human-readable display name from the discovery response.
    pub name: String,
    /// Full HTTP(S) URL to the server root.
    pub url: String,
    /// The backend type: `"subsonic"`, `"jellyfin"`, or `"plex"`.
    pub service_type: String,
}

/// mDNS service types we browse for.
const SUBSONIC_SERVICE: &str = "_subsonic._tcp.local.";
const PLEX_SERVICE: &str = "_plexmediasvr._tcp.local.";
const DAAP_SERVICE: &str = "_daap._tcp.local.";

/// Jellyfin UDP discovery port.
const JELLYFIN_DISCOVERY_PORT: u16 = 7359;
/// Message to broadcast for Jellyfin discovery.
const JELLYFIN_DISCOVERY_MSG: &[u8] = b"Who is JellyfinServer?";

/// Start all background discovery mechanisms. Discovered servers are
/// sent through the returned receiver. The sender stays alive as long
/// as the discovery threads are running.
///
/// Call this once from the GTK startup path and consume the receiver
/// in a `glib::MainContext::default().spawn_local()` loop.
pub fn start_discovery() -> async_channel::Receiver<DiscoveredServer> {
    let (tx, rx) = async_channel::unbounded();

    // ── mDNS discovery (Subsonic + Plex) ────────────────────────────
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            run_mdns_discovery(tx);
        });
    }

    // ── Jellyfin UDP broadcast discovery ────────────────────────────
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            run_jellyfin_udp_discovery(tx);
        });
    }

    rx
}

/// Run mDNS discovery for Subsonic and Plex services.
fn run_mdns_discovery(tx: async_channel::Sender<DiscoveredServer>) {
    let daemon = match mdns_sd::ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            warn!("mDNS daemon failed to start: {e}");
            return;
        }
    };

    // Browse for both service types.
    let subsonic_rx = match daemon.browse(SUBSONIC_SERVICE) {
        Ok(r) => Some(r),
        Err(e) => {
            warn!("mDNS browse failed for {SUBSONIC_SERVICE}: {e}");
            None
        }
    };

    let plex_rx = match daemon.browse(PLEX_SERVICE) {
        Ok(r) => Some(r),
        Err(e) => {
            warn!("mDNS browse failed for {PLEX_SERVICE}: {e}");
            None
        }
    };

    let daap_rx = match daemon.browse(DAAP_SERVICE) {
        Ok(r) => Some(r),
        Err(e) => {
            warn!("mDNS browse failed for {DAAP_SERVICE}: {e}");
            None
        }
    };

    if subsonic_rx.is_none() && plex_rx.is_none() && daap_rx.is_none() {
        warn!("No mDNS services could be browsed");
        return;
    }

    info!("mDNS discovery started for Subsonic + Plex + DAAP");

    let mut seen = HashSet::new();

    // We need to poll both receivers. Use a simple loop with the
    // subsonic receiver as the primary (it blocks), and check plex
    // periodically. Since mdns_sd uses crossbeam channels, we'll
    // use a unified approach with try_recv + sleep.
    loop {
        let mut got_event = false;

        if let Some(ref rx) = subsonic_rx {
            while let Ok(event) = rx.try_recv() {
                got_event = true;
                if let Some(server) = process_mdns_event(event, "subsonic", &mut seen) {
                    if tx.try_send(server).is_err() {
                        return;
                    }
                }
            }
        }

        if let Some(ref rx) = plex_rx {
            while let Ok(event) = rx.try_recv() {
                got_event = true;
                if let Some(server) = process_mdns_event(event, "plex", &mut seen) {
                    if tx.try_send(server).is_err() {
                        return;
                    }
                }
            }
        }

        if let Some(ref rx) = daap_rx {
            while let Ok(event) = rx.try_recv() {
                got_event = true;
                if let Some(server) = process_mdns_event(event, "daap", &mut seen) {
                    if tx.try_send(server).is_err() {
                        return;
                    }
                }
            }
        }

        if !got_event {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

/// Process a single mDNS event and return a `DiscoveredServer` if it's
/// a new, resolved service.
fn process_mdns_event(
    event: mdns_sd::ServiceEvent,
    service_type: &str,
    seen: &mut HashSet<String>,
) -> Option<DiscoveredServer> {
    match event {
        mdns_sd::ServiceEvent::ServiceResolved(info) => {
            let host = info.get_hostname().trim_end_matches('.').to_string();
            let port = info.get_port();
            let key = format!("{service_type}:{host}:{port}");

            if !seen.insert(key.clone()) {
                debug!(key, "mDNS: duplicate, skipping");
                return None;
            }

            let name = info
                .get_property_val_str("name")
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    info.get_fullname()
                        .split('.')
                        .next()
                        .unwrap_or(&host)
                        .to_string()
                });

            let scheme = if port == 443
                || info
                    .get_property_val_str("https")
                    .is_some_and(|v| v == "1" || v == "true")
            {
                "https"
            } else {
                "http"
            };

            let url = if port == 80 || port == 443 {
                format!("{scheme}://{host}")
            } else {
                format!("{scheme}://{host}:{port}")
            };

            info!(
                name = %name,
                url = %url,
                backend = %service_type,
                "mDNS: server discovered"
            );

            Some(DiscoveredServer {
                name,
                url,
                service_type: service_type.to_string(),
            })
        }
        mdns_sd::ServiceEvent::SearchStarted(svc) => {
            debug!(service = %svc, "mDNS search started");
            None
        }
        _ => None,
    }
}

/// Run Jellyfin UDP broadcast discovery.
///
/// Sends `"Who is JellyfinServer?"` to `255.255.255.255:7359` and
/// parses JSON responses containing `{ "Id", "Address", "Name" }`.
fn run_jellyfin_udp_discovery(tx: async_channel::Sender<DiscoveredServer>) {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to bind UDP socket for Jellyfin discovery: {e}");
            return;
        }
    };

    if let Err(e) = socket.set_broadcast(true) {
        warn!("Failed to enable UDP broadcast: {e}");
        return;
    }

    // Set a receive timeout so we don't block forever.
    let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));

    info!("Jellyfin UDP discovery: broadcasting to 255.255.255.255:{JELLYFIN_DISCOVERY_PORT}");

    if let Err(e) = socket.send_to(
        JELLYFIN_DISCOVERY_MSG,
        format!("255.255.255.255:{JELLYFIN_DISCOVERY_PORT}"),
    ) {
        warn!("Failed to send Jellyfin discovery broadcast: {e}");
        return;
    }

    let mut seen = HashSet::new();
    let mut buf = [0u8; 4096];

    // Read responses until timeout.
    while let Ok((len, _addr)) = socket.recv_from(&mut buf) {
        let response = String::from_utf8_lossy(&buf[..len]);
        debug!(response = %response, "Jellyfin UDP response");

        match serde_json::from_str::<crate::jellyfin::api::JellyfinDiscoveryResponse>(&response) {
            Ok(discovery) => {
                if seen.insert(discovery.address.clone()) {
                    info!(
                        name = %discovery.name,
                        url = %discovery.address,
                        "Jellyfin server discovered via UDP"
                    );

                    let server = DiscoveredServer {
                        name: discovery.name,
                        url: discovery.address,
                        service_type: "jellyfin".to_string(),
                    };

                    if tx.try_send(server).is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                debug!(error = %e, "Failed to parse Jellyfin discovery response");
            }
        }
    }

    info!("Jellyfin UDP discovery complete");
}
