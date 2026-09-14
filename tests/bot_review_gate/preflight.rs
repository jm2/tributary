//! Tests for the read-only rollout/preflight validator
//! (`scripts/preflight_bot_review_gate.sh`).
//!
//! The validator is what an operator runs before trusting the staged gate; its
//! three phases (staged inactive, active-but-incomplete, externally validated)
//! and its strictly read-only posture are pinned here. The offline tests drive
//! recorded documents directly; the live tests put a stub `gh` on PATH so the
//! validator's own pagination, nested-failure, malformed-input, and
//! environment-policy reads are exercised end to end — a static success fixture
//! cannot prove that a failed nested read or a truncated inventory fails closed.

use std::os::unix::fs::PermissionsExt;
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

fn scratch_tree(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root().join(format!(
        "preflight-fixture-{}-{tag}-{serial}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("preflight scratch must be creatable");
    root
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        Self {
            root: scratch_tree(tag),
        }
    }

    fn file(&self, relative: &str, content: &str) -> &Self {
        write_file(&self.root, relative, content);
        self
    }

    fn run(&self) -> Output {
        let script = repository_root().join("scripts/preflight_bot_review_gate.sh");
        Command::new("bash")
            .current_dir(repository_root())
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

const PREFLIGHT_GH_STUB: &str = include_str!("../fixtures/bot_review_gate/preflight_gh_stub.sh");

/// A live-path fixture: the validator runs without `--offline`, `gh` resolves
/// to the stub, and every page the validator requests is served from `pages/`.
struct LiveFixture {
    root: PathBuf,
}

impl LiveFixture {
    fn new(tag: &str) -> Self {
        let root = scratch_tree(tag);
        for component in ["bin", "pages", "state"] {
            std::fs::create_dir_all(root.join(component))
                .expect("live fixture component must be creatable");
        }
        let stub = root.join("bin/gh");
        std::fs::write(&stub, PREFLIGHT_GH_STUB).expect("stub must be writable");
        let mut permissions = std::fs::metadata(&stub)
            .expect("stub must exist")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&stub, permissions).expect("stub must be executable");
        Self { root }
    }

    fn page(&self, relative: &str, content: &str) -> &Self {
        write_file(&self.root.join("pages"), relative, content);
        self
    }

    fn run(&self) -> Output {
        let script = repository_root().join("scripts/preflight_bot_review_gate.sh");
        let system_path = std::env::var("PATH").unwrap_or_default();
        let path = format!("{}:{system_path}", self.root.join("bin").display());
        Command::new("bash")
            .current_dir(repository_root())
            .arg(&script)
            .env("PATH", path)
            .env("GH_TOKEN", "stub-token")
            .env("GITHUB_REPOSITORY", "jm2/tributary")
            .env("GH_STUB_PAGES", self.root.join("pages"))
            .env("GH_STUB_STATE", self.root.join("state"))
            .output()
            .expect("the preflight validator must run under bash")
    }
}

impl Drop for LiveFixture {
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

const GATE_RULESET: &str = r#"{"rules":[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"Bot Review Gate","integration_id":424242}]}}]}"#;

const EXACT_MAIN_POLICY: &str =
    r#"{"total_count":1,"branch_policies":[{"id":1,"name":"main","type":"branch"}]}"#;

const ENV_CUSTOM_MAIN: &str = r#"{"deployment_branch_policy":{"custom_branch_policies":true,"protected_branches":false},"protection_rules":[]}"#;

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

// ── Live-path adversarial tests (stub `gh`) ─────────────────────────────────

/// Page slugs the live stub resolves: owner/repo stripped and `/` -> `_`.
const ACTIVATION_PAGE: &str = "actions_variables_BOT_REVIEW_GATE_ACTIVATION.json";
const APP_ID_PAGE: &str = "actions_variables_BOT_REVIEW_GATE_APP_ID.json";
const ENV_PAGE: &str = "environments_bot-review-gate-publisher.json";
const ENV_SECRETS_PAGE: &str = "environments_bot-review-gate-publisher_secrets.json";
const DDB_SECRETS_PAGE: &str = "dependabot_secrets.json";
const DEPLOYMENT_POLICIES_PAGE: &str =
    "environments_bot-review-gate-publisher_deployment-branch-policies.page.1.json";

fn valid_active_pages(fixture: &LiveFixture) -> &LiveFixture {
    fixture
        .page(ACTIVATION_PAGE, r#"{"value":"active"}"#)
        .page(APP_ID_PAGE, r#"{"value":"424242"}"#)
        .page(ENV_PAGE, ENV_CUSTOM_MAIN)
        .page(
            ENV_SECRETS_PAGE,
            r#"{"total_count":2,"secrets":[{"name":"BOT_REVIEW_GATE_APP_ID"},{"name":"BOT_REVIEW_GATE_PRIVATE_KEY"}]}"#,
        )
        .page(
            DDB_SECRETS_PAGE,
            r#"{"total_count":2,"secrets":[{"name":"RULESET_READER_APP_ID"},{"name":"RULESET_READER_APP_PRIVATE_KEY"}]}"#,
        )
        .page(
            "rules_branches_main.page.1.json",
            r#"[{"ruleset_id":17650907}]"#,
        )
        .page("rulesets_17650907.json", &validated_ruleset(424_242))
        .page(DEPLOYMENT_POLICIES_PAGE, EXACT_MAIN_POLICY)
}

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
