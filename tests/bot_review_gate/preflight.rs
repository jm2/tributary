//! Tests for the read-only rollout/preflight validator
//! (`scripts/preflight_bot_review_gate.sh`).
//!
//! The validator is what an operator runs before trusting the staged gate; its
//! three phases (staged inactive, active-but-incomplete, externally validated)
//! and its strictly read-only posture are pinned here against offline fixture
//! directories, so the behaviour is deterministic and needs no network.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn scratch_root() -> PathBuf {
    if let Ok(target_tmp) = std::env::var("CARGO_TARGET_TMPDIR") {
        return PathBuf::from(target_tmp);
    }
    repository_root().join("target/scratch")
}

fn write_file(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture directory must be creatable");
    }
    std::fs::write(&path, content)
        .unwrap_or_else(|error| panic!("fixture {} must be writable: {error}", path.display()));
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = scratch_root().join(format!(
            "preflight-fixture-{}-{tag}-{serial}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("preflight scratch must be creatable");
        Self { root }
    }

    fn file(&self, relative: &str, content: &str) -> &Self {
        write_file(&self.root, relative, content);
        self
    }

    fn run(&self) -> Output {
        let script = repository_root().join("scripts/preflight_bot_review_gate.sh");
        Command::new("bash")
            .arg(&script)
            .arg("--offline")
            .arg(&self.root)
            .output()
            .expect("the preflight validator must run under bash")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

const NARROW_RULESET: &str = r#"{"rules":[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"Security Audit","integration_id":15368}]}}]}"#;

fn validated_ruleset(app_id: u64) -> String {
    let checks: [(&str, Option<u64>); 18] = [
        ("Security Audit", Some(15368)),
        ("Linux (x86_64)", Some(15368)),
        ("Linux (aarch64)", Some(15368)),
        ("macOS (aarch64)", Some(15368)),
        ("Windows (x86_64)", Some(15368)),
        ("Windows (aarch64)", Some(15368)),
        ("Flatpak (Linux)", Some(15368)),
        ("MSRV", Some(15368)),
        ("Coverage (Linux x86_64)", Some(15368)),
        ("Desktop Metadata", Some(15368)),
        ("SHA256 Checksums", Some(15368)),
        ("Bot Review Gate", Some(app_id)),
        ("CodeQL", Some(57789)),
        ("Analyze (python)", Some(57789)),
        ("Analyze (rust)", Some(57789)),
        ("Analyze (actions)", Some(57789)),
        ("Codacy Static Code Analysis", Some(56611)),
        ("CodeRabbit", None),
    ];
    let items: Vec<String> = checks
        .iter()
        .map(|(context, integration)| {
            integration.as_ref().map_or_else(
                || format!(r#"{{"context":"{context}"}}"#),
                |id| format!(r#"{{"context":"{context}","integration_id":{id}}}"#),
            )
        })
        .collect();
    format!(
        r#"{{"rules":[{{"type":"required_status_checks","parameters":{{"required_status_checks":[{}]}}}},{{"type":"pull_request","parameters":{{"required_review_thread_resolution":true}}}}]}}"#,
        items.join(",")
    )
}

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
        .file(
            "environment.json",
            r#"{"deployment_branch_policy":{"protected_branches":true,"custom_branch_policies":false}}"#,
        )
        .file(
            "environment-secrets.json",
            r#"{"total_count":2,"secrets":[{"name":"BOT_REVIEW_GATE_APP_ID"},{"name":"BOT_REVIEW_GATE_PRIVATE_KEY"}]}"#,
        )
        .file(
            "dependabot-secrets.json",
            r#"{"total_count":2,"secrets":[{"name":"RULESET_READER_APP_ID"},{"name":"RULESET_READER_APP_PRIVATE_KEY"}]}"#,
        )
        .file("branch-rules.json", r#"[{"ruleset_id":17650907}]"#)
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
