//! Exact, preview-before-write migration from a Rhythmbox profile.
//!
//! Parsing lives in [`super::rhythmbox_import`]. This module owns the local
//! database authority boundary: policy is explicit and digestible, preview
//! evidence is opaque outside the local layer, and apply revalidates that
//! evidence before one all-or-nothing transaction.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ConnectionTrait, DatabaseConnection, EntityTrait, Set, TransactionTrait,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::architecture::SourceId;
use crate::db::entities::rhythmbox_import_receipt::{
    StoredRhythmboxImportReceipt, RHYTHMBOX_IMPORTER_VERSION_V1, RHYTHMBOX_POLICY_DIGEST_DOMAIN,
};
use crate::db::entities::{playlist, playlist_entry, rhythmbox_import_receipt, track};

use super::rhythmbox_import::{
    RhythmboxDocument, RhythmboxImport, RhythmboxImportIssue, RhythmboxImportIssueKind,
    RhythmboxLocationIssue, RhythmboxNumericField, RhythmboxPlaylistKind, RhythmboxRating,
};
use super::smart_rules::SmartRules;

/// Maximum number of actionable rows retained in each migration-report
/// category. Additional rows are counted without retaining their content.
pub const RHYTHMBOX_MIGRATION_REPORT_DETAIL_LIMIT: usize = 100;
/// Largest UTF-8 path accepted from either a source location or an explicit
/// root remap after component-wise mapping.
pub const RHYTHMBOX_MIGRATION_PATH_BYTE_LIMIT: usize = 64 * 1024;
/// Aggregate mapped-path bytes admitted while building one private plan.
/// Every source song and every playlist occurrence is charged independently
/// before the planner retains any additional path clone.
pub const RHYTHMBOX_MIGRATION_PLANNER_PATH_BYTE_LIMIT: usize = 128 * 1024 * 1024;

/// How a positive Rhythmbox rating should interact with an existing local
/// Tributary rating.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RhythmboxRatingConflictPolicy {
    /// Populate only currently-unrated tracks. A different existing value is
    /// reported and retained.
    KeepTributary,
    /// Replace a different existing value after the user explicitly selected
    /// this policy before preview.
    UseRhythmbox,
}

/// One explicit, reviewed relocation from the paths recorded by Rhythmbox to
/// the current local-library root.
#[derive(Clone, Eq, PartialEq)]
pub struct RhythmboxRootRemap {
    from: PathBuf,
    to: PathBuf,
}

impl RhythmboxRootRemap {
    pub fn new(from: PathBuf, to: PathBuf) -> Result<Self, RhythmboxPolicyError> {
        validate_root(&from)?;
        validate_root(&to)?;
        if from == to {
            return Err(RhythmboxPolicyError::IdenticalRoots);
        }
        Ok(Self { from, to })
    }

    pub fn from(&self) -> &Path {
        &self.from
    }

    pub fn to(&self) -> &Path {
        &self.to
    }

    /// Apply only an exact component-prefix relocation. Paths outside the old
    /// root are intentionally left unchanged; no basename search or metadata
    /// fallback is permitted.
    fn map(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.from)
            .map_or_else(|_| path.to_path_buf(), |relative| self.to.join(relative))
    }
}

impl fmt::Debug for RhythmboxRootRemap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxRootRemap")
            .field("paths", &"<redacted>")
            .finish()
    }
}

fn validate_root(path: &Path) -> Result<(), RhythmboxPolicyError> {
    if !path.is_absolute() {
        return Err(RhythmboxPolicyError::RootNotAbsolute);
    }
    let text = path.to_str().ok_or(RhythmboxPolicyError::RootNotUtf8)?;
    if text.len() > RHYTHMBOX_MIGRATION_PATH_BYTE_LIMIT {
        return Err(RhythmboxPolicyError::RootTooLong);
    }
    // `Path::components` normalizes current-directory components away. Inspect
    // the original text first so `/music/./library` and `/music/library/.`
    // cannot be admitted as if the user supplied their normalized spellings.
    // `is_separator` follows the target platform, including both accepted
    // Windows separators without treating a Unix backslash as structural.
    if text
        .split(std::path::is_separator)
        .any(|segment| matches!(segment, "." | ".."))
    {
        return Err(RhythmboxPolicyError::RootNotNormalized);
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(RhythmboxPolicyError::RootNotNormalized);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RhythmboxPolicyError {
    #[error("migration roots must be absolute")]
    RootNotAbsolute,
    #[error("migration roots must be valid Unicode")]
    RootNotUtf8,
    #[error("migration roots must not contain dot components")]
    RootNotNormalized,
    #[error("migration roots exceed the path-length limit")]
    RootTooLong,
    #[error("migration roots must differ")]
    IdenticalRoots,
}

/// Every user-visible choice that can change durable migration results.
///
/// Its digest is stored with the source snapshot digest. This makes an exact
/// retry idempotent without retaining paths, playlist names, or media data in
/// the receipt table.
#[derive(Clone, Eq, PartialEq)]
pub struct RhythmboxMigrationPolicy {
    pub import_ratings: bool,
    pub rating_conflicts: RhythmboxRatingConflictPolicy,
    pub import_play_counts: bool,
    pub import_last_played: bool,
    root_remap: Option<RhythmboxRootRemap>,
}

impl Default for RhythmboxMigrationPolicy {
    fn default() -> Self {
        Self {
            import_ratings: true,
            rating_conflicts: RhythmboxRatingConflictPolicy::KeepTributary,
            import_play_counts: true,
            // A last-played timestamp is more sensitive than an aggregate
            // count and was not named by issue #57, so it requires a separate
            // affirmative choice.
            import_last_played: false,
            root_remap: None,
        }
    }
}

impl RhythmboxMigrationPolicy {
    pub fn with_root_remap(mut self, remap: RhythmboxRootRemap) -> Self {
        self.root_remap = Some(remap);
        self
    }

    pub fn root_remap(&self) -> Option<&RhythmboxRootRemap> {
        self.root_remap.as_ref()
    }

    fn map_path(&self, path: &Path) -> PathBuf {
        self.root_remap
            .as_ref()
            .map_or_else(|| path.to_path_buf(), |remap| remap.map(path))
    }

    pub(crate) fn digest(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(RHYTHMBOX_POLICY_DIGEST_DOMAIN);
        digest.update([
            u8::from(self.import_ratings),
            match self.rating_conflicts {
                RhythmboxRatingConflictPolicy::KeepTributary => 0,
                RhythmboxRatingConflictPolicy::UseRhythmbox => 1,
            },
            u8::from(self.import_play_counts),
            u8::from(self.import_last_played),
            u8::from(self.root_remap.is_some()),
        ]);
        if let Some(remap) = &self.root_remap {
            hash_framed(
                &mut digest,
                remap
                    .from
                    .to_str()
                    .expect("validated Rhythmbox remap root is Unicode")
                    .as_bytes(),
            );
            hash_framed(
                &mut digest,
                remap
                    .to
                    .to_str()
                    .expect("validated Rhythmbox remap root is Unicode")
                    .as_bytes(),
            );
        }
        digest.finalize().into()
    }
}

fn retain_bounded_mapped_path(
    policy: &RhythmboxMigrationPolicy,
    path: &Path,
    retained_path_bytes: &mut usize,
) -> Result<PathBuf, RhythmboxMigrationError> {
    let mapped = policy.map_path(path);
    let bytes = mapped
        .to_str()
        .ok_or(RhythmboxMigrationError::InvalidSnapshot)?
        .len();
    if bytes > RHYTHMBOX_MIGRATION_PATH_BYTE_LIMIT {
        return Err(RhythmboxMigrationError::LimitExceeded);
    }
    *retained_path_bytes = retained_path_bytes
        .checked_add(bytes)
        .filter(|total| *total <= RHYTHMBOX_MIGRATION_PLANNER_PATH_BYTE_LIMIT)
        .ok_or(RhythmboxMigrationError::LimitExceeded)?;
    Ok(mapped)
}

impl fmt::Debug for RhythmboxMigrationPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxMigrationPolicy")
            .field("import_ratings", &self.import_ratings)
            .field("rating_conflicts", &self.rating_conflicts)
            .field("import_play_counts", &self.import_play_counts)
            .field("import_last_played", &self.import_last_played)
            .field("root_remap", &self.root_remap.is_some())
            .finish()
    }
}

fn hash_framed(digest: &mut Sha256, bytes: &[u8]) {
    let byte_len = u64::try_from(bytes.len()).expect("usize always fits in u64");
    digest.update(byte_len.to_be_bytes());
    digest.update(bytes);
}

/// Content-free counts safe to present in GTK and diagnostic logs.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RhythmboxMigrationSummary {
    /// Valid, parsed local song rows, not every raw `rhythmdb.xml` entry.
    pub source_tracks: usize,
    pub matched_tracks: usize,
    pub unmatched_tracks: usize,
    pub duplicate_track_locations: usize,
    pub ratings_to_update: usize,
    pub rating_conflicts_kept: usize,
    pub rating_conflicts_replaced: usize,
    pub play_counts_to_update: usize,
    pub last_played_to_update: usize,
    pub static_playlists_to_create: usize,
    pub automatic_playlists_to_create: usize,
    pub playlist_name_conflicts: usize,
    pub playlist_entries_matched: usize,
    pub playlist_entries_unmatched: usize,
    pub playlist_entries_invalid: usize,
    pub queues_skipped: usize,
    pub unsupported_playlists: usize,
    pub parser_issues: usize,
    pub already_applied: bool,
}

/// One independently bounded category in a migration report.
#[derive(Clone, Eq, PartialEq)]
pub struct RhythmboxMigrationReportSection<T> {
    details: Vec<T>,
    omitted: usize,
}

impl<T> Default for RhythmboxMigrationReportSection<T> {
    fn default() -> Self {
        Self {
            details: Vec::new(),
            omitted: 0,
        }
    }
}

impl<T> RhythmboxMigrationReportSection<T> {
    pub fn details(&self) -> &[T] {
        &self.details
    }

    pub const fn omitted(&self) -> usize {
        self.omitted
    }

    const fn has_any(&self) -> bool {
        !self.details.is_empty() || self.omitted != 0
    }

    fn push(&mut self, detail: T) -> Result<(), RhythmboxMigrationError> {
        if self.details.len() < RHYTHMBOX_MIGRATION_REPORT_DETAIL_LIMIT {
            self.details.push(detail);
        } else {
            self.omitted = self
                .omitted
                .checked_add(1)
                .ok_or(RhythmboxMigrationError::LimitExceeded)?;
        }
        Ok(())
    }
}

impl<T> fmt::Debug for RhythmboxMigrationReportSection<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxMigrationReportSection")
            .field("retained", &self.details.len())
            .field("omitted", &self.omitted)
            .finish()
    }
}

/// Source document containing a parser issue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RhythmboxMigrationSourceDocument {
    RhythmDb,
    Playlists,
}

/// Closed, content-free parser issue category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RhythmboxParserIssueReason {
    MissingLocation,
    MalformedLocation,
    NonFileLocation,
    RemoteLocation,
    LocationCredentials,
    LocationPort,
    LocationQuery,
    LocationFragment,
    NonAbsoluteLocation,
    NonUnicodeLocation,
    LocationContainsNul,
    LocationParentTraversal,
    InvalidRating,
    InvalidPlayCount,
    InvalidLastPlayed,
    UnsupportedEntryType,
    UnsupportedPlaylistType,
}

/// One parser issue identified solely by source ordinals and a closed reason.
#[derive(Clone, Eq, PartialEq)]
pub struct RhythmboxParserIssueDetail {
    document: RhythmboxMigrationSourceDocument,
    item_ordinal: usize,
    entry_ordinal: Option<usize>,
    reason: RhythmboxParserIssueReason,
}

impl RhythmboxParserIssueDetail {
    pub const fn document(&self) -> RhythmboxMigrationSourceDocument {
        self.document
    }

    pub const fn item_ordinal(&self) -> usize {
        self.item_ordinal
    }

    pub const fn entry_ordinal(&self) -> Option<usize> {
        self.entry_ordinal
    }

    pub const fn reason(&self) -> RhythmboxParserIssueReason {
        self.reason
    }
}

impl fmt::Debug for RhythmboxParserIssueDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxParserIssueDetail")
            .field("document", &self.document)
            .field("item_ordinal", &self.item_ordinal)
            .field("entry_ordinal", &self.entry_ordinal)
            .field("reason", &self.reason)
            .finish()
    }
}

/// One source track whose mapped path has no exact local-library match.
#[derive(Clone, Eq, PartialEq)]
pub struct RhythmboxUnmatchedTrackDetail {
    source_ordinal: usize,
    path: PathBuf,
}

impl RhythmboxUnmatchedTrackDetail {
    pub const fn source_ordinal(&self) -> usize {
        self.source_ordinal
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Debug for RhythmboxUnmatchedTrackDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxUnmatchedTrackDetail")
            .field("source_ordinal", &self.source_ordinal)
            .field("path", &"<redacted>")
            .finish()
    }
}

/// One mapped path shared by multiple Rhythmbox source rows.
#[derive(Clone, Eq, PartialEq)]
pub struct RhythmboxDuplicateLocationDetail {
    path: PathBuf,
    source_count: usize,
}

impl RhythmboxDuplicateLocationDetail {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub const fn source_count(&self) -> usize {
        self.source_count
    }
}

impl fmt::Debug for RhythmboxDuplicateLocationDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxDuplicateLocationDetail")
            .field("path", &"<redacted>")
            .field("source_count", &self.source_count)
            .finish()
    }
}

/// Resolution selected for a different existing and incoming rating.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RhythmboxRatingConflictResolution {
    KeptTributary,
    ReplacedWithRhythmbox,
}

/// One rating conflict at an exactly matched path.
#[derive(Clone, Eq, PartialEq)]
pub struct RhythmboxRatingConflictDetail {
    path: PathBuf,
    resolution: RhythmboxRatingConflictResolution,
}

impl RhythmboxRatingConflictDetail {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub const fn resolution(&self) -> RhythmboxRatingConflictResolution {
        self.resolution
    }
}

impl fmt::Debug for RhythmboxRatingConflictDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxRatingConflictDetail")
            .field("path", &"<redacted>")
            .field("resolution", &self.resolution)
            .finish()
    }
}

/// Why a source playlist name cannot be created unchanged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RhythmboxPlaylistNameConflictReason {
    Empty,
    AlreadyExists,
    DuplicateInSource,
}

/// One skipped playlist name, identified by source ordinal.
#[derive(Clone, Eq, PartialEq)]
pub struct RhythmboxPlaylistNameConflictDetail {
    source_ordinal: usize,
    name: String,
    reason: RhythmboxPlaylistNameConflictReason,
}

impl RhythmboxPlaylistNameConflictDetail {
    pub const fn source_ordinal(&self) -> usize {
        self.source_ordinal
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn reason(&self) -> RhythmboxPlaylistNameConflictReason {
        self.reason
    }
}

impl fmt::Debug for RhythmboxPlaylistNameConflictDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxPlaylistNameConflictDetail")
            .field("source_ordinal", &self.source_ordinal)
            .field("name", &"<redacted>")
            .field("reason", &self.reason)
            .finish()
    }
}

/// One intentionally skipped Rhythmbox queue.
#[derive(Clone, Eq, PartialEq)]
pub struct RhythmboxQueueDetail {
    source_ordinal: usize,
    name: String,
    entry_count: usize,
}

impl RhythmboxQueueDetail {
    pub const fn source_ordinal(&self) -> usize {
        self.source_ordinal
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn entry_count(&self) -> usize {
        self.entry_count
    }
}

impl fmt::Debug for RhythmboxQueueDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxQueueDetail")
            .field("source_ordinal", &self.source_ordinal)
            .field("name", &"<redacted>")
            .field("entry_count", &self.entry_count)
            .finish()
    }
}

/// Closed, broad reason why a playlist cannot be represented exactly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RhythmboxUnsupportedPlaylistReason {
    UnsupportedSourceType,
    AutomaticAttributes,
    AutomaticLimit,
    AutomaticSort,
    AutomaticQueryShape,
    AutomaticBooleanShape,
    AutomaticPredicate,
    AutomaticRatingSemantics,
}

/// One source playlist omitted because its semantics are unsupported.
#[derive(Clone, Eq, PartialEq)]
pub struct RhythmboxUnsupportedPlaylistDetail {
    source_ordinal: usize,
    name: String,
    reason: RhythmboxUnsupportedPlaylistReason,
}

impl RhythmboxUnsupportedPlaylistDetail {
    pub const fn source_ordinal(&self) -> usize {
        self.source_ordinal
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn reason(&self) -> RhythmboxUnsupportedPlaylistReason {
        self.reason
    }
}

impl fmt::Debug for RhythmboxUnsupportedPlaylistDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxUnsupportedPlaylistDetail")
            .field("source_ordinal", &self.source_ordinal)
            .field("name", &"<redacted>")
            .field("reason", &self.reason)
            .finish()
    }
}

/// One static-playlist occurrence with no valid local file location.
#[derive(Clone, Eq, PartialEq)]
pub struct RhythmboxInvalidStaticOccurrenceDetail {
    playlist_name: String,
    entry_ordinal: usize,
}

impl RhythmboxInvalidStaticOccurrenceDetail {
    pub fn playlist_name(&self) -> &str {
        &self.playlist_name
    }

    pub const fn entry_ordinal(&self) -> usize {
        self.entry_ordinal
    }
}

impl fmt::Debug for RhythmboxInvalidStaticOccurrenceDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxInvalidStaticOccurrenceDetail")
            .field("playlist_name", &"<redacted>")
            .field("entry_ordinal", &self.entry_ordinal)
            .finish()
    }
}

/// One path-only static-playlist occurrence with no exact local track match.
#[derive(Clone, Eq, PartialEq)]
pub struct RhythmboxUnmatchedPlaylistOccurrenceDetail {
    playlist_name: String,
    entry_ordinal: usize,
    path: PathBuf,
}

impl RhythmboxUnmatchedPlaylistOccurrenceDetail {
    pub fn playlist_name(&self) -> &str {
        &self.playlist_name
    }

    pub const fn entry_ordinal(&self) -> usize {
        self.entry_ordinal
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Debug for RhythmboxUnmatchedPlaylistOccurrenceDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxUnmatchedPlaylistOccurrenceDetail")
            .field("playlist_name", &"<redacted>")
            .field("entry_ordinal", &self.entry_ordinal)
            .field("path", &"<redacted>")
            .finish()
    }
}

/// Actionable, independently bounded detail for every safe-subset outcome.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct RhythmboxMigrationReport {
    parser_issues: RhythmboxMigrationReportSection<RhythmboxParserIssueDetail>,
    unmatched_tracks: RhythmboxMigrationReportSection<RhythmboxUnmatchedTrackDetail>,
    duplicate_locations: RhythmboxMigrationReportSection<RhythmboxDuplicateLocationDetail>,
    rating_conflicts: RhythmboxMigrationReportSection<RhythmboxRatingConflictDetail>,
    playlist_name_conflicts: RhythmboxMigrationReportSection<RhythmboxPlaylistNameConflictDetail>,
    queues: RhythmboxMigrationReportSection<RhythmboxQueueDetail>,
    unsupported_playlists: RhythmboxMigrationReportSection<RhythmboxUnsupportedPlaylistDetail>,
    invalid_static_occurrences:
        RhythmboxMigrationReportSection<RhythmboxInvalidStaticOccurrenceDetail>,
    unmatched_playlist_occurrences:
        RhythmboxMigrationReportSection<RhythmboxUnmatchedPlaylistOccurrenceDetail>,
}

impl RhythmboxMigrationReport {
    pub const fn parser_issues(
        &self,
    ) -> &RhythmboxMigrationReportSection<RhythmboxParserIssueDetail> {
        &self.parser_issues
    }

    pub const fn unmatched_tracks(
        &self,
    ) -> &RhythmboxMigrationReportSection<RhythmboxUnmatchedTrackDetail> {
        &self.unmatched_tracks
    }

    pub const fn duplicate_locations(
        &self,
    ) -> &RhythmboxMigrationReportSection<RhythmboxDuplicateLocationDetail> {
        &self.duplicate_locations
    }

    pub const fn rating_conflicts(
        &self,
    ) -> &RhythmboxMigrationReportSection<RhythmboxRatingConflictDetail> {
        &self.rating_conflicts
    }

    pub const fn playlist_name_conflicts(
        &self,
    ) -> &RhythmboxMigrationReportSection<RhythmboxPlaylistNameConflictDetail> {
        &self.playlist_name_conflicts
    }

    pub const fn queues(&self) -> &RhythmboxMigrationReportSection<RhythmboxQueueDetail> {
        &self.queues
    }

    pub const fn unsupported_playlists(
        &self,
    ) -> &RhythmboxMigrationReportSection<RhythmboxUnsupportedPlaylistDetail> {
        &self.unsupported_playlists
    }

    pub const fn invalid_static_occurrences(
        &self,
    ) -> &RhythmboxMigrationReportSection<RhythmboxInvalidStaticOccurrenceDetail> {
        &self.invalid_static_occurrences
    }

    pub const fn unmatched_playlist_occurrences(
        &self,
    ) -> &RhythmboxMigrationReportSection<RhythmboxUnmatchedPlaylistOccurrenceDetail> {
        &self.unmatched_playlist_occurrences
    }

    /// Whether applying this preview requires explicit confirmation of a
    /// safe-subset omission, conflict resolution, or path-only result.
    pub const fn requires_acknowledgement(&self) -> bool {
        self.parser_issues.has_any()
            || self.unmatched_tracks.has_any()
            || self.duplicate_locations.has_any()
            || self.rating_conflicts.has_any()
            || self.playlist_name_conflicts.has_any()
            || self.queues.has_any()
            || self.unsupported_playlists.has_any()
            || self.invalid_static_occurrences.has_any()
            || self.unmatched_playlist_occurrences.has_any()
    }
}

impl fmt::Debug for RhythmboxMigrationReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxMigrationReport")
            .field("parser_issues", &self.parser_issues)
            .field("unmatched_tracks", &self.unmatched_tracks)
            .field("duplicate_locations", &self.duplicate_locations)
            .field("rating_conflicts", &self.rating_conflicts)
            .field("playlist_name_conflicts", &self.playlist_name_conflicts)
            .field("queues", &self.queues)
            .field("unsupported_playlists", &self.unsupported_playlists)
            .field(
                "invalid_static_occurrences",
                &self.invalid_static_occurrences,
            )
            .field(
                "unmatched_playlist_occurrences",
                &self.unmatched_playlist_occurrences,
            )
            .finish()
    }
}

/// Opaque preview evidence. GTK can display the bounded local-only report and
/// return this token for apply, but exact track IDs, expected database state,
/// and playlist mutation evidence remain owned by the local migration layer.
pub struct RhythmboxMigrationRequest {
    request_id: Uuid,
    summary: RhythmboxMigrationSummary,
    report: RhythmboxMigrationReport,
    // Filled by the planner below. Keeping the field private is the security
    // boundary even though the token crosses the GTK callback layer.
    prepared: PreparedRhythmboxMigration,
}

impl RhythmboxMigrationRequest {
    pub fn request_id(&self) -> Uuid {
        self.request_id
    }

    pub fn summary(&self) -> &RhythmboxMigrationSummary {
        &self.summary
    }

    pub fn report(&self) -> &RhythmboxMigrationReport {
        &self.report
    }

    pub const fn requires_acknowledgement(&self) -> bool {
        self.report.requires_acknowledgement()
    }
}

impl fmt::Debug for RhythmboxMigrationRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RhythmboxMigrationRequest")
            .field("request_id", &self.request_id)
            .field("summary", &self.summary)
            .field("report", &self.report)
            .field("evidence", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// Result of applying one still-current preview token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RhythmboxMigrationOutcome {
    Applied,
    AlreadyApplied,
}

/// Closed, content-free result sent back to GTK after the serialized command
/// finishes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RhythmboxMigrationCompletion {
    Applied,
    /// The transaction committed, but one or more UI publication paths were
    /// unavailable. Retrying the exact migration will return `AlreadyApplied`.
    AppliedRefreshFailed,
    AlreadyApplied,
    Stale,
    Failed,
}

#[derive(Error)]
pub enum RhythmboxMigrationError {
    #[error("the local library changed after the Rhythmbox preview")]
    Stale,
    #[error("the Rhythmbox migration plan exceeds a supported bound")]
    LimitExceeded,
    #[error("the Rhythmbox migration snapshot is not internally consistent")]
    InvalidSnapshot,
    #[error("the Rhythmbox migration could not access local storage")]
    Storage(#[source] sea_orm::DbErr),
}

impl fmt::Debug for RhythmboxMigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stale => formatter.write_str("RhythmboxMigrationError::Stale"),
            Self::LimitExceeded => formatter.write_str("RhythmboxMigrationError::LimitExceeded"),
            Self::InvalidSnapshot => {
                formatter.write_str("RhythmboxMigrationError::InvalidSnapshot")
            }
            Self::Storage(_) => formatter
                .debug_tuple("RhythmboxMigrationError::Storage")
                .field(&"<redacted>")
                .finish(),
        }
    }
}

impl From<sea_orm::DbErr> for RhythmboxMigrationError {
    fn from(error: sea_orm::DbErr) -> Self {
        Self::Storage(error)
    }
}

struct ExpectedTrackState {
    id: String,
    file_path: String,
    play_count: i32,
    last_played_at_ms: Option<i64>,
    rating: Option<i32>,
}

struct ExpectedPathMatch {
    file_path: String,
    track_id: Option<String>,
}

struct ExpectedPlaylistNamePresence {
    name: String,
    present: bool,
}

struct PreparedTrackUpdate {
    expected: ExpectedTrackState,
    play_count: i32,
    last_played_at_ms: Option<i64>,
    rating: Option<i32>,
}

struct PreparedPlaylistEntry {
    track_id: Option<String>,
    file_path: String,
}

enum PreparedPlaylistKind {
    Regular(Vec<PreparedPlaylistEntry>),
    Smart(SmartRules),
}

struct PreparedPlaylist {
    name: String,
    kind: PreparedPlaylistKind,
}

/// Private transaction plan retained by the opaque request token.
#[derive(Default)]
struct PreparedRhythmboxMigration {
    snapshot_digest: [u8; 32],
    policy_digest: [u8; 32],
    expected_path_matches: Vec<ExpectedPathMatch>,
    expected_tracks: Vec<ExpectedTrackState>,
    // One row per unique incoming exact name, bounded by the parser's
    // playlist ceiling. This includes names skipped during preview.
    expected_playlist_name_presence: Vec<ExpectedPlaylistNamePresence>,
    track_updates: Vec<PreparedTrackUpdate>,
    playlists: Vec<PreparedPlaylist>,
}

fn parser_issue_detail(issue: RhythmboxImportIssue) -> RhythmboxParserIssueDetail {
    let document = match issue.document {
        RhythmboxDocument::RhythmDb => RhythmboxMigrationSourceDocument::RhythmDb,
        RhythmboxDocument::Playlists => RhythmboxMigrationSourceDocument::Playlists,
    };
    let reason = match issue.kind {
        RhythmboxImportIssueKind::MissingLocation => RhythmboxParserIssueReason::MissingLocation,
        RhythmboxImportIssueKind::InvalidLocation(reason) => match reason {
            RhythmboxLocationIssue::Malformed => RhythmboxParserIssueReason::MalformedLocation,
            RhythmboxLocationIssue::NonFileScheme => RhythmboxParserIssueReason::NonFileLocation,
            RhythmboxLocationIssue::RemoteAuthority => RhythmboxParserIssueReason::RemoteLocation,
            RhythmboxLocationIssue::Credentials => RhythmboxParserIssueReason::LocationCredentials,
            RhythmboxLocationIssue::Port => RhythmboxParserIssueReason::LocationPort,
            RhythmboxLocationIssue::Query => RhythmboxParserIssueReason::LocationQuery,
            RhythmboxLocationIssue::Fragment => RhythmboxParserIssueReason::LocationFragment,
            RhythmboxLocationIssue::NotAbsolute => RhythmboxParserIssueReason::NonAbsoluteLocation,
            RhythmboxLocationIssue::NotUtf8 => RhythmboxParserIssueReason::NonUnicodeLocation,
            RhythmboxLocationIssue::ContainsNul => RhythmboxParserIssueReason::LocationContainsNul,
            RhythmboxLocationIssue::ParentTraversal => {
                RhythmboxParserIssueReason::LocationParentTraversal
            }
        },
        RhythmboxImportIssueKind::InvalidNumeric(field) => match field {
            RhythmboxNumericField::Rating => RhythmboxParserIssueReason::InvalidRating,
            RhythmboxNumericField::PlayCount => RhythmboxParserIssueReason::InvalidPlayCount,
            RhythmboxNumericField::LastPlayed => RhythmboxParserIssueReason::InvalidLastPlayed,
        },
        RhythmboxImportIssueKind::UnsupportedEntryType => {
            RhythmboxParserIssueReason::UnsupportedEntryType
        }
        RhythmboxImportIssueKind::UnsupportedPlaylistType => {
            RhythmboxParserIssueReason::UnsupportedPlaylistType
        }
    };
    RhythmboxParserIssueDetail {
        document,
        item_ordinal: issue.item_ordinal,
        entry_ordinal: issue.entry_ordinal,
        reason,
    }
}

const fn unsupported_automatic_reason(
    reason: super::rhythmbox_smart_playlist::RhythmboxSmartPlaylistUnsupported,
) -> RhythmboxUnsupportedPlaylistReason {
    use super::rhythmbox_smart_playlist::RhythmboxSmartPlaylistUnsupported as Reason;

    match reason {
        Reason::PlaylistAttribute => RhythmboxUnsupportedPlaylistReason::AutomaticAttributes,
        Reason::LimitKind | Reason::LimitValue | Reason::LimitOrdering => {
            RhythmboxUnsupportedPlaylistReason::AutomaticLimit
        }
        Reason::SortConfiguration | Reason::SortTieBreak => {
            RhythmboxUnsupportedPlaylistReason::AutomaticSort
        }
        Reason::QueryRoot
        | Reason::OuterShape
        | Reason::SongTypeGuard
        | Reason::SubqueryShape
        | Reason::EmptyQuery => RhythmboxUnsupportedPlaylistReason::AutomaticQueryShape,
        Reason::BooleanShape | Reason::BooleanSeparator => {
            RhythmboxUnsupportedPlaylistReason::AutomaticBooleanShape
        }
        Reason::PredicateShape
        | Reason::PredicateProperty
        | Reason::PredicateOperator
        | Reason::PredicateValue
        | Reason::PredicateRange => RhythmboxUnsupportedPlaylistReason::AutomaticPredicate,
        Reason::PredicateMissingValueSemantics | Reason::RatingNotEqual | Reason::RatingGrid => {
            RhythmboxUnsupportedPlaylistReason::AutomaticRatingSemantics
        }
    }
}

/// Build a display-safe summary and an opaque exact-state token without
/// mutating the local database.
pub async fn prepare_rhythmbox_migration(
    db: &DatabaseConnection,
    import: RhythmboxImport,
    policy: RhythmboxMigrationPolicy,
) -> Result<RhythmboxMigrationRequest, RhythmboxMigrationError> {
    let snapshot_digest = *import.semantic_digest.as_bytes();
    let policy_digest = policy.digest();
    let mut summary = RhythmboxMigrationSummary {
        source_tracks: import.tracks.len(),
        parser_issues: import.issues.len(),
        ..RhythmboxMigrationSummary::default()
    };
    let mut report = RhythmboxMigrationReport::default();
    for issue in &import.issues {
        report.parser_issues.push(parser_issue_detail(*issue))?;
    }

    let database_tracks = track::Entity::find().all(db).await?;
    let tracks_by_path: HashMap<&str, &track::Model> = database_tracks
        .iter()
        .map(|track| (track.file_path.as_str(), track))
        .collect();
    let mut expected_tracks = HashMap::<String, ExpectedTrackState>::new();
    let mut expected_path_matches = HashMap::<String, Option<String>>::new();
    let mut mapped_source_tracks = HashMap::<PathBuf, Vec<usize>>::new();
    let mut retained_mapped_path_bytes = 0usize;

    for (index, source) in import.tracks.iter().enumerate() {
        let mapped = retain_bounded_mapped_path(
            &policy,
            source.location.as_path(),
            &mut retained_mapped_path_bytes,
        )?;
        mapped_source_tracks.entry(mapped).or_default().push(index);
    }

    let mut track_updates = Vec::new();
    let mut mapped_paths: Vec<PathBuf> = mapped_source_tracks.keys().cloned().collect();
    mapped_paths.sort();
    for mapped_path in mapped_paths {
        let source_indices = mapped_source_tracks
            .get(&mapped_path)
            .expect("mapped path was collected from this map");
        let Some(mapped_path_text) = mapped_path.to_str() else {
            for source_index in source_indices {
                summary.unmatched_tracks += 1;
                report
                    .unmatched_tracks
                    .push(RhythmboxUnmatchedTrackDetail {
                        source_ordinal: import.tracks[*source_index].source_ordinal,
                        path: mapped_path.clone(),
                    })?;
            }
            continue;
        };
        expected_path_matches
            .entry(mapped_path_text.to_string())
            .or_insert_with(|| {
                tracks_by_path
                    .get(mapped_path_text)
                    .map(|track| track.id.clone())
            });
        if source_indices.len() != 1 {
            summary.duplicate_track_locations = summary
                .duplicate_track_locations
                .checked_add(source_indices.len())
                .ok_or(RhythmboxMigrationError::LimitExceeded)?;
            report
                .duplicate_locations
                .push(RhythmboxDuplicateLocationDetail {
                    path: mapped_path,
                    source_count: source_indices.len(),
                })?;
            continue;
        }
        let source = &import.tracks[source_indices[0]];
        let Some(target) = tracks_by_path.get(mapped_path_text).copied() else {
            summary.unmatched_tracks += 1;
            report
                .unmatched_tracks
                .push(RhythmboxUnmatchedTrackDetail {
                    source_ordinal: source.source_ordinal,
                    path: mapped_path,
                })?;
            continue;
        };
        summary.matched_tracks += 1;

        expected_tracks
            .entry(target.id.clone())
            .or_insert_with(|| expected_track_state(target));

        let mut proposed = PreparedTrackUpdate {
            expected: expected_track_state(target),
            play_count: target.play_count,
            last_played_at_ms: target.last_played_at_ms,
            rating: target.rating,
        };
        let mut changed = false;

        if policy.import_play_counts {
            if let Some(incoming) = source.play_count {
                if let Ok(incoming) = i32::try_from(incoming) {
                    let merged = target.play_count.max(0).max(incoming);
                    if merged != target.play_count {
                        proposed.play_count = merged;
                        summary.play_counts_to_update += 1;
                        changed = true;
                    }
                }
            }
        }

        if policy.import_last_played {
            if let Some(seconds) = source.last_played_unix_seconds {
                if seconds != 0 {
                    if let Some(milliseconds) = seconds
                        .checked_mul(1_000)
                        .and_then(|value| i64::try_from(value).ok())
                    {
                        let merged = target
                            .last_played_at_ms
                            .map_or(milliseconds, |existing| existing.max(milliseconds));
                        if Some(merged) != target.last_played_at_ms {
                            proposed.last_played_at_ms = Some(merged);
                            summary.last_played_to_update += 1;
                            changed = true;
                        }
                    }
                }
            }
        }

        if policy.import_ratings {
            if let Some(incoming) = source.rating.and_then(canonical_rating) {
                match target.rating {
                    None => {
                        proposed.rating = Some(incoming);
                        summary.ratings_to_update += 1;
                        changed = true;
                    }
                    Some(existing) if existing == incoming => {}
                    Some(_)
                        if policy.rating_conflicts
                            == RhythmboxRatingConflictPolicy::UseRhythmbox =>
                    {
                        proposed.rating = Some(incoming);
                        summary.ratings_to_update += 1;
                        summary.rating_conflicts_replaced += 1;
                        report
                            .rating_conflicts
                            .push(RhythmboxRatingConflictDetail {
                                path: mapped_path.clone(),
                                resolution:
                                    RhythmboxRatingConflictResolution::ReplacedWithRhythmbox,
                            })?;
                        changed = true;
                    }
                    Some(_) => {
                        summary.rating_conflicts_kept += 1;
                        report
                            .rating_conflicts
                            .push(RhythmboxRatingConflictDetail {
                                path: mapped_path.clone(),
                                resolution: RhythmboxRatingConflictResolution::KeptTributary,
                            })?;
                    }
                }
            }
        }

        if changed {
            track_updates.push(proposed);
        }
    }

    let existing_playlist_names: HashSet<String> = crate::db::entities::playlist::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|playlist| playlist.name)
        .collect();
    let mut incoming_name_counts = HashMap::<&str, usize>::new();
    for playlist in &import.playlists {
        *incoming_name_counts
            .entry(playlist.name.as_str())
            .or_default() += 1;
    }
    let mut expected_playlist_name_presence: Vec<ExpectedPlaylistNamePresence> =
        incoming_name_counts
            .keys()
            .map(|name| ExpectedPlaylistNamePresence {
                name: (*name).to_string(),
                present: existing_playlist_names.contains(*name),
            })
            .collect();
    expected_playlist_name_presence.sort_by(|left, right| left.name.cmp(&right.name));

    let source_ratings: Vec<RhythmboxRating> = import
        .tracks
        .iter()
        .filter_map(|track| track.rating)
        .collect();
    let mut playlists = Vec::new();
    for playlist in &import.playlists {
        let name_conflict = if playlist.name.is_empty() {
            Some(RhythmboxPlaylistNameConflictReason::Empty)
        } else if existing_playlist_names.contains(&playlist.name) {
            Some(RhythmboxPlaylistNameConflictReason::AlreadyExists)
        } else if incoming_name_counts
            .get(playlist.name.as_str())
            .is_some_and(|count| *count > 1)
        {
            Some(RhythmboxPlaylistNameConflictReason::DuplicateInSource)
        } else {
            None
        };
        if let Some(reason) = name_conflict {
            summary.playlist_name_conflicts += 1;
            report
                .playlist_name_conflicts
                .push(RhythmboxPlaylistNameConflictDetail {
                    source_ordinal: playlist.source_ordinal,
                    name: playlist.name.clone(),
                    reason,
                })?;
            continue;
        }

        match &playlist.kind {
            RhythmboxPlaylistKind::Static(entries) => {
                let mut prepared_entries = Vec::with_capacity(entries.len());
                for entry in entries {
                    let Some(location) = &entry.location else {
                        summary.playlist_entries_invalid += 1;
                        report.invalid_static_occurrences.push(
                            RhythmboxInvalidStaticOccurrenceDetail {
                                playlist_name: playlist.name.clone(),
                                entry_ordinal: entry.source_ordinal,
                            },
                        )?;
                        continue;
                    };
                    let mapped = retain_bounded_mapped_path(
                        &policy,
                        location.as_path(),
                        &mut retained_mapped_path_bytes,
                    )?;
                    let Some(file_path) = mapped.to_str().map(str::to_string) else {
                        summary.playlist_entries_invalid += 1;
                        report.invalid_static_occurrences.push(
                            RhythmboxInvalidStaticOccurrenceDetail {
                                playlist_name: playlist.name.clone(),
                                entry_ordinal: entry.source_ordinal,
                            },
                        )?;
                        continue;
                    };
                    let track_id = tracks_by_path.get(file_path.as_str()).map(|track| {
                        expected_tracks
                            .entry(track.id.clone())
                            .or_insert_with(|| expected_track_state(track));
                        track.id.clone()
                    });
                    expected_path_matches
                        .entry(file_path.clone())
                        .or_insert_with(|| track_id.clone());
                    if track_id.is_some() {
                        summary.playlist_entries_matched += 1;
                    } else {
                        summary.playlist_entries_unmatched += 1;
                        report.unmatched_playlist_occurrences.push(
                            RhythmboxUnmatchedPlaylistOccurrenceDetail {
                                playlist_name: playlist.name.clone(),
                                entry_ordinal: entry.source_ordinal,
                                path: mapped,
                            },
                        )?;
                    }
                    prepared_entries.push(PreparedPlaylistEntry {
                        track_id,
                        file_path,
                    });
                }
                playlists.push(PreparedPlaylist {
                    name: playlist.name.clone(),
                    kind: PreparedPlaylistKind::Regular(prepared_entries),
                });
                summary.static_playlists_to_create += 1;
            }
            RhythmboxPlaylistKind::Automatic(automatic) => {
                match super::rhythmbox_smart_playlist::translate_automatic_playlist(
                    automatic,
                    &source_ratings,
                ) {
                    Ok(rules) => {
                        playlists.push(PreparedPlaylist {
                            name: playlist.name.clone(),
                            kind: PreparedPlaylistKind::Smart(rules),
                        });
                        summary.automatic_playlists_to_create += 1;
                    }
                    Err(reason) => {
                        summary.unsupported_playlists += 1;
                        report
                            .unsupported_playlists
                            .push(RhythmboxUnsupportedPlaylistDetail {
                                source_ordinal: playlist.source_ordinal,
                                name: playlist.name.clone(),
                                reason: unsupported_automatic_reason(reason),
                            })?;
                    }
                }
            }
            RhythmboxPlaylistKind::Queue(entries) => {
                summary.queues_skipped += 1;
                report.queues.push(RhythmboxQueueDetail {
                    source_ordinal: playlist.source_ordinal,
                    name: playlist.name.clone(),
                    entry_count: entries.len(),
                })?;
            }
            RhythmboxPlaylistKind::Unsupported { .. } => {
                summary.unsupported_playlists += 1;
                report
                    .unsupported_playlists
                    .push(RhythmboxUnsupportedPlaylistDetail {
                        source_ordinal: playlist.source_ordinal,
                        name: playlist.name.clone(),
                        reason: RhythmboxUnsupportedPlaylistReason::UnsupportedSourceType,
                    })?;
            }
        }
    }

    let already_applied = rhythmbox_import_receipt::Entity::find_by_id((
        snapshot_digest.to_vec(),
        RHYTHMBOX_IMPORTER_VERSION_V1,
        policy_digest.to_vec(),
    ))
    .one(db)
    .await?
    .map(StoredRhythmboxImportReceipt::try_from)
    .transpose()
    .map_err(|error| RhythmboxMigrationError::Storage(sea_orm::DbErr::Custom(error.to_string())))?
    .is_some();
    summary.already_applied = already_applied;

    Ok(RhythmboxMigrationRequest {
        request_id: Uuid::new_v4(),
        summary,
        report,
        prepared: PreparedRhythmboxMigration {
            snapshot_digest,
            policy_digest,
            expected_path_matches: expected_path_matches
                .into_iter()
                .map(|(file_path, track_id)| ExpectedPathMatch {
                    file_path,
                    track_id,
                })
                .collect(),
            expected_tracks: expected_tracks.into_values().collect(),
            expected_playlist_name_presence,
            track_updates,
            playlists,
        },
    })
}

fn expected_track_state(track: &track::Model) -> ExpectedTrackState {
    ExpectedTrackState {
        id: track.id.clone(),
        file_path: track.file_path.clone(),
        play_count: track.play_count,
        last_played_at_ms: track.last_played_at_ms,
        rating: track.rating,
    }
}

fn canonical_rating(rating: RhythmboxRating) -> Option<i32> {
    let native = rating.value();
    if native == 0.0 {
        return None;
    }
    let canonical = (native * 20.0).round().clamp(1.0, 100.0);
    #[allow(clippy::cast_possible_truncation)]
    Some(canonical as i32)
}

/// Revalidate and commit an opaque preview as one transaction.
pub async fn apply_rhythmbox_migration(
    db: &DatabaseConnection,
    request: &RhythmboxMigrationRequest,
) -> Result<RhythmboxMigrationOutcome, RhythmboxMigrationError> {
    let prepared = &request.prepared;
    let transaction = db.begin().await?;

    let result = apply_rhythmbox_migration_in(&transaction, prepared).await;
    match result {
        Ok(RhythmboxMigrationOutcome::AlreadyApplied) => {
            transaction.rollback().await?;
            Ok(RhythmboxMigrationOutcome::AlreadyApplied)
        }
        Ok(RhythmboxMigrationOutcome::Applied) => match transaction.commit().await {
            Ok(()) => Ok(RhythmboxMigrationOutcome::Applied),
            Err(error) => {
                // A concurrent process may have committed the exact receipt
                // between preview and our final insert. The transaction has
                // already failed/rolled back; recognize only that exact key.
                if receipt_exists(db, prepared).await? {
                    Ok(RhythmboxMigrationOutcome::AlreadyApplied)
                } else {
                    Err(RhythmboxMigrationError::Storage(error))
                }
            }
        },
        Err(error) => {
            let _ = transaction.rollback().await;
            classify_post_rollback_error(db, prepared, error).await
        }
    }
}

async fn classify_post_rollback_error(
    db: &DatabaseConnection,
    prepared: &PreparedRhythmboxMigration,
    error: RhythmboxMigrationError,
) -> Result<RhythmboxMigrationOutcome, RhythmboxMigrationError> {
    // A concurrent exact retry can win after our initial receipt check but
    // before our receipt insert. SQLite reports that losing insert from
    // inside `apply_rhythmbox_migration_in`, so it reaches this branch rather
    // than the commit-error branch. Recognize only the same bounded digest
    // tuple, and only after this transaction has been rolled back.
    if matches!(&error, RhythmboxMigrationError::Storage(_)) && receipt_exists(db, prepared).await?
    {
        Ok(RhythmboxMigrationOutcome::AlreadyApplied)
    } else {
        Err(error)
    }
}

async fn apply_rhythmbox_migration_in<C>(
    transaction: &C,
    prepared: &PreparedRhythmboxMigration,
) -> Result<RhythmboxMigrationOutcome, RhythmboxMigrationError>
where
    C: ConnectionTrait,
{
    if receipt_exists(transaction, prepared).await? {
        return Ok(RhythmboxMigrationOutcome::AlreadyApplied);
    }

    let current_tracks = track::Entity::find().all(transaction).await?;
    let current_by_id: HashMap<&str, &track::Model> = current_tracks
        .iter()
        .map(|track| (track.id.as_str(), track))
        .collect();
    let current_by_path: HashMap<&str, &track::Model> = current_tracks
        .iter()
        .map(|track| (track.file_path.as_str(), track))
        .collect();
    for expected in &prepared.expected_path_matches {
        let current_track_id = current_by_path
            .get(expected.file_path.as_str())
            .map(|track| track.id.as_str());
        if current_track_id != expected.track_id.as_deref() {
            return Err(RhythmboxMigrationError::Stale);
        }
    }
    for expected in &prepared.expected_tracks {
        let Some(current) = current_by_id.get(expected.id.as_str()).copied() else {
            return Err(RhythmboxMigrationError::Stale);
        };
        if current.file_path != expected.file_path
            || current.play_count != expected.play_count
            || current.last_played_at_ms != expected.last_played_at_ms
            || current.rating != expected.rating
        {
            return Err(RhythmboxMigrationError::Stale);
        }
    }

    let current_playlist_names: HashSet<String> = playlist::Entity::find()
        .all(transaction)
        .await?
        .into_iter()
        .map(|playlist| playlist.name)
        .collect();
    if prepared
        .expected_playlist_name_presence
        .iter()
        .any(|expected| current_playlist_names.contains(&expected.name) != expected.present)
    {
        return Err(RhythmboxMigrationError::Stale);
    }

    for update in &prepared.track_updates {
        if !(1..=100).contains(&update.rating.unwrap_or(1)) || update.play_count < 0 {
            return Err(RhythmboxMigrationError::InvalidSnapshot);
        }
        let current = current_by_id
            .get(update.expected.id.as_str())
            .copied()
            .ok_or(RhythmboxMigrationError::Stale)?;
        let mut active: track::ActiveModel = current.clone().into();
        active.play_count = Set(update.play_count);
        active.last_played_at_ms = Set(update.last_played_at_ms);
        active.rating = Set(update.rating);
        active.update(transaction).await?;
    }

    let now = Utc::now().to_rfc3339();
    for prepared_playlist in &prepared.playlists {
        let id = Uuid::new_v4().to_string();
        let (
            is_smart,
            smart_rules_json,
            match_mode,
            limit_enabled,
            limit_value,
            limit_unit,
            limit_sort,
        ) = match &prepared_playlist.kind {
            PreparedPlaylistKind::Regular(_) => {
                (false, None, "all".to_string(), false, None, None, None)
            }
            PreparedPlaylistKind::Smart(rules) => smart_rule_storage(rules)?,
        };
        playlist::ActiveModel {
            id: Set(id.clone()),
            name: Set(prepared_playlist.name.clone()),
            is_smart: Set(is_smart),
            smart_rules_json: Set(smart_rules_json),
            limit_enabled: Set(limit_enabled),
            limit_value: Set(limit_value),
            limit_unit: Set(limit_unit),
            limit_sort: Set(limit_sort),
            match_mode: Set(match_mode),
            live_updating: Set(true),
            created_at: Set(now.clone()),
            updated_at: Set(now.clone()),
        }
        .insert(transaction)
        .await?;

        let PreparedPlaylistKind::Regular(entries) = &prepared_playlist.kind else {
            continue;
        };
        for (index, entry) in entries.iter().enumerate() {
            let position =
                i32::try_from(index).map_err(|_| RhythmboxMigrationError::LimitExceeded)?;
            playlist_entry::ActiveModel {
                id: Set(Uuid::new_v4().to_string()),
                playlist_id: Set(id.clone()),
                position: Set(position),
                source_id: Set(SourceId::local().to_string()),
                track_id: Set(entry.track_id.clone()),
                local_track_id: Set(entry.track_id.clone()),
                match_title: Set(String::new()),
                match_artist: Set(String::new()),
                match_album: Set(String::new()),
                match_duration_secs: Set(None),
                match_file_path: Set(Some(entry.file_path.clone())),
            }
            .insert(transaction)
            .await?;
        }
    }

    rhythmbox_import_receipt::ActiveModel {
        snapshot_digest: Set(prepared.snapshot_digest.to_vec()),
        importer_version: Set(RHYTHMBOX_IMPORTER_VERSION_V1),
        policy_digest: Set(prepared.policy_digest.to_vec()),
    }
    .insert(transaction)
    .await?;

    Ok(RhythmboxMigrationOutcome::Applied)
}

async fn receipt_exists<C>(
    connection: &C,
    prepared: &PreparedRhythmboxMigration,
) -> Result<bool, RhythmboxMigrationError>
where
    C: ConnectionTrait,
{
    let receipt = rhythmbox_import_receipt::Entity::find_by_id((
        prepared.snapshot_digest.to_vec(),
        RHYTHMBOX_IMPORTER_VERSION_V1,
        prepared.policy_digest.to_vec(),
    ))
    .one(connection)
    .await?;
    receipt
        .map(StoredRhythmboxImportReceipt::try_from)
        .transpose()
        .map(|receipt| receipt.is_some())
        .map_err(|error| {
            RhythmboxMigrationError::Storage(sea_orm::DbErr::Custom(error.to_string()))
        })
}

#[allow(clippy::type_complexity)]
fn smart_rule_storage(
    rules: &SmartRules,
) -> Result<
    (
        bool,
        Option<String>,
        String,
        bool,
        Option<i32>,
        Option<String>,
        Option<String>,
    ),
    RhythmboxMigrationError,
> {
    let json = serde_json::to_string(rules).map_err(|error| {
        RhythmboxMigrationError::Storage(sea_orm::DbErr::Custom(format!(
            "failed to serialize validated Rhythmbox smart rules: {error}"
        )))
    })?;
    let match_mode = match rules.match_mode {
        super::smart_rules::MatchMode::All => "all",
        super::smart_rules::MatchMode::Any => "any",
    }
    .to_string();
    let (limit_enabled, limit_value, limit_unit, limit_sort) = match &rules.limit {
        Some(limit) => (
            true,
            Some(i32::try_from(limit.value).map_err(|_| RhythmboxMigrationError::LimitExceeded)?),
            Some(serde_json::to_string(&limit.unit).map_err(|error| {
                RhythmboxMigrationError::Storage(sea_orm::DbErr::Custom(format!(
                    "failed to serialize validated Rhythmbox smart limit unit: {error}"
                )))
            })?),
            Some(serde_json::to_string(&limit.selected_by).map_err(|error| {
                RhythmboxMigrationError::Storage(sea_orm::DbErr::Custom(format!(
                    "failed to serialize validated Rhythmbox smart limit sort: {error}"
                )))
            })?),
        ),
        None => (false, None, None, None),
    };
    Ok((
        true,
        Some(json),
        match_mode,
        limit_enabled,
        limit_value,
        limit_unit,
        limit_sort,
    ))
}

#[cfg(test)]
mod tests {
    use sea_orm::{
        ActiveModelTrait, ConnectionTrait, Database, DbBackend, EntityTrait, Set, Statement,
    };
    use sea_orm_migration::MigratorTrait;

    use crate::db::entities::{playlist_entry, rhythmbox_import_receipt, track};
    use crate::db::migration::Migrator;

    use super::super::rhythmbox_import::{parse_rhythmbox_documents, RhythmboxImportLimits};
    use super::*;

    async fn database() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory migration database");
        Migrator::up(&db, None).await.expect("run migrations");
        db
    }

    fn local_path(name: &str) -> PathBuf {
        #[cfg(windows)]
        {
            PathBuf::from(format!(r"C:\music\{name}"))
        }
        #[cfg(not(windows))]
        {
            PathBuf::from(format!("/music/{name}"))
        }
    }

    fn file_uri(path: &Path) -> String {
        url::Url::from_file_path(path)
            .expect("absolute fixture path")
            .to_string()
    }

    async fn seed_track(db: &DatabaseConnection, id: &str, path: &Path) -> track::Model {
        track::ActiveModel {
            id: Set(id.to_string()),
            file_path: Set(path.to_str().expect("Unicode fixture path").to_string()),
            title: Set("Untrusted similarity bait".to_string()),
            artist_name: Set("Artist".to_string()),
            album_artist_name: Set(None),
            album_title: Set("Album".to_string()),
            genre: Set(None),
            composer: Set(None),
            year: Set(None),
            track_number: Set(None),
            disc_number: Set(None),
            duration_secs: Set(Some(180)),
            bitrate_kbps: Set(None),
            sample_rate_hz: Set(None),
            format: Set(Some("FLAC".to_string())),
            play_count: Set(2),
            last_played_at_ms: Set(None),
            rating: Set(None),
            date_added: Set("2026-07-20T00:00:00Z".to_string()),
            date_modified: Set("2026-07-20T00:00:00Z".to_string()),
            file_size_bytes: Set(None),
        }
        .insert(db)
        .await
        .expect("seed local track")
    }

    async fn seed_playlist(db: &DatabaseConnection, id: &str, name: &str) {
        playlist::ActiveModel {
            id: Set(id.to_string()),
            name: Set(name.to_string()),
            is_smart: Set(false),
            smart_rules_json: Set(None),
            limit_enabled: Set(false),
            limit_value: Set(None),
            limit_unit: Set(None),
            limit_sort: Set(None),
            match_mode: Set("all".to_string()),
            live_updating: Set(true),
            created_at: Set("2026-07-20T00:00:00Z".to_string()),
            updated_at: Set("2026-07-20T00:00:00Z".to_string()),
        }
        .insert(db)
        .await
        .expect("seed local playlist");
    }

    fn parsed_static_import(matched: &Path, unmatched: &Path) -> RhythmboxImport {
        let matched_uri = file_uri(matched);
        let unmatched_uri = file_uri(unmatched);
        let rhythmdb = format!(
            "<rhythmdb version=\"2.0\"><entry type=\"song\"><location>{matched_uri}</location><rating>4.5</rating><play-count>5</play-count><last-played>123</last-played></entry><entry type=\"song\"><location>{unmatched_uri}</location><play-count>9</play-count></entry></rhythmdb>"
        );
        let playlists = format!(
            "<rhythmdb-playlists><playlist name=\"Migrated\" type=\"static\"><location>{matched_uri}</location><location>{matched_uri}</location><location>{unmatched_uri}</location></playlist></rhythmdb-playlists>"
        );
        parse_rhythmbox_documents(
            rhythmdb.as_bytes(),
            Some(playlists.as_bytes()),
            RhythmboxImportLimits::default(),
        )
        .expect("parse migration fixture")
    }

    #[test]
    fn policy_defaults_keep_existing_ratings_and_exclude_last_played() {
        let policy = RhythmboxMigrationPolicy::default();
        assert!(policy.import_ratings);
        assert!(policy.import_play_counts);
        assert!(!policy.import_last_played);
        assert_eq!(
            policy.rating_conflicts,
            RhythmboxRatingConflictPolicy::KeepTributary
        );
        assert!(policy.root_remap().is_none());
    }

    #[test]
    fn policy_digest_changes_for_every_result_affecting_choice() {
        let baseline = RhythmboxMigrationPolicy::default();
        let mut policies = Vec::new();

        let mut ratings = baseline.clone();
        ratings.import_ratings = false;
        policies.push(ratings);

        let mut overwrite = baseline.clone();
        overwrite.rating_conflicts = RhythmboxRatingConflictPolicy::UseRhythmbox;
        policies.push(overwrite);

        let mut counts = baseline.clone();
        counts.import_play_counts = false;
        policies.push(counts);

        let mut history = baseline.clone();
        history.import_last_played = true;
        policies.push(history);

        #[cfg(unix)]
        policies.push(
            baseline.clone().with_root_remap(
                RhythmboxRootRemap::new(PathBuf::from("/old/music"), PathBuf::from("/new/music"))
                    .unwrap(),
            ),
        );

        for policy in policies {
            assert_ne!(baseline.digest(), policy.digest());
        }
    }

    #[test]
    fn debug_output_never_contains_remap_paths() {
        #[cfg(unix)]
        {
            let remap = RhythmboxRootRemap::new(
                PathBuf::from("/private/old-library"),
                PathBuf::from("/secret/new-library"),
            )
            .unwrap();
            let policy = RhythmboxMigrationPolicy::default().with_root_remap(remap.clone());
            let remap_debug = format!("{remap:?}");
            let policy_debug = format!("{policy:?}");
            assert!(!remap_debug.contains("private"));
            assert!(!policy_debug.contains("private"));
            assert!(!policy_debug.contains("secret"));
        }
    }

    #[test]
    fn migration_error_debug_redacts_storage_error_content() {
        let marker = "private/path-and-playlist-name";
        let error = RhythmboxMigrationError::Storage(sea_orm::DbErr::Custom(marker.to_string()));

        let debug = format!("{error:?}");
        assert!(debug.contains("RhythmboxMigrationError::Storage"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(marker));
        assert!(!error.to_string().contains(marker));

        let source = std::error::Error::source(&error).expect("storage source is retained");
        assert!(source.to_string().contains(marker));
    }

    #[test]
    fn root_remap_is_component_exact() {
        #[cfg(unix)]
        {
            let remap =
                RhythmboxRootRemap::new(PathBuf::from("/old/music"), PathBuf::from("/new/music"))
                    .unwrap();
            assert_eq!(
                remap.map(Path::new("/old/music/Artist/Song.flac")),
                PathBuf::from("/new/music/Artist/Song.flac")
            );
            assert_eq!(
                remap.map(Path::new("/old/music-copy/Song.flac")),
                PathBuf::from("/old/music-copy/Song.flac")
            );
        }
    }

    #[test]
    fn roots_must_be_absolute_normalized_unicode_paths() {
        assert_eq!(
            RhythmboxRootRemap::new(PathBuf::from("relative"), PathBuf::from("also-relative")),
            Err(RhythmboxPolicyError::RootNotAbsolute)
        );
        #[cfg(unix)]
        assert_eq!(
            RhythmboxRootRemap::new(PathBuf::from("/old/../music"), PathBuf::from("/new/music")),
            Err(RhythmboxPolicyError::RootNotNormalized)
        );
        #[cfg(unix)]
        for root in ["/old/./music", "/old/music/."] {
            assert_eq!(
                RhythmboxRootRemap::new(PathBuf::from(root), PathBuf::from("/new/music")),
                Err(RhythmboxPolicyError::RootNotNormalized),
                "raw dot component was accepted: {root}"
            );
        }
        #[cfg(windows)]
        for root in [
            r"C:\old\.\music",
            r"C:\old\music\.",
            "C:/old/./music",
            "C:/old/music/.",
            r"C:\old\..\music",
        ] {
            assert_eq!(
                RhythmboxRootRemap::new(PathBuf::from(root), PathBuf::from(r"C:\new\music")),
                Err(RhythmboxPolicyError::RootNotNormalized),
                "raw dot component was accepted: {root}"
            );
        }

        #[cfg(unix)]
        let oversized_root = PathBuf::from(format!(
            "/{}",
            "x".repeat(RHYTHMBOX_MIGRATION_PATH_BYTE_LIMIT)
        ));
        #[cfg(windows)]
        let oversized_root = PathBuf::from(format!(
            "C:\\{}",
            "x".repeat(RHYTHMBOX_MIGRATION_PATH_BYTE_LIMIT)
        ));
        assert_eq!(
            RhythmboxRootRemap::new(oversized_root, local_path("bounded-root")),
            Err(RhythmboxPolicyError::RootTooLong)
        );
    }

    #[test]
    fn mapped_paths_have_individual_and_cumulative_planner_budgets() {
        let policy = RhythmboxMigrationPolicy::default();
        let path = local_path("budget.flac");
        let path_bytes = path.to_str().expect("Unicode fixture").len();
        let mut retained = RHYTHMBOX_MIGRATION_PLANNER_PATH_BYTE_LIMIT - path_bytes;
        assert_eq!(
            retain_bounded_mapped_path(&policy, &path, &mut retained).unwrap(),
            path
        );
        assert_eq!(retained, RHYTHMBOX_MIGRATION_PLANNER_PATH_BYTE_LIMIT);
        assert!(matches!(
            retain_bounded_mapped_path(&policy, &path, &mut retained),
            Err(RhythmboxMigrationError::LimitExceeded)
        ));

        #[cfg(unix)]
        let oversized = PathBuf::from(format!(
            "/{}",
            "x".repeat(RHYTHMBOX_MIGRATION_PATH_BYTE_LIMIT)
        ));
        #[cfg(windows)]
        let oversized = PathBuf::from(format!(
            "C:\\{}",
            "x".repeat(RHYTHMBOX_MIGRATION_PATH_BYTE_LIMIT)
        ));
        let mut retained = 0;
        assert!(matches!(
            retain_bounded_mapped_path(&policy, &oversized, &mut retained),
            Err(RhythmboxMigrationError::LimitExceeded)
        ));
    }

    #[tokio::test]
    async fn report_retains_actionable_categories_but_redacts_debug_output() {
        let db = database().await;
        let matched = local_path("private-matched.flac");
        let unmatched = local_path("private-unmatched.flac");
        let duplicate = local_path("private-duplicate.flac");
        let matched_uri = file_uri(&matched);
        let unmatched_uri = file_uri(&unmatched);
        let duplicate_uri = file_uri(&duplicate);

        let seeded = seed_track(&db, "private-track-id", &matched).await;
        let mut rated: track::ActiveModel = seeded.into();
        rated.rating = Set(Some(70));
        rated.update(&db).await.unwrap();
        seed_playlist(&db, "private-playlist-id", "Private Existing").await;

        let rhythmdb = format!(
            "<rhythmdb version=\"2.0\">\
             <entry type=\"song\"><location>{matched_uri}</location><rating>4</rating></entry>\
             <entry type=\"song\"><location>{unmatched_uri}</location></entry>\
             <entry type=\"song\"><location>{duplicate_uri}</location></entry>\
             <entry type=\"song\"><location>{duplicate_uri}</location></entry>\
             <entry type=\"song\"/>\
             </rhythmdb>"
        );
        let playlists = format!(
            "<rhythmdb-playlists>\
             <playlist name=\"Private Existing\" type=\"static\"/>\
             <playlist name=\"Private Queue\" type=\"queue\"><location>{matched_uri}</location></playlist>\
             <playlist name=\"Private Legacy\" type=\"legacy\"/>\
             <playlist name=\"Private Automatic\" type=\"automatic\"/>\
             <playlist name=\"Private Static\" type=\"static\">\
             <location>https://private.invalid/secret.flac</location>\
             <location>{unmatched_uri}</location>\
             </playlist>\
             </rhythmdb-playlists>"
        );
        let import = parse_rhythmbox_documents(
            rhythmdb.as_bytes(),
            Some(playlists.as_bytes()),
            RhythmboxImportLimits::default(),
        )
        .expect("parse report fixture");

        let request =
            prepare_rhythmbox_migration(&db, import.clone(), RhythmboxMigrationPolicy::default())
                .await
                .expect("prepare report fixture");
        let report = request.report();

        assert!(request.requires_acknowledgement());
        assert!(report.parser_issues().details().iter().any(|detail| {
            detail.item_ordinal() == 5
                && detail.entry_ordinal().is_none()
                && detail.reason() == RhythmboxParserIssueReason::MissingLocation
        }));
        assert_eq!(report.unmatched_tracks().details().len(), 1);
        assert_eq!(report.unmatched_tracks().details()[0].source_ordinal(), 2);
        assert_eq!(report.unmatched_tracks().details()[0].path(), unmatched);
        assert_eq!(report.duplicate_locations().details().len(), 1);
        assert_eq!(report.duplicate_locations().details()[0].path(), duplicate);
        assert_eq!(report.duplicate_locations().details()[0].source_count(), 2);
        assert_eq!(report.rating_conflicts().details().len(), 1);
        assert_eq!(report.rating_conflicts().details()[0].path(), matched);
        assert_eq!(
            report.rating_conflicts().details()[0].resolution(),
            RhythmboxRatingConflictResolution::KeptTributary
        );
        assert_eq!(report.playlist_name_conflicts().details().len(), 1);
        assert_eq!(
            report.playlist_name_conflicts().details()[0].source_ordinal(),
            1
        );
        assert_eq!(
            report.playlist_name_conflicts().details()[0].name(),
            "Private Existing"
        );
        assert_eq!(report.queues().details()[0].entry_count(), 1);
        assert_eq!(report.queues().details()[0].name(), "Private Queue");
        assert_eq!(report.unsupported_playlists().details().len(), 2);
        assert_eq!(
            report.unsupported_playlists().details()[0].reason(),
            RhythmboxUnsupportedPlaylistReason::UnsupportedSourceType
        );
        assert_eq!(
            report.unsupported_playlists().details()[1].reason(),
            RhythmboxUnsupportedPlaylistReason::AutomaticQueryShape
        );
        assert_eq!(report.invalid_static_occurrences().details().len(), 1);
        assert_eq!(
            report.invalid_static_occurrences().details()[0].playlist_name(),
            "Private Static"
        );
        assert_eq!(
            report.invalid_static_occurrences().details()[0].entry_ordinal(),
            1
        );
        assert_eq!(
            report.unmatched_playlist_occurrences().details()[0].playlist_name(),
            "Private Static"
        );
        assert_eq!(
            report.unmatched_playlist_occurrences().details()[0].entry_ordinal(),
            2
        );
        assert_eq!(
            report.unmatched_playlist_occurrences().details()[0].path(),
            unmatched
        );

        let overwrite = RhythmboxMigrationPolicy {
            rating_conflicts: RhythmboxRatingConflictPolicy::UseRhythmbox,
            ..RhythmboxMigrationPolicy::default()
        };
        let replacement = prepare_rhythmbox_migration(&db, import, overwrite)
            .await
            .expect("prepare replacement policy");
        assert_eq!(replacement.summary().rating_conflicts_replaced, 1);
        assert_eq!(
            replacement.report().rating_conflicts().details()[0].resolution(),
            RhythmboxRatingConflictResolution::ReplacedWithRhythmbox
        );

        let debug = format!(
            "{request:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?}",
            report.unmatched_tracks().details()[0],
            report.duplicate_locations().details()[0],
            report.rating_conflicts().details()[0],
            report.playlist_name_conflicts().details()[0],
            report.queues().details()[0],
            report.unsupported_playlists().details()[0],
            report.invalid_static_occurrences().details()[0],
            report.unmatched_playlist_occurrences().details()[0],
        );
        for secret in [
            "private-matched",
            "private-unmatched",
            "private-duplicate",
            "Private Existing",
            "Private Queue",
            "Private Legacy",
            "Private Static",
            "private-track-id",
        ] {
            assert!(!debug.contains(secret), "Debug exposed {secret}");
        }
    }

    #[tokio::test]
    async fn report_caps_each_category_and_counts_every_omitted_detail() {
        let db = database().await;
        let entries = (0..(RHYTHMBOX_MIGRATION_REPORT_DETAIL_LIMIT + 7))
            .map(|_| "<entry type=\"song\"/>")
            .collect::<String>();
        let rhythmdb = format!("<rhythmdb version=\"2.0\">{entries}</rhythmdb>");
        let import =
            parse_rhythmbox_documents(rhythmdb.as_bytes(), None, RhythmboxImportLimits::default())
                .expect("parse bounded report fixture");
        let request = prepare_rhythmbox_migration(&db, import, RhythmboxMigrationPolicy::default())
            .await
            .expect("prepare bounded report fixture");

        assert_eq!(
            request.report().parser_issues().details().len(),
            RHYTHMBOX_MIGRATION_REPORT_DETAIL_LIMIT
        );
        assert_eq!(request.report().parser_issues().omitted(), 7);
        assert_eq!(
            request.report().parser_issues().details()[0].item_ordinal(),
            1
        );
        assert_eq!(
            request.report().parser_issues().details()[99].item_ordinal(),
            100
        );
        assert!(request.requires_acknowledgement());
    }

    #[tokio::test]
    async fn exact_clean_preview_does_not_require_acknowledgement() {
        let db = database().await;
        let matched = local_path("clean.flac");
        seed_track(&db, "clean-track", &matched).await;
        let uri = file_uri(&matched);
        let rhythmdb = format!(
            "<rhythmdb version=\"2.0\"><entry type=\"song\"><location>{uri}</location></entry></rhythmdb>"
        );
        let import =
            parse_rhythmbox_documents(rhythmdb.as_bytes(), None, RhythmboxImportLimits::default())
                .unwrap();
        let request = prepare_rhythmbox_migration(&db, import, RhythmboxMigrationPolicy::default())
            .await
            .unwrap();

        assert!(!request.requires_acknowledgement());
    }

    #[tokio::test]
    async fn apply_rejects_deletion_of_an_existing_name_conflict() {
        let db = database().await;
        seed_playlist(&db, "deleted-conflict", "Existing Conflict").await;
        let playlists = b"<rhythmdb-playlists><playlist name=\"Existing Conflict\" type=\"static\"/></rhythmdb-playlists>";
        let import = parse_rhythmbox_documents(
            b"<rhythmdb version=\"2.0\"></rhythmdb>",
            Some(playlists),
            RhythmboxImportLimits::default(),
        )
        .unwrap();
        let request = prepare_rhythmbox_migration(&db, import, RhythmboxMigrationPolicy::default())
            .await
            .unwrap();
        assert_eq!(request.summary().playlist_name_conflicts, 1);

        playlist::Entity::delete_by_id("deleted-conflict")
            .exec(&db)
            .await
            .unwrap();

        assert!(matches!(
            apply_rhythmbox_migration(&db, &request).await,
            Err(RhythmboxMigrationError::Stale)
        ));
        assert!(rhythmbox_import_receipt::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn apply_rejects_creation_of_a_name_skipped_as_a_source_duplicate() {
        let db = database().await;
        let playlists = b"<rhythmdb-playlists>\
            <playlist name=\"Appeared Later\" type=\"static\"/>\
            <playlist name=\"Appeared Later\" type=\"static\"/>\
            </rhythmdb-playlists>";
        let import = parse_rhythmbox_documents(
            b"<rhythmdb version=\"2.0\"></rhythmdb>",
            Some(playlists),
            RhythmboxImportLimits::default(),
        )
        .unwrap();
        let request = prepare_rhythmbox_migration(&db, import, RhythmboxMigrationPolicy::default())
            .await
            .unwrap();
        assert_eq!(request.summary().playlist_name_conflicts, 2);

        seed_playlist(&db, "created-after-preview", "Appeared Later").await;

        assert!(matches!(
            apply_rhythmbox_migration(&db, &request).await,
            Err(RhythmboxMigrationError::Stale)
        ));
        assert!(rhythmbox_import_receipt::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn exact_preview_and_apply_are_atomic_and_idempotent() {
        let db = database().await;
        let matched = local_path("matched.flac");
        let unmatched = local_path("unmatched.flac");
        seed_track(&db, "local-track", &matched).await;

        let import = parsed_static_import(&matched, &unmatched);
        let request =
            prepare_rhythmbox_migration(&db, import.clone(), RhythmboxMigrationPolicy::default())
                .await
                .expect("prepare exact migration");
        assert_eq!(
            request.summary(),
            &RhythmboxMigrationSummary {
                source_tracks: 2,
                matched_tracks: 1,
                unmatched_tracks: 1,
                ratings_to_update: 1,
                play_counts_to_update: 1,
                static_playlists_to_create: 1,
                playlist_entries_matched: 2,
                playlist_entries_unmatched: 1,
                ..RhythmboxMigrationSummary::default()
            }
        );

        assert_eq!(
            apply_rhythmbox_migration(&db, &request)
                .await
                .expect("apply exact migration"),
            RhythmboxMigrationOutcome::Applied
        );
        let updated = track::Entity::find_by_id("local-track")
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.play_count, 5);
        assert_eq!(updated.rating, Some(90));
        assert_eq!(updated.last_played_at_ms, None);

        let stored_entries = playlist_entry::Entity::find().all(&db).await.unwrap();
        assert_eq!(stored_entries.len(), 3);
        assert_eq!(stored_entries[0].track_id.as_deref(), Some("local-track"));
        assert_eq!(stored_entries[1].track_id.as_deref(), Some("local-track"));
        assert_eq!(stored_entries[2].track_id, None);
        assert!(stored_entries[2].match_title.is_empty());
        assert_eq!(
            stored_entries[2].match_file_path.as_deref(),
            unmatched.to_str()
        );
        assert_eq!(
            rhythmbox_import_receipt::Entity::find()
                .all(&db)
                .await
                .unwrap()
                .len(),
            1
        );

        assert_eq!(
            apply_rhythmbox_migration(&db, &request)
                .await
                .expect("repeat exact migration"),
            RhythmboxMigrationOutcome::AlreadyApplied
        );
        let repeated =
            prepare_rhythmbox_migration(&db, import, RhythmboxMigrationPolicy::default())
                .await
                .expect("prepare exact retry");
        assert!(repeated.summary().already_applied);
        assert_eq!(
            crate::db::entities::playlist::Entity::find()
                .all(&db)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn apply_rejects_a_changed_preview_target_without_writes() {
        let db = database().await;
        let matched = local_path("stale.flac");
        let unmatched = local_path("still-unmatched.flac");
        let seeded = seed_track(&db, "stale-track", &matched).await;
        let request = prepare_rhythmbox_migration(
            &db,
            parsed_static_import(&matched, &unmatched),
            RhythmboxMigrationPolicy::default(),
        )
        .await
        .unwrap();

        let mut changed: track::ActiveModel = seeded.into();
        changed.rating = Set(Some(77));
        changed.update(&db).await.unwrap();

        assert!(matches!(
            apply_rhythmbox_migration(&db, &request).await,
            Err(RhythmboxMigrationError::Stale)
        ));
        assert!(crate::db::entities::playlist::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .is_empty());
        assert!(rhythmbox_import_receipt::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn receipt_failure_rolls_back_metadata_and_playlists() {
        let db = database().await;
        let matched = local_path("rollback.flac");
        let unmatched = local_path("rollback-unmatched.flac");
        seed_track(&db, "rollback-track", &matched).await;
        let request = prepare_rhythmbox_migration(
            &db,
            parsed_static_import(&matched, &unmatched),
            RhythmboxMigrationPolicy::default(),
        )
        .await
        .unwrap();
        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            "CREATE TRIGGER reject_rhythmbox_receipt BEFORE INSERT ON rhythmbox_import_receipts BEGIN SELECT RAISE(ABORT, 'forced receipt failure'); END".to_string(),
        ))
        .await
        .unwrap();

        assert!(matches!(
            apply_rhythmbox_migration(&db, &request).await,
            Err(RhythmboxMigrationError::Storage(_))
        ));
        let unchanged = track::Entity::find_by_id("rollback-track")
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unchanged.play_count, 2);
        assert_eq!(unchanged.rating, None);
        assert!(crate::db::entities::playlist::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .is_empty());
        assert!(rhythmbox_import_receipt::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn post_rollback_storage_error_recognizes_an_exact_concurrent_receipt() {
        let db = database().await;
        let matched = local_path("concurrent-retry.flac");
        let unmatched = local_path("concurrent-retry-unmatched.flac");
        seed_track(&db, "concurrent-retry-track", &matched).await;
        let request = prepare_rhythmbox_migration(
            &db,
            parsed_static_import(&matched, &unmatched),
            RhythmboxMigrationPolicy::default(),
        )
        .await
        .unwrap();
        rhythmbox_import_receipt::ActiveModel {
            snapshot_digest: Set(request.prepared.snapshot_digest.to_vec()),
            importer_version: Set(RHYTHMBOX_IMPORTER_VERSION_V1),
            policy_digest: Set(request.prepared.policy_digest.to_vec()),
        }
        .insert(&db)
        .await
        .unwrap();

        assert_eq!(
            classify_post_rollback_error(
                &db,
                &request.prepared,
                RhythmboxMigrationError::Storage(sea_orm::DbErr::Custom(
                    "simulated losing receipt insert".to_string(),
                )),
            )
            .await
            .unwrap(),
            RhythmboxMigrationOutcome::AlreadyApplied
        );
    }
}
