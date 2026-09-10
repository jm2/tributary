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
fn a_missing_gate_app_identity_refuses_to_publish() {
    // The required context must never be published under the shared
    // workflow identity: every pull-request-controlled job publishes its
    // check runs under that same GitHub Actions integration, so a
    // same-named forged check would satisfy the context. A missing
    // gate-publisher App token (the mint step failed) must leave the
    // context unreported — merge blocked — instead of degrading.
    let sandbox = GateSandbox::new("publish-no-gate-identity");
    sandbox.use_scenario("clean");
    let output = sandbox.run_without_gate_token("pull_request", Some(HEAD_SHA));
    assert!(
        !output.status.success(),
        "publishing without the gate identity must fail closed:\n{}",
        report(&output)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refusing to publish the required context under any shared identity"),
        "the identity refusal must be explicit:\n{}",
        report(&output)
    );
    assert!(
        sandbox.check_runs().is_empty(),
        "no verdict may be published under the shared workflow identity"
    );
}

#[test]
fn a_commit_without_an_open_main_pull_request_publishes_the_not_evaluated_verdict() {
    // The announcer can be dispatched on any ref; a run whose commit heads
    // no open main pull request merges nowhere, so the run itself is not
    // an error. But an empty candidate list is not evidence of cleanliness
    // either — a read-after-write lag on the discovery endpoint, a
    // just-closed pull request, or a branch without a pull request all
    // land here — so the not-evaluated verdict is published red at the
    // announced commit, superseding any earlier verdict; opening or
    // retargeting a pull request headed there fires its own announcement,
    // which publishes the fresh verdict.
    let sandbox = GateSandbox::new("publish-nothing");
    sandbox.use_scenario("no-associated-pr");
    let output = sandbox.run("workflow_dispatch", Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "an announcement without a main pull request is not an error:\n{}",
        report(&output)
    );
    let check_run = sandbox.single_check_run();
    assert!(
        check_run.contains("name=Bot Review Gate")
            && check_run.contains(&format!("head_sha={HEAD_SHA}"))
            && check_run.contains("conclusion=failure"),
        "the not-evaluated verdict must supersede any earlier one at the announced head:\n{check_run}"
    );
}

#[test]
fn a_discovery_query_failure_publishes_the_superseding_failure_at_the_announced_head() {
    // When the API cannot say which pull requests the announcing commit
    // heads, no verdict can be honestly bound — but exiting without a
    // publication would NOT block the merge at a refresh: the latest
    // completed run under the required name decides, so the head's
    // previous verdict (including a green one predating a bot change
    // request submitted at this same head, or kept alive by this very
    // failure) would stand. The refresh therefore publishes the
    // superseding failure at the announced head before failing.
    let sandbox = GateSandbox::new("discovery-fails");
    sandbox.use_scenario("discovery-query-failure");
    let output = sandbox.run("pull_request", Some(HEAD_SHA));
    assert_blocked(&output, &[], "Associated-pull-request query failed");
    let check_run = sandbox.single_check_run();
    assert!(
        check_run.contains("name=Bot Review Gate")
            && check_run.contains(&format!("head_sha={HEAD_SHA}"))
            && check_run.contains("conclusion=failure"),
        "a discovery failure must supersede any earlier verdict at the announced head:\n{check_run}"
    );
}

#[test]
fn associated_pull_requests_sharing_one_head_get_one_shared_verdict() {
    // Check runs attach to commits, not pull requests: two open main pull
    // requests sharing one head commit would otherwise publish competing
    // verdicts under the one required context name, and the last-published
    // verdict would decide for both. The evaluations are recorded and
    // exactly one shared verdict is published — here both candidates are
    // clean, so the single shared verdict is green.
    let sandbox = GateSandbox::new("publish-shared-clean");
    sandbox.use_scenario("two-associated-prs");
    let output = sandbox.run("pull_request", Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "clean evidence across every associated pull request must pass:\n{}",
        report(&output)
    );
    let check_run = sandbox.single_check_run();
    assert!(
        check_run.contains("name=Bot Review Gate")
            && check_run.contains(&format!("head_sha={HEAD_SHA}"))
            && check_run.contains("conclusion=success"),
        "the one shared verdict must be the required context at the evaluated head:\n{check_run}"
    );
}

#[test]
fn one_dirty_associate_forces_the_shared_verdict_red() {
    // A clean sibling pull request sharing the head must not be able to
    // mask a dirty evaluation: with per-PR publications, the clean duplicate
    // evaluated last made the dirty pull request appear green. The shared
    // verdict fails if any candidate fails.
    let sandbox = GateSandbox::new("publish-shared-dirty");
    sandbox.use_scenario("two-associated-prs-dirty");
    let output = sandbox.run("pull_request", Some(HEAD_SHA));
    assert_blocked(
        &output,
        &[],
        "1 of 2 associated pull request(s) are blocked",
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no longer matches pull request head"),
        "the blocked candidate's reason must be reported:\n{}",
        report(&output)
    );
    let check_run = sandbox.single_check_run();
    assert!(
        check_run.contains("name=Bot Review Gate")
            && check_run.contains(&format!("head_sha={HEAD_SHA}"))
            && check_run.contains("conclusion=failure"),
        "the shared verdict must be red at the evaluated head despite the clean sibling:\n{check_run}"
    );
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
    // publication call — the one write the minted gate-publisher App token
    // covers.
    let workflow: serde_yaml::Value =
        serde_yaml::from_str(super::harness::BOT_REVIEW_GATE_PUBLISHER_YAML)
            .expect("bot review gate publisher workflow must parse");
    let steps = workflow["jobs"]["publish"]["steps"]
        .as_sequence()
        .expect("the publisher workflow must define its job steps");
    let run = steps
        .iter()
        .find_map(|step| step["run"].as_str())
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
fn every_refusal_records_before_returning_and_only_the_driver_publishes() {
    // The evaluation loop swallows `set -e`, so a refusal site that merely
    // recorded a verdict and fell through would continue evaluating — and
    // might still contribute a clean verdict afterwards. Every record call
    // site must therefore be followed by an explicit `return 1`, the
    // evaluator must never publish (it only records verdicts), and the
    // shared required context must be published exclusively by the driver:
    // the aggregated failure branch, the aggregated success branch, and
    // the three discovery-level supersession paths (discovery failure, no
    // candidate, all candidates skipped).
    let script = super::harness::gate_run_script();
    let lines: Vec<&str> = script.lines().collect();
    let mut call_sites = 0;
    for (index, line) in lines.iter().enumerate() {
        if line.contains("record_blocked \"${") {
            call_sites += 1;
            let next = lines[index + 1..]
                .iter()
                .find(|candidate| !candidate.trim().is_empty())
                .copied()
                .unwrap_or("");
            assert_eq!(
                next.trim(),
                "return 1",
                "a record_blocked call site must be followed by an explicit return: {line}"
            );
        }
    }
    assert!(
        call_sites >= 9,
        "every query, pagination, and head-binding refusal must record its verdict: {call_sites}"
    );
    let driver_marker = "One verdict per announced head";
    let driver_start = script
        .find(driver_marker)
        .expect("the driver must carry the shared-verdict marker");
    assert_eq!(
        script[..driver_start]
            .matches("publish_gate_check_run \"")
            .count(),
        0,
        "the evaluator must never publish: it records verdicts, the driver publishes"
    );
    let publications = script
        .matches("publish_gate_check_run \"${announcer_head}\"")
        .count();
    assert_eq!(
        publications, 5,
        "exactly the driver's five terminal paths may publish the shared verdict: \
         aggregated failure, aggregated success, discovery failure, no candidate, \
         all candidates skipped"
    );
}
