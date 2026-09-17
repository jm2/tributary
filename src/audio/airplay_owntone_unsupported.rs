//! Fail-closed OwnTone sender for platforms with no supported acquisition path.
//!
//! The real adapter ([`super::airplay_owntone`] on Linux) owns a FIFO, an
//! advisory `flock`, a `/proc`-verified process binding and a blocking
//! loopback JSON-API client built on `rustix`/`std::os::unix`. Those APIs are
//! not all available here (macOS has no `/proc` and no `mkfifoat`), so this shim
//! provides the same public shape with the fail-closed behavior the design
//! requires on every unsupported target (design §4.3, §4.4, §9 platform
//! scope): an explicit `TRIBUTARY_AIRPLAY_SENDER=owntone` is recognized — so
//! selection never silently falls back to another sender — and every load is
//! refused with localized, actionable guidance instead of being misreported
//! as an OwnTone session.

use super::airplay_sender::{AirplaySender, OpenOutcome, SenderError, SenderOpenContext};

/// Selects the OwnTone adapter instead of the default GStreamer `raopsink`
/// adapter. The name matches the Unix adapter so configuration is identical
/// on every target.
const ENV_SELECT: &str = "TRIBUTARY_AIRPLAY_SENDER";

/// Localized refusal reason: no OwnTone acquisition path is documented for
/// this target.
const UNSUPPORTED_REASON: &str = "this platform has no supported OwnTone acquisition path";

fn unavailable(reason: &str) -> SenderError {
    SenderError::Dependency(
        rust_i18n::t!(
            "errors.playback.airplay_owntone_unavailable",
            reason = reason,
            locale = rust_i18n::locale().as_ref()
        )
        .into_owned(),
    )
}

/// Whether an explicit `TRIBUTARY_AIRPLAY_SENDER` value selects the OwnTone
/// adapter. Selection is configuration, never a silent fallback, so only the
/// exact configured value counts.
fn selection_is_owntone(value: Option<&str>) -> bool {
    match value {
        Some(value) => value.eq_ignore_ascii_case("owntone"),
        None => false,
    }
}

/// The OwnTone transmission path on targets without a supported acquisition
/// path. It is constructible and selectable, but always refuses.
pub(super) struct OwnToneSender;

impl OwnToneSender {
    /// Resolve the adapter from explicit configuration. There is no config to
    /// load on this target; the sender is still constructed so selection is
    /// observable and refusal is explicit.
    pub(super) fn from_env() -> Self {
        Self
    }

    /// `true` when the process is configured to select this adapter.
    pub(super) fn selected() -> bool {
        selection_is_owntone(std::env::var(ENV_SELECT).ok().as_deref())
    }
}

impl AirplaySender for OwnToneSender {
    fn name(&self) -> &'static str {
        "owntone"
    }

    fn probe(&self) -> Result<(), SenderError> {
        Err(unavailable(UNSUPPORTED_REASON))
    }

    fn open_session(&self, _ctx: &SenderOpenContext) -> OpenOutcome {
        OpenOutcome::Failed(unavailable(UNSUPPORTED_REASON))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_is_explicit_and_exact() {
        assert!(selection_is_owntone(Some("owntone")));
        assert!(selection_is_owntone(Some("OwnTone")));
        assert!(selection_is_owntone(Some("OWNTONE")));
        assert!(!selection_is_owntone(Some("raopsink")));
        assert!(!selection_is_owntone(Some("")));
        assert!(!selection_is_owntone(None));
    }

    #[test]
    fn unconfigured_sender_probe_fails_closed() {
        let sender = OwnToneSender;
        let error = sender.probe().expect_err("unsupported sender must refuse");
        assert!(
            error
                .message()
                .contains("supported OwnTone acquisition path"),
            "{}",
            error.message()
        );
    }
}
