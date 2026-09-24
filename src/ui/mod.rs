//! UI module — GTK4 / libadwaita interface components.

pub mod album_art;
pub mod album_art_cache;
pub mod album_art_cell;
pub mod album_pane_art;
pub mod browser;
mod confirm_dialog;
pub mod context_menu;
pub mod discovery_handler;
pub mod equalizer_panel;
pub mod folder_browser;
pub mod header_bar;
mod l10n;
pub mod lastfm_settings;
mod library_commands;
mod local_row_batch;
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
// process-global default context.
//
// Note that the mutex SERIALIZES but does not give THREAD AFFINITY: a
// second GTK-initializing `#[test]` still runs on its own worker thread
// and, seeing `gtk::is_initialized()` already true, would skip init and
// construct widgets off the initializing thread — tripping gtk-rs
// main-thread checks. The crate therefore keeps exactly ONE
// GTK-initializing `#[test]` (the consolidated widget-contracts test in
// `browser.rs`); a new GTK-touching contract must join that test's body as
// a helper, never become a second `#[test]`.
//
// That rule is enforced (#274): `with_session` records the thread that ran
// `gtk::init` and FAILS LOUDLY if a later call from a different thread
// tries to reuse the initialized session. It also honors strict mode —
// `TRIBUTARY_GTK_GATE=require` from the real-display CI job, or
// `TRIBUTARY_WIDGET_TESTS_FAIL_CLOSED` from the acceptance harness — which
// converts every would-be skip (no display session, or GTK unavailable)
// into a panic so a skipped widget body can never masquerade as a pass.
#[cfg(all(test, not(target_os = "macos")))]
pub mod widget_test_session {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::thread::ThreadId;

    /// How a missing display session is treated.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum GateMode {
        /// No display session: print the reason and let the caller skip.
        /// This is the developer default.
        SkipWhenUnavailable,
        /// No display session: refuse to skip. CI sets
        /// `TRIBUTARY_GTK_GATE=require` and the positive acceptance runs set
        /// `TRIBUTARY_WIDGET_TESTS_FAIL_CLOSED`, so the widget contracts
        /// either run for real or fail the job.
        Require,
    }

    impl GateMode {
        /// Parses the `TRIBUTARY_GTK_GATE` value. Only an explicit
        /// require-ish value opts in; anything else (including unset and
        /// empty) keeps the lenient developer behavior.
        fn from_env_value(value: Option<&str>) -> Self {
            match value.map(str::trim) {
                Some("require" | "required" | "1" | "true") => Self::Require,
                _ => Self::SkipWhenUnavailable,
            }
        }

        fn from_env() -> Self {
            // Strict mode has two opt-ins: the CI display gate sets
            // `TRIBUTARY_GTK_GATE=require`, and the positive display-backed
            // acceptance runs documented in `docs/acceptance-p2.3-c.md` set
            // `TRIBUTARY_WIDGET_TESTS_FAIL_CLOSED`. Either one converts every
            // would-be skip into a panic, so a green strict-mode suite can
            // only mean the widget contracts actually executed.
            if std::env::var_os("TRIBUTARY_WIDGET_TESTS_FAIL_CLOSED").is_some() {
                return Self::Require;
            }
            let value = std::env::var("TRIBUTARY_GTK_GATE").ok();
            Self::from_env_value(value.as_deref())
        }
    }

    /// The process-wide GTK test lock.
    fn lock() -> &'static Mutex<()> {
        static GTK_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        GTK_TEST_LOCK.get_or_init(|| Mutex::new(()))
    }

    /// The thread that owns the initialized GTK session. GTK must be used
    /// from exactly one thread for the life of the process, so this is what
    /// [`with_session`] checks before handing out a session to a later
    /// caller.
    fn gtk_owner() -> &'static OnceLock<ThreadId> {
        static GTK_OWNER: OnceLock<ThreadId> = OnceLock::new();
        &GTK_OWNER
    }

    /// Set once a widget session has been established (GTK initialized
    /// under the lock), so the calling test can assert it did not run as a
    /// vacuous no-op.
    static WIDGET_SESSION_ESTABLISHED: AtomicBool = AtomicBool::new(false);

    /// True when [`with_session`] has established a GTK session in this
    /// process.
    pub fn was_established() -> bool {
        WIDGET_SESSION_ESTABLISHED.load(Ordering::SeqCst)
    }

    /// True when any environment entry names a display server GTK can
    /// attach to. X11 (`$DISPLAY`) and Wayland (`$WAYLAND_DISPLAY`) are the
    /// desktop sessions; Broadway (`$BROADWAY_DISPLAY`, or
    /// `GDK_BACKEND=broadway`) is GTK's own headless display server and is
    /// accepted so the gate can be exercised on a box with no desktop.
    fn display_session_available() -> bool {
        let display = std::env::var("DISPLAY").ok();
        let wayland = std::env::var("WAYLAND_DISPLAY").ok();
        let broadway = std::env::var("BROADWAY_DISPLAY").ok();
        let gdk_backend = std::env::var("GDK_BACKEND").ok();
        display_session_available_from(
            display.as_deref(),
            wayland.as_deref(),
            broadway.as_deref(),
            gdk_backend.as_deref(),
        )
    }

    /// Pure decision core for [`display_session_available`], unit-tested
    /// without touching the process environment.
    fn display_session_available_from(
        display: Option<&str>,
        wayland: Option<&str>,
        broadway: Option<&str>,
        gdk_backend: Option<&str>,
    ) -> bool {
        let set = |value: Option<&str>| value.is_some_and(|v| !v.trim().is_empty());
        set(display)
            || set(wayland)
            || set(broadway)
            || gdk_backend.is_some_and(|v| v.trim().eq_ignore_ascii_case("broadway"))
    }

    /// Handles the "no usable display" outcome. In strict mode this panics
    /// with the reason; otherwise it prints an attributable skip message
    /// and releases the caller.
    fn unavailable<R>(label: &str, reason: &str, mode: GateMode) -> Option<R> {
        match mode {
            GateMode::Require => panic!(
                "GTK widget gate ({label}): {reason}. Strict mode \
                 (TRIBUTARY_GTK_GATE=require or TRIBUTARY_WIDGET_TESTS_FAIL_CLOSED) \
                 refuses to skip: run this contract under a real display server \
                 (the CI job uses Xvfb; the acceptance harness uses gtk4-broadwayd) \
                 with GTK available."
            ),
            GateMode::SkipWhenUnavailable => {
                eprintln!(
                    "{label}: {reason}; skipping. Re-run inside a desktop session \
                     (or under a Broadway headless server: GDK_BACKEND=broadway \
                     BROADWAY_DISPLAY=:N) to exercise the contract."
                );
                None
            }
        }
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
    /// - no display session (`$WAYLAND_DISPLAY`/`$DISPLAY`/`$BROADWAY_DISPLAY`
    ///   all unset): a test process that initialized GTK without a real
    ///   display session segfaults in GTK teardown at exit (observed as
    ///   SIGSEGV after all tests passed on headless Linux CI, run
    ///   33921896331), so the gate fires BEFORE any GTK call;
    /// - GTK cannot initialize (no display server reachable): same skip
    ///   path, with the reason printed.
    ///
    /// Both outcomes panic instead of skipping when
    /// `TRIBUTARY_GTK_GATE=require`.
    ///
    /// Panics if a different thread already owns the initialized GTK
    /// session: GTK is single-threaded, and silently constructing widgets
    /// off the initializing thread would trip gtk-rs main-thread checks.
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
    /// Leave both variables unset for the default headless behavior, where
    /// skipping is the intended, reported outcome.
    pub fn with_session<R>(label: &str, body: impl FnOnce() -> R) -> Option<R> {
        // A panicked earlier test must not cascade into every later widget
        // test: the data the guard protects is stateless (just ordering),
        // so a poisoned lock is safe to carry on from. `_guard` is
        // load-bearing despite its name: the binding stays alive — and the
        // mutex locked — until `with_session` returns.
        let _guard = lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mode = GateMode::from_env();
        let current = std::thread::current().id();

        if gtk::is_initialized() {
            // GTK is already up. Refuse to hand the session to a second
            // thread: gtk-rs asserts GTK is used from its initializing
            // thread, and the crate deliberately keeps one GTK test so this
            // never trips in practice. Reject an externally initialized
            // session observed from a foreign thread BEFORE recording
            // ownership — otherwise get_or_init would capture this thread
            // as the owner and the check below would pass vacuously while
            // every widget call here runs off GTK's actual main thread.
            assert!(
                gtk::is_initialized_main_thread(),
                "GTK widget gate ({label}): GTK was initialized on another \
                 thread but {label} is running on {current:?}. GTK must be \
                 exercised from the single thread that initialized it; fold \
                 this contract into that session instead of starting a \
                 second GTK test."
            );
            let owner = *gtk_owner().get_or_init(|| current);
            assert!(
                owner == current,
                "GTK widget gate ({label}): GTK is owned by thread {owner:?} but \
                 {label} is running on {current:?}. GTK must be exercised from the \
                 single thread that initialized it; fold this contract into that \
                 session instead of starting a second GTK test."
            );
            WIDGET_SESSION_ESTABLISHED.store(true, Ordering::SeqCst);
        } else {
            if !display_session_available() {
                return unavailable(
                    label,
                    "no display session ($WAYLAND_DISPLAY/$DISPLAY/$BROADWAY_DISPLAY unset)",
                    mode,
                );
            }

            match gtk::init() {
                Ok(()) => {
                    let _ = gtk_owner().set(current);
                    WIDGET_SESSION_ESTABLISHED.store(true, Ordering::SeqCst);
                }
                Err(e) => {
                    return unavailable(label, &format!("GTK unavailable ({e})"), mode);
                }
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

    #[cfg(test)]
    mod gate_decision_tests {
        use super::{display_session_available_from, GateMode};

        #[test]
        fn gate_mode_defaults_to_skip_and_requires_an_explicit_opt_in() {
            assert_eq!(
                GateMode::from_env_value(None),
                GateMode::SkipWhenUnavailable
            );
            assert_eq!(
                GateMode::from_env_value(Some("")),
                GateMode::SkipWhenUnavailable
            );
            assert_eq!(
                GateMode::from_env_value(Some("skip")),
                GateMode::SkipWhenUnavailable
            );
            assert_eq!(GateMode::from_env_value(Some("require")), GateMode::Require);
            assert_eq!(
                GateMode::from_env_value(Some(" required ")),
                GateMode::Require
            );
            assert_eq!(GateMode::from_env_value(Some("1")), GateMode::Require);
            assert_eq!(GateMode::from_env_value(Some("true")), GateMode::Require);
        }

        #[test]
        fn display_detection_covers_x11_wayland_and_broadway_only() {
            assert!(display_session_available_from(
                Some(":99"),
                None,
                None,
                None
            ));
            assert!(display_session_available_from(
                None,
                Some("wayland-0"),
                None,
                None
            ));
            assert!(display_session_available_from(None, None, Some(":5"), None));
            assert!(display_session_available_from(
                None,
                None,
                None,
                Some("broadway")
            ));
            // Blank values are not a display session, and an unrelated GDK
            // backend must not be mistaken for one.
            assert!(!display_session_available_from(
                Some(""),
                Some("  "),
                None,
                None
            ));
            assert!(!display_session_available_from(
                None,
                None,
                None,
                Some("x11")
            ));
            assert!(!display_session_available_from(None, None, None, None));
        }
    }
}
