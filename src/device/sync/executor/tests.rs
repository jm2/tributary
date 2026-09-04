use super::*;
use crate::device::sync::mapping::PlaylistPair;
use crate::device::sync::planner::{HostTrackEntry, HostTrackSet, SyncPlanner, SyncRequest};
use crate::device::sync::policy::{ConflictResolution, SyncPolicy};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn host(value: &str) -> super::super::HostPlaylistId {
    super::super::HostPlaylistId::new(value).expect("host")
}

fn enabled_policy() -> SyncPolicy {
    SyncPolicy::enable(PathBuf::from("Music"), ConflictResolution::HostWins).expect("policy")
}

fn disabled_policy() -> SyncPolicy {
    SyncPolicy::disabled()
}

fn make_pair() -> PlaylistPair {
    PlaylistPair::new(host("a"), "device-a", PathBuf::from("Music")).expect("pair")
}

fn build_inputs(tracks: Vec<HostTrackEntry>, state: IncrementalSyncState) -> SyncPlan {
    let pair = make_pair();
    let mut map = PlaylistMap::new();
    map.insert(pair.clone()).expect("insert");
    let mut states = BTreeMap::new();
    states.insert(host("a"), state);
    SyncPlanner::new()
        .plan(&SyncRequest {
            pairs: vec![pair],
            tracks_by_pair: vec![HostTrackSet {
                host: host("a"),
                tracks,
                state: states.get(&host("a")).cloned().unwrap_or_default(),
            }],
            state_by_pair: vec![states.get(&host("a")).cloned().unwrap_or_default()],
        })
        .expect("plan")
}

#[test]
fn executor_records_open_session_and_browse_storage() {
    let plan = build_inputs(vec![], IncrementalSyncState::new());
    let mut map = PlaylistMap::new();
    map.insert(make_pair()).expect("insert");
    let mut state_by_pair = BTreeMap::new();
    state_by_pair.insert(host("a"), IncrementalSyncState::new());
    let mut policies = BTreeMap::new();
    policies.insert(host("a"), disabled_policy());
    let guard = SyncSessionGuard::attached();
    let mut executor = SyncExecutor::new(plan, policies, map, state_by_pair, guard.clone(), 1);
    let summary = executor.run();
    assert!(summary.completed);
    let events = guard.snapshot_events();
    assert!(events.iter().any(|e| matches!(
        e,
        super::super::recovery::AttachDetachEvent::StageCompleted { stage } if *stage == SyncStage::OpenSession
    )));
}

#[test]
fn executor_skips_removal_when_policy_disallows_delete_missing() {
    let mut state = IncrementalSyncState::new();
    state.record_synced("track-1", "fp1", "Music/track-1.bin", 100);
    let plan = build_inputs(vec![], state.clone());
    let mut map = PlaylistMap::new();
    map.insert(make_pair()).expect("insert");
    let mut state_by_pair = BTreeMap::new();
    state_by_pair.insert(host("a"), state);
    let mut policies = BTreeMap::new();
    policies.insert(host("a"), enabled_policy());
    let guard = SyncSessionGuard::attached();
    let mut executor = SyncExecutor::new(plan, policies, map, state_by_pair, guard, 200);
    let summary = executor.run();
    assert_eq!(summary.skipped_conflicts, 1);
    assert_eq!(summary.removed, 0);
}

#[test]
fn executor_detaches_when_guard_is_marked_detached() {
    let plan = build_inputs(vec![], IncrementalSyncState::new());
    let mut map = PlaylistMap::new();
    map.insert(make_pair()).expect("insert");
    let mut state_by_pair = BTreeMap::new();
    state_by_pair.insert(host("a"), IncrementalSyncState::new());
    let mut policies = BTreeMap::new();
    policies.insert(host("a"), disabled_policy());
    let guard = SyncSessionGuard::attached();
    guard.mark_detached();
    let mut executor = SyncExecutor::new(plan, policies, map, state_by_pair, guard.clone(), 1);
    let summary = executor.run();
    assert!(!summary.completed);
    let verdict = super::super::recovery::AttachDetachRecovery::verdict(&guard);
    assert!(matches!(
        verdict,
        super::super::recovery::RecoveryVerdict::Failed { .. }
    ));
}

/// Build an executor for one pair with one policy and one recorded
/// state, planning the given host tracks.
fn make_executor(
    tracks: Vec<HostTrackEntry>,
    state: IncrementalSyncState,
    policy: SyncPolicy,
) -> SyncExecutor {
    let pair = make_pair();
    let mut map = PlaylistMap::new();
    map.insert(pair.clone()).expect("insert");
    let mut policies = BTreeMap::new();
    policies.insert(host("a"), policy);
    let mut state_by_pair = BTreeMap::new();
    state_by_pair.insert(host("a"), state.clone());
    let plan = SyncPlanner::new()
        .plan(&SyncRequest {
            pairs: vec![pair],
            tracks_by_pair: vec![HostTrackSet {
                host: host("a"),
                tracks,
                state: state.clone(),
            }],
            state_by_pair: vec![state],
        })
        .expect("plan");
    SyncExecutor::new(
        plan,
        policies,
        map,
        state_by_pair,
        SyncSessionGuard::attached(),
        200,
    )
}

fn track(id: &str, fingerprint: &str, path: &str, size_bytes: u64) -> HostTrackEntry {
    HostTrackEntry {
        track_id: id.into(),
        fingerprint: fingerprint.into(),
        device_relative_path: PathBuf::from(path),
        size_bytes,
    }
}

#[test]
fn executor_records_planner_fingerprint_and_path() {
    let mut executor = make_executor(
        vec![track("t1", "fp-new", "Artist/Song.flac", 100)],
        IncrementalSyncState::new(),
        enabled_policy(),
    );
    let summary = executor.run();
    assert_eq!(summary.written, 1);
    assert!(summary.completed);
    let state = executor.state_by_pair().get(&host("a")).expect("state");
    let Some(TrackSyncStatus::Synced {
        fingerprint,
        device_relative_path,
        last_synced_at,
    }) = state.status("t1")
    else {
        panic!("expected a synced entry, got {:?}", state.status("t1"));
    };
    // The recorded fingerprint is the host's current value, not a
    // stale or empty one, and the path is the planner-validated one.
    assert_eq!(fingerprint, "fp-new");
    assert_eq!(device_relative_path, "Music/Artist/Song.flac");
    assert_eq!(*last_synced_at, 200);
}

#[test]
fn executor_write_makes_next_plan_unchanged() {
    let track = track("t1", "fp1", "song.flac", 100);
    let mut executor = make_executor(
        vec![track.clone()],
        IncrementalSyncState::new(),
        enabled_policy(),
    );
    executor.run();
    let recorded = executor.state_by_pair().get(&host("a")).expect("state");
    // A second plan over the same inputs must now see the track as
    // unchanged — proof the run recorded the new fingerprint.
    let plan = SyncPlanner::new()
        .plan(&SyncRequest {
            pairs: vec![make_pair()],
            tracks_by_pair: vec![HostTrackSet {
                host: host("a"),
                tracks: vec![track],
                state: recorded.clone(),
            }],
            state_by_pair: vec![recorded.clone()],
        })
        .expect("plan");
    assert_eq!(plan.write_count, 0);
    assert!(matches!(
        plan.deltas[0].kind,
        SyncDeltaKind::Unchanged { .. }
    ));
}

#[test]
fn executor_device_wins_writes_new_and_marks_modified_stale() {
    let mut state = IncrementalSyncState::new();
    state.record_synced("t1", "fp1", "Music/song.flac", 100);
    let device_wins =
        SyncPolicy::enable(PathBuf::from("Music"), ConflictResolution::DeviceWins).expect("policy");
    let mut executor = make_executor(
        vec![
            track("t1", "fp2", "song.flac", 100),
            track("t2", "fp-new", "other.flac", 100),
        ],
        state,
        device_wins,
    );
    let summary = executor.run();
    // The new track writes (no device copy to preserve); the
    // modified track is declined and the host copy is marked stale.
    assert_eq!(summary.written, 1);
    assert_eq!(summary.skipped_conflicts, 1);
    assert!(summary.completed);
    let state = executor.state_by_pair().get(&host("a")).expect("state");
    let Some(TrackSyncStatus::Modified { fingerprint }) = state.status("t1") else {
        panic!("expected a stale entry, got {:?}", state.status("t1"));
    };
    assert_eq!(fingerprint, "fp2");
    assert!(matches!(
        state.status("t2"),
        Some(TrackSyncStatus::Synced { .. })
    ));
}

#[test]
fn executor_skip_strategy_leaves_state_untouched() {
    let mut state = IncrementalSyncState::new();
    state.record_synced("t1", "fp1", "Music/song.flac", 100);
    let skip =
        SyncPolicy::enable(PathBuf::from("Music"), ConflictResolution::Skip).expect("policy");
    let mut executor = make_executor(vec![track("t1", "fp2", "song.flac", 100)], state, skip);
    let summary = executor.run();
    assert_eq!(summary.written, 0);
    assert_eq!(summary.skipped_conflicts, 1);
    assert!(summary.completed);
    let state = executor.state_by_pair().get(&host("a")).expect("state");
    assert_eq!(
        state.status("t1"),
        Some(&TrackSyncStatus::Synced {
            fingerprint: "fp1".into(),
            device_relative_path: "Music/song.flac".into(),
            last_synced_at: 100,
        })
    );
}

#[test]
fn executor_fail_strategy_reports_conflict_but_completes() {
    let mut state = IncrementalSyncState::new();
    state.record_synced("t1", "fp1", "Music/song.flac", 100);
    let fail =
        SyncPolicy::enable(PathBuf::from("Music"), ConflictResolution::Fail).expect("policy");
    let mut executor = make_executor(vec![track("t1", "fp2", "song.flac", 100)], state, fail);
    let summary = executor.run();
    assert_eq!(summary.failed_conflicts, 1);
    assert_eq!(summary.written, 0);
    assert!(summary.completed);
}

#[test]
fn executor_disabled_policy_never_writes() {
    let mut executor = make_executor(
        vec![track("t1", "fp1", "song.flac", 100)],
        IncrementalSyncState::new(),
        disabled_policy(),
    );
    let summary = executor.run();
    assert_eq!(summary.written, 0);
    assert!(summary.completed);
    let state = executor.state_by_pair().get(&host("a")).expect("state");
    assert!(state.status("t1").is_none());
}

#[test]
fn executor_run_consumes_each_delta_once() {
    let mut executor = make_executor(
        vec![track("t1", "fp1", "song.flac", 100)],
        IncrementalSyncState::new(),
        enabled_policy(),
    );
    let first = executor.run();
    assert_eq!(first.written, 1);
    // A second run over the same executor must not replay the
    // executed deltas.
    let second = executor.run();
    assert_eq!(second.written, 0);
    assert!(second.completed);
}

#[test]
fn executor_records_pair_sync_instant_for_opted_in_policy() {
    let mut executor = make_executor(vec![], IncrementalSyncState::new(), enabled_policy());
    let summary = executor.run();
    assert!(summary.completed);
    let pair = executor.map().get_by_host(&host("a")).expect("pair");
    assert_eq!(pair.last_synced_at(), Some(200));
}
