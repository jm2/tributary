//! UI module — GTK4 / libadwaita interface components.

pub mod album_art;
pub mod album_art_cache;
pub mod album_art_cell;
pub mod album_pane_art;
pub mod browser;
pub mod context_menu;
pub mod discovery_handler;
pub mod dummy_data;
pub mod folder_browser;
pub mod header_bar;
mod library_commands;
pub mod objects;
pub mod open_files;
pub mod output_dialogs;
pub mod output_switch;
pub mod persistence;
pub mod playback;
pub mod playlist_actions;
pub mod playlist_editor;
pub mod playlist_projection;
pub mod preferences;
pub mod properties_dialog;
pub mod radio;
pub mod removable_media;
mod rhythmbox_migration;
pub mod root_trust;
pub mod server_dialogs;
mod server_playlist_recovery;
mod server_playlists;
pub mod sidebar;
pub mod source_connect;
pub mod source_navigation;
pub mod tracklist;
#[cfg(target_os = "windows")]
pub mod win32_snap;
pub mod window;
pub mod window_state;

// GTK must be initialized exactly once per process and used from a single
// thread afterwards, but libtest runs each `#[test]` function on its own
// worker thread. Two widget tests that both pass their display gate can
// therefore reach `gtk::init()` concurrently — or on successive worker
// threads — which panics or races GTK's single-threaded state on any
// machine with a real display session. Headless CI never sees this because
// `gtk::init` fails there and every test skips. Every widget test in this
// crate funnels through [`widget_test_session::acquire`], which
// serializes them behind one process-wide mutex held across `gtk::init()`
// AND all widget construction/assertions, while keeping the display-gated
// skip messages on machines without a display session.
//
// Note that the mutex SERIALIZES but does not give THREAD AFFINITY: a
// second GTK-initializing `#[test]` still runs on its own worker thread
// and, seeing `gtk::is_initialized()` already true, would skip init and
// construct widgets off the initializing thread — tripping gtk-rs
// main-thread checks (2026-09-09 review rejection, PR #179). The crate
// therefore keeps exactly ONE GTK-initializing `#[test]` (the consolidated
// widget-contracts test in `browser.rs`); a new GTK-touching contract must
// join that test's body as a helper, never become a second `#[test]`.
#[cfg(all(test, not(target_os = "macos")))]
pub mod widget_test_session {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// The process-wide GTK test lock.
    fn lock() -> &'static Mutex<()> {
        static GTK_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        GTK_TEST_LOCK.get_or_init(|| Mutex::new(()))
    }

    /// Acquires the process-wide GTK test lock, then — still holding it —
    /// applies the display-session gate and initializes GTK at most once
    /// per process.
    ///
    /// Returns the guard that the caller MUST hold across every widget
    /// construction and assertion in the test body (that is the whole
    /// point of the lock: a second GTK-initializing test must not touch
    /// GTK state while the first is mid-flight, on this or any other
    /// worker thread). Returns `None` — releasing the lock — when the
    /// caller must skip:
    ///
    /// - no display session (`$WAYLAND_DISPLAY`/`$DISPLAY` both unset):
    ///   headless GTK can still come up via its Broadway fallback, and a
    ///   test process that initialized GTK without a real display session
    ///   segfaults in GTK teardown at exit (observed as SIGSEGV after all
    ///   tests passed on headless Linux CI, run 33921896331), so the gate
    ///   fires BEFORE any GTK call;
    /// - GTK cannot initialize (no display server reachable): same skip
    ///   path, with the reason printed.
    ///
    /// `label` names the calling test so the printed skip reason stays
    /// attributable to the test that produced it.
    pub fn acquire(label: &str) -> Option<MutexGuard<'static, ()>> {
        // A panicked earlier test must not cascade into every later widget
        // test: the data the guard protects is stateless (just ordering),
        // so a poisoned lock is safe to carry on from.
        let guard = lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
            eprintln!(
                "{label}: no display session ($WAYLAND_DISPLAY/$DISPLAY \
                 unset); skipping. Re-run inside a desktop session to \
                 exercise the contract."
            );
            return None;
        }

        if !gtk::is_initialized() {
            if let Err(e) = gtk::init() {
                eprintln!(
                    "{label}: GTK unavailable ({e}); skipping. Re-run on a \
                     box with a display session (or under a Broadway \
                     headless server) to exercise the contract."
                );
                return None;
            }
        }

        Some(guard)
    }
}
