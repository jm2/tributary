//! Fail-closed and head-binding behavior: the announcer-head binding,
//! pagination completeness, query failures, and the documented refresh paths.

use super::harness::{
    assert_blocked, report, run_scenario, GateSandbox, GATE_PR_NUMBER, HEAD_SHA, OTHER_SHA,
};

#[test]
fn the_publisher_requires_the_announcing_runs_head() {
    // The publisher binds its verdict to the announcing run's head commit.
    // Without that commit — and without a run record to recover it from —
    // there is nothing it is willing to evaluate, and no check-run it
    // could honestly publish or supersede.
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
fn a_missing_event_head_is_recovered_from_the_announcing_run_record() {
    // A completion event that carried no head commit must not skip the
    // refresh: the Actions run record is the same authority GitHub derived
    // the event field from, so the head is recovered from it and the
    // refresh proceeds — publishing at the recovered head like any other
    // announcer completion.
    let sandbox = GateSandbox::new("head-from-run-record");
    sandbox.use_scenario("head-recovered-from-run-record");
    let output = sandbox.run("workflow_dispatch", None);
    assert!(
        output.status.success(),
        "the recovered head must drive a normal refresh:\n{}",
        report(&output)
    );
    let check_run = sandbox.opened_and_finalized_verdict();
    assert!(
        check_run.contains(&format!("head_sha={HEAD_SHA}"))
            && check_run.contains("conclusion=success"),
        "the verdict must bind to the head recovered from the run record:\n{check_run}"
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
    let check_run = sandbox.opened_and_finalized_verdict();
    assert!(
        check_run.contains(&format!("head_sha={HEAD_SHA}"))
            && check_run.contains("conclusion=success"),
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
fn a_null_reviews_connection_fails_closed_as_a_partial_response() {
    // A partial-200 shape — a valid pull-request record whose reviews
    // connection is null — passes the pull-request guard, but the evidence
    // it carries is unproven: the decision filter would silently iterate
    // the null connection as empty and could read "no reviews" out of a
    // response that answered nothing. It must fail closed instead.
    let output = run_scenario("decision-null-reviews", "pull_request", Some(HEAD_SHA));
    assert_blocked(
        &output,
        &[],
        "Review response was partial (GraphQL errors or a missing reviews connection); failing closed.",
    );
}

#[test]
fn a_null_review_threads_connection_fails_closed_as_a_partial_response() {
    // The threads connection follows the same rule: a null connection under
    // an otherwise complete pull request is unproven evidence, never a
    // vacuously clean answer.
    let output = run_scenario(
        "decision-null-review-threads",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert_blocked(
        &output,
        &[],
        "Review-thread response was partial (GraphQL errors or a missing reviewThreads connection); failing closed.",
    );
}

#[test]
fn graphql_errors_beside_a_valid_head_fail_closed() {
    // A GraphQL error entry can ride beside a valid head on an HTTP 200, so
    // the transport-level success of the query proves nothing on its own: a
    // page carrying `errors` is a partial answer and must block instead of
    // lending its valid-looking head to a clean verdict.
    let output = run_scenario("decision-graphql-errors", "pull_request", Some(HEAD_SHA));
    assert_blocked(
        &output,
        &[],
        "Review-thread response was partial (GraphQL errors or a missing reviewThreads connection); failing closed.",
    );
}

#[test]
fn a_failed_decision_evaluation_fails_closed_instead_of_publishing_a_clean_verdict() {
    // The fail-open the exact-head review caught: the candidate loop invokes
    // the evaluator with `|| true`, which swallows errexit for the whole
    // function body, so the decision filter's exit status was never checked
    // and an empty `evaluated` fell through to a published clean verdict.
    // This fixture passes EVERY fetch guard (pull request present, head
    // bound, connections non-null, no GraphQL errors) and then makes the
    // decision filter itself fail — a thread-nodes field that is not a list.
    // The gate must catch the failed evaluation explicitly, record the
    // blocked verdict, and let the shared publication go out red at the
    // evaluated head — never `.clean`.
    let sandbox = GateSandbox::new("decision-evaluation-failure");
    sandbox.use_scenario("decision-evaluation-failure");
    let output = sandbox.run("pull_request", Some(HEAD_SHA));
    assert_blocked(
        &output,
        &[],
        "Decision evaluation failed or returned a malformed verdict; failing closed.",
    );
    let check_run = sandbox.opened_and_finalized_verdict();
    assert!(
        check_run.contains(&format!("head_sha={HEAD_SHA}"))
            && check_run.contains("conclusion=failure"),
        "a failed decision evaluation must publish the blocked verdict at the evaluated head:\n{check_run}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("clean at"),
        "a failed evaluation must never produce a clean summary:\n{stdout}"
    );
}

#[test]
fn head_moved_during_evaluation_fails_closed() {
    let output = run_scenario("head-moved", "pull_request", Some(HEAD_SHA));
    assert_blocked(&output, &[], "Pull request head moved to");
}

#[test]
fn non_main_base_fails_closed_and_supersedes_the_head_verdict() {
    // A candidate whose record no longer targets main is skipped, and the
    // retarget fired no Actions event — so this refresh can be the only
    // chance to replace the head's previous verdict before a retarget back
    // to main (which also fires no refresh) would let it decide again. The
    // all-skipped path therefore publishes the superseding failure at the
    // announced head.
    let sandbox = GateSandbox::new("non-main-base-supersedes");
    sandbox.use_scenario("non-main-base");
    let output = sandbox.run("pull_request", Some(HEAD_SHA));
    assert_blocked(&output, &[], "not main");
    let check_run = sandbox.opened_and_finalized_verdict();
    assert!(
        check_run.contains("name=Bot Review Gate")
            && check_run.contains(&format!("head_sha={HEAD_SHA}"))
            && check_run.contains("conclusion=failure"),
        "the all-skipped path must supersede any earlier verdict at the announced head:\n{check_run}"
    );
}

#[test]
fn an_announced_commit_only_a_descendant_contains_is_never_evaluated() {
    // `commits/<sha>/pulls` also returns stacked descendants that merely
    // CONTAIN the announced commit. Evaluating one anyway hits the
    // head-mismatch refusal and publishes a failing verdict at the
    // announced commit — blocking the genuine pull request headed there on
    // every refresh. Selection by exact head SHA leaves nothing to
    // evaluate: the descendant's evidence is never queried, and the
    // not-evaluated verdict is published red at the announced commit —
    // superseding any stale verdict at that commit, where it is inert (a
    // commit that heads no open main pull request merges nowhere) — while
    // the genuine pull request's own announcements produce its real
    // verdict.
    let sandbox = GateSandbox::new("descendant-not-evaluated");
    sandbox.use_scenario("event-head-stale");
    let output = sandbox.run("pull_request", Some(OTHER_SHA));
    assert!(
        output.status.success(),
        "a commit that heads no open main pull request is not an error:\n{}",
        report(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!(
            "No open pull request against main is headed by the announced commit {OTHER_SHA}"
        )),
        "the run must say why nothing was evaluated:\n{}",
        report(&output)
    );
    let invocations =
        std::fs::read_to_string(sandbox.root.join("state/invocations.log")).unwrap_or_default();
    assert!(
        !invocations.contains("graphql"),
        "a descendant that merely contains the announced commit must never be evaluated:\n{invocations}"
    );
    let check_run = sandbox.opened_and_finalized_verdict();
    assert!(
        check_run.contains(&format!("head_sha={OTHER_SHA}"))
            && check_run.contains("conclusion=failure"),
        "the not-evaluated verdict must supersede any earlier one at the announced commit:\n{check_run}"
    );
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
