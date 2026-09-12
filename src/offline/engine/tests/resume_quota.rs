//! Resume discipline (journaled-offset resume, torn-tail healing,
//! validator-less restart) and quota pressure at the commit point.

use super::*;

// -- partial files -------------------------------------------------------

#[test]
fn a_paused_job_resumes_from_the_journaled_offset_and_heals_torn_tails() {
    let payload = payload(10 * 1024, 8); // 3 segments at 4 KiB
    let server = FakeServer::serving(payload.clone()).failing_read_at(2);
    let (mut engine, _dir) = engine(server, QUOTA);
    let src = source(7);
    declared(&mut engine, src);
    let media = key(src, "resume-track");
    assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
    // First pass: segment 1 lands, the second read hits the transient
    // failure and the pass pauses with 4096 journaled bytes.
    assert_eq!(engine.drive(&media), Some(JobState::Receiving));
    assert_eq!(engine.jobs[&media].record.current_bytes, 4096);

    // Simulate an interrupted write: a torn tail exists on disk beyond
    // the journaled offset.
    let reservation = engine.jobs[&media].reservation.as_ref().unwrap();
    let torn = reservation.temp_path().to_path_buf();
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&torn)
            .unwrap();
        file.write_all(b"TORN-TAIL").unwrap();
    }

    // The resume pass truncates the torn tail, revalidates with If-Range,
    // and continues from exactly the journaled offset.
    assert_eq!(
        drive_until_terminal(&mut engine, &media),
        JobState::Committed
    );
    assert!(engine.backend.reads.iter().any(|(start, _)| *start == 4096));
    let snapshot = committed_snapshot(&engine, &media);
    let on_disk = std::fs::read(&snapshot.cache_path).unwrap();
    assert_eq!(on_disk, payload);
    assert_eq!(snapshot.byte_size, payload.len() as u64);
}

#[test]
fn a_job_without_a_validator_resumes_by_full_restart_only() {
    let payload = payload(10 * 1024, 9);
    let server = FakeServer::serving(payload.clone())
        .without_validator()
        .failing_read_at(2);
    let (mut engine, _dir) = engine(server, QUOTA);
    let src = source(8);
    declared(&mut engine, src);
    let media = key(src, "restart-track");
    assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
    assert_eq!(engine.drive(&media), Some(JobState::Receiving));
    assert_eq!(engine.jobs[&media].record.current_bytes, 4096);
    assert_eq!(
        drive_until_terminal(&mut engine, &media),
        JobState::Committed
    );
    // Every read after the pause starts from zero again.
    let zero_reads = engine
        .backend
        .reads
        .iter()
        .filter(|(start, _)| *start == 0)
        .count();
    assert!(
        zero_reads >= 2,
        "resume without a validator must restart from zero"
    );
    let snapshot = committed_snapshot(&engine, &media);
    assert_eq!(std::fs::read(&snapshot.cache_path).unwrap(), payload);
}

// -- quota pressure -------------------------------------------------------

#[test]
fn quota_pressure_evicts_oldest_source_first_then_newest_within_source() {
    let (mut engine, _dir) = engine(FakeServer::serving(payload(1024, 1)), QUOTA);
    let a = source(10);
    let b = source(11);
    declared(&mut engine, a);
    declared(&mut engine, b);

    // Committed: b-1 at t=50 (oldest source), a-old at t=100, a-new at
    // t=200. Total 9 KiB of the 10 KiB quota.
    for (src, track, at, seed) in [
        (b, "b-1", 50u64, 1u8),
        (a, "a-old", 100, 2),
        (a, "a-new", 200, 3),
    ] {
        let server = FakeServer::serving(payload(1024, seed));
        let media = key(src, track);
        engine.advance_clock(at);
        assert_eq!(engine.admit(media.clone(), labels(track), None), Ok(()));
        // Swap the backend for the per-track payload.
        engine.backend = server;
        assert_eq!(
            drive_until_terminal(&mut engine, &media),
            JobState::Committed
        );
    }
    assert_eq!(engine.board().committed_bytes, 3 * 1024);

    // A 9 KiB download forces eviction: b first (oldest source), then
    // a-new (newest within a), leaving a-old intact.
    let big_payload = payload(9 * 1024, 9);
    engine.backend = FakeServer::serving(big_payload.clone());
    let big = key(a, "big");
    engine.advance_clock(300);
    assert_eq!(engine.admit(big.clone(), labels("big"), None), Ok(()));
    assert_eq!(drive_until_terminal(&mut engine, &big), JobState::Committed);
    assert_eq!(
        engine.catalogue(&key(b, "b-1")),
        OfflineCatalogueEntry::LiveOnly,
        "oldest source evicted first"
    );
    assert_eq!(
        engine.catalogue(&key(a, "a-new")),
        OfflineCatalogueEntry::LiveOnly,
        "newest within the walked source is surrendered first"
    );
    assert!(matches!(
        engine.catalogue(&key(a, "a-old")),
        OfflineCatalogueEntry::Cached(_)
    ));
    // The evicted files are gone; the survivor and the new commit remain.
    assert_eq!(
        std::fs::read(&committed_snapshot(&engine, &big).cache_path).unwrap(),
        big_payload
    );
}

#[test]
fn a_file_larger_than_the_whole_quota_fails_quota_exceeded_terminally() {
    let (mut engine, _dir) = engine(FakeServer::serving(payload(2 * 1024, 4)), 2 * 1024);
    let src = source(12);
    declared(&mut engine, src);
    let small = key(src, "fits");
    assert_eq!(engine.admit(small.clone(), labels("t"), None), Ok(()));
    assert_eq!(
        drive_until_terminal(&mut engine, &small),
        JobState::Committed
    );

    engine.backend = FakeServer::serving(payload(9 * 1024, 5));
    let huge = key(src, "huge");
    engine.advance_clock(500);
    assert_eq!(engine.admit(huge.clone(), labels("t"), None), Ok(()));
    assert_eq!(drive_until_terminal(&mut engine, &huge), JobState::Failed);
    let row = engine
        .board()
        .rows
        .into_iter()
        .find(|row| row.media_key == huge)
        .unwrap();
    assert_eq!(row.failure, Some(OfflineError::QuotaExceeded));
    assert_eq!(engine.catalogue(&huge), OfflineCatalogueEntry::LiveOnly);
    // Nothing half-promoted: only the committed row's bytes are charged.
    assert_eq!(engine.board().committed_bytes, 2 * 1024);
}
