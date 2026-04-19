//! Zero-config network discovery for Subsonic, Jellyfin, Plex, DAAP servers,
//! and AirPlay (RAOP) audio receivers.
//!
//! - **Subsonic:** mDNS browse for `_subsonic._tcp.local.`
//! - **Plex:** mDNS browse for `_plexmediasvr._tcp.local.`
//! - **DAAP:** mDNS browse for `_daap._tcp.local.`
//! - **AirPlay:** mDNS browse for `_raop._tcp.local.` + `_airplay._tcp.local.`
//! - **Chromecast:** mDNS browse for `_googlecast._tcp.local.`
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
/// AirPlay (RAOP) receivers for audio output streaming.
const RAOP_SERVICE: &str = "_raop._tcp.local.";
/// AirPlay 2 receivers — newer devices (HomePod, Apple TV, etc.) advertise
/// via `_airplay._tcp.local.` instead of (or in addition to) legacy RAOP.
const AIRPLAY2_SERVICE: &str = "_airplay._tcp.local.";
/// Chromecast (Cast V2) devices for audio output streaming.
const CHROMECAST_SERVICE: &str = "_googlecast._tcp.local.";

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

    let raop_rx = match daemon.browse(RAOP_SERVICE) {
        Ok(r) => Some(r),
        Err(e) => {
            warn!("mDNS browse failed for {RAOP_SERVICE}: {e}");
            None
        }
    };

    let airplay2_rx = match daemon.browse(AIRPLAY2_SERVICE) {
        Ok(r) => Some(r),
        Err(e) => {
            warn!("mDNS browse failed for {AIRPLAY2_SERVICE}: {e}");
            None
        }
    };

    let chromecast_rx = match daemon.browse(CHROMECAST_SERVICE) {
        Ok(r) => Some(r),
        Err(e) => {
            warn!("mDNS browse failed for {CHROMECAST_SERVICE}: {e}");
            None
        }
    };

    if subsonic_rx.is_none()
        && plex_rx.is_none()
        && daap_rx.is_none()
        && raop_rx.is_none()
        && airplay2_rx.is_none()
        && chromecast_rx.is_none()
    {
        warn!("No mDNS services could be browsed");
        return;
    }

    info!("mDNS discovery started for Subsonic + Plex + DAAP + AirPlay + AirPlay2 + Chromecast");

    // `seen` maps `key` → `url` so we can reconstruct the URL on removal.
    let mut seen: HashMap<String, String> = HashMap::new();

    // ── macOS re-browse support ──────────────────────────────────────
    // On macOS, the first launch triggers a Local Network permission
    // prompt.  The mDNS daemon is already running its browse() calls,
    // but macOS blocks mDNS traffic until the user grants permission.
    // After granting, the daemon doesn't automatically re-browse.
    //
    // We periodically re-issue browse() calls if no servers have been
    // found yet, up to a maximum number of retries.
    #[cfg(target_os = "macos")]
    const REBROWSE_INTERVAL: Duration = Duration::from_secs(30);
    #[cfg(target_os = "macos")]
    const REBROWSE_MAX_ATTEMPTS: u32 = 3;
    #[cfg(target_os = "macos")]
    let mut rebrowse_attempts: u32 = 0;
    #[cfg(target_os = "macos")]
    let mut last_rebrowse = std::time::Instant::now();

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

        if let Some(ref rx) = raop_rx {
            while let Ok(event) = rx.try_recv() {
                got_event = true;
                process_mdns_event(event, "airplay", &mut seen, &tx);
            }
        }

        if let Some(ref rx) = airplay2_rx {
            while let Ok(event) = rx.try_recv() {
                got_event = true;
                // AirPlay 2 devices are treated the same as legacy RAOP
                // for discovery purposes — both use "airplay" service type.
                // The `seen` HashMap deduplicates by host:port, so devices
                // advertising both _raop._tcp and _airplay._tcp only appear once.
                process_mdns_event(event, "airplay", &mut seen, &tx);
            }
        }

        if let Some(ref rx) = chromecast_rx {
            while let Ok(event) = rx.try_recv() {
                got_event = true;
                process_chromecast_event(event, &mut seen, &tx);
            }
        }

        // ── macOS: re-browse if no servers found yet ─────────────────
        // After the user grants Local Network permission, the daemon
        // needs fresh browse() calls to discover services.
        #[cfg(target_os = "macos")]
        {
            if seen.is_empty()
                && rebrowse_attempts < REBROWSE_MAX_ATTEMPTS
                && last_rebrowse.elapsed() >= REBROWSE_INTERVAL
            {
                rebrowse_attempts += 1;
                last_rebrowse = std::time::Instant::now();
                info!(
                    attempt = rebrowse_attempts,
                    "macOS: re-browsing mDNS services (no servers found yet)"
                );

                // Re-issue browse for each service type.  The daemon
                // is still running; this just re-registers the queries.
                let _ = daemon.browse(SUBSONIC_SERVICE);
                let _ = daemon.browse(PLEX_SERVICE);
                let _ = daemon.browse(DAAP_SERVICE);
                let _ = daemon.browse(RAOP_SERVICE);
                let _ = daemon.browse(AIRPLAY2_SERVICE);
                let _ = daemon.browse(CHROMECAST_SERVICE);
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
            let raw_host = info.get_hostname().trim_end_matches('.').to_string();
            let port = info.get_port();

            // Strip Avahi conflict-resolution suffix (e.g., "myhost-2" → "myhost").
            // Avahi appends "-N" when it discovers a naming conflict during late
            // service registration.  We normalise to the base hostname so the
            // dedup key matches the original and the duplicate is silently dropped.
            let host = strip_avahi_suffix(&raw_host);

            let key = format!("{service_type}:{host}:{port}");

            if seen.contains_key(&key) {
                debug!(key, "mDNS: duplicate, skipping");
                return;
            }

            let raw_name = info
                .get_property_val_str("name")
                .map(|s| {
                    // Strip Avahi conflict suffix from the display name.
                    // Some services (e.g. Navidrome) embed the hostname in
                    // their TXT name like "Navidrome (hostname-2)".  Clean
                    // the suffix both from bare names and parenthesized hostnames.
                    strip_avahi_name_suffix(s)
                })
                .unwrap_or_else(|| {
                    let raw_name = info
                        .get_fullname()
                        .split('.')
                        .next()
                        .unwrap_or(&host)
                        .to_string();
                    // Use strip_avahi_name_suffix to handle both bare
                    // hostnames and parenthesized patterns like
                    // "Navidrome (nr400-2)".
                    strip_avahi_name_suffix(&raw_name)
                });

            // AirPlay / RAOP devices often use "MAC@DeviceName" as
            // their mDNS instance name (e.g. "8EE58A500A56@Rear Lounge TV").
            // Strip the MAC prefix for a cleaner display name.
            let name = if service_type == "airplay" {
                strip_airplay_mac_prefix(&raw_name)
            } else {
                raw_name
            };

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
        for (address, (name, misses)) in &mut known {
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

// ── Chromecast mDNS event processing ────────────────────────────────────

/// Process a Chromecast mDNS event.
///
/// Chromecast devices use `_googlecast._tcp.local.` and store the
/// user-friendly device name in the `fn` TXT record field.  The Cast V2
/// protocol port (typically 8009) comes from the SRV record.
///
/// Unlike other mDNS services, Chromecast URLs are formatted as
/// `cast://<host>:<port>` (not HTTP) because the output selector
/// extracts host:port directly.
fn process_chromecast_event(
    event: mdns_sd::ServiceEvent,
    seen: &mut HashMap<String, String>,
    tx: &async_channel::Sender<DiscoveryEvent>,
) {
    let service_type = "chromecast";

    match event {
        mdns_sd::ServiceEvent::ServiceResolved(info) => {
            let raw_host = info.get_hostname().trim_end_matches('.').to_string();
            let port = info.get_port();
            let host = strip_avahi_suffix(&raw_host);

            let key = format!("{service_type}:{host}:{port}");

            if seen.contains_key(&key) {
                debug!(key, "mDNS: Chromecast duplicate, skipping");
                return;
            }

            // Extract friendly name from the `fn` TXT record field.
            // Chromecast devices always publish this.  Fall back to the
            // mDNS instance name if `fn` is missing.
            let name = info
                .get_property_val_str("fn")
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    info.get_fullname()
                        .split('.')
                        .next()
                        .unwrap_or(&host)
                        .to_string()
                });

            // Use a cast:// URL scheme so the output selector can
            // distinguish Chromecast URLs from HTTP server URLs.
            let url = format!("cast://{host}:{port}");

            seen.insert(key.clone(), url.clone());

            info!(
                name = %name,
                url = %url,
                "mDNS: Chromecast device discovered"
            );

            let _ = tx.try_send(DiscoveryEvent::Found(DiscoveredServer {
                name,
                url,
                service_type: service_type.to_string(),
                requires_password: None,
            }));
        }

        mdns_sd::ServiceEvent::ServiceRemoved(_svc_type, fullname) => {
            let instance = fullname.split('.').next().unwrap_or("").to_string();

            let mut removed_url = None;
            let mut removed_key = None;
            for (key, url) in seen.iter() {
                if key.starts_with(&format!("{service_type}:")) {
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
                    fullname = %fullname,
                    "mDNS: Chromecast device removed"
                );

                let _ = tx.try_send(DiscoveryEvent::Lost {
                    url,
                    service_type: service_type.to_string(),
                });
            } else {
                debug!(
                    fullname = %fullname,
                    "mDNS: Chromecast ServiceRemoved for unknown device, ignoring"
                );
            }
        }

        mdns_sd::ServiceEvent::SearchStarted(svc) => {
            debug!(service = %svc, "mDNS Chromecast search started");
        }

        _ => {}
    }
}

// ── AirPlay name helpers ────────────────────────────────────────────────

/// Strip the `MAC@` prefix from AirPlay / RAOP device names.
///
/// AirPlay devices often register their mDNS instance name as
/// `HEXMAC@FriendlyName` (e.g. `"8EE58A500A56@Rear Lounge TV"`).
/// This function strips the MAC prefix to produce just `"Rear Lounge TV"`.
///
/// If no `@` is present or the prefix doesn't look like a hex MAC,
/// the name is returned unchanged.
fn strip_airplay_mac_prefix(name: &str) -> String {
    if let Some(at_pos) = name.find('@') {
        let prefix = &name[..at_pos];
        // MAC addresses are 12 hex characters (6 bytes, no separators)
        // or sometimes with colons/dashes.  Accept any all-hex prefix
        // of reasonable length (≥ 6 chars).
        if prefix.len() >= 6 && prefix.chars().all(|c| c.is_ascii_hexdigit()) {
            return name[at_pos + 1..].to_string();
        }
    }
    name.to_string()
}

// ── Avahi hostname helpers ──────────────────────────────────────────────

/// Strip the Avahi conflict-resolution suffix from a hostname.
///
/// When Avahi detects a naming conflict during late service registration
/// it appends `-2`, `-3`, etc. to the hostname.  This function strips
/// that suffix so we can dedup against the original hostname.
///
/// Examples:
/// - `"myhost-2"` → `"myhost"`
/// - `"myhost-12"` → `"myhost"`
/// - `"myhost"` → `"myhost"` (unchanged)
/// - `"my-host"` → `"my-host"` (unchanged — no trailing digits)
fn strip_avahi_suffix(hostname: &str) -> String {
    // Match the pattern: ends with "-" followed by one or more digits.
    if let Some(dash_pos) = hostname.rfind('-') {
        let suffix = &hostname[dash_pos + 1..];
        // Only strip if the suffix is purely numeric AND >= 2
        // (Avahi conflict suffixes start at -2).
        if !suffix.is_empty()
            && suffix.chars().all(|c| c.is_ascii_digit())
            && suffix.parse::<u32>().is_ok_and(|n| n >= 2)
        {
            return hostname[..dash_pos].to_string();
        }
    }
    hostname.to_string()
}

/// Strip conflict-resolution suffixes from a service display name.
///
/// Handles three patterns:
/// 1. Bare hostname: `"nr400-2"` → `"nr400"` (Avahi `-N` suffix)
/// 2. Parenthesized hostname: `"Navidrome (nr400-2)"` → `"Navidrome (nr400)"` (Avahi)
/// 3. Bare numeric suffix: `"Rear Lounge TV (2)"` → `"Rear Lounge TV"` (Windows mDNS)
fn strip_avahi_name_suffix(name: &str) -> String {
    // Check for parenthesized pattern: "Something (…)"
    if let (Some(open), Some(close)) = (name.rfind('('), name.rfind(')')) {
        if open < close {
            let inside = name[open + 1..close].trim();

            // Windows mDNS conflict pattern: bare number ≥ 2, e.g. "(2)", "(3)".
            // Strip the entire parenthesized suffix including any preceding space.
            if inside.chars().all(|c| c.is_ascii_digit())
                && inside.parse::<u32>().is_ok_and(|n| n >= 2)
            {
                return name[..open].trim_end().to_string();
            }

            // Avahi conflict pattern: hostname with -N suffix inside parens.
            let cleaned = strip_avahi_suffix(inside);
            if cleaned != inside {
                return format!("{}({}){}", &name[..open], cleaned, &name[close + 1..]);
            }
        }
    }
    // Fall back to stripping the bare name (Avahi -N on hostname).
    strip_avahi_suffix(name)
}

#[cfg(test)]
mod tests {
    use super::{strip_airplay_mac_prefix, strip_avahi_name_suffix, strip_avahi_suffix};

    #[test]
    fn test_strip_avahi_suffix() {
        assert_eq!(strip_avahi_suffix("myhost-2"), "myhost");
        assert_eq!(strip_avahi_suffix("myhost-12"), "myhost");
        assert_eq!(strip_avahi_suffix("myhost"), "myhost");
        assert_eq!(strip_avahi_suffix("my-host"), "my-host");
        assert_eq!(strip_avahi_suffix("my-host-3"), "my-host");
        assert_eq!(strip_avahi_suffix("host-1"), "host-1"); // -1 is not a conflict suffix
        assert_eq!(strip_avahi_suffix("host-0"), "host-0"); // -0 is not a conflict suffix
    }

    #[test]
    fn test_strip_avahi_name_suffix() {
        assert_eq!(
            strip_avahi_name_suffix("Navidrome (nr400-2)"),
            "Navidrome (nr400)"
        );
        assert_eq!(
            strip_avahi_name_suffix("Navidrome (nr400)"),
            "Navidrome (nr400)"
        );
        assert_eq!(strip_avahi_name_suffix("nr400-2"), "nr400");
        assert_eq!(strip_avahi_name_suffix("nr400"), "nr400");
        assert_eq!(
            strip_avahi_name_suffix("My Server (my-host-3)"),
            "My Server (my-host)"
        );
        // Windows mDNS conflict suffix: bare number in parens.
        assert_eq!(
            strip_avahi_name_suffix("Rear Lounge TV (2)"),
            "Rear Lounge TV"
        );
        assert_eq!(strip_avahi_name_suffix("Device (3)"), "Device");
        // (1) is not a conflict suffix — unchanged.
        assert_eq!(strip_avahi_name_suffix("Device (1)"), "Device (1)");
        // (0) is not a conflict suffix — unchanged.
        assert_eq!(strip_avahi_name_suffix("Device (0)"), "Device (0)");
    }

    #[test]
    fn test_strip_airplay_mac_prefix() {
        // Standard RAOP format: 12-char hex MAC @ device name.
        assert_eq!(
            strip_airplay_mac_prefix("8EE58A500A56@Rear Lounge TV"),
            "Rear Lounge TV"
        );
        assert_eq!(
            strip_airplay_mac_prefix("8A79AB138BA9@main bedroom TV"),
            "main bedroom TV"
        );
        // No @ sign — unchanged.
        assert_eq!(
            strip_airplay_mac_prefix("Living Room HomePod"),
            "Living Room HomePod"
        );
        // @ present but prefix is not hex — unchanged.
        assert_eq!(strip_airplay_mac_prefix("user@hostname"), "user@hostname");
        // Short hex prefix (< 6 chars) — unchanged.
        assert_eq!(strip_airplay_mac_prefix("ABCD@Device"), "ABCD@Device");
        // Exactly 6 hex chars — stripped.
        assert_eq!(strip_airplay_mac_prefix("AABBCC@Speaker"), "Speaker");
    }
}
