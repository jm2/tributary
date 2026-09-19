//! Offline ACTIVE-phase resolution-evidence regressions for the read-only
//! rollout/preflight validator (`scripts/preflight_bot_review_gate.sh`):
//! conversation-resolution evidence must be read as structural boolean
//! data — only boolean `true` enforces, boolean `false` and an absent flag
//! are legitimate not-enforced observations, and malformed parameters or a
//! non-boolean flag fail closed instead of being upgraded into enforcement
//! or hidden by another valid rule. Split from `preflight.rs` to keep each
//! module within the repository's per-file size budget.

use crate::preflight_support::{
    resolution_flag_absent_ruleset, resolution_ruleset, stderr_of, stdout_of, validated_ruleset,
    Fixture, ENV_CUSTOM_MAIN, EXACT_MAIN_POLICY,
};

/// A full externally validated ACTIVE configuration whose single applicable
/// ruleset carries `ruleset`. Resolution-evidence regressions vary exactly
/// that one document.
fn active_resolution_fixture<'a>(fixture: &'a Fixture, ruleset: &str) -> &'a Fixture {
    fixture
        .file("activation.json", r#"{"value":"active"}"#)
        .file("app-id-variable.json", r#"{"value":"424242"}"#)
        .file("environment.json", ENV_CUSTOM_MAIN)
        .file(
            "environment-secrets.json",
            r#"{"total_count":2,"secrets":[{"name":"BOT_REVIEW_GATE_APP_ID"},{"name":"BOT_REVIEW_GATE_PRIVATE_KEY"}]}"#,
        )
        .file(
            "dependabot-secrets.json",
            r#"{"total_count":2,"secrets":[{"name":"RULESET_READER_APP_ID"},{"name":"RULESET_READER_APP_PRIVATE_KEY"}]}"#,
        )
        .file("branch-rules.json", r#"[{"ruleset_id":17650907}]"#)
        .file("deployment-branch-policies.json", EXACT_MAIN_POLICY)
        .file("rulesets/17650907.json", ruleset)
}

#[test]
fn active_non_boolean_resolution_evidence_fails_closed() {
    // Reproduced rejection: the reduction fed
    // required_review_thread_resolution to jq `all`, whose truthiness
    // promotes any non-null/non-false value — the string "false", 0, {}
    // and [] — into "enforced". A full otherwise-valid ACTIVE
    // configuration carrying any of those values must fail closed instead
    // of reporting the configuration consistent.
    for flag in [r#""false""#, "0", "{}", "[]"] {
        let fixture = Fixture::new("resolution-not-boolean");
        active_resolution_fixture(&fixture, &resolution_ruleset(424_242, flag));
        let output = fixture.run();
        assert!(
            !output.status.success(),
            "a {flag} resolution flag must fail the preflight closed:\n{}\n{}",
            stdout_of(&output),
            stderr_of(&output)
        );
        let stderr = stderr_of(&output);
        assert!(
            stderr.contains("PARSE-FAILED") && stderr.contains("required_review_thread_resolution"),
            "the non-boolean resolution evidence must be reported as malformed:\n{stderr}"
        );
    }
}

#[test]
fn active_malformed_resolution_evidence_is_not_hidden_by_another_valid_rule() {
    // One applicable ruleset carries genuine boolean true and the other the
    // demonstrated string "false". The malformed observation must fail the
    // validation closed even though a valid rule already enforces the
    // requirement — a wrong-typed flag is unreadable evidence, not a
    // not-enforced rule that another rule may outvote.
    let fixture = Fixture::new("resolution-malformed-hidden");
    active_resolution_fixture(&fixture, &validated_ruleset(424_242))
        .file(
            "branch-rules.json",
            r#"[{"ruleset_id":17650907},{"ruleset_id":2}]"#,
        )
        .file(
            "rulesets/2.json",
            &resolution_ruleset(424_242, r#""false""#),
        );
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "malformed resolution evidence must not be hidden by a valid rule:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("PARSE-FAILED") && stderr.contains("required_review_thread_resolution"),
        "the malformed ruleset must be reported:\n{stderr}"
    );
}

#[test]
fn active_boolean_false_resolution_is_legitimate_not_enforced_evidence() {
    // Control preserved from the pre-fix semantics: a genuine boolean false
    // is a complete observation ("not enforced"), never malformed; the
    // active phase reports the unmet requirement instead of a parse
    // failure.
    let fixture = Fixture::new("resolution-false-legitimate");
    active_resolution_fixture(&fixture, &resolution_ruleset(424_242, "false"));
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "the requirement is not enforced, so the ACTIVE configuration must fail:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains(
            "MISSING: no applicable main ruleset enforces require-conversation-resolution"
        ),
        "the unmet requirement must be reported:\n{stdout}"
    );
    assert!(
        !stderr_of(&output).contains("PARSE-FAILED"),
        "a boolean false must not be classified as malformed:\n{}",
        stderr_of(&output)
    );
}

#[test]
fn active_absent_resolution_flag_is_legitimate_not_enforced_evidence() {
    // Control: a pull_request rule whose parameters omit the flag entirely
    // is a genuine absence (GitHub's shape for "the requirement is unset"),
    // not a malformed observation; the active phase reports the requirement
    // unmet without a parse failure.
    let fixture = Fixture::new("resolution-absent-legitimate");
    active_resolution_fixture(&fixture, &resolution_flag_absent_ruleset(424_242));
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "an unset requirement is not enforced, so the ACTIVE configuration must fail:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains(
            "MISSING: no applicable main ruleset enforces require-conversation-resolution"
        ),
        "the unmet requirement must be reported:\n{stdout}"
    );
    assert!(
        !stderr_of(&output).contains("PARSE-FAILED"),
        "an absent flag must not be classified as malformed:\n{}",
        stderr_of(&output)
    );
}
