use super::*;
use crate::architecture::{SourceId, TrackId};
use uuid::Uuid;

fn fixture_media_key() -> MediaKey {
    MediaKey::new(SourceId::local(), TrackId::new("track-1").unwrap())
}

fn fixture_incarnation() -> SourceIncarnationId {
    SourceIncarnationId::from_uuid(Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0))
}

fn fixture_snapshot(cache_path: String) -> Result<CommittedSnapshot, OfflineError> {
    CommittedSnapshot::new(
        SnapshotIdentity::new(fixture_media_key(), fixture_incarnation(), 1),
        4096,
        [0xab; 32],
        DigestProvenance::DoubleFetch,
        cache_path,
        OperationalLicence::SourceDeclared(LicenceLabel::new("subsonic-streaming-self").unwrap()),
        1_700_000_000,
    )
}

#[test]
fn operational_licence_defaults_to_denied() {
    assert_eq!(OperationalLicence::default(), OperationalLicence::Denied);
}

#[test]
fn job_state_is_terminal_only_for_committed_failed_cancelled() {
    for state in [
        JobState::Queued,
        JobState::Connecting,
        JobState::Receiving,
        JobState::Verifying,
        JobState::Committing,
    ] {
        assert!(!state.is_terminal(), "{state:?} must not be terminal");
    }
    for state in [JobState::Committed, JobState::Failed, JobState::Cancelled] {
        assert!(state.is_terminal(), "{state:?} must be terminal");
    }
}

#[test]
fn offline_error_display_is_redacted() {
    // No variant name should leak a URL, status, header, body, or
    // credential — every display impl is the variant label only.
    let labels = [
        (OfflineError::Network, "network"),
        (OfflineError::AuthExpired, "auth-expired"),
        (OfflineError::LeaseRevoked, "lease-revoked"),
        (OfflineError::Denied, "denied"),
        (OfflineError::IntegrityMismatch, "integrity-mismatch"),
        (
            OfflineError::IntegrityUnverifiable,
            "integrity-unverifiable",
        ),
        (OfflineError::LicenceDenied, "licence-denied"),
        (OfflineError::QuotaExceeded, "quota-exceeded"),
        (
            OfflineError::PublishRetriesExhausted,
            "publish-retries-exhausted",
        ),
        (OfflineError::StorageUnavailable, "storage-unavailable"),
        (OfflineError::UnsupportedSource, "unsupported-source"),
    ];
    for (variant, label) in labels {
        let rendered = variant.to_string();
        assert_eq!(rendered, label, "{variant:?} label drifted");
        for forbidden in ["http://", "https://", "token=", "password=", "Bearer "] {
            assert!(
                !rendered.contains(forbidden),
                "{variant:?} leaked {forbidden:?} in display"
            );
        }
    }
}

#[test]
fn offline_error_serde_round_trips_kebab_labels() {
    for (variant, label) in [
        (OfflineError::Network, "\"network\""),
        (OfflineError::AuthExpired, "\"auth-expired\""),
        (OfflineError::LeaseRevoked, "\"lease-revoked\""),
        (OfflineError::Denied, "\"denied\""),
        (OfflineError::IntegrityMismatch, "\"integrity-mismatch\""),
        (
            OfflineError::IntegrityUnverifiable,
            "\"integrity-unverifiable\"",
        ),
        (OfflineError::LicenceDenied, "\"licence-denied\""),
        (OfflineError::QuotaExceeded, "\"quota-exceeded\""),
        (
            OfflineError::PublishRetriesExhausted,
            "\"publish-retries-exhausted\"",
        ),
        (OfflineError::StorageUnavailable, "\"storage-unavailable\""),
        (OfflineError::UnsupportedSource, "\"unsupported-source\""),
    ] {
        let json = serde_json::to_string(&variant).expect("serialize");
        assert_eq!(json, label);
        let back: OfflineError = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, variant);
    }
}

#[test]
fn declared_total_above_payload_cap_fails_quota_exceeded() {
    // Real audio payloads are megabytes; they must never be rejected
    // for being themselves.
    let cap: u64 = 256 * 1024 * 1024;
    assert_eq!(check_declared_total(5 * 1024 * 1024, cap), Ok(()));
    assert_eq!(check_declared_total(200 * 1024 * 1024, cap), Ok(()));
    // Equality passes: an exact-cap payload is not a quota failure.
    assert_eq!(check_declared_total(cap, cap), Ok(()));
    // Strictly above the cap fails QuotaExceeded before any network
    // work, including the u64::MAX pathological header.
    assert_eq!(
        check_declared_total(cap + 1, cap),
        Err(OfflineError::QuotaExceeded)
    );
    assert_eq!(
        check_declared_total(u64::MAX, cap),
        Err(OfflineError::QuotaExceeded)
    );
}

#[test]
fn job_identity_is_media_key_and_source_incarnation() {
    let key = fixture_media_key();
    let first = JobRecord::new(key.clone(), fixture_incarnation(), 1);
    let second = JobRecord::new(
        key.clone(),
        SourceIncarnationId::from_uuid(Uuid::from_u128(0x9876_5432_10fe_dcba_9876_5432_10fe_dcba)),
        1,
    );
    // Same media_key and capability_epoch, different durable
    // incarnation: the identity differs, exactly as the contract's
    // identity rule requires.
    assert_ne!(first.source_incarnation, second.source_incarnation);
    assert_eq!(first.capability_epoch, second.capability_epoch);
    assert_eq!(first.media_key, second.media_key);
    assert_ne!(first, second);
    // Defaults from the job-model table.
    assert_eq!(first.journal_bytes, 0);
    assert!(first.requested_bytes.is_none());
    assert!(first.resume_validator.is_none());
    assert!(first.current_sha256.is_none());
    assert!(first.last_lease.is_none());
    assert!(first.failure.is_none());
    assert_eq!(first.state, JobState::Queued);
}

#[test]
fn media_key_round_trip_via_source_and_track_ids() {
    let key = MediaKey::new(SourceId::local(), TrackId::new("track-1").unwrap());
    let record = JobRecord::new(key.clone(), fixture_incarnation(), 1);
    assert_eq!(record.media_key, key);
    assert_eq!(record.state, JobState::Queued);
    assert_eq!(record.capability_epoch, 1);
    assert_eq!(record.source_incarnation, fixture_incarnation());
    assert!(record.failure.is_none());
    assert_eq!(record.current_bytes, 0);
    assert_eq!(record.journal_bytes, 0);
    assert!(record.requested_bytes.is_none());
    assert!(record.resume_validator.is_none());
    assert!(record.current_sha256.is_none());
    assert!(record.last_lease.is_none());
}

#[test]
fn source_incarnation_id_is_opaque_serialisable_and_checked() {
    let id = fixture_incarnation();
    assert_eq!(id.as_uuid(), id.to_string().parse::<Uuid>().unwrap());
    let parsed: SourceIncarnationId = id.to_string().parse().expect("FromStr");
    assert_eq!(parsed, id);
    assert_ne!(SourceIncarnationId::random(), SourceIncarnationId::random());
    let json = serde_json::to_string(&id).expect("serialize");
    let back: SourceIncarnationId = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, id);
    assert!("not-a-uuid".parse::<SourceIncarnationId>().is_err());
}

#[test]
fn entity_validator_bounds_redaction_and_serde() {
    // Bound, bound + 1, empty.
    let at_bound =
        EntityValidator::etag("x".repeat(MAX_OFFLINE_METADATA_BYTES)).expect("validator at bound");
    assert!(
        EntityValidator::etag("x".repeat(MAX_OFFLINE_METADATA_BYTES + 1)).is_none(),
        "over-bound validator must be discarded"
    );
    assert!(
        EntityValidator::etag("").is_none(),
        "empty must be discarded"
    );
    assert!(EntityValidator::last_modified("y".repeat(MAX_OFFLINE_METADATA_BYTES + 1)).is_none());

    assert!(at_bound.is_strong());
    let last_modified =
        EntityValidator::last_modified("Wed, 21 Oct 2015 07:28:00 GMT").expect("validator");
    assert!(!last_modified.is_strong());
    assert_eq!(last_modified.kind(), "last-modified");
    assert_eq!(last_modified.value(), "Wed, 21 Oct 2015 07:28:00 GMT");

    // Redacted debug: kind + byte length, never the payload.
    let debug = format!("{at_bound:?}");
    assert!(debug.contains("etag"), "{debug}");
    assert!(debug.contains("byte_len"), "{debug}");
    assert!(!debug.contains("xxxx"), "{debug}");

    // Display label == serde variant label.
    assert_eq!(at_bound.to_string(), "etag");
    assert_eq!(last_modified.to_string(), "last-modified");
    let small = EntityValidator::etag("abc").expect("validator");
    assert_eq!(
        serde_json::to_string(&small).expect("serialize"),
        "{\"etag\":\"abc\"}"
    );
    let back: EntityValidator = serde_json::from_str("{\"etag\":\"abc\"}").expect("deserialize");
    assert_eq!(back, small);
    let back_lm: EntityValidator =
        serde_json::from_str("{\"last-modified\":\"Tue, 01 Sep 2026 00:00:00 GMT\"}")
            .expect("deserialize");
    assert_eq!(back_lm.kind(), "last-modified");
    // Serde rejection goes through the checked constructor.
    assert!(serde_json::from_str::<EntityValidator>("{\"etag\":\"\"}").is_err());
    let oversized = serde_json::json!({ "etag": "x".repeat(MAX_OFFLINE_METADATA_BYTES + 1) });
    assert!(serde_json::from_value::<EntityValidator>(oversized).is_err());
}

#[test]
fn validator_payload_bounds_redaction_and_serde() {
    // The payload newtype enforces the same bound on its own: bound,
    // bound + 1, empty, serde round-trip and serde reject, redacted
    // debug.
    let payload = ValidatorPayload::new("abc").expect("payload");
    assert_eq!(payload.as_str(), "abc");
    assert!(ValidatorPayload::new("x".repeat(MAX_OFFLINE_METADATA_BYTES + 1)).is_none());
    assert!(ValidatorPayload::new("").is_none());
    let payload_json = serde_json::to_string(&payload).expect("serialize payload");
    assert_eq!(payload_json, "\"abc\"");
    let payload_back: ValidatorPayload =
        serde_json::from_str(&payload_json).expect("deserialize payload");
    assert_eq!(payload_back, payload);
    assert!(serde_json::from_str::<ValidatorPayload>("\"\"").is_err());
    let payload_debug = format!("{payload:?}");
    assert!(payload_debug.contains("byte_len"), "{payload_debug}");
    assert!(!payload_debug.contains("abc"), "{payload_debug}");
}

#[test]
fn licence_label_bounds_redaction_and_serde() {
    let at_bound =
        LicenceLabel::new("x".repeat(MAX_OFFLINE_METADATA_BYTES)).expect("label at bound");
    assert_eq!(at_bound.as_str().len(), MAX_OFFLINE_METADATA_BYTES);
    assert!(LicenceLabel::new("x".repeat(MAX_OFFLINE_METADATA_BYTES + 1)).is_none());
    assert!(LicenceLabel::new("").is_none());

    let label = LicenceLabel::new("subsonic-streaming-self").expect("label");
    assert_eq!(label.as_str(), "subsonic-streaming-self");
    assert_eq!(label.to_string(), "subsonic-streaming-self");
    let debug = format!("{label:?}");
    assert!(debug.contains("byte_len"), "{debug}");
    assert!(!debug.contains("subsonic"), "{debug}");

    let json = serde_json::to_string(&label).expect("serialize");
    assert_eq!(json, "\"subsonic-streaming-self\"");
    let back: LicenceLabel = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, label);
    assert!(serde_json::from_str::<LicenceLabel>("\"\"").is_err());
    let oversized = serde_json::Value::String("x".repeat(MAX_OFFLINE_METADATA_BYTES + 1));
    assert!(serde_json::from_value::<LicenceLabel>(oversized).is_err());
}

#[test]
fn operational_licence_serde_and_display() {
    let declared =
        OperationalLicence::SourceDeclared(LicenceLabel::new("jellyfin-streaming-self").unwrap());
    assert_eq!(
        serde_json::to_string(&OperationalLicence::Denied).unwrap(),
        "\"denied\""
    );
    assert_eq!(
        serde_json::to_string(&declared).unwrap(),
        "{\"source-declared\":\"jellyfin-streaming-self\"}"
    );
    assert_eq!(
        serde_json::to_string(&OperationalLicence::Revoked).unwrap(),
        "\"revoked\""
    );
    let back: OperationalLicence =
        serde_json::from_str("{\"source-declared\":\"plex-streaming-self\"}").expect("deserialize");
    assert_eq!(
        back,
        OperationalLicence::SourceDeclared(LicenceLabel::new("plex-streaming-self").unwrap())
    );
    // Display renders the persisted label for SourceDeclared.
    assert_eq!(back.to_string(), "plex-streaming-self");
    assert_eq!(OperationalLicence::Denied.to_string(), "denied");
    assert_eq!(OperationalLicence::Revoked.to_string(), "revoked");
}

#[test]
fn job_state_serde_matches_display() {
    for (state, label) in [
        (JobState::Queued, "queued"),
        (JobState::Connecting, "connecting"),
        (JobState::Receiving, "receiving"),
        (JobState::Verifying, "verifying"),
        (JobState::Committing, "committing"),
        (JobState::Committed, "committed"),
        (JobState::Failed, "failed"),
        (JobState::Cancelled, "cancelled"),
    ] {
        assert_eq!(state.to_string(), label);
        let json = serde_json::to_string(&state).expect("serialize");
        assert_eq!(json, format!("\"{label}\""));
        let back: JobState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, state);
    }
}

#[test]
fn digest_provenance_serde_matches_display() {
    for (provenance, label) in [
        (DigestProvenance::Advertised, "advertised"),
        (DigestProvenance::DoubleFetch, "double-fetch"),
    ] {
        assert_eq!(provenance.to_string(), label);
        let json = serde_json::to_string(&provenance).expect("serialize");
        assert_eq!(json, format!("\"{label}\""));
        let back: DigestProvenance = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, provenance);
    }
}

#[test]
fn committed_snapshot_checked_constructor_enforces_path_bound() {
    let max_path = "c/".to_string() + &"a".repeat(MAX_OFFLINE_SNAPSHOT_PATH_BYTES - 2);
    assert_eq!(max_path.len(), MAX_OFFLINE_SNAPSHOT_PATH_BYTES);
    let at_bound = fixture_snapshot(max_path).expect("snapshot at path bound");
    // The identity triple is carried and exposed as one value; the
    // per-field accessors agree with it.
    let expected_identity = SnapshotIdentity::new(fixture_media_key(), fixture_incarnation(), 1);
    assert_eq!(at_bound.identity(), &expected_identity);
    assert_eq!(at_bound.media_key(), &fixture_media_key());
    assert_eq!(at_bound.source_incarnation(), fixture_incarnation());
    assert_eq!(at_bound.capability_epoch(), 1);
    assert_eq!(at_bound.byte_size(), 4096);
    assert_eq!(at_bound.sha256(), &[0xab; 32]);
    assert_eq!(at_bound.sha256_hex(), "ab".repeat(32));
    assert_eq!(at_bound.digest_provenance(), DigestProvenance::DoubleFetch);
    assert_eq!(at_bound.committed_at_epoch_secs(), 1_700_000_000);
    match at_bound.licence() {
        OperationalLicence::SourceDeclared(label) => {
            assert_eq!(label.as_str(), "subsonic-streaming-self");
        }
        other => panic!("unexpected licence {other:?}"),
    }

    let over_bound = "c/".to_string() + &"a".repeat(MAX_OFFLINE_SNAPSHOT_PATH_BYTES - 1);
    assert!(over_bound.len() > MAX_OFFLINE_SNAPSHOT_PATH_BYTES);
    assert_eq!(
        fixture_snapshot(over_bound),
        Err(OfflineError::StorageUnavailable)
    );
    // Direct validator contract (used by the constructor).
    assert_eq!(
        validate_snapshot_path_bytes(MAX_OFFLINE_SNAPSHOT_PATH_BYTES),
        Ok(())
    );
    assert_eq!(
        validate_snapshot_path_bytes(MAX_OFFLINE_SNAPSHOT_PATH_BYTES + 1),
        Err(OfflineError::StorageUnavailable)
    );
}

#[test]
fn offline_catalogue_entries_construct_all_variants() {
    let live = OfflineCatalogueEntry::LiveOnly;
    let snapshot = fixture_snapshot("snapshots/ab".to_string()).expect("snapshot");
    let cached = OfflineCatalogueEntry::Cached(snapshot.clone());
    let revoked = OfflineCatalogueEntry::Revoked(snapshot);
    assert_eq!(live, OfflineCatalogueEntry::LiveOnly);
    match cached {
        OfflineCatalogueEntry::Cached(inner) => {
            assert_eq!(inner.sha256(), &[0xab; 32]);
        }
        other => panic!("unexpected entry {other:?}"),
    }
    match revoked {
        OfflineCatalogueEntry::Revoked(inner) => {
            assert_eq!(inner.cache_path(), "snapshots/ab");
        }
        other => panic!("unexpected entry {other:?}"),
    }
    // The capability holder keeps its constructor exercised too.
    let capability = OfflineSnapshot::new(64 * 1024 * 1024);
    assert_eq!(capability.source_byte_cap, 64 * 1024 * 1024);
}

#[test]
fn lease_id_is_opaque_and_compares_by_raw() {
    let a = LeaseId::from_raw(7);
    let b = LeaseId::from_raw(7);
    let c = LeaseId::from_raw(8);
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert_eq!(a.raw(), 7);
}
