//! Review-evidence decisions: threads, change requests, dismissals, and the
//! exact-head binding of bot review evidence.

use super::harness::{assert_blocked, report, run_scenario, HEAD_SHA, OTHER_SHA};

#[test]
fn clean_evidence_at_the_evaluated_head_passes() {
    let output = run_scenario("clean", "pull_request", Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "resolved threads plus a current bot approval must pass:\n{}",
        report(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("clean at {HEAD_SHA}")),
        "the published result must be bound to the exact evaluated head:\n{}",
        report(&output)
    );
    assert!(
        stdout.contains("review threads evaluated: 2") && stdout.contains("reviews evaluated: 2"),
        "the published result must report the paginated evidence it evaluated:\n{}",
        report(&output)
    );
}

#[test]
fn unresolved_current_bot_thread_blocks() {
    let output = run_scenario("unresolved-current-thread", "pull_request", Some(HEAD_SHA));
    assert_blocked(
        &output,
        &["UNRESOLVED BOT REVIEW THREAD", "u/current-thread"],
        "not clean",
    );
}

#[test]
fn unresolved_outdated_bot_thread_still_blocks() {
    // The audit's P1b finding: filtering unresolved threads by
    // `isOutdated == false` dropped findings whenever their diff location
    // moved. Movement is not resolution, so an outdated unresolved thread
    // must still block, with its outdatedness disclosed.
    let output = run_scenario("unresolved-outdated-thread", "pull_request", Some(HEAD_SHA));
    assert_blocked(
        &output,
        &[
            "UNRESOLVED BOT REVIEW THREAD",
            "outdated",
            "u/outdated-thread",
        ],
        "not clean",
    );
}

#[test]
fn human_threads_stay_outside_the_bot_evidence_gate() {
    let output = run_scenario("human-thread-only", "pull_request", Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "only bot-started threads are the gate's scope:\n{}",
        report(&output)
    );
}

#[test]
fn outstanding_bot_change_request_blocks_without_a_thread() {
    // The audit's P1a finding: a bot can request changes without opening an
    // inline thread, and a threads-only query cannot see it. The fixture's
    // change-request review is bound to the evaluated head, so the block is
    // attributable to the outstanding conclusion alone — a current-head,
    // threadless change request blocks.
    let output = run_scenario("outstanding-change-request", "pull_request", Some(HEAD_SHA));
    assert_blocked(
        &output,
        &["OUTSTANDING BOT CHANGE REQUEST", "u/cr-review"],
        "not clean",
    );
}

#[test]
fn superseding_bot_approval_clears_a_change_request() {
    let output = run_scenario("change-request-superseded", "pull_request", Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "the author's latest review decides: a later current-head approval supersedes the change request:\n{}",
        report(&output)
    );
}

#[test]
fn dismissing_a_bot_change_request_clears_it() {
    let output = run_scenario("change-request-dismissed", "pull_request", Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "a formally dismissed review leaves no outstanding conclusion:\n{}",
        report(&output)
    );
}

#[test]
fn comment_after_a_dismissal_is_still_bound_to_the_head() {
    // A formal dismissal clears the dismissed conclusion, but it exempts
    // only itself from the head binding: a review submitted after the
    // dismissal is ordinary evidence again, so a comment left at a
    // since-superseded head must report stale evidence instead of passing.
    let output = run_scenario(
        "change-request-dismissed-then-stale-comment",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert_blocked(
        &output,
        &["STALE BOT REVIEW EVIDENCE", OTHER_SHA, HEAD_SHA],
        "not clean",
    );
}

#[test]
fn current_comment_after_a_dismissal_passes() {
    let output = run_scenario(
        "change-request-dismissed-then-current-comment",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert!(
        output.status.success(),
        "a dismissal followed by a current-head review provides current evidence:\n{}",
        report(&output)
    );
}

#[test]
fn comment_submitted_before_the_dismissal_is_cleared_with_it() {
    // A dismissal mutates the dismissed review in place and keeps its
    // database ID, so review IDs cannot order evidence around a dismissal:
    // ranking by ID alone treats a comment submitted before the formal
    // dismissal as post-dismissal evidence forever and blocks at every
    // later head. The recorded dismissal time from the review timeline
    // decides — a pre-dismissal comment is cleared with the dismissal and
    // must not resurrect after a head change.
    let output = run_scenario(
        "dismissal-comment-before-dismissal-then-head-change",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert!(
        output.status.success(),
        "a comment submitted before the dismissal must be cleared with it:\n{}",
        report(&output)
    );
}

#[test]
fn comment_only_bot_review_does_not_clear_a_change_request() {
    // GitHub clears Request-changes only on a later approval or a formal
    // dismissal. A comment-only review carries no conclusion, so a bot
    // commenting at the evaluated head must not lift its outstanding
    // change request — ranking every review state together would let the
    // newest comment silence the block.
    let output = run_scenario(
        "change-request-then-commented",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert_blocked(
        &output,
        &["OUTSTANDING BOT CHANGE REQUEST", "u/ctc-cr"],
        "not clean",
    );
}

#[test]
fn stale_bot_review_evidence_blocks() {
    // A bot whose latest review predates the evaluated head provides no
    // evidence about the commit being merged.
    let output = run_scenario("stale-approval", "pull_request", Some(HEAD_SHA));
    assert_blocked(
        &output,
        &["STALE BOT REVIEW EVIDENCE", OTHER_SHA, HEAD_SHA],
        "not clean",
    );
}

#[test]
fn reviews_beyond_the_first_pagination_page_reach_the_decisions() {
    let output = run_scenario("review-pagination-boundary", "pull_request", Some(HEAD_SHA));
    assert_blocked(
        &output,
        &["OUTSTANDING BOT CHANGE REQUEST", "u/pg-cr"],
        "not clean",
    );
}
