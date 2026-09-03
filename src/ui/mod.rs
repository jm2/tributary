//! UI module — GTK4 / libadwaita interface components.

pub mod album_art;
pub mod browser;
pub mod context_menu;
pub mod discovery_handler;
pub mod dummy_data;
pub mod equalizer;
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
// crate funnels through [`widget_test_session::with_session`], which
// serializes them behind one process-wide mutex held across `gtk::init()`
// AND all widget construction/assertions, while keeping the display-gated
// skip messages on machines without a display session. The session also
// pushes a dedicated main context as the thread default, so widget
// realization never dispatches sources other tests left on the
// process-global default context (see `acquire`).
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
    use std::sync::{Mutex, OnceLock};

    /// The process-wide GTK test lock.
    fn lock() -> &'static Mutex<()> {
        static GTK_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        GTK_TEST_LOCK.get_or_init(|| Mutex::new(()))
    }

    /// Acquires the process-wide GTK test lock, then — still holding it —
    /// applies the display-session gate, initializes GTK at most once per
    /// process, and runs `body` with a **dedicated main context** pushed
    /// as the calling thread's thread-default context. The push is what
    /// makes main-context pumping safe inside the session: parallel
    /// non-widget tests (the audio suite in particular) construct players
    /// whose production code attaches thread-affine glib sources —
    /// `timeout_add_local` position timers, debounced
    /// `timeout_add_local_once` persistence saves — to the process-global
    /// default context, and those tests never run a main loop, so the
    /// sources stay pending there. Any test that pumped the global default
    /// context would dispatch a foreign source on its own worker thread,
    /// tripping glib's `ThreadGuard` ("Value accessed from different
    /// thread than where it was created") inside a non-unwinding C
    /// trampoline and aborting the entire test binary (tr-8wtab). Pump the
    /// session's context instead — helpers can fetch it with
    /// `MainContext::thread_default()`, which returns `Some` exactly
    /// because the session pushed it — so only sources this same thread
    /// scheduled ever dispatch. When the session pops the context, its
    /// undrained sources are simply dropped with the thread.
    ///
    /// `body` runs entirely inside the session, so the type system — not
    /// a returned guard — enforces that every widget construction and
    /// assertion happens while the lock is held: a second GTK-initializing
    /// test must not touch GTK state while the first is mid-flight, on
    /// this or any other worker thread. The push uses glib's public,
    /// panic-safe scoped API `MainContext::with_thread_default` (glib's
    /// private `ThreadDefaultContext` RAII, usable here without a single
    /// `unsafe` — which is the whole point of routing through it instead
    /// of hand-rolling the FFI pair: the static-analysis gate counts any
    /// new `unsafe` usage as an actionable finding). The context is
    /// acquired, pushed, and popped again — even if `body` panics — all
    /// before `_guard` (the GTK test mutex) is released at scope end, so
    /// the push/pop pair never straddles a lock handoff to the next
    /// widget test.
    ///
    /// Returns `None` — without calling `body` — when the caller must skip:
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
    ///
    /// Positive display-backed acceptance runs (see
    /// `docs/acceptance-p2.3-c.md`) set `TRIBUTARY_WIDGET_TESTS_FAIL_CLOSED`:
    /// both skip paths below then panic instead of returning `None`, so a
    /// green suite can only mean the widget contracts actually executed. A
    /// misconfigured display setup (e.g. a Broadway daemon GTK 4 cannot
    /// initialize against) then fails the run loudly instead of silently
    /// skipping every widget assertion behind an otherwise-green suite.
    /// Leave the variable unset for the default headless behavior, where
    /// skipping is the intended, reported outcome.
    pub fn with_session<R>(label: &str, body: impl FnOnce() -> R) -> Option<R> {
        let fail_closed = std::env::var_os("TRIBUTARY_WIDGET_TESTS_FAIL_CLOSED").is_some();

        // A panicked earlier test must not cascade into every later widget
        // test: the data the guard protects is stateless (just ordering),
        // so a poisoned lock is safe to carry on from. `_guard` is
        // load-bearing despite its name: the binding stays alive — and the
        // mutex locked — until `with_session` returns.
        let _guard = lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let has_display_session =
            std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
        if !has_display_session {
            assert!(
                !fail_closed,
                "{label}: FAIL-CLOSED positive run: no display session \
                 ($WAYLAND_DISPLAY/$DISPLAY unset); refusing to skip. \
                 Start the matching GTK 4 Broadway daemon \
                 (gtk4-broadwayd) and export DISPLAY/BROADWAY_DISPLAY \
                 before re-running."
            );
            eprintln!(
                "{label}: no display session ($WAYLAND_DISPLAY/$DISPLAY \
                 unset); skipping. Re-run inside a desktop session to \
                 exercise the contract."
            );
            return None;
        }

        if !gtk::is_initialized() {
            if let Err(e) = gtk::init() {
                assert!(
                    !fail_closed,
                    "{label}: FAIL-CLOSED positive run: GTK unavailable \
                     ({e}); refusing to skip. Verify the display session \
                     belongs to a GTK 4-compatible server (gtk4-broadwayd, \
                     not the GTK 3 broadwayd) before re-running."
                );
                eprintln!(
                    "{label}: GTK unavailable ({e}); skipping. Re-run on a \
                     box with a display session (or under a Broadway \
                     headless server) to exercise the contract."
                );
                return None;
            }
        }

        // Push the session's dedicated main context as the thread default
        // for exactly the duration of `body` (see the doc comment above
        // for why the global default context is a cross-thread dispatch
        // hazard here). A fresh context always acquires: nothing else owns
        // it yet.
        let context = gtk::glib::MainContext::new();
        Some(
            context
                .with_thread_default(body)
                .expect("fresh main context always acquires: nothing else owns it yet"),
        )
    }
}
