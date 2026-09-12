//! Review-evidence decisions: threads, change requests, dismissals, and the
//! exact-head binding of bot review evidence.

use super::harness::{assert_blocked, report, run_scenario, GateSandbox, HEAD_SHA, OTHER_SHA};

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
        stdout.contains("review threads evaluated: 2") && stdout.contains("reviews evaluated: 3"),
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

#[test]
fn a_pending_review_attempt_blocks_and_suspends_the_author_s_earlier_conclusion() {
    // A PENDING review is an attempt in flight: while it exists, none of the
    // author's earlier conclusions count at this head — the pending attempt
    // may overturn them. The scenario carries an APPROVED review by the bot
    // at the exact evaluated head plus a newer PENDING attempt by the same
    // bot: the gate must block, and the report must name the attempt (its
    // database id and commit) so the block is bound to exactly what is in
    // flight. Without this rule, a review being rewritten — the one state
    // the submitted-reviews API cannot surface — would leave the old green
    // verdict mergeable.
    let output = run_scenario("pending-bot-review", "pull_request", Some(HEAD_SHA));
    assert_blocked(
        &output,
        &[
            "PENDING BOT REVIEW ATTEMPT",
            "database id 21",
            &format!("at {HEAD_SHA}"),
        ],
        "not clean",
    );
}

#[test]
fn a_listed_trusted_reviewer_with_no_review_at_all_fails_closed() {
    // The expected-reviewer set is fixed by enumeration, not observation:
    // every listed identity must have a review on the pull request, and a
    // listed identity with zero reviews — an outage, a rate limit, a rename
    // — fails closed exactly like a rejected review, threads or no threads.
    // The scenario's only reviews are a human comment and no review by the
    // listed reviewer at all.
    let output = run_scenario("missing-expected-reviewer", "pull_request", Some(HEAD_SHA));
    assert_blocked(
        &output,
        &[
            "STALE BOT REVIEW EVIDENCE by coderabbitai",
            "no review submitted",
        ],
        "not clean",
    );
}

#[test]
fn an_empty_trusted_reviewer_set_is_a_configuration_failure_not_a_pass() {
    // The expected set comes from the publisher's enumerated env. An EMPTY
    // configured set would leave the all-green policy with nobody expected
    // and pass vacuously on any pull request, so it emits its own
    // non-waivable blocking violation instead.
    let sandbox = GateSandbox::new("empty-expected-reviewers");
    sandbox.use_scenario("clean");
    let output = sandbox.run_with_expected_reviewers("pull_request", Some(HEAD_SHA), "");
    assert_blocked(
        &output,
        &[
            "TRUSTED REVIEWER SET EMPTY",
            "EXPECTED_BOT_REVIEWERS in bot-review-gate-publisher.yml",
        ],
        "not clean",
    );
}

#[test]
fn an_outstanding_re_review_request_invalidates_the_reviewer_s_earlier_clean_result() {
    // docs/refinery-config.md: a re-review request addressed to a trusted
    // expected reviewer is an attempt in flight, and until a review is
    // submitted at the evaluated head in response to it, it invalidates
    // that reviewer's earlier clean result there. The scenario carries the
    // bot's APPROVED review at an EARLIER head plus an API-visible
    // requested_reviewers entry for it, and no timeline events at all: no
    // review-request event exists to order against, so the handshake
    // cannot be proven acknowledged — the fail-closed path — and the
    // earlier approval stops counting. The gate must block, naming the
    // outstanding request and its reviewer.
    let output = run_scenario(
        "requested-review-outstanding",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert_blocked(
        &output,
        &["OUTSTANDING BOT REVIEW REQUEST by coderabbitai"],
        "not clean",
    );
}

#[test]
fn a_re_review_request_acknowledged_by_a_review_at_the_head_does_not_block() {
    // GitHub clears a reviewer's request when the reviewer submits, so a
    // listed request standing beside that reviewer's review at the exact
    // evaluated head is the bounded API-lag race, not an attempt in
    // flight. The acknowledgment is attempt-bound: the fixture's timeline
    // carries the review-request event PREDATING the at-head review, so
    // the submitted at-head review provably answers that specific request
    // and the prior verdict logic applies. A request event that predated
    // nothing — one postdating the at-head review — is the blocking
    // scenario below. The request alone never blocks a head its reviewer
    // has already re-reviewed.
    let sandbox = GateSandbox::new("requested-review-acknowledged-at-head");
    sandbox.use_scenario("requested-review-acknowledged-at-head");
    let output = sandbox.run("pull_request", Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "the at-head review must acknowledge the outstanding request:\n{}",
        report(&output)
    );
    let check_run = sandbox.opened_and_finalized_verdict();
    assert!(
        check_run.contains(&format!("head_sha={HEAD_SHA}"))
            && check_run.contains("conclusion=success"),
        "the acknowledged request must leave the clean verdict green at the evaluated head:\n{check_run}"
    );
}

#[test]
fn a_re_review_request_that_postdates_the_reviewer_s_at_head_review_blocks() {
    // The attempt-bound handshake: a review-request event that POSTDATES
    // the reviewer's at-head review is a re-review attempt in flight that
    // the older review cannot acknowledge — it predates the request. The
    // requested_reviewers snapshot still lists the reviewer, the reviewer's
    // APPROVED review sits at the exact evaluated head, and yet the gate
    // must block: without attempt-binding, a manual refresh would publish
    // green for the duration of the in-flight re-review.
    let output = run_scenario(
        "requested-review-request-postdates-head-review",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert_blocked(
        &output,
        &[
            "OUTSTANDING BOT REVIEW REQUEST by coderabbitai",
            "re-review requested after the reviewer's review at this head",
        ],
        "not clean",
    );
}

#[test]
fn a_formally_removed_re_review_request_does_not_block() {
    // A review-request removal event after the latest request formally
    // withdraws the attempt (GitHub also auto-removes a request when the
    // reviewer submits), so the request no longer stands even while the
    // requested_reviewers snapshot still carries the reviewer — the
    // fixture deliberately stages that lag to prove the timeline ordering,
    // not list membership, decides. The reviewer's at-head approval is
    // otherwise current, so the gate must stay green.
    let sandbox = GateSandbox::new("requested-review-request-removed");
    sandbox.use_scenario("requested-review-request-removed");
    let output = sandbox.run("pull_request", Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "a withdrawn re-review request must not block:\n{}",
        report(&output)
    );
    let check_run = sandbox.opened_and_finalized_verdict();
    assert!(
        check_run.contains(&format!("head_sha={HEAD_SHA}"))
            && check_run.contains("conclusion=success"),
        "the withdrawn request must leave the clean verdict green at the evaluated head:\n{check_run}"
    );
}

#[test]
fn re_review_requests_outside_the_trusted_set_do_not_block() {
    // The handshake binds only the enumerated trusted set: a re-review
    // request addressed to any other login — a human, a non-review
    // integration — is out of scope and must not block an otherwise clean
    // pull request.
    let sandbox = GateSandbox::new("requested-review-out-of-scope");
    sandbox.use_scenario("requested-review-out-of-scope");
    let output = sandbox.run("pull_request", Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "out-of-scope review requests must not block:\n{}",
        report(&output)
    );
    let check_run = sandbox.opened_and_finalized_verdict();
    assert!(
        check_run.contains("conclusion=success"),
        "out-of-scope requests must leave the clean verdict green:\n{check_run}"
    );
}

#[test]
fn a_failed_requested_reviewers_query_fails_closed() {
    // The requested-reviewers read is evidence like any other: a query
    // failure must block the gate instead of silently reading as "no
    // outstanding requests".
    let output = run_scenario(
        "requested-review-query-failure",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert_blocked(
        &output,
        &[],
        "Requested-reviewers query failed; failing closed.",
    );
}
