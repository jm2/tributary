//! UI module — GTK4 / libadwaita interface components.

pub mod album_art;
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
    /// While the lock is held, the session also pushes a **dedicated main
    /// context** as the calling thread's thread-default context; the guard
    /// pops it again on drop. This is what makes main-context pumping safe
    /// inside the session: parallel non-widget tests (the audio suite in
    /// particular) construct players whose production code attaches
    /// thread-affine glib sources — `timeout_add_local` position timers,
    /// debounced `timeout_add_local_once` persistence saves — to the
    /// process-global default context, and those tests never run a main
    /// loop, so the sources stay pending there. Any test that pumped the
    /// global default context would dispatch a foreign source on its own
    /// worker thread, tripping glib's `ThreadGuard`
    /// ("Value accessed from different thread than where it was created")
    /// inside a non-unwinding C trampoline and aborting the entire test
    /// binary (tr-8wtab). Pump the session's context instead — helpers can
    /// fetch it with `MainContext::thread_default()`, which returns `Some`
    /// exactly because the session pushed it — so only sources this same
    /// thread scheduled ever dispatch. When the session pops the context,
    /// its undrained sources are simply dropped with the thread.
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
    pub fn acquire(label: &str) -> Option<SessionGuard> {
        let fail_closed = std::env::var_os("TRIBUTARY_WIDGET_TESTS_FAIL_CLOSED").is_some();

        // A panicked earlier test must not cascade into every later widget
        // test: the data the guard protects is stateless (just ordering),
        // so a poisoned lock is safe to carry on from.
        let guard = lock()
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
        // (see the doc comment above for why the global default context is
        // a cross-thread dispatch hazard here). `with_thread_default` is
        // scoped to a closure, but the push must span the caller's entire
        // widget session, so this mirrors glib's private
        // `ThreadDefaultContext` RAII with an explicit pop on drop. A fresh
        // context always acquires: nothing else owns it yet.
        let thread_default = ThreadDefaultMainContext::push(gtk::glib::MainContext::new());

        Some(SessionGuard {
            _thread_default: thread_default,
            _lock: guard,
        })
    }

    /// Holds the GTK test mutex and the session's pushed thread-default
    /// main context for the lifetime of a widget test session. Dropping it
    /// pops the context first and then releases the lock: Rust drops
    /// struct fields in declaration order, so the context member is
    /// declared before the lock member and is therefore torn down first —
    /// the push/pop pair never straddles a lock handoff to the next
    /// widget test.
    pub struct SessionGuard {
        _thread_default: ThreadDefaultMainContext,
        _lock: MutexGuard<'static, ()>,
    }

    /// RAII for `g_main_context_push_thread_default` /
    /// `g_main_context_pop_thread_default` (mirrors glib's private
    /// `ThreadDefaultContext`, which is not public).
    struct ThreadDefaultMainContext {
        context: gtk::glib::MainContext,
    }

    impl ThreadDefaultMainContext {
        fn push(context: gtk::glib::MainContext) -> Self {
            use gtk::glib::translate::ToGlibPtr;
            unsafe {
                gtk::glib::ffi::g_main_context_push_thread_default(context.to_glib_none().0);
            }
            Self { context }
        }
    }

    impl Drop for ThreadDefaultMainContext {
        fn drop(&mut self) {
            use gtk::glib::translate::ToGlibPtr;
            unsafe {
                gtk::glib::ffi::g_main_context_pop_thread_default(self.context.to_glib_none().0);
            }
        }
    }
}
