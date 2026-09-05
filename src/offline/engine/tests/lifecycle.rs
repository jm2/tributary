//! Lifecycle transitions: online-to-offline, revocation, logout, cache
//! deletion, source replacement, and the redaction boundary.

use super::*;

// -- the online-to-offline transition ----------------------------------

#[test]
fn committed_rows_play_offline_while_live_rows_do_not() {
    let expected_bytes = payload(10 * 1024, 7);
    let (mut engine, _cache_root) = engine(FakeServer::serving(expected_bytes.clone()), QUOTA);
    let src = source(1);
    declared(&mut engine, src);
    let offline = key(src, "offline-track");
    let live = key(src, "live-track");
    engine.advance_clock(100);
    assert_eq!(
        engine.admit(offline.clone(), labels("offline"), None),
        Ok(())
    );
    assert_eq!(
        drive_until_terminal(&mut engine, &offline),
        JobState::Committed
    );
    // Offline rendering: the committed row is playable with no live
    // authority, and its bytes on disk hash to the committed digest.
    let snapshot = committed_snapshot(&engine, &offline);
    assert_eq!(snapshot.byte_size, expected_bytes.len() as u64);
    assert_eq!(snapshot.sha256_hex, hex_of(&sha(&expected_bytes)));
    let on_disk = std::fs::read(&snapshot.cache_path).unwrap();
    assert_eq!(on_disk, expected_bytes);

    // The source goes away (disconnect/logout of the live session).
    engine.on_source_disconnected(&src);
    assert!(matches!(
        engine.catalogue(&offline),
        OfflineCatalogueEntry::Cached(_)
    ));

    // A track that never committed is live-only; a download attempt
    // while the network is unreachable exhausts the resume budget and
    // fails with the redacted transient-network cause.
    engine.backend = FakeServer::serving(payload(1024, 9)).goes_offline_after_open();
    engine.advance_clock(200);
    assert_eq!(engine.admit(live.clone(), labels("live"), None), Ok(()));
    assert_eq!(drive_until_terminal(&mut engine, &live), JobState::Failed);
    let board = engine.board();
    let offline_row = board
        .rows
        .iter()
        .find(|row| row.media_key == offline)
        .unwrap();
    assert_eq!(offline_row.state, JobState::Committed);
    assert!(offline_row.cached.as_ref().unwrap().playable);
    let live_row = board.rows.iter().find(|row| row.media_key == live).unwrap();
    assert_eq!(live_row.state, JobState::Failed);
    assert_eq!(live_row.failure, Some(OfflineError::Network));
}

#[test]
fn licence_revocation_retires_rows_but_preserves_files() {
    let (mut engine, _dir) = engine(FakeServer::serving(payload(2048, 3)), QUOTA);
    let src = source(2);
    declared(&mut engine, src);
    let media = key(src, "licensed-track");
    assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
    assert_eq!(
        drive_until_terminal(&mut engine, &media),
        JobState::Committed
    );
    let path = committed_snapshot(&engine, &media).cache_path.clone();

    engine.reconcile_licence_revoked(&src);
    match engine.catalogue(&media) {
        OfflineCatalogueEntry::Revoked(snapshot) => {
            assert_eq!(snapshot.byte_size, 2048);
        }
        other => panic!("expected revoked, got {other:?}"),
    }
    // The file is the user's: retirement preserves it on disk.
    assert!(std::path::Path::new(&path).exists());
    // Board marks it not playable.
    let row = engine
        .board()
        .rows
        .into_iter()
        .find(|row| row.media_key == media)
        .unwrap();
    assert!(!row.cached.unwrap().playable);
}

// -- logout ----------------------------------------------------------------

#[test]
fn logout_cancels_in_flight_jobs_but_committed_rows_survive() {
    let (mut engine, _dir) = engine(FakeServer::serving(payload(10 * 1024, 11)), QUOTA);
    let src = source(13);
    declared(&mut engine, src);
    let committed = key(src, "survivor");
    let in_flight = key(src, "victim");
    assert_eq!(
        engine.admit(committed.clone(), labels("survivor"), None),
        Ok(())
    );
    assert_eq!(
        drive_until_terminal(&mut engine, &committed),
        JobState::Committed
    );

    engine.backend = FakeServer::serving(payload(10 * 1024, 12)).failing_read_at(2);
    assert_eq!(
        engine.admit(in_flight.clone(), labels("victim"), None),
        Ok(())
    );
    assert_eq!(engine.drive(&in_flight), Some(JobState::Receiving));

    // Logout: the in-flight lease revokes; the committed row is not
    // touched (DAAP rule).
    engine.on_source_disconnected(&src);
    assert_eq!(
        engine.catalogue(&in_flight),
        OfflineCatalogueEntry::LiveOnly
    );
    assert!(matches!(
        engine.catalogue(&committed),
        OfflineCatalogueEntry::Cached(_)
    ));
    let row = engine
        .board()
        .rows
        .into_iter()
        .find(|row| row.media_key == in_flight)
        .unwrap();
    assert_eq!(row.state, JobState::Cancelled);
    assert!(row.cached.is_none());
    // The cancelled temp left nothing behind.
    let track_dir = engine.store.track_dir(&in_flight);
    let leftovers: Vec<_> = std::fs::read_dir(track_dir).unwrap().collect();
    assert!(leftovers.is_empty());
}

// -- cache deletion ----------------------------------------------------------

#[test]
fn user_cache_deletion_unlinks_bytes_releases_quota_and_clears_the_row() {
    let (mut engine, _dir) = engine(FakeServer::serving(payload(3 * 1024, 13)), QUOTA);
    let src = source(14);
    declared(&mut engine, src);
    let media = key(src, "deletable");
    assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
    assert_eq!(
        drive_until_terminal(&mut engine, &media),
        JobState::Committed
    );
    let path = committed_snapshot(&engine, &media).cache_path.clone();
    assert_eq!(engine.board().committed_bytes, 3 * 1024);

    assert!(engine.delete_cached(&media).unwrap());
    assert!(
        !std::path::Path::new(&path).exists(),
        "deletion unlinks the file"
    );
    assert_eq!(engine.catalogue(&media), OfflineCatalogueEntry::LiveOnly);
    assert_eq!(engine.board().committed_bytes, 0);
    assert!(
        !engine.delete_cached(&media).unwrap(),
        "second delete reports nothing to do"
    );
}

#[test]
fn user_cache_deletion_also_clears_a_revoked_row() {
    let (mut engine, _dir) = engine(FakeServer::serving(payload(2048, 18)), QUOTA);
    let src = source(25);
    declared(&mut engine, src);
    let media = key(src, "revoked-deletable");
    assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
    assert_eq!(
        drive_until_terminal(&mut engine, &media),
        JobState::Committed
    );
    engine.reconcile_licence_revoked(&src);
    let path = match engine.catalogue(&media) {
        OfflineCatalogueEntry::Revoked(snapshot) => snapshot.cache_path.clone(),
        other => panic!("expected revoked, got {other:?}"),
    };
    // A revoked row stays charged while its file is preserved.
    assert_eq!(engine.board().committed_bytes, 2048);
    assert!(engine.delete_cached(&media).unwrap());
    assert!(!std::path::Path::new(&path).exists());
    assert_eq!(engine.catalogue(&media), OfflineCatalogueEntry::LiveOnly);
    assert_eq!(engine.board().committed_bytes, 0);
}

#[test]
fn a_refresh_after_revocation_settles_the_revoked_charge() {
    let (mut engine, _dir) = engine(FakeServer::serving(payload(2048, 19)), QUOTA);
    let src = source(26);
    declared(&mut engine, src);
    let media = key(src, "revoked-then-refreshed");
    assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
    assert_eq!(
        drive_until_terminal(&mut engine, &media),
        JobState::Committed
    );
    engine.reconcile_licence_revoked(&src);
    assert!(matches!(
        engine.catalogue(&media),
        OfflineCatalogueEntry::Revoked(_)
    ));

    // The licence returns and the user re-downloads the same track: the
    // revoked predecessor's charge must be released at publish, never
    // orphaned with no row left to evict.
    engine.set_source_position(
        src,
        SourceOfflinePosition::Declared(OperationalLicence::SourceDeclared),
    );
    engine.backend = FakeServer::serving(payload(2048, 20));
    assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
    assert_eq!(
        drive_until_terminal(&mut engine, &media),
        JobState::Committed
    );
    assert_eq!(
        engine.board().committed_bytes,
        2048,
        "the ledger holds exactly the fresh snapshot's charge"
    );
}

// -- source replacement ---------------------------------------------------------

#[test]
fn bumping_the_epoch_cancels_stale_jobs_but_keeps_committed_rows() {
    let (mut engine, _dir) = engine(FakeServer::serving(payload(4096, 14)), QUOTA);
    let src = source(15);
    declared(&mut engine, src);
    let settled = key(src, "settled");
    assert_eq!(engine.admit(settled.clone(), labels("t"), None), Ok(()));
    assert_eq!(
        drive_until_terminal(&mut engine, &settled),
        JobState::Committed
    );

    // 10 KiB spans several segments so the injected transient failure
    // pauses the job mid-transfer instead of completing it in one pass.
    engine.backend = FakeServer::serving(payload(10 * 1024, 15)).failing_read_at(2);
    let stale = key(src, "stale");
    assert_eq!(engine.admit(stale.clone(), labels("t"), None), Ok(()));
    assert_eq!(engine.drive(&stale), Some(JobState::Receiving));

    // Source replacement bumps the generation: the stale job cancels,
    // the committed row survives.
    engine.bump_epoch(&src);
    assert_eq!(engine.epoch_of(&src), 1);
    let row = engine
        .board()
        .rows
        .into_iter()
        .find(|row| row.media_key == stale)
        .unwrap();
    assert_eq!(row.state, JobState::Cancelled);
    assert!(matches!(
        engine.catalogue(&settled),
        OfflineCatalogueEntry::Cached(_)
    ));
}

// -- redaction ---------------------------------------------------------------

#[test]
fn board_projection_never_exposes_paths_urls_or_credentials() {
    let (mut engine, _dir) = engine(FakeServer::serving(payload(4096, 16)), QUOTA);
    let src = source(16);
    declared(&mut engine, src);
    let good = key(src, "clean");
    assert_eq!(engine.admit(good.clone(), labels("clean"), None), Ok(()));
    assert_eq!(
        drive_until_terminal(&mut engine, &good),
        JobState::Committed
    );

    engine.backend = FakeServer::serving(payload(4096, 17)).with_wrong_advertised_digest();
    let bad = key(src, "corrupt");
    assert_eq!(engine.admit(bad.clone(), labels("corrupt"), None), Ok(()));
    assert_eq!(drive_until_terminal(&mut engine, &bad), JobState::Failed);

    let board = engine.board();
    let rendered: Vec<String> = board
        .rows
        .iter()
        .map(|row| format!("{:?}{:?}{:?}", row.labels, row.state, row.failure))
        .collect();
    let joined = rendered.join("\n");
    for forbidden in ["http://", "https://", "token=", "Bearer ", "password="] {
        assert!(
            !joined.contains(forbidden),
            "projection leaked {forbidden:?}"
        );
    }
    // No on-disk path reaches the projection.
    let committed_path = committed_snapshot(&engine, &good).cache_path;
    assert!(!joined.contains(&committed_path));
    // Committed rows expose exactly the licence label, never text.
    let clean_row = board.rows.iter().find(|row| row.media_key == good).unwrap();
    assert_eq!(
        clean_row.cached.as_ref().unwrap().licence_label,
        OperationalLicence::SourceDeclared
    );
}
