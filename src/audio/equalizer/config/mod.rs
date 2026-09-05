//! Equalizer persistence: the `equalizer.cfg` grammar, strict parsing,
//! and the atomic-replace writer (contract: *Persistence format*).
//!
//! Bounded submodules keep each file small and focused: [`render`]
//! produces the canonical on-disk content, [`parse`] implements the
//! strict `key="value"` grammar with validation and clamping, and
//! [`write`] owns the atomic-replace protocol. The load/save
//! orchestration and its outcome types live here.

mod parse;
mod render;
mod write;

use std::path::PathBuf;

use super::EqSettings;

pub use render::render_equalizer_file;

use parse::parse_equalizer_file;
use write::write_equalizer_file_atomic;

/// The only supported on-disk schema version.
const SCHEMA_VERSION: &str = "1";

// ── Load ────────────────────────────────────────────────────────────────

/// Path to the equalizer state file: `<data_dir>/tributary/equalizer.cfg`,
/// beside the `volume` file. Owned exclusively by this module.
fn equalizer_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("tributary").join("equalizer.cfg"))
}

/// What happened when the on-disk state was read.
#[derive(Debug)]
pub enum EqLoadOutcome {
    /// The file was valid (coercions and clamps may still have been
    /// applied per the validation rules), or no file exists (fresh
    /// install).
    Loaded(EqSettings),
    /// The file was malformed or carried an unsupported schema version.
    /// Defaults are returned and have been re-written to disk via the
    /// same atomic-replace protocol.
    ReplacedWithDefaults {
        settings: EqSettings,
        diagnostic: EqFileDiagnostic,
    },
    /// The file exists but could not be read (permissions, transient
    /// I/O). This is *not* a malformed file: defaults run in memory for
    /// the session and the on-disk bytes are left exactly as they are.
    /// The caller must suppress every persistence path — the debounced
    /// writer and the shutdown flush — until a subsequent read succeeds
    /// and reconciles the in-memory state with disk, so a valid file can
    /// never be overwritten with defaults (contract: *Persistence*,
    /// transient read failure).
    TransientReadFailure(EqSettings),
}

/// Coarse outcome of a settings load, for callers that must react to a
/// transient read failure (suppressing persistence) without carrying
/// the full outcome payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EqLoadStatus {
    Loaded,
    ReplacedWithDefaults,
    TransientReadFailure,
}

/// Bounded diagnostic for a replaced malformed file: file path, byte
/// count, and the offending key only — never the file content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EqFileDiagnostic {
    pub path: String,
    pub byte_count: u64,
    pub bad_key: String,
}

/// Shared reader for startup and the settings UI's reload escape hatch:
/// load the persisted state, replace a malformed file, and report
/// exactly one bounded diagnostic per replacement or transient failure.
/// Both callers must share this path so the diagnostic wording cannot
/// drift. The returned status tells the caller whether persistence must
/// stay suppressed (transient read failure) until a successful read.
pub fn load_settings_with_status() -> (EqSettings, EqLoadStatus) {
    match load_equalizer_settings_from_disk() {
        EqLoadOutcome::Loaded(settings) => (settings, EqLoadStatus::Loaded),
        EqLoadOutcome::ReplacedWithDefaults {
            settings,
            diagnostic,
        } => {
            tracing::warn!(
                path = %diagnostic.path,
                byte_count = diagnostic.byte_count,
                bad_key = %diagnostic.bad_key,
                "Malformed equalizer.cfg replaced with default state"
            );
            (settings, EqLoadStatus::ReplacedWithDefaults)
        }
        EqLoadOutcome::TransientReadFailure(settings) => {
            tracing::warn!(
                path = %equalizer_path()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
                "equalizer.cfg exists but is unreadable; persistence is suppressed \
                 until a successful read reconciles state with disk"
            );
            (settings, EqLoadStatus::TransientReadFailure)
        }
    }
}

/// Read `equalizer.cfg`, validate, clamp, and coerce per the contract.
pub fn load_equalizer_settings_from_disk() -> EqLoadOutcome {
    match equalizer_path() {
        Some(path) => load_equalizer_settings_from_path(&path),
        None => EqLoadOutcome::Loaded(EqSettings::default()),
    }
}

/// Path-injected load used by startup and by tests: every caller must
/// share this validation path so on-disk semantics cannot drift.
fn load_equalizer_settings_from_path(path: &std::path::Path) -> EqLoadOutcome {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        // Missing file is the fresh-install shape: defaults, no write.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return EqLoadOutcome::Loaded(EqSettings::default());
        }
        Err(_) => {
            // Unreadable (permissions, transient I/O): run with defaults
            // in memory, keep the on-disk state, and let the caller
            // suppress persistence. A read that never succeeded cannot
            // establish that the file is malformed, so the defaults-
            // overwrite path below must stay out of reach — one failed
            // read must never schedule a write over valid disk state.
            return EqLoadOutcome::TransientReadFailure(EqSettings::default());
        }
    };
    match parse_equalizer_file(&bytes) {
        Ok(settings) => EqLoadOutcome::Loaded(settings),
        Err(bad_key) => replace_with_defaults(path, bytes.len() as u64, &bad_key),
    }
}

fn replace_with_defaults(path: &std::path::Path, byte_count: u64, bad_key: &str) -> EqLoadOutcome {
    let settings = EqSettings::default();
    let diagnostic = EqFileDiagnostic {
        path: path.display().to_string(),
        byte_count,
        bad_key: bad_key.to_string(),
    };
    let _ = write_equalizer_file_atomic(path, &render_equalizer_file(&settings));
    EqLoadOutcome::ReplacedWithDefaults {
        settings,
        diagnostic,
    }
}

// ── Save ────────────────────────────────────────────────────────────────

/// Persist settings with the atomic-replace protocol: temp file
/// (`O_EXCL`, single write, fsync), `rename(2)`, then directory fsync.
/// A concurrent or failed writer never leaves a partial file visible.
pub fn save_equalizer_settings_to_disk(settings: &EqSettings) -> bool {
    let Some(path) = equalizer_path() else {
        return false;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    write_equalizer_file_atomic(&path, &render_equalizer_file(settings)).is_ok()
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Disk-level load outcomes ────────────────────────────────────

    /// Regression (contract acceptance 18): a *transient* read failure
    /// is not a malformed file. The outcome is
    /// `TransientReadFailure` with in-memory defaults, the on-disk bytes
    /// stay exactly as they were, and no defaults-overwrite is scheduled.
    #[test]
    fn transient_unreadable_file_reports_failure_without_touching_disk() {
        let base = tempfile::tempdir().expect("temporary config root");
        let path = base.path().join("equalizer.cfg");
        // A directory at the config path is portable way to make
        // `std::fs::read` fail with a non-NotFound error on every host.
        std::fs::create_dir(&path).expect("unreadable fixture");

        let outcome = load_equalizer_settings_from_path(&path);
        let settings = match &outcome {
            EqLoadOutcome::TransientReadFailure(settings) => *settings,
            other => panic!("an unreadable file must be a transient failure, not {other:?}"),
        };
        assert_eq!(settings, EqSettings::default());
        // The unreadable fixture is untouched: no atomic replace ran
        // over it, and no temp sibling was scheduled.
        assert!(path.is_dir());
        assert!(!write::temp_sibling(&path).exists());
    }

    /// The malformed-content path keeps its contract behavior: the
    /// defaults-overwrite repair runs only for a read that *succeeded*,
    /// with the bounded diagnostic carrying the bad key.
    #[test]
    fn malformed_content_is_repaired_at_the_path_level() {
        let base = tempfile::tempdir().expect("temporary config root");
        let path = base.path().join("equalizer.cfg");
        let malformed = "preset=flat\n";
        std::fs::write(&path, malformed).expect("seed malformed file");

        let outcome = load_equalizer_settings_from_path(&path);
        let (settings, diagnostic) = match &outcome {
            EqLoadOutcome::ReplacedWithDefaults {
                settings,
                diagnostic,
            } => (*settings, diagnostic.clone()),
            other => panic!("malformed content must be replaced with defaults, not {other:?}"),
        };
        assert_eq!(settings, EqSettings::default());
        assert_eq!(diagnostic.bad_key, "preset");
        assert_eq!(diagnostic.byte_count, malformed.len() as u64);
        // The repair wrote the default file content back to disk.
        assert_eq!(
            std::fs::read_to_string(&path).ok().as_deref(),
            Some(render_equalizer_file(&EqSettings::default()).as_str())
        );
    }
}
