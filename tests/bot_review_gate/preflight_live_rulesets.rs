//! Live ruleset-detail and rules-record validation tests for the read-only
//! rollout/preflight validator (`scripts/preflight_bot_review_gate.sh`). A
//! stub `gh` on PATH serves recorded branch-rules and ruleset pages so the
//! validator's own malformed-input and incompleteness handling runs end to
//! end: a ruleset detail that cannot be read, omits `.rules`, or carries
//! malformed/nested-check records must fail closed, and only genuine
//! valid-empty observations may stay consistent. The inventory read-resolution
//! and environment-policy variants live in `preflight_live.rs` and the
//! resolution-evidence variants in `preflight_live_resolution.rs`.

use crate::preflight_support::{stderr_of, stdout_of, LiveFixture};

// ── Live ruleset-detail validation (stub `gh`) ──────────────────────────────

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
