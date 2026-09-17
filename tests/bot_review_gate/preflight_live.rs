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
fn live_missing_ruleset_rules_fails_closed() {
    // A readable ruleset detail that omits `.rules` is an incomplete
    // observation. Staged-inactive validation must not read it as "no required
    // contexts" and declare the configuration consistent.
    let fixture = LiveFixture::new("live-rules-missing");
    fixture
        .page(
            "rules_branches_main.page.1.json",
            r#"[{"ruleset_id":17650907}]"#,
        )
        .page("rulesets_17650907.json", "{}");
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a ruleset detail without .rules must fail closed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("PARSE-FAILED"),
        "the incomplete detail must be reported:\n{}",
        stderr_of(&output)
    );
}

#[test]
fn live_null_ruleset_rules_fails_closed() {
    let fixture = LiveFixture::new("live-rules-null");
    fixture
        .page(
            "rules_branches_main.page.1.json",
            r#"[{"ruleset_id":17650907}]"#,
        )
        .page("rulesets_17650907.json", r#"{"rules":null}"#);
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a ruleset detail with null .rules must fail closed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("PARSE-FAILED"),
        "the null rules array must be reported:\n{}",
        stderr_of(&output)
    );
}

#[test]
fn live_empty_ruleset_rules_remains_valid() {
    // The valid-empty control: a genuine empty array still passes the
    // staged-inactive preflight, so the missing/null rejection is targeted at
    // incompleteness, not at rulesets without required checks.
    let fixture = LiveFixture::new("live-rules-empty");
    fixture
        .page(
            "rules_branches_main.page.1.json",
            r#"[{"ruleset_id":17650907}]"#,
        )
        .page("rulesets_17650907.json", r#"{"rules":[]}"#);
    let output = fixture.run();
    assert!(
        output.status.success(),
        "a genuine empty .rules array must stay valid:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(stdout_of(&output).contains("configuration is consistent"));
}

#[test]
fn live_missing_nested_required_status_checks_fails_closed() {
    // The reproduced defect: a required_status_checks rule whose parameters
    // omit the nested list was coerced to `[]`, so the staged-inactive
    // preflight declared the configuration consistent. It must fail closed.
    let fixture = LiveFixture::new("live-nested-checks-missing");
    fixture
        .page(
            "rules_branches_main.page.1.json",
            r#"[{"ruleset_id":17650907}]"#,
        )
        .page(
            "rulesets_17650907.json",
            r#"{"rules":[{"type":"required_status_checks","parameters":{}}]}"#,
        );
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a missing nested check list must fail closed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("PARSE-FAILED"),
        "the incomplete nested inventory must be reported:\n{}",
        stderr_of(&output)
    );
}

#[test]
fn live_null_nested_required_status_checks_fails_closed() {
    let fixture = LiveFixture::new("live-nested-checks-null");
    fixture
        .page(
            "rules_branches_main.page.1.json",
            r#"[{"ruleset_id":17650907}]"#,
        )
        .page(
            "rulesets_17650907.json",
            r#"{"rules":[{"type":"required_status_checks","parameters":{"required_status_checks":null}}]}"#,
        );
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a null nested check list must fail closed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("PARSE-FAILED"),
        "the null nested check list must be reported:\n{}",
        stderr_of(&output)
    );
}

#[test]
fn live_wrong_type_nested_required_status_checks_fails_closed() {
    let fixture = LiveFixture::new("live-nested-checks-object");
    fixture
        .page(
            "rules_branches_main.page.1.json",
            r#"[{"ruleset_id":17650907}]"#,
        )
        .page(
            "rulesets_17650907.json",
            r#"{"rules":[{"type":"required_status_checks","parameters":{"required_status_checks":{}}}]}"#,
        );
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a non-array nested check list must fail closed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("PARSE-FAILED"),
        "the non-array nested check list must be reported:\n{}",
        stderr_of(&output)
    );
}

#[test]
fn live_malformed_check_entry_fails_closed() {
    let fixture = LiveFixture::new("live-nested-check-entry-scalar");
    fixture
        .page(
            "rules_branches_main.page.1.json",
            r#"[{"ruleset_id":17650907}]"#,
        )
        .page(
            "rulesets_17650907.json",
            r#"{"rules":[{"type":"required_status_checks","parameters":{"required_status_checks":[42]}}]}"#,
        );
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a scalar check entry must fail closed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("PARSE-FAILED"),
        "the malformed check entry must be reported:\n{}",
        stderr_of(&output)
    );
}

#[test]
fn live_empty_nested_required_status_checks_remains_valid() {
    // Control: a genuine empty nested check list still passes, so the fix is
    // targeted at incompleteness and malformed entries, not at rulesets
    // without required checks.
    let fixture = LiveFixture::new("live-nested-checks-empty");
    fixture
        .page(
            "rules_branches_main.page.1.json",
            r#"[{"ruleset_id":17650907}]"#,
        )
        .page(
            "rulesets_17650907.json",
            r#"{"rules":[{"type":"required_status_checks","parameters":{"required_status_checks":[]}}]}"#,
        );
    let output = fixture.run();
    assert!(
        output.status.success(),
        "a genuine empty nested check list must stay valid:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(stdout_of(&output).contains("configuration is consistent"));
}

#[test]
fn live_null_branch_rules_record_fails_closed() {
    // A null record served on a live branch-rules page must be reported as a
    // malformed record; filtering it would leave an empty inventory that reads
    // as "no rulesets apply to main".
    let fixture = LiveFixture::new("live-branch-rules-null-record");
    fixture.page("rules_branches_main.page.1.json", r"[null]");
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a null branch-rules record must fail closed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("PARSE-FAILED")
            && stderr.contains("malformed branch-rules record (entry-not-object)"),
        "the null branch-rules record must be reported:\n{stderr}"
    );
}

#[test]
fn live_string_ruleset_id_fails_closed() {
    // The reproduced defect shape: a string-typed ruleset id on the live
    // branch-rules page was silently filtered out, so no detail was fetched
    // and the staged-inactive preflight reported success. It must be reported
    // as a malformed reference instead.
    let fixture = LiveFixture::new("live-branch-rules-string-id");
    fixture.page(
        "rules_branches_main.page.1.json",
        r#"[{"ruleset_id":"17650907"}]"#,
    );
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a string-typed ruleset id must fail closed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("PARSE-FAILED")
            && stderr.contains("malformed branch-rules record (ruleset-id-not-number)"),
        "the string-typed ruleset id must be reported:\n{stderr}"
    );
}

#[test]
fn live_null_ruleset_rule_fails_closed() {
    // A live ruleset detail whose rules array carries a null entry must fail
    // closed: the entry cannot be classified, so the gate's absence cannot be
    // established from this observation.
    let fixture = LiveFixture::new("live-rules-null-entry");
    fixture
        .page(
            "rules_branches_main.page.1.json",
            r#"[{"ruleset_id":17650907}]"#,
        )
        .page("rulesets_17650907.json", r#"{"rules":[null]}"#);
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a null live rules entry must fail closed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("PARSE-FAILED") && stderr.contains("malformed .rules entry"),
        "the null rules entry must be reported:\n{stderr}"
    );
}

#[test]
fn live_ruleset_rule_without_type_fails_closed() {
    // A live rule entry carrying a required_status_checks list without the
    // `type` discriminator was invisible to the recognized-type selectors; it
    // must be rejected as a malformed entry.
    let fixture = LiveFixture::new("live-rule-type-missing");
    fixture
        .page(
            "rules_branches_main.page.1.json",
            r#"[{"ruleset_id":17650907}]"#,
        )
        .page(
            "rulesets_17650907.json",
            r#"{"rules":[{"parameters":{"required_status_checks":[{"context":"Bot Review Gate","integration_id":424242}]}}]}"#,
        );
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a typeless live rules entry must fail closed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("PARSE-FAILED") && stderr.contains("malformed .rules entry"),
        "the typeless rules entry must be reported:\n{stderr}"
    );
}

#[test]
fn live_unknown_rule_type_remains_valid() {
    // Control: a live rule entry with a valid (unrecognized) type stays a
    // well-formed record and the staged-inactive preflight stays consistent.
    let fixture = LiveFixture::new("live-rule-type-unknown");
    fixture
        .page(
            "rules_branches_main.page.1.json",
            r#"[{"ruleset_id":17650907}]"#,
        )
        .page(
            "rulesets_17650907.json",
            r#"{"rules":[{"type":"scalars","parameters":{"ref":"main"}}]}"#,
        );
    let output = fixture.run();
    assert!(
        output.status.success(),
        "an unknown but well-formed live rule type must stay valid:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(stdout_of(&output).contains("configuration is consistent"));
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
