//! Process-wide panic diagnostics that never render panic payloads.
//!
//! Panic payloads can contain credentials or other private runtime values. A
//! caught panic still invokes Rust's global hook before control returns to the
//! catcher, so sanitizing the eventual application error is not sufficient.
//! Install this hook before starting any application work. It reports only
//! the compile-time source location and the thread name from
//! [`std::panic::PanicHookInfo`], never the payload.

use std::backtrace::Backtrace;
use std::io::Write;

const FIXED_PANIC_DIAGNOSTIC: &str =
    "Tributary encountered an unexpected internal failure; panic payload omitted.";
const FIXED_BACKTRACE_HEADING: &str = "Internal backtrace (panic details omitted):";
/// File under `<cache_dir>/tributary` that collects one line per panic, so a
/// release build without a console still leaves something to report.
const CRASH_LOG_NAME: &str = "crash.log";
/// Past this size the crash log starts over rather than growing forever.
const CRASH_LOG_LIMIT_BYTES: u64 = 256 * 1024;

/// Replace Rust's payload-rendering default panic hook for this process.
///
/// The hook emits application-owned fixed text plus the panic's source
/// location and thread name, and appends the same to the crash log. When the
/// user explicitly enables `RUST_BACKTRACE`, it also captures a stack trace;
/// a stack trace contains code locations, not the ignored panic payload.
pub fn install_privacy_preserving_panic_hook() {
    std::panic::set_hook(Box::new(|panic_info| {
        let location = panic_info.location().map_or_else(
            || "an unknown location".to_string(),
            |location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            },
        );
        let thread = std::thread::current();
        let summary = format!(
            "panicked at {location} on thread '{}'",
            thread.name().unwrap_or("<unnamed>")
        );

        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{FIXED_PANIC_DIAGNOSTIC}");
        let _ = writeln!(stderr, "Tributary {summary}.");

        if backtrace_requested() {
            let _ = writeln!(stderr, "{FIXED_BACKTRACE_HEADING}");
            let _ = writeln!(stderr, "{}", Backtrace::force_capture());
        }
        drop(stderr);
        append_crash_log(&summary);
    }));
}

fn backtrace_requested() -> bool {
    std::env::var_os("RUST_BACKTRACE").is_some_and(|value| value != "0")
}

/// Best effort: a panic report must never fail because the log cannot be
/// written.
fn append_crash_log(summary: &str) {
    let Some(dir) = crate::paths::cache_dir().map(|dir| dir.join("tributary")) else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(CRASH_LOG_NAME);
    let start_over = std::fs::metadata(&path).is_ok_and(|meta| meta.len() > CRASH_LOG_LIMIT_BYTES);
    let mut options = std::fs::OpenOptions::new();
    if start_over {
        options.write(true).truncate(true);
    } else {
        options.append(true);
    }
    if let Ok(mut file) = options.create(true).open(&path) {
        let _ = writeln!(
            file,
            "{} Tributary {} {summary}",
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            env!("CARGO_PKG_VERSION"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const CHILD_MARKER: &str = "TRIBUTARY_REDACTED_PANIC_HOOK_CHILD";
    const CHILD_MARKER_VALUE: &str = "tributary-redacted-panic-hook-child-v1";
    const PRIVATE_SENTINEL: &str = "lastfm-session-secret-must-not-reach-stderr-4bd53e31";
    /// The harness names each test's thread after the test.
    const CHILD_TEST_NAME: &str =
        "panic_reporting::tests::panic_hook_omits_payload_even_when_a_backtrace_is_requested";

    #[test]
    fn panic_hook_omits_payload_even_when_a_backtrace_is_requested() {
        if std::env::var(CHILD_MARKER).as_deref() == Ok(CHILD_MARKER_VALUE) {
            install_privacy_preserving_panic_hook();
            panic!("{PRIVATE_SENTINEL}");
        }

        let sandbox = tempfile::tempdir().expect("child user-state sandbox");
        let output = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                CHILD_TEST_NAME,
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_MARKER, CHILD_MARKER_VALUE)
            .env(crate::paths::TEST_USER_STATE_DIR_ENV, sandbox.path())
            .env("RUST_BACKTRACE", "1")
            .output()
            .expect("run isolated redacted-panic child");

        assert!(!output.status.success(), "panicking child must fail");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(FIXED_PANIC_DIAGNOSTIC),
            "fixed panic diagnostic missing from child stderr"
        );
        assert!(
            stderr.contains(FIXED_BACKTRACE_HEADING),
            "requested backtrace heading missing from child stderr"
        );
        assert!(
            !stderr.contains(PRIVATE_SENTINEL),
            "panic payload escaped through the process-wide hook"
        );
        assert!(
            stderr.contains(&format!("panicked at {}:", file!())),
            "panic location missing from child stderr"
        );
        assert!(
            stderr.contains(&format!("on thread '{CHILD_TEST_NAME}'")),
            "panicking thread name missing from child stderr"
        );
        let crash_log = std::fs::read_to_string(
            sandbox
                .path()
                .join("cache")
                .join("tributary")
                .join(CRASH_LOG_NAME),
        )
        .expect("child crash log");
        assert!(crash_log.contains(&format!("panicked at {}:", file!())));
        assert!(!crash_log.contains(PRIVATE_SENTINEL));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !stdout.contains(PRIVATE_SENTINEL),
            "panic payload escaped through the test harness"
        );
    }
}
