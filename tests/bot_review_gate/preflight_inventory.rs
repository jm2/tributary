//! Offline regression tests for malformed inventory records in the read-only
//! rollout/preflight validator (`scripts/preflight_bot_review_gate.sh`): the
//! validator must classify every branch-rules reference and every ruleset
//! rule entry BEFORE any recognized-shape filtering, so a malformed record
//! fails the observation closed instead of being silently dropped into an
//! empty successful inventory. Unknown but well-formed rule types stay valid.
//! The phase-level behavior lives in `preflight.rs`; the live stubbed-`gh`
//! variants of these regressions live in `preflight_live.rs`.

use crate::preflight_support::{stderr_of, stdout_of, Fixture};

#[test]
fn offline_null_branch_rules_record_fails_closed() {
    // A null record in the branch-rules inventory must be reported as a
    // malformed record, not silently filtered into an empty inventory that
    // reads as "no rulesets apply".
    let fixture = Fixture::new("branch-rules-null-record");
    fixture.file("branch-rules.json", r"[null]");
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a null branch-rules record must fail closed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("malformed branch-rules record (entry-not-object)"),
        "the null branch-rules record must be reported:\n{}",
        stderr_of(&output)
    );
}

#[test]
fn offline_missing_ruleset_id_fails_closed() {
    // A branch-rules record without a ruleset id cannot name a ruleset whose
    // absence of the gate could be verified; it is an incomplete observation.
    let fixture = Fixture::new("branch-rules-id-missing");
    fixture.file(
        "branch-rules.json",
        r#"[{"ruleset_source":"jm2/tributary"}]"#,
    );
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a branch-rules record without a ruleset id must fail closed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("malformed branch-rules record (ruleset-id-missing)"),
        "the id-less branch-rules record must be reported:\n{}",
        stderr_of(&output)
    );
}

#[test]
fn offline_string_ruleset_id_fails_closed() {
    // GitHub reports numeric ruleset ids; a string-typed id is a malformed
    // reference. Dropping it would hide the referenced ruleset from the
    // inventory and let a staged-inactive preflight report success without
    // ever establishing that the gate is absent.
    let fixture = Fixture::new("branch-rules-string-id");
    fixture.file("branch-rules.json", r#"[{"ruleset_id":"17650907"}]"#);
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a string-typed ruleset id must fail closed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("malformed branch-rules record (ruleset-id-not-number)"),
        "the string-typed ruleset id must be reported:\n{}",
        stderr_of(&output)
    );
}

#[test]
fn offline_null_ruleset_rule_fails_closed() {
    // A null entry in a ruleset's rules array would be silently dropped by the
    // recognized-type selectors; the inventory cannot describe its own rules,
    // so the detail must fail closed instead of reading as gate-absent.
    let fixture = Fixture::new("rules-null-entry");
    fixture
        .file("branch-rules.json", r#"[{"ruleset_id":17650907}]"#)
        .file("rulesets/17650907.json", r#"{"rules":[null]}"#);
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a null rules entry must fail closed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("PARSE-FAILED")
            && stderr_of(&output).contains("malformed .rules entry"),
        "the null rules entry must be reported:\n{}",
        stderr_of(&output)
    );
}

#[test]
fn offline_ruleset_rule_without_type_fails_closed() {
    // A rule entry whose parameters carry a required_status_checks list but
    // which lacks the `type` discriminator is invisible to the recognized-type
    // selectors; it must be rejected as a malformed entry, never filtered.
    let fixture = Fixture::new("rule-type-missing");
    fixture
        .file("branch-rules.json", r#"[{"ruleset_id":17650907}]"#)
        .file(
            "rulesets/17650907.json",
            r#"{"rules":[{"parameters":{"required_status_checks":[{"context":"Bot Review Gate","integration_id":424242}]}}]}"#,
        );
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "a typeless rules entry must fail closed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("PARSE-FAILED")
            && stderr_of(&output).contains("malformed .rules entry"),
        "the typeless rules entry must be reported:\n{}",
        stderr_of(&output)
    );
}

#[test]
fn offline_unknown_rule_type_remains_valid() {
    // Control: a rule entry with a valid (unrecognized) type discriminator is
    // a well-formed record for a rule type this validator does not interpret;
    // it stays valid and the staged-inactive preflight remains consistent.
    let fixture = Fixture::new("rule-type-unknown");
    fixture
        .file("branch-rules.json", r#"[{"ruleset_id":17650907}]"#)
        .file(
            "rulesets/17650907.json",
            r#"{"rules":[{"type":"scalars","parameters":{"ref":"main"}},{"type":"pull_request","parameters":{"required_review_thread_resolution":true}}]}"#,
        );
    let output = fixture.run();
    assert!(
        output.status.success(),
        "an unknown but well-formed rule type must stay valid:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(stdout_of(&output).contains("configuration is consistent"));
}
