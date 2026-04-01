//! Zero-config mDNS service discovery for Subsonic-compatible servers.
//!
//! Runs [`mdns_sd::ServiceDaemon`] in the background and streams
//! [`DiscoveredServer`] structs to the GTK main thread via an
//! [`async_channel`].

use std::collections::HashSet;

use tracing::{debug, info, warn};

/// A server found via mDNS.
#[derive(Debug, Clone)]
pub struct DiscoveredServer {
    /// Human-readable display name from the mDNS TXT record or service name.
    pub name: String,
    /// Full HTTP(S) URL to the Subsonic REST API root.
    pub url: String,
    /// The mDNS service type that matched (e.g. `_subsonic._tcp`).
    #[allow(dead_code)]
    pub service_type: String,
}

/// Service types we browse for.
const SUBSONIC_SERVICE: &str = "_subsonic._tcp.local.";

/// Start background mDNS discovery.  Discovered servers are sent through
/// the returned receiver.  The sender stays alive as long as the daemon
/// thread is running.
///
/// Call this once from the GTK startup path and consume the receiver
/// in a `glib::MainContext::default().spawn_local()` loop.
pub fn start_discovery() -> async_channel::Receiver<DiscoveredServer> {
    let (tx, rx) = async_channel::unbounded();

    std::thread::spawn(move || {
        let daemon = match mdns_sd::ServiceDaemon::new() {
            Ok(d) => d,
            Err(e) => {
                warn!("mDNS daemon failed to start: {e}");
                return;
            }
        };

        let receiver = match daemon.browse(SUBSONIC_SERVICE) {
            Ok(r) => r,
            Err(e) => {
                warn!("mDNS browse failed for {SUBSONIC_SERVICE}: {e}");
                return;
            }
        };

        info!("mDNS discovery started for {SUBSONIC_SERVICE}");

        let mut seen = HashSet::new();

        // Block on the mDNS receiver — this thread is dedicated.
        while let Ok(event) = receiver.recv() {
            match event {
                mdns_sd::ServiceEvent::ServiceResolved(info) => {
                    let host = info.get_hostname().trim_end_matches('.').to_string();
                    let port = info.get_port();
                    let key = format!("{host}:{port}");

                    if !seen.insert(key.clone()) {
                        debug!(key, "mDNS: duplicate, skipping");
                        continue;
                    }

                    // Prefer a name from the TXT record, fall back to instance name.
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

                    // Determine scheme from TXT or default to http.
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

                    info!(name = %name, url = %url, "mDNS: Subsonic server discovered");

                    let server = DiscoveredServer {
                        name,
                        url,
                        service_type: SUBSONIC_SERVICE.to_string(),
                    };

                    if tx.try_send(server).is_err() {
                        break; // Receiver dropped — app is shutting down.
                    }
                }
                mdns_sd::ServiceEvent::SearchStarted(_) => {
                    debug!("mDNS search started");
                }
                _ => {}
            }
        }

        info!("mDNS discovery thread exiting");
    });

    rx
}
