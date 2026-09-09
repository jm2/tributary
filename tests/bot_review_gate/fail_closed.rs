//! Fail-closed and head-binding behavior: dispatches, pagination
//! completeness, query failures, and the documented refresh paths.

use super::harness::{
    assert_blocked, report, run_scenario, GateSandbox, GATE_PR_NUMBER, HEAD_SHA, OTHER_SHA,
};

#[test]
fn dispatch_on_a_ref_other_than_the_head_is_refused() {
    // A workflow_dispatch run is attached to the dispatched ref's tip
    // (GITHUB_SHA) while pr_number only names the pull request to evaluate.
    // A dispatch on any other ref — another branch, or main — would attach
    // this pull request's required check to a run whose evidence was
    // evaluated at a different commit, so the gate must refuse it before
    // evaluating anything.
    let sandbox = GateSandbox::new("dispatch-cross-bound");
    sandbox.use_scenario("clean");
    let output = sandbox.run_with_github_sha("workflow_dispatch", None, Some(OTHER_SHA));
    assert_blocked(&output, &[], "is not pull request head");
}

#[test]
fn dispatch_on_the_head_branch_publishes_evidence_at_that_head() {
    // The documented refresh path dispatches on the head branch, whose tip
    // equals the pull request head, so the run must proceed and publish its
    // result bound to exactly that head.
    let sandbox = GateSandbox::new("dispatch-head-bound");
    sandbox.use_scenario("clean");
    let output = sandbox.run_with_github_sha("workflow_dispatch", None, Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "the documented dispatch refresh path must pass on the head branch:\n{}",
        report(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("clean at {HEAD_SHA}")),
        "the dispatched run must publish evidence at the pull request head:\n{}",
        report(&output)
    );
}

#[test]
fn incomplete_thread_pagination_fails_closed() {
    let output = run_scenario(
        "thread-pagination-incomplete",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert_blocked(
        &output,
        &[],
        "Review-thread response omitted the pull request; failing closed.",
    );
}

#[test]
fn failed_graphql_query_fails_closed() {
    let output = run_scenario("query-failure", "pull_request", Some(HEAD_SHA));
    assert_blocked(&output, &[], "Review-thread query failed; failing closed.");
}

#[test]
fn failed_timeline_query_fails_closed() {
    // The dismissal timeline is evidence like any other: a query failure
    // must fail the check instead of silently reading as "no dismissals".
    let output = run_scenario("query-failure-timeline", "pull_request", Some(HEAD_SHA));
    assert_blocked(
        &output,
        &[],
        "Review-timeline query failed; failing closed.",
    );
}

#[test]
fn incomplete_timeline_pagination_fails_closed() {
    let output = run_scenario(
        "timeline-pagination-incomplete",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert_blocked(
        &output,
        &[],
        "Review-timeline response omitted the pull request; failing closed.",
    );
}

#[test]
fn head_moved_during_evaluation_fails_closed() {
    let output = run_scenario("head-moved", "pull_request", Some(HEAD_SHA));
    assert_blocked(&output, &[], "Pull request head moved to");
}

#[test]
fn workflow_dispatch_derives_the_head_from_the_pull_request() {
    // workflow_dispatch carries no pull-request context, so the head comes
    // from the pull request record itself, bound to the dispatched ref's
    // tip — the documented resolution refresh path dispatches on the head
    // branch, whose tip is the pull request head.
    let sandbox = GateSandbox::new("dispatch");
    sandbox.use_scenario("dispatch");
    let output = sandbox.run_with_github_sha("workflow_dispatch", None, Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "a dispatched refresh of clean evidence must pass:\n{}",
        report(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("clean at {HEAD_SHA}")),
        "the dispatched result must bind to the recorded head:\n{}",
        report(&output)
    );
}

#[test]
fn non_main_base_fails_closed() {
    let output = run_scenario("non-main-base", "workflow_dispatch", None);
    assert_blocked(&output, &[], "not main");
}

#[test]
fn stale_event_head_fails_closed() {
    let output = run_scenario("event-head-stale", "pull_request", Some(OTHER_SHA));
    assert_blocked(&output, &[], "no longer matches pull request head");
}

#[test]
fn thread_failure_prints_the_documented_refresh_paths() {
    // Thread resolution fires no Actions event, so the failure output must
    // point at the supported refresh paths: a check re-run or a targeted
    // workflow_dispatch on the pull request's head branch.
    let output = run_scenario("unresolved-current-thread", "pull_request", Some(HEAD_SHA));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("gh workflow run bot-review-gate.yml --ref <head branch>")
            && stderr.contains(&format!("pr_number={GATE_PR_NUMBER}")),
        "the failure must print the dispatch refresh path:\n{}",
        report(&output)
    );
}
