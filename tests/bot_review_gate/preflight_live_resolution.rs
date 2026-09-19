//! Live-path resolution-evidence regressions for the read-only
//! rollout/preflight validator (`scripts/preflight_bot_review_gate.sh`):
//! the stubbed-`gh` end-to-end counterparts of the offline cases in
//! `preflight_resolution.rs`. Conversation-resolution evidence must be
//! read as structural boolean data through the live reads too — a forged
//! non-boolean flag must fail the preflight closed, while a genuine
//! boolean false stays a legitimate not-enforced observation. Split from
//! `preflight_live.rs` to keep each module within the repository's
//! per-file size budget.

use crate::preflight_support::{
    resolution_ruleset, stderr_of, stdout_of, valid_active_pages, LiveFixture,
};

#[test]
fn live_non_boolean_resolution_evidence_fails_closed() {
    // Live repro of the reproduced rejection: the readiness read fed
    // required_review_thread_resolution to jq `all`, whose truthiness
    // promotes the string "false", 0, {} and [] into "enforced". A full
    // otherwise-valid live ACTIVE configuration carrying any of those
    // values must fail closed through the stubbed reads too.
    for flag in [r#""false""#, "0", "{}", "[]"] {
        let fixture = LiveFixture::new("live-resolution-not-boolean");
        valid_active_pages(&fixture);
        fixture.page("rulesets_17650907.json", &resolution_ruleset(424_242, flag));
        let output = fixture.run();
        assert!(
            !output.status.success(),
            "a {flag} resolution flag must fail the live preflight closed:\n{}\n{}",
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
fn live_boolean_false_resolution_reports_missing_enforcement() {
    // Control preserved from the pre-fix semantics: a genuine boolean false
    // is a complete not-enforced observation, never malformed; the live
    // active phase reports the unmet requirement without a parse failure.
    let fixture = LiveFixture::new("live-resolution-false-off");
    valid_active_pages(&fixture);
    fixture.page(
        "rulesets_17650907.json",
        &resolution_ruleset(424_242, "false"),
    );
    let output = fixture.run();
    assert!(
        !output.status.success(),
        "the requirement is not enforced, so the live ACTIVE configuration must fail:\n{}\n{}",
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
