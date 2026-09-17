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

/// The refusal in the current locale. The reason
/// (`errors.playback.airplay_owntone_no_acquisition_path`: no OwnTone
/// acquisition path is documented for this target) is localized in the same
/// catalog as the outer message, so a translated sentence never carries an
/// English clause (PR #270 review thread).
fn unavailable() -> SenderError {
    unavailable_in(rust_i18n::locale().as_ref())
}

/// The refusal as rendered by `locale`'s catalog: outer message and reason
/// from the same catalog.
fn unavailable_in(locale: &str) -> SenderError {
    let reason = rust_i18n::t!(
        "errors.playback.airplay_owntone_no_acquisition_path",
        locale = locale
    );
    SenderError::Dependency(
        rust_i18n::t!(
            "errors.playback.airplay_owntone_unavailable",
            reason = reason.as_ref(),
            locale = locale
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
        Err(unavailable())
    }

    fn open_session(&self, _ctx: &SenderOpenContext) -> OpenOutcome {
        OpenOutcome::Failed(unavailable())
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
        // The refusal is the current catalog's rendering of the unavailable
        // message with the localized reason — never the key path and never
        // an English clause inside a translated sentence.
        let locale = rust_i18n::locale();
        let reason = rust_i18n::t!(
            "errors.playback.airplay_owntone_no_acquisition_path",
            locale = locale.as_ref()
        );
        assert!(
            !reason.contains("airplay_owntone_no_acquisition_path"),
            "{reason}"
        );
        assert!(
            error.message().contains(reason.as_ref()),
            "{}",
            error.message()
        );
        assert_eq!(error.message(), unavailable_in(locale.as_ref()).message());
    }

    #[test]
    fn refusal_reason_is_rendered_from_the_selected_catalog() {
        let english = unavailable_in("en");
        assert_eq!(
            english.message(),
            "AirPlay via the OwnTone sender is unavailable: this platform has no supported OwnTone acquisition path"
        );
        let german = unavailable_in("de");
        assert!(
            german
                .message()
                .contains("diese Plattform hat keinen unterstützten OwnTone-Erfassungspfad"),
            "{}",
            german.message()
        );
        assert!(
            !german
                .message()
                .contains("no supported OwnTone acquisition path"),
            "the reason must not fall back to English: {}",
            german.message()
        );
        assert_ne!(german.message(), english.message());
    }
}
