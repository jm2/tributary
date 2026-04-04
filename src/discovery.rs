//! Zero-config network discovery for Subsonic, Jellyfin, Plex, and DAAP servers.
//!
//! - **Subsonic:** mDNS browse for `_subsonic._tcp.local.`
//! - **Plex:** mDNS browse for `_plexmediasvr._tcp.local.`
//! - **DAAP:** mDNS browse for `_daap._tcp.local.`
//! - **Jellyfin:** UDP broadcast `"Who is JellyfinServer?"` to `255.255.255.255:7359`
//!
//! All discovered servers are streamed to the GTK main thread via a
//! single [`async_channel`] carrying [`DiscoveryEvent`] messages.

use std::collections::{HashMap, HashSet};
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
    /// The backend type: `"subsonic"`, `"jellyfin"`, `"plex"`, or `"daap"`.
    pub service_type: String,
    /// Whether this server requires a password.
    /// `Some(true)` = password required, `Some(false)` = open,
    /// `None` = unknown (probe not yet completed or not applicable).
    pub requires_password: Option<bool>,
}

/// Events sent from the discovery background threads to the GTK main thread.
#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    /// A new server was found on the network.
    Found(DiscoveredServer),
    /// A previously-discovered server is no longer available.
    Lost {
        /// The URL of the server that went away.
        url: String,
        /// The backend type (`"subsonic"`, `"jellyfin"`, `"plex"`, `"daap"`).
        service_type: String,
    },
}

/// mDNS service types we browse for.
const SUBSONIC_SERVICE: &str = "_subsonic._tcp.local.";
const PLEX_SERVICE: &str = "_plexmediasvr._tcp.local.";
const DAAP_SERVICE: &str = "_daap._tcp.local.";

/// Jellyfin UDP discovery port.
const JELLYFIN_DISCOVERY_PORT: u16 = 7359;
/// Message to broadcast for Jellyfin discovery.
const JELLYFIN_DISCOVERY_MSG: &[u8] = b"Who is JellyfinServer?";

/// Jellyfin UDP re-broadcast interval.
const JELLYFIN_BROADCAST_INTERVAL: Duration = Duration::from_secs(60);
/// Number of consecutive missed cycles before declaring a Jellyfin server lost.
const JELLYFIN_MISS_THRESHOLD: u32 = 3;

/// Start all background discovery mechanisms. Discovered servers are
/// sent through the returned receiver. The sender stays alive as long
/// as the discovery threads are running.
///
/// Call this once from the GTK startup path and consume the receiver
/// in a `glib::MainContext::default().spawn_local()` loop.
pub fn start_discovery() -> async_channel::Receiver<DiscoveryEvent> {
    let (tx, rx) = async_channel::unbounded();

    // ── mDNS discovery (Subsonic + Plex + DAAP) ─────────────────────
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            run_mdns_discovery(tx);
        });
    }

    // ── Jellyfin UDP broadcast discovery (periodic) ─────────────────
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            run_jellyfin_udp_discovery(tx);
        });
    }

    rx
}

/// Run mDNS discovery for Subsonic, Plex, and DAAP services.
///
/// Handles both `ServiceResolved` (found) and `ServiceRemoved` (lost)
/// events, enabling dynamic sidebar updates.
fn run_mdns_discovery(tx: async_channel::Sender<DiscoveryEvent>) {
    let daemon = match mdns_sd::ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            warn!("mDNS daemon failed to start: {e}");
            return;
        }
    };

    // Browse for all service types.
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

    // `seen` maps `key` → `url` so we can reconstruct the URL on removal.
    let mut seen: HashMap<String, String> = HashMap::new();

    loop {
        let mut got_event = false;

        if let Some(ref rx) = subsonic_rx {
            while let Ok(event) = rx.try_recv() {
                got_event = true;
                process_mdns_event(event, "subsonic", &mut seen, &tx);
            }
        }

        if let Some(ref rx) = plex_rx {
            while let Ok(event) = rx.try_recv() {
                got_event = true;
                process_mdns_event(event, "plex", &mut seen, &tx);
            }
        }

        if let Some(ref rx) = daap_rx {
            while let Ok(event) = rx.try_recv() {
                got_event = true;
                process_mdns_event(event, "daap", &mut seen, &tx);
            }
        }

        if !got_event {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

/// Process a single mDNS event. Sends `DiscoveryEvent::Found` for new
/// resolved services and `DiscoveryEvent::Lost` when a service is removed.
fn process_mdns_event(
    event: mdns_sd::ServiceEvent,
    service_type: &str,
    seen: &mut HashMap<String, String>,
    tx: &async_channel::Sender<DiscoveryEvent>,
) {
    match event {
        mdns_sd::ServiceEvent::ServiceResolved(info) => {
            let host = info.get_hostname().trim_end_matches('.').to_string();
            let port = info.get_port();
            let key = format!("{service_type}:{host}:{port}");

            if seen.contains_key(&key) {
                debug!(key, "mDNS: duplicate, skipping");
                return;
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

            seen.insert(key.clone(), url.clone());

            info!(
                name = %name,
                url = %url,
                backend = %service_type,
                "mDNS: server discovered"
            );

            let _ = tx.try_send(DiscoveryEvent::Found(DiscoveredServer {
                name,
                url,
                service_type: service_type.to_string(),
                requires_password: None,
            }));
        }

        mdns_sd::ServiceEvent::ServiceRemoved(_svc_type, fullname) => {
            // The fullname looks like "MyServer._subsonic._tcp.local."
            // We need to find the matching key in our `seen` map.
            // Extract the instance name (everything before the first service type segment).
            let instance = fullname.split('.').next().unwrap_or("").to_string();

            // Find and remove the matching entry from `seen`.
            let mut removed_url = None;
            let mut removed_key = None;
            for (key, url) in seen.iter() {
                if key.starts_with(&format!("{service_type}:")) {
                    // Check if this key's host portion matches the instance name,
                    // or just match by prefix since we may not have the exact host.
                    // The fullname contains the instance name which was used as the
                    // display name or host. Try to match by checking if the key
                    // contains relevant info.
                    //
                    // More robust: iterate all keys for this service_type and
                    // check if the fullname contains the host from the key.
                    let key_host = key
                        .strip_prefix(&format!("{service_type}:"))
                        .unwrap_or("")
                        .split(':')
                        .next()
                        .unwrap_or("");
                    if fullname.contains(key_host) || instance == key_host {
                        removed_url = Some(url.clone());
                        removed_key = Some(key.clone());
                        break;
                    }
                }
            }

            if let (Some(key), Some(url)) = (removed_key, removed_url) {
                seen.remove(&key);

                info!(
                    url = %url,
                    backend = %service_type,
                    fullname = %fullname,
                    "mDNS: server removed"
                );

                let _ = tx.try_send(DiscoveryEvent::Lost {
                    url,
                    service_type: service_type.to_string(),
                });
            } else {
                debug!(
                    fullname = %fullname,
                    backend = %service_type,
                    "mDNS: ServiceRemoved for unknown service, ignoring"
                );
            }
        }

        mdns_sd::ServiceEvent::SearchStarted(svc) => {
            debug!(service = %svc, "mDNS search started");
        }

        _ => {}
    }
}

/// Run Jellyfin UDP broadcast discovery with periodic re-broadcasts.
///
/// Sends `"Who is JellyfinServer?"` to `255.255.255.255:7359` every
/// 60 seconds. Tracks which servers respond each cycle. After 3
/// consecutive missed cycles, sends `DiscoveryEvent::Lost`.
fn run_jellyfin_udp_discovery(tx: async_channel::Sender<DiscoveryEvent>) {
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

    // Set a receive timeout for each read cycle.
    let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));

    info!(
        "Jellyfin UDP discovery: periodic broadcast to 255.255.255.255:{JELLYFIN_DISCOVERY_PORT}"
    );

    // Track known servers: address → (name, consecutive_misses).
    let mut known: HashMap<String, (String, u32)> = HashMap::new();

    loop {
        // ── Send broadcast ──────────────────────────────────────────
        if let Err(e) = socket.send_to(
            JELLYFIN_DISCOVERY_MSG,
            format!("255.255.255.255:{JELLYFIN_DISCOVERY_PORT}"),
        ) {
            warn!("Failed to send Jellyfin discovery broadcast: {e}");
            // Sleep and retry next cycle.
            std::thread::sleep(JELLYFIN_BROADCAST_INTERVAL);
            continue;
        }

        // ── Collect responses for this cycle ────────────────────────
        let mut responded_this_cycle: HashSet<String> = HashSet::new();
        let mut buf = [0u8; 4096];

        // Read responses until the 5-second timeout fires.
        while let Ok((len, _addr)) = socket.recv_from(&mut buf) {
            let response = String::from_utf8_lossy(&buf[..len]);
            debug!(response = %response, "Jellyfin UDP response");

            match serde_json::from_str::<crate::jellyfin::api::JellyfinDiscoveryResponse>(&response)
            {
                Ok(discovery) => {
                    responded_this_cycle.insert(discovery.address.clone());

                    if !known.contains_key(&discovery.address) {
                        // New server discovered.
                        info!(
                            name = %discovery.name,
                            url = %discovery.address,
                            "Jellyfin server discovered via UDP"
                        );

                        let _ = tx.try_send(DiscoveryEvent::Found(DiscoveredServer {
                            name: discovery.name.clone(),
                            url: discovery.address.clone(),
                            service_type: "jellyfin".to_string(),
                            requires_password: None,
                        }));
                    }

                    // Reset miss counter (or insert new entry).
                    known.insert(discovery.address, (discovery.name, 0));
                }
                Err(e) => {
                    debug!(error = %e, "Failed to parse Jellyfin discovery response");
                }
            }
        }

        // ── Update miss counters and remove stale servers ───────────
        let mut to_remove = Vec::new();
        for (address, (name, misses)) in known.iter_mut() {
            if responded_this_cycle.contains(address) {
                *misses = 0;
            } else {
                *misses += 1;
                debug!(
                    url = %address,
                    name = %name,
                    misses = *misses,
                    "Jellyfin server missed a broadcast cycle"
                );

                if *misses >= JELLYFIN_MISS_THRESHOLD {
                    info!(
                        url = %address,
                        name = %name,
                        "Jellyfin server lost after {JELLYFIN_MISS_THRESHOLD} missed cycles"
                    );
                    to_remove.push(address.clone());
                }
            }
        }

        for address in to_remove {
            known.remove(&address);
            let _ = tx.try_send(DiscoveryEvent::Lost {
                url: address,
                service_type: "jellyfin".to_string(),
            });
        }

        // ── Wait before next broadcast cycle ────────────────────────
        std::thread::sleep(JELLYFIN_BROADCAST_INTERVAL);
    }
}
