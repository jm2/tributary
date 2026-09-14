//! Live-path adversarial tests for the read-only rollout/preflight validator
//! (`scripts/preflight_bot_review_gate.sh`). A stub `gh` on PATH lets the
//! validator's own pagination, nested-failure, malformed-input, and
//! environment-policy reads run end to end — a static success fixture cannot
//! prove that a failed nested read or a truncated inventory fails closed.

use crate::preflight_support::{
    stderr_of, stdout_of, valid_active_pages, LiveFixture, ACTIVATION_PAGE,
    DEPLOYMENT_POLICIES_PAGE, ENV_PAGE, ENV_SECRETS_PAGE, GATE_RULESET, NARROW_RULESET,
};

// ── Live-path adversarial tests (stub `gh`) ─────────────────────────────────

#[test]
fn live_exact_main_active_configuration_passes() {
    let fixture = LiveFixture::new("live-active-validated");
    valid_active_pages(&fixture);
    let output = fixture.run();
    assert!(
        output.status.success(),
        "a fully validated live configuration must pass:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(stdout.contains("Phase: ACTIVE"), "{stdout}");
    assert!(stdout.contains("configuration is consistent"), "{stdout}");
}

#[test]
fn live_pagination_reads_a_required_gate_on_a_later_branch_rule_page() {
    // Page one names an unrelated ruleset; the required (never-published) gate
    // lives on page two. A validator that read only the first page would call
    // this staged-inactive configuration consistent.
    let fixture = LiveFixture::new("live-branch-rule-pagination");
    fixture
        .page(
            "rules_branches_main.page.1.json",
            r#"[{"ruleset_id":111111}]"#,
        )
        .page("rulesets_111111.json", NARROW_RULESET)
        .page(
            "rules_branches_main.page.2.json",
            r#"[{"ruleset_id":17650907}]"#,
        )
        .page("rulesets_17650907.json", GATE_RULESET);
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a required App context on a later page must fail the staged-inactive preflight:\n{}",
        stdout_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(stdout.contains("INCOMPATIBLE"), "{stdout}");
    assert!(stdout.contains("blocked"), "{stdout}");
}

#[test]
fn live_referenced_ruleset_that_cannot_be_read_fails_closed() {
    // The branch rules name a ruleset whose detail read returns 404. That is an
    // incomplete observation, not proof that no required gate applies.
    let fixture = LiveFixture::new("live-referenced-404");
    fixture.page(
        "rules_branches_main.page.1.json",
        r#"[{"ruleset_id":17650907}]"#,
    );
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "an unreadable referenced ruleset must fail the preflight:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("READ-FAILED") && stderr.contains("referenced ruleset 17650907"),
        "the nested ruleset read failure must be reported:\n{stderr}"
    );
}

#[test]
fn live_nested_ruleset_read_error_fails_closed() {
    let fixture = LiveFixture::new("live-nested-read-error");
    fixture
        .page(
            "rules_branches_main.page.1.json",
            r#"[{"ruleset_id":17650907}]"#,
        )
        .page("fail", "error:rulesets_17650907\n");
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a non-404 nested ruleset read error must fail the preflight:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("READ-FAILED"),
        "the read failure must be reported:\n{}",
        stderr_of(&output)
    );
}

#[test]
fn live_malformed_ruleset_detail_fails_closed() {
    let fixture = LiveFixture::new("live-malformed-ruleset");
    fixture
        .page(
            "rules_branches_main.page.1.json",
            r#"[{"ruleset_id":17650907}]"#,
        )
        .page("rulesets_17650907.json", "{not valid json");
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a malformed ruleset detail must fail the preflight:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("PARSE-FAILED"),
        "the malformed document must be reported:\n{}",
        stderr_of(&output)
    );
}

#[test]
fn live_malformed_activation_document_fails_closed() {
    // A present-but-unparseable activation document must not read as unset.
    let fixture = LiveFixture::new("live-malformed-activation");
    fixture
        .page(ACTIVATION_PAGE, "{not valid json")
        .page("rules_branches_main.page.1.json", "[]");
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a malformed activation document must fail the preflight:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("PARSE-FAILED"),
        "the malformed activation document must be reported:\n{}",
        stderr_of(&output)
    );
}

#[test]
fn live_reviewer_gated_environment_is_incompatible() {
    let fixture = LiveFixture::new("live-reviewer-gated");
    valid_active_pages(&fixture);
    fixture.page(
        ENV_PAGE,
        r#"{"deployment_branch_policy":{"custom_branch_policies":true,"protected_branches":false},"protection_rules":[{"type":"required_reviewers","reviewers":[{"type":"User","reviewer":{"login":"operator"}}]}]}"#,
    );
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a required-reviewer environment must be incompatible with the unattended publisher:\n{}",
        stdout_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(stdout.contains("INCOMPATIBLE"), "{stdout}");
    assert!(stdout.contains("requires reviewers"), "{stdout}");
}

#[test]
fn live_additional_deployment_branch_is_incompatible() {
    let fixture = LiveFixture::new("live-extra-branch");
    valid_active_pages(&fixture);
    fixture.page(
        DEPLOYMENT_POLICIES_PAGE,
        r#"{"total_count":2,"branch_policies":[{"id":1,"name":"main","type":"branch"},{"id":2,"name":"release/*","type":"branch"}]}"#,
    );
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a non-exact-main deployment policy must be incompatible:\n{}",
        stdout_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(stdout.contains("INCOMPATIBLE"), "{stdout}");
    assert!(stdout.contains("deployment branch policies"), "{stdout}");
}

#[test]
fn live_incomplete_secret_inventory_fails_closed() {
    // The listing declares two secrets but the inventory terminates after one.
    let fixture = LiveFixture::new("live-incomplete-secrets");
    valid_active_pages(&fixture);
    fixture.page(
        ENV_SECRETS_PAGE,
        r#"{"total_count":2,"secrets":[{"name":"BOT_REVIEW_GATE_APP_ID"}]}"#,
    );
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "an inventory short of its declared total must fail closed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("incomplete inventory"),
        "the incomplete inventory must be reported:\n{}",
        stderr_of(&output)
    );
}
