//! Accessible download/progress/storage UI for the offline cache.
//!
//! GTK retains only the credential-free [`OfflineBoard`] projection published
//! by the headless engine (`crate::offline::engine`). Every visible detail is
//! decided by the pure [`OfflineRowPlan`] builder — headless-testable, fully
//! localized, and redacted — while [`OfflineStoragePanel`] is a thin renderer
//! that never appears in tests. Rows carry title/artist/album metadata the
//! source itself published, the structured source label, per-job byte
//! progress, and the redacted failure or licence label; no URL, token, or
//! on-disk path is ever displayed or placed in the accessibility tree.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use crate::architecture::offline::{JobState, OfflineError, OperationalLicence};
use crate::offline::engine::{OfflineBoard, OfflineRowSnapshot};

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
    let licence_text = row
        .cached
        .as_ref()
        .map(|cached| licence_label(cached.licence_label, locale));
    let (kind, icon_name, status_key, show_spinner, primary_action) = classify(row);
    let progress = progress_of(row);
    let progress_text = progress_text(row, locale);
    let status = status_key
        .map(|key| rust_i18n::t!(key, locale = locale).into_owned())
        .unwrap_or_default();
    let mut accessible = if artist.is_empty() {
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
    };
    if let Some(licence) = licence_text {
        let suffix = rust_i18n::t!("offline.licence_suffix", licence = licence, locale = locale)
            .into_owned();
        accessible.push(' ');
        accessible.push_str(&suffix);
    }
    if let Some(text) = &progress_text {
        let suffix =
            rust_i18n::t!("offline.progress_suffix", progress = text, locale = locale).into_owned();
        accessible.push(' ');
        accessible.push_str(&suffix);
    }
    OfflineRowPlan {
        kind,
        icon_name,
        status_key,
        progress_fraction: progress,
        progress_text,
        accessible_label: accessible,
        primary_action,
        show_spinner,
    }
}

fn classify(
    row: &OfflineRowSnapshot,
) -> (
    OfflineRowKind,
    Option<&'static str>,
    Option<&'static str>,
    bool,
    Option<OfflineRowAction>,
) {
    // Licence-revoked rows keep their cached bytes but stop being playable.
    if let Some(cached) = &row.cached {
        if !cached.playable {
            return (
                OfflineRowKind::Revoked,
                Some("dialog-warning-symbolic"),
                Some("offline.status_revoked"),
                false,
                Some(OfflineRowAction::Delete),
            );
        }
    }
    match row.state {
        JobState::Failed => {
            let status_key = failure_key(row.failure.unwrap_or(OfflineError::Network));
            (
                OfflineRowKind::Failed,
                Some("dialog-warning-symbolic"),
                Some(status_key),
                false,
                Some(OfflineRowAction::Retry),
            )
        }
        JobState::Cancelled => (
            OfflineRowKind::Cancelled,
            None,
            Some("offline.status_cancelled"),
            false,
            Some(OfflineRowAction::Retry),
        ),
        JobState::Committed => (
            OfflineRowKind::Committed,
            None,
            Some("offline.status_committed"),
            false,
            Some(OfflineRowAction::Delete),
        ),
        JobState::Queued => (
            OfflineRowKind::Queued,
            None,
            Some("offline.status_queued"),
            true,
            Some(OfflineRowAction::Cancel),
        ),
        JobState::Connecting => (
            OfflineRowKind::Active,
            None,
            Some("offline.status_connecting"),
            true,
            Some(OfflineRowAction::Cancel),
        ),
        JobState::Receiving => (
            OfflineRowKind::Active,
            None,
            Some("offline.status_receiving"),
            true,
            Some(OfflineRowAction::Cancel),
        ),
        JobState::Verifying | JobState::Committing => (
            OfflineRowKind::Active,
            None,
            if row.state == JobState::Verifying {
                Some("offline.status_verifying")
            } else {
                Some("offline.status_committing")
            },
            true,
            Some(OfflineRowAction::Cancel),
        ),
    }
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

/// Truncate at a char boundary with an ellipsis.
fn truncated(label: &str) -> String {
    if label.chars().count() <= MAX_ROW_LABEL_CHARS {
        return label.to_string();
    }
    let cut: String = label.chars().take(MAX_ROW_LABEL_CHARS).collect();
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

/// The GTK storage panel: a header quota line plus one accessible row per
/// board entry. Construction requires a GTK display context, so tests only
/// exercise the pure plans above.
pub struct OfflineStoragePanel {
    root: gtk::Box,
    quota_label: gtk::Label,
    list: gtk::ListBox,
    action_handler: Rc<RefCell<dyn FnMut(usize, OfflineRowAction)>>,
}

// The application window embeds this panel with the follow-up wiring
// slice (source-lifecycle → engine → panel). Until then the renderer is
// exercised only through construction in downstream slices and tests, so
// the bin-root dead-code lint is silenced locally rather than forcing a
// premature window hookup.
#[allow(dead_code)]
impl OfflineStoragePanel {
    /// Build the panel. `action_handler` receives the board-row index and
    /// the activated action; wiring to the engine lives with the caller.
    #[must_use]
    pub fn new(action_handler: impl FnMut(usize, OfflineRowAction) + 'static) -> Self {
        let action_handler: Rc<RefCell<dyn FnMut(usize, OfflineRowAction)>> =
            Rc::new(RefCell::new(action_handler));
        let quota_label = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .css_classes(["dim-label", "caption"])
            .build();
        quota_label.set_accessible_role(gtk::AccessibleRole::Status);
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        root.append(&quota_label);
        root.append(&list);
        root.set_accessible_role(gtk::AccessibleRole::Group);
        Self {
            root,
            quota_label,
            list,
            action_handler,
        }
    }

    /// The panel widget for embedding.
    #[must_use]
    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Re-render the panel from a board. Rows are rebuilt in board order so
    /// the action handler's index always matches the current board.
    pub fn render(&self, board: &OfflineBoard, locale: &str) {
        self.quota_label.set_text(&quota_text(board, locale));
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for (index, row) in board.rows.iter().enumerate() {
            let row_plan = plan(row, locale);
            let handler = Rc::clone(&self.action_handler);
            let list_row = Self::build_row(&row_plan, row, locale, move |action| {
                (handler.borrow_mut())(index, action);
            });
            self.list.append(&list_row);
        }
    }

    fn build_row(
        row_plan: &OfflineRowPlan,
        row: &OfflineRowSnapshot,
        locale: &str,
        on_action: impl Fn(OfflineRowAction) + 'static,
    ) -> gtk::ListBoxRow {
        let grid = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(12)
            .build();
        let title = truncated(&row.labels.title);
        let heading = if row.labels.artist.is_empty() {
            title
        } else {
            format!(
                "{} — {}",
                truncated(&row.labels.title),
                truncated(&row.labels.artist)
            )
        };
        let title_label = gtk::Label::builder()
            .label(heading)
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .single_line_mode(true)
            .max_width_chars(48)
            .build();
        grid.append(&title_label);

        let status_line = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        if let Some(icon_name) = row_plan.icon_name {
            let icon = gtk::Image::builder().icon_name(icon_name).build();
            icon.set_accessible_role(gtk::AccessibleRole::Presentation);
            status_line.append(&icon);
        }
        if row_plan.show_spinner {
            let spinner = gtk::Spinner::builder().spinning(true).build();
            spinner.set_accessible_role(gtk::AccessibleRole::Presentation);
            status_line.append(&spinner);
        }
        let status_text = row_plan
            .status_key
            .map(|key| rust_i18n::t!(key, locale = locale).into_owned())
            .unwrap_or_default();
        let status_label = gtk::Label::builder()
            .label(status_text)
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        status_label.set_accessible_role(gtk::AccessibleRole::Status);
        status_line.append(&status_label);
        grid.append(&status_line);

        if let Some(fraction) = row_plan.progress_fraction {
            let bar = gtk::ProgressBar::builder()
                .fraction(fraction)
                .show_text(false)
                .hexpand(true)
                .build();
            bar.set_accessible_role(gtk::AccessibleRole::ProgressBar);
            grid.append(&bar);
        }

        if let Some(action) = row_plan.primary_action {
            let button = gtk::Button::builder()
                .label(action.label(locale))
                .halign(gtk::Align::Start)
                .css_classes(["flat"])
                .build();
            button.connect_clicked(move |_| on_action(action));
            grid.append(&button);
        }

        let list_row = gtk::ListBoxRow::builder().child(&grid).build();
        list_row.update_property(&[gtk::accessible::Property::Label(&row_plan.accessible_label)]);
        list_row
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::architecture::identity::{MediaKey, SourceId, TrackId};
    use crate::architecture::offline::JobState;
    use crate::offline::engine::{CachedRowView, OfflineRowLabels, OfflineRowSnapshot};

    const LOCALE: &str = "en";

    fn labels(title: &str) -> OfflineRowLabels {
        OfflineRowLabels {
            title: title.to_string(),
            artist: "Artist".to_string(),
            album: Some("Album".to_string()),
            source_label: "Subsonic — example.com".to_string(),
        }
    }

    fn cached_view(playable: bool) -> CachedRowView {
        CachedRowView {
            byte_size: 4096,
            committed_at_epoch_secs: 0,
            licence_label: OperationalLicence::SourceDeclared,
            playable,
        }
    }

    fn row(state: JobState, total: Option<u64>, current: u64) -> OfflineRowSnapshot {
        OfflineRowSnapshot {
            media_key: MediaKey::new(SourceId::local(), TrackId::new("track-1").unwrap()),
            labels: labels("Song"),
            state,
            failure: None,
            current_bytes: current,
            total_bytes: total,
            cached: None,
        }
    }

    #[test]
    fn committed_rows_offer_delete_and_show_the_licence_label() {
        let mut committed = row(JobState::Committed, None, 0);
        committed.cached = Some(cached_view(true));
        let row_plan = plan(&committed, LOCALE);
        assert_eq!(row_plan.kind, OfflineRowKind::Committed);
        assert_eq!(row_plan.primary_action, Some(OfflineRowAction::Delete));
        assert!(!row_plan.show_spinner);
        assert!(row_plan.accessible_label.contains("Song"));
        assert!(row_plan.accessible_label.contains("Artist"));
        assert!(
            !row_plan.accessible_label.contains("declared"),
            "raw licence label must be localized, not raw"
        );
    }

    #[test]
    fn active_rows_spin_show_progress_and_offer_cancel() {
        let row_plan = plan(&row(JobState::Receiving, Some(10_000), 4_000), LOCALE);
        assert_eq!(row_plan.kind, OfflineRowKind::Active);
        assert!(row_plan.show_spinner);
        assert_eq!(row_plan.primary_action, Some(OfflineRowAction::Cancel));
        assert_eq!(row_plan.progress_fraction, Some(0.4));
        assert!(row_plan.progress_text.is_some());
        assert!(row_plan
            .accessible_label
            .contains(&row_plan.progress_text.clone().unwrap()));
    }

    #[test]
    fn unknown_totals_render_an_indeterminate_progress() {
        let row_plan = plan(&row(JobState::Receiving, None, 2_048), LOCALE);
        assert_eq!(row_plan.progress_fraction, None);
        assert!(row_plan.progress_text.is_some());
        assert_eq!(row_plan.primary_action, Some(OfflineRowAction::Cancel));
    }

    #[test]
    fn failed_rows_show_a_redacted_reason_and_retry() {
        let mut failed = row(JobState::Failed, Some(100), 0);
        failed.failure = Some(OfflineError::AuthExpired);
        let row_plan = plan(&failed, LOCALE);
        assert_eq!(row_plan.kind, OfflineRowKind::Failed);
        assert_eq!(row_plan.status_key, Some("offline.failure_auth_expired"));
        assert_eq!(row_plan.primary_action, Some(OfflineRowAction::Retry));
        assert_eq!(row_plan.icon_name, Some("dialog-warning-symbolic"));
    }

    #[test]
    fn cancelled_rows_offer_retry() {
        let row_plan = plan(&row(JobState::Cancelled, Some(100), 0), LOCALE);
        assert_eq!(row_plan.kind, OfflineRowKind::Cancelled);
        assert_eq!(row_plan.primary_action, Some(OfflineRowAction::Retry));
    }

    #[test]
    fn revoked_rows_warn_and_offer_delete() {
        let mut revoked = row(JobState::Committed, None, 0);
        revoked.cached = Some(cached_view(false));
        let row_plan = plan(&revoked, LOCALE);
        assert_eq!(row_plan.kind, OfflineRowKind::Revoked);
        assert_eq!(row_plan.status_key, Some("offline.status_revoked"));
        assert_eq!(row_plan.primary_action, Some(OfflineRowAction::Delete));
        assert_eq!(row_plan.icon_name, Some("dialog-warning-symbolic"));
    }

    #[test]
    fn every_failure_variant_maps_to_a_localized_key() {
        for failure in [
            OfflineError::Network,
            OfflineError::AuthExpired,
            OfflineError::LeaseRevoked,
            OfflineError::IntegrityMismatch,
            OfflineError::IntegrityUnverifiable,
            OfflineError::LicenceDenied,
            OfflineError::QuotaExceeded,
            OfflineError::StorageUnavailable,
            OfflineError::UnsupportedSource,
        ] {
            let mut failed = row(JobState::Failed, None, 0);
            failed.failure = Some(failure);
            let row_plan = plan(&failed, LOCALE);
            let key = row_plan.status_key.unwrap_or_default();
            assert!(key.starts_with("offline.failure_"), "{failure:?} unmapped");
            // Redaction: nothing rendered for a failed row may leak transfer
            // plumbing.
            for forbidden in ["http://", "https://", "token=", "Bearer ", "password="] {
                assert!(
                    !row_plan.accessible_label.contains(forbidden),
                    "{failure:?} leaked {forbidden:?}"
                );
            }
        }
    }

    #[test]
    fn oversized_labels_are_truncated_at_the_projection_boundary() {
        let mut long = row(JobState::Committed, None, 0);
        long.labels.title = "x".repeat(500);
        let row_plan = plan(&long, LOCALE);
        assert!(row_plan.accessible_label.chars().count() < 200);
    }

    #[test]
    fn byte_formatting_is_bounded_and_readable() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn quota_text_mentions_used_and_total() {
        let board = OfflineBoard {
            rows: Vec::new(),
            committed_bytes: 4096,
            quota_bytes: 10 * 1024,
        };
        let text = quota_text(&board, LOCALE);
        assert!(text.contains("4.0 KiB"), "used bytes missing: {text}");
        assert!(text.contains("10.0 KiB"), "quota missing: {text}");
    }

    #[test]
    fn board_plans_cover_every_row_in_order() {
        let mut committed = row(JobState::Committed, None, 0);
        committed.cached = Some(cached_view(true));
        let board = OfflineBoard {
            rows: vec![committed, row(JobState::Queued, Some(100), 0)],
            committed_bytes: 4096,
            quota_bytes: 10 * 1024,
        };
        let row_plans = plans(&board, LOCALE);
        assert_eq!(row_plans.len(), 2);
        assert_eq!(row_plans[0].kind, OfflineRowKind::Committed);
        assert_eq!(row_plans[1].kind, OfflineRowKind::Queued);
    }
}
