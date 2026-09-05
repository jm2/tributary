//! Integrity: stale servers, lying digests, double-fetch disagreement, and
//! mid-transfer auth expiry — all terminal, none publish.

use super::*;

// -- stale servers ------------------------------------------------------

#[test]
fn a_stale_server_serving_changed_content_restarts_from_zero_and_commits() {
    let original = payload(10 * 1024, 1);
    let replacement = payload(10 * 1024, 2);
    // The server swaps its entity mid-transfer: every read from op 2 on
    // answers with the new content under a new ETag.
    let server = FakeServer::serving(original)
        .without_advertised_digest()
        .swaps_content_at(2, replacement.clone());
    let (mut engine, _dir) = engine(server, QUOTA);
    let src = source(3);
    declared(&mut engine, src);
    let media = key(src, "stale-track");
    assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
    assert_eq!(
        drive_until_terminal(&mut engine, &media),
        JobState::Committed
    );

    let server_state = engine.board();
    let row = server_state
        .rows
        .into_iter()
        .find(|row| row.media_key == media)
        .unwrap();
    assert_eq!(row.state, JobState::Committed);
    // The committed bytes are the CURRENT entity, not the stale prefix.
    let snapshot = committed_snapshot(&engine, &media);
    let on_disk = std::fs::read(&snapshot.cache_path).unwrap();
    assert_eq!(on_disk, replacement);
    // The journal restarted: the entity was re-read from zero after the
    // swap.
    let zero_reads = engine
        .backend
        .reads
        .iter()
        .filter(|(start, _)| *start == 0)
        .count();
    assert!(
        zero_reads >= 2,
        "expected the entity to be re-read from zero after the swap"
    );
}

#[test]
fn a_lying_advertised_digest_fails_integrity_and_never_publishes() {
    let server = FakeServer::serving(payload(4096, 5)).with_wrong_advertised_digest();
    let (mut engine, _dir) = engine(server, QUOTA);
    let src = source(4);
    declared(&mut engine, src);
    let media = key(src, "corrupt-track");
    assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
    assert_eq!(drive_until_terminal(&mut engine, &media), JobState::Failed);
    assert_eq!(
        engine.catalogue(&media),
        OfflineCatalogueEntry::LiveOnly,
        "a failed row never becomes playable"
    );
    let row = engine
        .board()
        .rows
        .into_iter()
        .find(|row| row.media_key == media)
        .unwrap();
    assert_eq!(row.failure, Some(OfflineError::IntegrityMismatch));
    // No half-promoted state: the track directory holds no file at all.
    let track_dir = engine.store.track_dir(&media);
    let leftovers: Vec<_> = std::fs::read_dir(track_dir).unwrap().collect();
    assert!(leftovers.is_empty());
}

#[test]
fn a_double_fetch_disagreement_is_terminal_integrity_mismatch() {
    let server = FakeServer::serving(payload(4096, 6))
        .without_advertised_digest()
        .with_lying_double_fetch();
    let (mut engine, _dir) = engine(server, QUOTA);
    let src = source(5);
    declared(&mut engine, src);
    let media = key(src, "double-fetch-track");
    assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
    assert_eq!(drive_until_terminal(&mut engine, &media), JobState::Failed);
    assert_eq!(engine.catalogue(&media), OfflineCatalogueEntry::LiveOnly);
}

#[test]
fn an_incomplete_double_fetch_is_integrity_unverifiable() {
    let server = FakeServer::serving(payload(4096, 6))
        .without_advertised_digest()
        .with_failing_double_fetch();
    let (mut engine, _dir) = engine(server, QUOTA);
    let src = source(6);
    declared(&mut engine, src);
    let media = key(src, "unverifiable-track");
    assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
    assert_eq!(drive_until_terminal(&mut engine, &media), JobState::Failed);
    let row = engine
        .board()
        .rows
        .into_iter()
        .find(|row| row.media_key == media)
        .unwrap();
    assert_eq!(row.failure, Some(OfflineError::IntegrityUnverifiable));
}

#[test]
fn auth_expiry_mid_download_is_terminal_and_leaves_no_row() {
    let server = FakeServer::serving(payload(10 * 1024, 10)).auth_expires_at(2);
    let (mut engine, _dir) = engine(server, QUOTA);
    let src = source(9);
    declared(&mut engine, src);
    let media = key(src, "auth-track");
    assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
    assert_eq!(drive_until_terminal(&mut engine, &media), JobState::Failed);
    let row = engine
        .board()
        .rows
        .into_iter()
        .find(|row| row.media_key == media)
        .unwrap();
    assert_eq!(row.failure, Some(OfflineError::AuthExpired));
    assert_eq!(engine.catalogue(&media), OfflineCatalogueEntry::LiveOnly);
    let track_dir = engine.store.track_dir(&media);
    let leftovers: Vec<_> = std::fs::read_dir(track_dir).unwrap().collect();
    assert!(leftovers.is_empty());
}

// -- refresh settlement ----------------------------------------------------

#[test]
fn a_refresh_over_a_cached_row_replaces_bytes_without_deleting_the_new_publish() {
    let first = payload(2048, 1);
    let second = payload(4096, 2);
    let (mut engine, _dir) = engine(FakeServer::serving(first), QUOTA);
    let src = source(20);
    declared(&mut engine, src);
    let media = key(src, "refreshed");
    assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
    assert_eq!(
        drive_until_terminal(&mut engine, &media),
        JobState::Committed
    );
    let superseded = committed_snapshot(&engine, &media);

    // The same track is offered again with new content: the refresh
    // commits a sibling whose key-derived path is identical, and the
    // fresh bytes must survive the predecessor settlement.
    engine.backend = FakeServer::serving(second.clone());
    assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
    assert_eq!(
        drive_until_terminal(&mut engine, &media),
        JobState::Committed
    );

    let fresh = committed_snapshot(&engine, &media);
    assert_eq!(
        fresh.cache_path, superseded.cache_path,
        "cache paths are key-derived, so a refresh shares the path"
    );
    let on_disk = std::fs::read(&fresh.cache_path).unwrap();
    assert_eq!(
        on_disk, second,
        "the fresh publish must survive the predecessor settlement"
    );
    assert_eq!(engine.board().committed_bytes, 4096);
}

#[test]
fn an_entity_restart_leaves_no_temp_files_behind() {
    let original = payload(10 * 1024, 7);
    let replacement = payload(10 * 1024, 8);
    let server = FakeServer::serving(original)
        .without_advertised_digest()
        .swaps_content_at(2, replacement.clone());
    let (mut engine, _dir) = engine(server, QUOTA);
    let src = source(23);
    declared(&mut engine, src);
    let media = key(src, "restart-clean");
    assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
    assert_eq!(
        drive_until_terminal(&mut engine, &media),
        JobState::Committed
    );
    // The entity swap re-enters the connect phase, which mints a fresh
    // temp reservation: every superseded temp must have been dropped,
    // leaving exactly the committed file in the track directory.
    let track_dir = engine.store.track_dir(&media);
    let leftovers: Vec<_> = std::fs::read_dir(track_dir).unwrap().collect();
    assert_eq!(
        leftovers.len(),
        1,
        "only the committed file remains after a restart-driven commit"
    );
}
