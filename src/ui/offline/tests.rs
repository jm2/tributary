//! Projection tests: every row kind, failure key mapping, truncation,
//! byte formatting, quota text, and the redaction boundary.

use super::*;
use crate::architecture::identity::{MediaKey, SourceId, TrackId};
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
