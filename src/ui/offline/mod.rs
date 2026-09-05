//! Accessible download/progress/storage UI for the offline cache.
//!
//! GTK retains only the credential-free [`OfflineBoard`] projection published
//! by the headless engine (`crate::offline::engine`). Every visible detail is
//! decided by the pure [`OfflineRowPlan`] builder — headless-testable, fully
//! localized, and redacted — while the GTK renderer in [`panel`] is a thin
//! applier that never appears in tests. Rows carry title/artist/album
//! metadata the source itself published, the structured source label, per-job
//! byte progress, and the redacted failure or licence label; no URL, token,
//! or on-disk path is ever displayed or placed in the accessibility tree.

use crate::architecture::offline::{JobState, OfflineError, OperationalLicence};
use crate::offline::engine::{OfflineBoard, OfflineRowSnapshot};

mod panel;
#[cfg(test)]
mod tests;

/// Longest persisted display label rendered per field, applied at the
/// projection boundary so over-long source metadata cannot distort the
/// panel layout or the accessibility tree.
const MAX_ROW_LABEL_CHARS: usize = 64;

/// What a row shows and offers. Pure data: the GTK renderer applies it and
/// the tests assert it.
#[derive(Clone, Debug, PartialEq)]
pub struct OfflineRowPlan {
    pub kind: OfflineRowKind,
    pub icon_name: Option<&'static str>,
    pub status_key: Option<&'static str>,
    /// 0.0–1.0 when the transfer total is known; `None` means indeterminate.
    pub progress_fraction: Option<f64>,
    /// Localized byte-progress text, when a transfer is in flight.
    pub progress_text: Option<String>,
    /// The full localized row description for screen readers.
    pub accessible_label: String,
    pub primary_action: Option<OfflineRowAction>,
    pub show_spinner: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OfflineRowKind {
    #[default]
    Queued,
    Active,
    Committed,
    Revoked,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OfflineRowAction {
    Cancel,
    Retry,
    Delete,
}

impl OfflineRowAction {
    /// Localized button text.
    // The application window wiring that invokes these actions lands with
    // the follow-up slice; until then only tests and the renderer reference
    // them, so the bin-root dead-code lint is silenced locally.
    #[allow(dead_code)]
    #[must_use]
    pub fn label(self, locale: &str) -> String {
        let key = match self {
            Self::Cancel => "offline.action_cancel",
            Self::Retry => "offline.action_retry",
            Self::Delete => "offline.action_delete",
        };
        rust_i18n::t!(key, locale = locale).into_owned()
    }
}

/// Plans for every row of a board, in board order.
// Consumed by the window wiring slice together with the panel; the pure
// projection below (`plan`) is what the panel's `render` calls today.
#[allow(dead_code)]
#[must_use]
pub fn plans(board: &OfflineBoard, locale: &str) -> Vec<OfflineRowPlan> {
    board.rows.iter().map(|row| plan(row, locale)).collect()
}

/// Build the plan for one board row.
#[must_use]
pub fn plan(row: &OfflineRowSnapshot, locale: &str) -> OfflineRowPlan {
    let title = truncated(&row.labels.title);
    let artist = truncated(&row.labels.artist);
    let presentation = classify(row);
    let licence_suffix = row.cached.as_ref().map(|cached| {
        rust_i18n::t!(
            "offline.licence_suffix",
            licence = licence_label(cached.licence_label, locale),
            locale = locale
        )
        .into_owned()
    });
    let progress_text = progress_text(row, locale);
    let progress_suffix = progress_text.as_ref().map(|text| {
        rust_i18n::t!("offline.progress_suffix", progress = text, locale = locale).into_owned()
    });
    let status = presentation
        .status_key
        .map(|key| rust_i18n::t!(key, locale = locale).into_owned())
        .unwrap_or_default();
    let mut accessible = base_accessible(&title, &artist, &status, locale);
    append_suffix(&mut accessible, licence_suffix);
    append_suffix(&mut accessible, progress_suffix);
    OfflineRowPlan {
        kind: presentation.kind,
        icon_name: presentation.icon_name,
        status_key: presentation.status_key,
        progress_fraction: progress_of(row),
        progress_text,
        accessible_label: accessible,
        primary_action: presentation.primary_action,
        show_spinner: presentation.show_spinner,
    }
}

/// The screen-reader row description: title (and artist when present) plus
/// the localized status.
fn base_accessible(title: &str, artist: &str, status: &str, locale: &str) -> String {
    if artist.is_empty() {
        rust_i18n::t!(
            "offline.row_accessible_title_only",
            title = title,
            status = status,
            locale = locale
        )
        .into_owned()
    } else {
        rust_i18n::t!(
            "offline.row_accessible",
            title = title,
            artist = artist,
            status = status,
            locale = locale
        )
        .into_owned()
    }
}

/// Append one already-localized suffix to the accessible description,
/// space-separated.
fn append_suffix(accessible: &mut String, suffix: Option<String>) {
    if let Some(suffix) = suffix {
        accessible.push(' ');
        accessible.push_str(&suffix);
    }
}

/// How a row presents: kind, icon, status key, spinner, and primary action.
struct RowPresentation {
    kind: OfflineRowKind,
    icon_name: Option<&'static str>,
    status_key: Option<&'static str>,
    show_spinner: bool,
    primary_action: Option<OfflineRowAction>,
}

fn present(
    kind: OfflineRowKind,
    icon_name: Option<&'static str>,
    status_key: &'static str,
    show_spinner: bool,
    primary_action: Option<OfflineRowAction>,
) -> RowPresentation {
    RowPresentation {
        kind,
        icon_name,
        status_key: Some(status_key),
        show_spinner,
        primary_action,
    }
}

fn classify(row: &OfflineRowSnapshot) -> RowPresentation {
    // Licence-revoked rows keep their cached bytes but stop being playable.
    if row.cached.as_ref().is_some_and(|cached| !cached.playable) {
        return present(
            OfflineRowKind::Revoked,
            Some("dialog-warning-symbolic"),
            "offline.status_revoked",
            false,
            Some(OfflineRowAction::Delete),
        );
    }
    classify_state(row.state, row.failure)
}

fn classify_state(state: JobState, failure: Option<OfflineError>) -> RowPresentation {
    match state {
        JobState::Failed => present(
            OfflineRowKind::Failed,
            Some("dialog-warning-symbolic"),
            failure_key(failure.unwrap_or(OfflineError::Network)),
            false,
            Some(OfflineRowAction::Retry),
        ),
        JobState::Cancelled => present(
            OfflineRowKind::Cancelled,
            None,
            "offline.status_cancelled",
            false,
            Some(OfflineRowAction::Retry),
        ),
        JobState::Committed => present(
            OfflineRowKind::Committed,
            None,
            "offline.status_committed",
            false,
            Some(OfflineRowAction::Delete),
        ),
        JobState::Queued => present(
            OfflineRowKind::Queued,
            None,
            "offline.status_queued",
            true,
            Some(OfflineRowAction::Cancel),
        ),
        _ => classify_in_flight(state),
    }
}

/// Every in-flight state presents identically (spinning, cancellable) and
/// differs only in its status text.
fn classify_in_flight(state: JobState) -> RowPresentation {
    let status_key = match state {
        JobState::Connecting => "offline.status_connecting",
        JobState::Receiving => "offline.status_receiving",
        JobState::Verifying => "offline.status_verifying",
        _ => "offline.status_committing",
    };
    present(
        OfflineRowKind::Active,
        None,
        status_key,
        true,
        Some(OfflineRowAction::Cancel),
    )
}

/// Redacted, localized failure text keys — the variant name is the only
/// thing the engine publishes, and each maps to one fixed explanation.
fn failure_key(failure: OfflineError) -> &'static str {
    match failure {
        OfflineError::Network => "offline.failure_network",
        OfflineError::AuthExpired => "offline.failure_auth_expired",
        OfflineError::LeaseRevoked => "offline.failure_lease_revoked",
        OfflineError::IntegrityMismatch => "offline.failure_integrity_mismatch",
        OfflineError::IntegrityUnverifiable => "offline.failure_integrity_unverifiable",
        OfflineError::LicenceDenied => "offline.failure_licence_denied",
        OfflineError::QuotaExceeded => "offline.failure_quota_exceeded",
        OfflineError::StorageUnavailable => "offline.failure_storage_unavailable",
        OfflineError::UnsupportedSource => "offline.failure_unsupported_source",
    }
}

/// The licence label the contract requires per committed row — the label
/// only, never the licence text.
fn licence_label(licence: OperationalLicence, locale: &str) -> String {
    match licence {
        OperationalLicence::SourceDeclared => {
            rust_i18n::t!("offline.licence_declared", locale = locale).into_owned()
        }
        OperationalLicence::Denied => {
            rust_i18n::t!("offline.licence_denied", locale = locale).into_owned()
        }
        OperationalLicence::Revoked => {
            rust_i18n::t!("offline.licence_revoked", locale = locale).into_owned()
        }
    }
}

fn progress_of(row: &OfflineRowSnapshot) -> Option<f64> {
    if !matches!(row.state, JobState::Receiving | JobState::Connecting) {
        return None;
    }
    row.total_bytes
        .filter(|total| *total > 0)
        .map(|total| (row.current_bytes as f64 / total as f64).clamp(0.0, 1.0))
}

fn progress_text(row: &OfflineRowSnapshot, locale: &str) -> Option<String> {
    if row.state != JobState::Receiving {
        return None;
    }
    Some(match row.total_bytes {
        Some(total) => rust_i18n::t!(
            "offline.bytes_progress_of",
            received = format_bytes(row.current_bytes),
            total = format_bytes(total),
            locale = locale
        )
        .into_owned(),
        None => rust_i18n::t!(
            "offline.bytes_progress_so_far",
            received = format_bytes(row.current_bytes),
            locale = locale
        )
        .into_owned(),
    })
}

/// Bounded human-readable byte count for progress text.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * KIB;
    const GIB: f64 = 1024.0 * MIB;
    let value = bytes as f64;
    if value >= GIB {
        format!("{:.1} GiB", value / GIB)
    } else if value >= MIB {
        format!("{:.1} MiB", value / MIB)
    } else if value >= KIB {
        format!("{:.1} KiB", value / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Truncate at a char boundary with an ellipsis. The result — including
/// the ellipsis — stays within `MAX_ROW_LABEL_CHARS`.
fn truncated(label: &str) -> String {
    if label.chars().count() <= MAX_ROW_LABEL_CHARS {
        return label.to_string();
    }
    let cut: String = label.chars().take(MAX_ROW_LABEL_CHARS - 1).collect();
    format!("{cut}…")
}

/// Localized quota usage line for the panel header.
#[must_use]
pub fn quota_text(board: &OfflineBoard, locale: &str) -> String {
    rust_i18n::t!(
        "offline.quota_label",
        used = format_bytes(board.committed_bytes),
        total = format_bytes(board.quota_bytes),
        locale = locale
    )
    .into_owned()
}
