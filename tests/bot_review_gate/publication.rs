//! Publication behavior: what the trusted publisher writes into the
//! required `Bot Review Gate` context, and what it deliberately never does.

use super::harness::{assert_blocked, report, GateSandbox, HEAD_SHA};

#[test]
fn blocked_evidence_publishes_the_failing_verdict_at_the_evaluated_head() {
    // A blocked evaluation must not merely exit red: the required context
    // itself has to carry the failing verdict at the exact evaluated commit,
    // so the merge is blocked by explicit published evidence rather than by
    // a dangling absent check.
    let sandbox = GateSandbox::new("publish-blocked");
    sandbox.use_scenario("unresolved-current-thread");
    let output = sandbox.run("pull_request", Some(HEAD_SHA));
    assert_blocked(&output, &["UNRESOLVED BOT REVIEW THREAD"], "not clean");
    let check_run = sandbox.single_check_run();
    assert!(
        check_run.contains("name=Bot Review Gate")
            && check_run.contains(&format!("head_sha={HEAD_SHA}"))
            && check_run.contains("conclusion=failure"),
        "the red verdict must be published as the required context at the evaluated head:\n{check_run}"
    );
}

#[test]
fn head_moved_publishes_the_refusal_at_the_evaluated_head() {
    // A head move during evaluation is a refusal, not a pass: the verdict
    // published at the evaluated head must be the failure, and the moved-to
    // head gets its own evidence from its own announcing run.
    let sandbox = GateSandbox::new("publish-head-moved");
    sandbox.use_scenario("head-moved");
    let output = sandbox.run("pull_request", Some(HEAD_SHA));
    assert_blocked(&output, &[], "Pull request head moved to");
    let check_run = sandbox.single_check_run();
    assert!(
        check_run.contains(&format!("head_sha={HEAD_SHA}"))
            && check_run.contains("conclusion=failure"),
        "the refusal must be published at the evaluated head, never at the moved-to head:\n{check_run}"
    );
}

#[test]
fn check_run_publication_failure_fails_the_publisher() {
    // If the verdict cannot be published, the required context stays
    // unreported — which blocks the merge — and the publisher run itself
    // must say so instead of passing silently.
    let sandbox = GateSandbox::new("publish-fails");
    sandbox.use_scenario("checkrun-publication-failure");
    let output = sandbox.run("pull_request", Some(HEAD_SHA));
    assert!(
        !output.status.success(),
        "a publication failure must fail the publisher run:\n{}",
        report(&output)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Check-run publication failed"),
        "the publication failure must be explained:\n{}",
        report(&output)
    );
    assert!(
        sandbox.check_runs().is_empty(),
        "a failed publication must not leave a verdict behind"
    );
}

#[test]
fn a_commit_without_an_open_main_pull_request_publishes_nothing() {
    // The announcer can be dispatched on any ref; a run whose commit heads
    // no open main pull request has no required context to publish, and the
    // publisher must exit cleanly without stamping anything.
    let sandbox = GateSandbox::new("publish-nothing");
    sandbox.use_scenario("no-associated-pr");
    let output = sandbox.run("workflow_dispatch", Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "an announcement without a main pull request is not an error:\n{}",
        report(&output)
    );
    assert!(
        sandbox.check_runs().is_empty(),
        "no required context may be published without an associated pull request"
    );
}

#[test]
fn a_discovery_query_failure_fails_closed_without_publishing() {
    // When the API cannot say which pull requests the announcing commit
    // heads, no verdict can be honestly bound, so the run fails and
    // publishes nothing; the unreported required check keeps the merge
    // blocked until a refreshed announcement re-fires the publisher.
    let sandbox = GateSandbox::new("discovery-fails");
    sandbox.use_scenario("discovery-query-failure");
    let output = sandbox.run("pull_request", Some(HEAD_SHA));
    assert_blocked(&output, &[], "Associated-pull-request query failed");
    assert!(
        sandbox.check_runs().is_empty(),
        "a discovery failure must not guess a pull request to publish for"
    );
}

#[test]
fn every_associated_pull_request_receives_its_own_verdict() {
    // Two open main pull requests can share one head commit; each carries
    // its own required context, so each receives its own evaluation and
    // publication.
    let sandbox = GateSandbox::new("publish-every-candidate");
    sandbox.use_scenario("two-associated-prs");
    let output = sandbox.run("pull_request", Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "clean evidence must pass for every associated pull request:\n{}",
        report(&output)
    );
    let check_runs = sandbox.check_runs();
    assert_eq!(
        check_runs.len(),
        2,
        "both associated pull requests must receive their verdict:\n{check_runs:?}"
    );
    for check_run in &check_runs {
        assert!(
            check_run.contains("name=Bot Review Gate")
                && check_run.contains(&format!("head_sha={HEAD_SHA}"))
                && check_run.contains("conclusion=success"),
            "every published verdict must be the required context at the evaluated head:\n{check_run}"
        );
    }
}

#[test]
fn the_announcers_own_check_name_never_collides_with_the_required_context() {
    // The announcer workflow runs pull-request-controlled content, so its
    // job check-run must never carry the required context name: that name
    // is written exclusively by the default-branch publisher.
    let workflow: serde_yaml::Value = serde_yaml::from_str(super::harness::BOT_REVIEW_GATE_YAML)
        .expect("bot review gate announcer workflow must parse");
    let job_name = workflow["jobs"]["bot-review-gate"]["name"]
        .as_str()
        .expect("the announcer job must declare its check-run name");
    assert_ne!(
        job_name, "Bot Review Gate",
        "the announcer's check-run name must never collide with the required context"
    );
    let run = workflow["jobs"]["bot-review-gate"]["steps"][0]["run"]
        .as_str()
        .expect("the announcer job must inline its run script");
    assert!(
        !run.contains("gh api") && !run.contains("graphql") && !run.contains("check-runs"),
        "the announcer must evaluate and publish nothing itself"
    );
}

#[test]
fn the_publisher_owns_the_required_context_exclusively() {
    // Within the publisher, the required context name appears only in the
    // publication call — the one write the `checks: write` grant covers.
    let workflow: serde_yaml::Value =
        serde_yaml::from_str(super::harness::BOT_REVIEW_GATE_PUBLISHER_YAML)
            .expect("bot review gate publisher workflow must parse");
    let run = workflow["jobs"]["publish"]["steps"][0]["run"]
        .as_str()
        .expect("the publisher step must inline its run script");
    let occurrences = run.matches("\"Bot Review Gate\"").count();
    assert_eq!(
        occurrences, 1,
        "the required context name must appear exactly once, in the check-run publication call"
    );
    assert!(
        run.contains("-F name=\"Bot Review Gate\""),
        "the published context must be created by the check-run API call"
    );
}

#[test]
fn every_refusal_publishes_before_returning() {
    // The evaluation loop swallows `set -e`, so a refusal site that merely
    // called finish_blocked and fell through would continue evaluating —
    // and might still publish a green verdict afterwards. Every call site
    // must therefore be followed by an explicit `return 1` (the base
    // mismatch included, which publishes nothing but must refuse).
    let script = super::harness::gate_run_script();
    let lines: Vec<&str> = script.lines().collect();
    let mut call_sites = 0;
    for (index, line) in lines.iter().enumerate() {
        if line.contains("finish_blocked \"${") {
            call_sites += 1;
            let next = lines[index + 1..]
                .iter()
                .find(|candidate| !candidate.trim().is_empty())
                .copied()
                .unwrap_or("");
            assert_eq!(
                next.trim(),
                "return 1",
                "a finish_blocked call site must be followed by an explicit return: {line}"
            );
        }
    }
    assert!(
        call_sites >= 9,
        "every query, pagination, and head-binding refusal must publish its verdict: {call_sites}"
    );
}
