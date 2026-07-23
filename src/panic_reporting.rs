//! Process-wide panic diagnostics that never render panic payloads.
//!
//! Panic payloads can contain credentials or other private runtime values. A
//! caught panic still invokes Rust's global hook before control returns to the
//! catcher, so sanitizing the eventual application error is not sufficient.
//! Install this hook before starting any application work and deliberately
//! ignore [`std::panic::PanicHookInfo`].

use std::backtrace::Backtrace;
use std::io::Write;

const FIXED_PANIC_DIAGNOSTIC: &str =
    "Tributary encountered an unexpected internal failure; panic payload omitted.";
const FIXED_BACKTRACE_HEADING: &str = "Internal backtrace (panic details omitted):";

/// Replace Rust's payload-rendering default panic hook for this process.
///
/// The hook emits only application-owned fixed text. When the user explicitly
/// enables `RUST_BACKTRACE`, it also captures a stack trace; a stack trace
/// contains code locations, not the ignored panic payload.
pub fn install_privacy_preserving_panic_hook() {
    std::panic::set_hook(Box::new(|_panic_info| {
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{FIXED_PANIC_DIAGNOSTIC}");

        if backtrace_requested() {
            let _ = writeln!(stderr, "{FIXED_BACKTRACE_HEADING}");
            let _ = writeln!(stderr, "{}", Backtrace::force_capture());
        }
    }));
}

fn backtrace_requested() -> bool {
    std::env::var_os("RUST_BACKTRACE").is_some_and(|value| value != "0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const CHILD_MARKER: &str = "TRIBUTARY_REDACTED_PANIC_HOOK_CHILD";
    const CHILD_MARKER_VALUE: &str = "tributary-redacted-panic-hook-child-v1";
    const PRIVATE_SENTINEL: &str = "lastfm-session-secret-must-not-reach-stderr-4bd53e31";

    #[test]
    fn panic_hook_omits_payload_even_when_a_backtrace_is_requested() {
        if std::env::var(CHILD_MARKER).as_deref() == Ok(CHILD_MARKER_VALUE) {
            install_privacy_preserving_panic_hook();
            panic!("{PRIVATE_SENTINEL}");
        }

        let output = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "panic_reporting::tests::panic_hook_omits_payload_even_when_a_backtrace_is_requested",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_MARKER, CHILD_MARKER_VALUE)
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
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !stdout.contains(PRIVATE_SENTINEL),
            "panic payload escaped through the test harness"
        );
    }
}
