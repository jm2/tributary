//! Fail-closed and head-binding behavior: the announcer-head binding,
//! pagination completeness, query failures, and the documented refresh paths.

use super::harness::{
    assert_blocked, report, run_scenario, GateSandbox, GATE_PR_NUMBER, HEAD_SHA, OTHER_SHA,
};

#[test]
fn the_publisher_requires_the_announcing_runs_head() {
    // The publisher binds its verdict to the announcing run's head commit.
    // Without that commit there is nothing it is willing to evaluate — and
    // no check-run it could honestly publish.
    let sandbox = GateSandbox::new("announcer-head-missing");
    sandbox.use_scenario("clean");
    let output = sandbox.run("workflow_dispatch", None);
    assert_blocked(&output, &[], "requires the announcing run's head commit");
    assert!(
        sandbox.check_runs().is_empty(),
        "no verdict may be published without a binding head"
    );
}

#[test]
fn an_announcer_run_at_the_pull_request_head_publishes_evidence_at_that_head() {
    // The documented refresh path dispatches the announcer on the head
    // branch, whose tip equals the pull request head, so the publisher
    // proceeds and publishes its verdict bound to exactly that head.
    let sandbox = GateSandbox::new("announcer-head-bound");
    sandbox.use_scenario("clean");
    let output = sandbox.run("workflow_dispatch", Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "the documented dispatch refresh path must pass on the head branch:\n{}",
        report(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("clean at {HEAD_SHA}")),
        "the published verdict must bind to the pull request head:\n{}",
        report(&output)
    );
    assert!(
        sandbox
            .single_check_run()
            .contains(&format!("head_sha={HEAD_SHA}"))
            && sandbox.single_check_run().contains("conclusion=success"),
        "the green verdict must be published as the required context at the evaluated head"
    );
}

#[test]
fn incomplete_thread_pagination_fails_closed() {
    // The incomplete page is page 1 of 2, and page 2 is valid: only
    // validation that inspects every page can catch the violation, so a
    // regression to validating the last page alone would let this scenario
    // pass and fail this test.
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
fn non_main_base_fails_closed() {
    let output = run_scenario("non-main-base", "pull_request", Some(HEAD_SHA));
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
    // announcer dispatch on the pull request's head branch.
    let output = run_scenario("unresolved-current-thread", "pull_request", Some(HEAD_SHA));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("gh workflow run bot-review-gate.yml --ref <head branch>")
            && stderr.contains(&format!("pr_number={GATE_PR_NUMBER}")),
        "the failure must print the dispatch refresh path:\n{}",
        report(&output)
    );
}
