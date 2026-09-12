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
    let check_run = sandbox.opened_and_finalized_verdict();
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
    let check_run = sandbox.opened_and_finalized_verdict();
    assert!(
        check_run.contains(&format!("head_sha={HEAD_SHA}"))
            && check_run.contains("conclusion=failure"),
        "the refusal must be published at the evaluated head, never at the moved-to head:\n{check_run}"
    );
}

#[test]
fn check_run_publication_failure_fails_the_publisher() {
    // If the required context cannot even be OPENED, nothing was superseded
    // and nothing may be finalized: the head's previous verdict keeps
    // standing — which blocks the merge — and the publisher run itself must
    // say so instead of passing silently.
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
        "a failed opening must not leave a check-run behind"
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
    let check_run = sandbox.opened_and_finalized_verdict();
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
    let check_run = sandbox.opened_and_finalized_verdict();
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
    let check_run = sandbox.opened_and_finalized_verdict();
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
    let check_run = sandbox.opened_and_finalized_verdict();
    assert!(
        check_run.contains("name=Bot Review Gate")
            && check_run.contains(&format!("head_sha={HEAD_SHA}"))
            && check_run.contains("conclusion=failure"),
        "the shared verdict must be red at the evaluated head despite the clean sibling:\n{check_run}"
    );
}

#[test]
fn discovery_pagination_reaches_the_candidate_on_later_pages() {
    // The association endpoint PAGINATES, and dropping later pages silently
    // shrinks the candidate set: this scenario's page 1 is a full
    // 100-record page of unrelated closed pull requests, and the open main
    // pull request headed by the announced commit sits on page 2. A
    // publisher that read only the first page would find no candidate and
    // publish the not-evaluated red; walking every page finds the candidate
    // and publishes its real verdict.
    let sandbox = GateSandbox::new("discovery-pagination-boundary");
    sandbox.use_scenario("discovery-pagination-boundary");
    let output = sandbox.run("pull_request", Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "the paginated discovery must find and cleanly evaluate the page-2 candidate:\n{}",
        report(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("clean at {HEAD_SHA}")),
        "the page-2 candidate must receive the fresh green verdict:\n{}",
        report(&output)
    );
    let check_run = sandbox.opened_and_finalized_verdict();
    assert!(
        check_run.contains("name=Bot Review Gate")
            && check_run.contains(&format!("head_sha={HEAD_SHA}"))
            && check_run.contains("conclusion=success"),
        "the shared verdict must be green at the evaluated head:\n{check_run}"
    );
}

#[test]
fn the_refresh_opens_the_context_in_progress_before_any_evaluation() {
    // The verdict is not a completed-only final POST: the refresh must
    // OPEN the required context as an in-progress App-authored check run
    // at the announced head before any discovery or evaluation, so a
    // pending required check immediately supersedes the previous verdict
    // and keeps the merge blocked for the whole refresh — an old green
    // verdict must never stay mergeable while evidence is being re-read.
    let sandbox = GateSandbox::new("publish-opens-in-progress");
    sandbox.use_scenario("clean");
    let invocations_before = std::fs::read_to_string(sandbox.root.join("state/invocations.log"));
    let output = sandbox.run("pull_request", Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "the clean scenario must pass:\n{}",
        report(&output)
    );
    let open = sandbox
        .check_runs()
        .first()
        .expect("the refresh must open the context")
        .clone();
    assert!(
        open.contains("target=repos/jm2/tributary/check-runs ")
            && open.contains("status=in_progress")
            && !open.contains("conclusion="),
        "the context must be opened as an in-progress run, not posted completed:\n{open}"
    );
    // The open happens before every evidence query: no GraphQL invocation
    // may precede the check-run POST.
    let invocations =
        std::fs::read_to_string(sandbox.root.join("state/invocations.log")).unwrap_or_default();
    let before = invocations_before.map_or(0, |s| s.lines().count());
    let all: Vec<&str> = invocations.lines().skip(before).collect();
    let open_position = all
        .iter()
        .position(|line| line.contains("check-runs"))
        .expect("the opening POST must be logged");
    assert!(
        all[..open_position]
            .iter()
            .all(|line| !line.contains("graphql")),
        "no evidence query may run before the in-progress context is opened:\n{all:?}"
    );
}

#[test]
fn a_dead_finalization_leaves_the_opened_pending_run_standing() {
    // If the terminal update fails, the opened in-progress run must stay
    // standing — a blocked merge and the failed job as the re-run signal —
    // instead of resurrecting the previous verdict. The run log must hold
    // the opening POST and no completed update.
    let sandbox = GateSandbox::new("publish-dead-finalization");
    sandbox.use_scenario("checkrun-finalization-failure");
    let output = sandbox.run("pull_request", Some(HEAD_SHA));
    assert!(
        !output.status.success(),
        "a failed finalization must fail the publisher run:\n{}",
        report(&output)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Check-run publication failed"),
        "the finalization failure must be explained:\n{}",
        report(&output)
    );
    let check_runs = sandbox.check_runs();
    assert_eq!(
        check_runs.len(),
        1,
        "only the opening POST may be logged when the final update dies:\n{check_runs:?}"
    );
    assert!(
        check_runs[0].contains("status=in_progress") && !check_runs[0].contains("conclusion="),
        "the stranded run must be the pending refresh check:\n{}",
        check_runs[0]
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
    // Within the publisher, the required context name is bound exactly
    // once, and both the opening POST and the terminal PATCH carry it via
    // that single binding — the one write the minted gate-publisher App
    // token covers.
    let run = super::harness::gate_run_script();
    let occurrences = run.matches("\"Bot Review Gate\"").count();
    assert_eq!(
        occurrences, 1,
        "the required context name must be bound exactly once, in the check-run publication call"
    );
    assert!(
        run.contains("gate_context=\"Bot Review Gate\""),
        "the context name must live in one shared binding the open and the finalize both use"
    );
    assert!(
        run.contains("-F name=\"${gate_context}\""),
        "the published context must be created by the check-run API call"
    );
}

#[test]
fn every_refusal_records_before_returning_and_only_the_driver_opens_and_finalizes() {
    let script = super::harness::gate_run_script();
    assert_every_refusal_records_before_returning(&script);
    assert_only_the_driver_opens_and_finalizes(&script);
}

// The evaluation loop swallows `set -e`, so a refusal site that merely
// recorded a verdict and fell through would continue evaluating — and
// might still contribute a clean verdict afterwards. Every record call
// site must therefore be followed by an explicit `return 1`.
fn assert_every_refusal_records_before_returning(script: &str) {
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
}

// The check run itself is touched only by the driver: exactly one
// in-progress start, and exactly the five terminal finalizations
// (aggregated failure, aggregated success, and the three discovery-level
// supersession paths).
fn assert_only_the_driver_opens_and_finalizes(script: &str) {
    let starts = script
        .matches("start_gate_check_run \"${announcer_head}\"")
        .count();
    assert_eq!(
        starts, 1,
        "the driver must open the in-progress context exactly once per refresh"
    );
    let driver_marker = "One verdict per announced head";
    let driver_start = script
        .find(driver_marker)
        .expect("the driver must carry the shared-verdict marker");
    for publication in ["start_gate_check_run", "finish_gate_check_run"] {
        assert_eq!(
            script[..driver_start].matches(publication).count(),
            0,
            "the evaluator must never touch the check run: only the driver opens and finalizes it"
        );
    }
    let finalizations = script
        .matches("finish_gate_check_run \"${announcer_head}\"")
        .count();
    assert_eq!(
        finalizations, 5,
        "exactly the driver's five terminal paths may finalize the shared verdict: \
         aggregated failure, aggregated success, discovery failure, no candidate, \
         all candidates skipped"
    );
}
