//! Offline phase tests for the read-only rollout/preflight validator
//! (`scripts/preflight_bot_review_gate.sh`): its three phases (staged inactive,
//! active-but-incomplete, externally validated), its strictly read-only
//! posture, and its fail-closed handling of a malformed manifest, driven
//! directly from recorded documents. The malformed branch-rules and ruleset
//! rule entry regressions live in `preflight_inventory.rs`; the live
//! stubbed-`gh` reads live in `preflight_live.rs`.

use std::process::Command;

use crate::preflight_support::{
    repository_root, resolution_flag_absent_ruleset, resolution_ruleset, stderr_of, stdout_of,
    validated_ruleset, Fixture, ENV_CUSTOM_MAIN, EXACT_MAIN_POLICY, NARROW_RULESET,
};

// ── Offline phase tests ─────────────────────────────────────────────────────

#[test]
fn staged_inactive_without_the_gate_requirement_is_consistent() {
    let fixture = Fixture::new("inactive-consistent");
    fixture
        .file("branch-rules.json", r#"[{"ruleset_id":17650907}]"#)
        .file("rulesets/17650907.json", NARROW_RULESET);
    let output = fixture.run();
    assert!(
        output.status.success(),
        "a consistent staged-inactive preflight must pass:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(stdout.contains("STAGED INACTIVE"), "{stdout}");
    assert!(
        stdout.contains("does not require the App-bound"),
        "the inactive report must state the context is not required:\n{stdout}"
    );
}

#[test]
fn staged_inactive_with_a_required_app_context_is_incompatible() {
    let fixture = Fixture::new("inactive-required");
    fixture
        .file("branch-rules.json", r#"[{"ruleset_id":17650907}]"#)
        .file(
            "rulesets/17650907.json",
            r#"{"rules":[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"Bot Review Gate","integration_id":424242}]}}]}"#,
        );
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a required App context while inactive must fail the preflight"
    );
    let stdout = stdout_of(&output);
    assert!(stdout.contains("INCOMPATIBLE"), "{stdout}");
    assert!(
        stdout.contains("blocked"),
        "the report must explain that the merge is blocked:\n{stdout}"
    );
}

#[test]
fn active_but_incomplete_configuration_lists_every_missing_prerequisite() {
    let fixture = Fixture::new("active-incomplete");
    fixture
        .file("activation.json", r#"{"value":"active"}"#)
        .file("branch-rules.json", "[]");
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "an active but incomplete configuration must fail closed"
    );
    let stdout = stdout_of(&output);
    assert!(stdout.contains("Phase: ACTIVE"), "{stdout}");
    assert!(
        stdout.contains("MISSING: protected environment"),
        "the missing environment must be reported:\n{stdout}"
    );
    assert!(
        stdout.contains("MISSING: repository variable BOT_REVIEW_GATE_APP_ID"),
        "the missing App-id variable must be reported:\n{stdout}"
    );
    assert!(
        stdout.contains("MISSING: environment secret 'BOT_REVIEW_GATE_APP_ID'"),
        "missing environment secret names must be reported:\n{stdout}"
    );
    assert!(
        stdout.contains("require-conversation-resolution"),
        "the missing native conversation-resolution rule must be reported:\n{stdout}"
    );
}

#[test]
fn externally_validated_configuration_passes() {
    let fixture = Fixture::new("active-validated");
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
        .file("rulesets/17650907.json", &validated_ruleset(424_242));
    let output = fixture.run();
    assert!(
        output.status.success(),
        "a fully validated configuration must pass:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(stdout.contains("Phase: ACTIVE"), "{stdout}");
    assert!(
        stdout.contains("configuration is consistent"),
        "the validated result must be explicit:\n{stdout}"
    );
}

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

#[test]
fn unrecognized_activation_value_is_incompatible() {
    let fixture = Fixture::new("activation-garbage");
    fixture
        .file("activation.json", r#"{"value":"yes-please"}"#)
        .file("branch-rules.json", "[]");
    let output = fixture.run();
    assert!(!output.status.success());
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("unrecognized value"),
        "the invalid activation value must be named:\n{stdout}"
    );
}

#[test]
fn offline_missing_ruleset_rules_fails_closed() {
    // A referenced ruleset detail that omits `.rules` is an incomplete
    // structural observation, not a ruleset with no required contexts.
    let fixture = Fixture::new("ruleset-rules-missing");
    fixture
        .file("branch-rules.json", r#"[{"ruleset_id":17650907}]"#)
        .file("rulesets/17650907.json", "{}");
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a ruleset detail with missing .rules must fail closed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("PARSE-FAILED"),
        "the incomplete ruleset detail must be reported:\n{}",
        stderr_of(&output)
    );
}

#[test]
fn offline_null_ruleset_rules_fails_closed() {
    // `{"rules":null}` is also incomplete: GitHub never reports a null rules
    // array, so it must not read as an empty one.
    let fixture = Fixture::new("ruleset-rules-null");
    fixture
        .file("branch-rules.json", r#"[{"ruleset_id":17650907}]"#)
        .file("rulesets/17650907.json", r#"{"rules":null}"#);
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
fn offline_empty_ruleset_rules_remains_valid() {
    // A genuine empty array is a valid observation: the ruleset exists and
    // applies no rules. This is the control that keeps the fix from rejecting
    // every ruleset without required status checks.
    let fixture = Fixture::new("ruleset-rules-empty");
    fixture
        .file("branch-rules.json", r#"[{"ruleset_id":17650907}]"#)
        .file("rulesets/17650907.json", r#"{"rules":[]}"#);
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
fn offline_missing_nested_required_status_checks_fails_closed() {
    // A `required_status_checks` rule whose `parameters` omit the check list
    // must fail closed: an incomplete rule is never proof that the Bot Review
    // Gate is not required.
    let fixture = Fixture::new("nested-checks-missing");
    fixture
        .file("branch-rules.json", r#"[{"ruleset_id":17650907}]"#)
        .file(
            "rulesets/17650907.json",
            r#"{"rules":[{"type":"required_status_checks","parameters":{}}]}"#,
        );
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a required_status_checks rule without a check list must fail closed:\n{}\n{}",
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
fn offline_null_nested_required_status_checks_fails_closed() {
    // `required_status_checks: null` is incomplete: GitHub never reports a null
    // check list, so it must not read as an empty one.
    let fixture = Fixture::new("nested-checks-null");
    fixture
        .file("branch-rules.json", r#"[{"ruleset_id":17650907}]"#)
        .file(
            "rulesets/17650907.json",
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
fn offline_wrong_type_nested_required_status_checks_fails_closed() {
    // An object (or any non-array) check list is malformed and must not be
    // coerced into an absent requirement.
    let fixture = Fixture::new("nested-checks-object");
    fixture
        .file("branch-rules.json", r#"[{"ruleset_id":17650907}]"#)
        .file(
            "rulesets/17650907.json",
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
fn offline_malformed_check_entry_fails_closed() {
    // A check list entry that is not an object cannot bind a context; it is
    // rejected rather than dropped from the inventory.
    let fixture = Fixture::new("nested-check-entry-scalar");
    fixture
        .file("branch-rules.json", r#"[{"ruleset_id":17650907}]"#)
        .file(
            "rulesets/17650907.json",
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
fn offline_check_entry_without_context_fails_closed() {
    // An object check entry without a non-empty `context` is malformed.
    let fixture = Fixture::new("nested-check-entry-no-context");
    fixture
        .file("branch-rules.json", r#"[{"ruleset_id":17650907}]"#)
        .file(
            "rulesets/17650907.json",
            r#"{"rules":[{"type":"required_status_checks","parameters":{"required_status_checks":[{"integration_id":1}]}}]}"#,
        );
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a check entry without a context must fail closed:\n{}\n{}",
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
fn offline_empty_nested_required_status_checks_remains_valid() {
    // Control: a genuine empty check list is a complete observation and the
    // staged-inactive preflight stays consistent.
    let fixture = Fixture::new("nested-checks-empty");
    fixture
        .file("branch-rules.json", r#"[{"ruleset_id":17650907}]"#)
        .file(
            "rulesets/17650907.json",
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
fn offline_unrelated_rule_type_without_checks_remains_valid() {
    // Control: a ruleset with no required_status_checks rule (only an unrelated
    // rule type) is not malformed and must keep passing.
    let fixture = Fixture::new("unrelated-rule-only");
    fixture
        .file("branch-rules.json", r#"[{"ruleset_id":17650907}]"#)
        .file(
            "rulesets/17650907.json",
            r#"{"rules":[{"type":"pull_request","parameters":{"required_review_thread_resolution":true}}]}"#,
        );
    let output = fixture.run();
    assert!(
        output.status.success(),
        "an unrelated rule type must stay valid:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(stdout_of(&output).contains("configuration is consistent"));
}

#[test]
fn offline_missing_referenced_ruleset_record_fails_closed() {
    // The applicable branch rules name a ruleset whose recorded detail is
    // absent; the inventory is incomplete and cannot be validated as consistent.
    let fixture = Fixture::new("referenced-record-missing");
    fixture.file("branch-rules.json", r#"[{"ruleset_id":17650907}]"#);
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "an absent referenced ruleset record must fail closed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("missing referenced ruleset record"),
        "the absent referenced record must be reported:\n{}",
        stderr_of(&output)
    );
}

#[test]
fn validator_is_strictly_read_only() {
    let script =
        std::fs::read_to_string(repository_root().join("scripts/preflight_bot_review_gate.sh"))
            .expect("the preflight validator must be readable");
    for forbidden in [
        "-X POST",
        "-X PATCH",
        "-X PUT",
        "-X DELETE",
        "--method POST",
        "--method PATCH",
        "--method PUT",
        "--method DELETE",
        "gh api -X",
        "gh secret set",
        "gh variable set",
        "gh api --method",
    ] {
        assert!(
            !script.contains(forbidden),
            "the preflight validator must never mutate settings ({forbidden})"
        );
    }
    // Secrets are named, never valued: the validator must not read a secret
    // value endpoint (GitHub's secret APIs only ever return metadata anyway).
    assert!(
        !script.contains("secret-value") && !script.contains("/actions/secrets/"),
        "the validator must read secret names, never values"
    );
}

#[test]
fn malformed_manifest_is_rejected() {
    // An unreadable manifest is a configuration error, never a silently empty
    // set of prerequisites.
    let fixture = Fixture::new("bad-manifest");
    fixture.file("branch-rules.json", "[]");
    let manifest = fixture.root.join("bad-manifest.json");
    std::fs::write(&manifest, "{not valid json").expect("bad manifest must be writable");
    let script = repository_root().join("scripts/preflight_bot_review_gate.sh");
    let output = Command::new("bash")
        .current_dir(repository_root())
        .arg(&script)
        .arg("--manifest")
        .arg(&manifest)
        .arg("--offline")
        .arg(&fixture.root)
        .output()
        .expect("the preflight validator must run under bash");
    assert!(
        !output.status.success(),
        "a malformed manifest must be rejected:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("MANIFEST-INVALID"),
        "the malformed manifest must be reported:\n{}",
        stderr_of(&output)
    );
}
