//! The operator rate-limit substitution contract: when a documented,
//! repo-owned policy waives a listed reviewer's stale evidence, and every
//! way the waiver refuses to apply.

use super::harness::{assert_blocked, report, run_scenario, GateSandbox, HEAD_SHA};

#[test]
fn documented_rate_limit_waives_stale_evidence_of_the_listed_reviewer() {
    // The operator's conditional substitution: with the rate limit
    // documented in the repo-owned policy file, the substitute reviewer's
    // APPROVED review at the exact evaluated head, every thread resolved,
    // and no outstanding change request, the listed reviewer's stale
    // evidence no longer blocks — and the waiver is reported.
    let output = run_scenario(
        "rate-limit-substitution-granted",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert!(
        output.status.success(),
        "the documented substitution must waive the listed reviewer's stale evidence:\n{}",
        report(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("RATE-LIMIT SUBSTITUTION") && stdout.contains("coderabbitai[bot]"),
        "the granted waiver must be reported for auditability:\n{}",
        report(&output)
    );
}

#[test]
fn substitution_requires_an_affirmative_substitute_outcome() {
    // A comment-only review by the substitute reviewer is never an
    // approval: the stale evidence of the rate-limited reviewer stays
    // blocking.
    let output = run_scenario(
        "rate-limit-substitution-denied-commented",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert_blocked(
        &output,
        &["STALE BOT REVIEW EVIDENCE", "coderabbitai[bot]"],
        "not clean",
    );
}

#[test]
fn substitution_binds_the_substitute_approval_to_the_evaluated_head() {
    // An approval at any commit other than the evaluated head is exactly
    // the stale evidence the gate exists to refuse.
    let output = run_scenario(
        "rate-limit-substitution-denied-old-approval",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert_blocked(
        &output,
        &["STALE BOT REVIEW EVIDENCE", "coderabbitai[bot]"],
        "not clean",
    );
}

#[test]
fn substitution_requires_every_thread_resolved() {
    // The waiver covers stale evidence only; an unresolved review thread —
    // outdated included — blocks regardless of the substitution.
    let output = run_scenario(
        "rate-limit-substitution-denied-thread",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert_blocked(
        &output,
        &["UNRESOLVED BOT REVIEW THREAD", "u/rl-open-thread"],
        "not clean",
    );
}

#[test]
fn substitution_requires_no_outstanding_change_request_from_any_author() {
    // A human change request is preserved: the substitution never papers
    // over an outstanding conclusion from any reviewer.
    let output = run_scenario(
        "rate-limit-substitution-denied-human-cr",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert_blocked(
        &output,
        &["STALE BOT REVIEW EVIDENCE", "coderabbitai[bot]"],
        "not clean",
    );
}

#[test]
fn substitution_without_a_readable_policy_file_stays_disabled() {
    // A repository without a policy file — or one whose policy read fails —
    // gets no waiver: the substitution is an opt-in relaxation whose
    // failure mode is "no waiver", never "gate passes".
    let sandbox = GateSandbox::new("substitution-no-policy");
    sandbox.use_scenario("rate-limit-substitution-granted");
    std::fs::remove_file(sandbox.root.join("pages/contents.json"))
        .expect("the policy fixture must be removable");
    let output = sandbox.run("pull_request", Some(HEAD_SHA));
    assert_blocked(
        &output,
        &["STALE BOT REVIEW EVIDENCE", "coderabbitai[bot]"],
        "not clean",
    );
}

#[test]
fn gate_demands_evidence_from_a_listed_reviewer_with_no_reviews() {
    // A policy-listed reviewer that was rate-limited before submitting any
    // review produces no review group to derive a violation from, so a gate
    // that only iterates existing reviews would pass with no substitute
    // approval at all — the exact bypass the substitution contract forbids.
    // The absence of a required reviewer is itself stale evidence: the gate
    // synthesizes the missing stale-evidence violation and blocks.
    let output = run_scenario(
        "rate-limit-substitution-denied-missing-reviewer",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert_blocked(
        &output,
        &[
            "STALE BOT REVIEW EVIDENCE",
            "coderabbitai[bot]",
            "no review submitted",
            "u/rate-limit-evidence",
        ],
        "not clean",
    );
}

#[test]
fn documented_rate_limit_waives_a_listed_reviewers_missing_evidence() {
    // The synthesized missing-evidence violation is waivable exactly like
    // derived stale evidence: with the substitution eligible (substitute
    // approval at the exact evaluated head, every thread resolved, no
    // outstanding change request from any author), the unavailable listed
    // reviewer no longer blocks — and the waiver is reported.
    let output = run_scenario(
        "rate-limit-substitution-granted-missing-reviewer",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert!(
        output.status.success(),
        "an eligible substitution must waive a listed reviewer's missing evidence:\n{}",
        report(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("RATE-LIMIT SUBSTITUTION") && stdout.contains("coderabbitai[bot]"),
        "the granted waiver must be reported for auditability:\n{}",
        report(&output)
    );
}

#[test]
fn substitution_waiver_matching_is_login_normalized_on_both_sides() {
    // The policy file is operator-authored and may spell a login with
    // different case or without GitHub's "[bot]" suffix, while the review
    // list carries the API spelling — here the policy lists "CoderabbitAI"
    // and the API surfaces "coderabbitai[bot]". The waiver must match the
    // two spellings as one identity on BOTH sides: the synthesized absence
    // violations and the derived stale-evidence violation of the same
    // reviewer must be waivable through the same normalized matching, or
    // the same reviewer would be blocking or waivable depending on which
    // check happened to see the absence first.
    let output = run_scenario(
        "rate-limit-substitution-granted-normalized-policy",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert!(
        output.status.success(),
        "the normalized policy must waive the listed reviewer's stale evidence:\n{}",
        report(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("RATE-LIMIT SUBSTITUTION")
            && stdout.contains(&format!("clean at {HEAD_SHA}")),
        "the waived result must disclose the substitution and pass:\n{}",
        report(&output)
    );
}
