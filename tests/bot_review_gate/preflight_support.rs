//! Shared support for the read-only rollout/preflight validator tests
//! (`scripts/preflight_bot_review_gate.sh`): an offline fixture tree, a live
//! `gh`-stub fixture, and the recorded policy documents both drive the
//! validator with.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn scratch_root() -> PathBuf {
    if let Ok(target_tmp) = std::env::var("CARGO_TARGET_TMPDIR") {
        return PathBuf::from(target_tmp);
    }
    repository_root().join("target/scratch")
}

pub fn write_file(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture directory must be creatable");
    }
    std::fs::write(&path, content)
        .unwrap_or_else(|error| panic!("fixture {} must be writable: {error}", path.display()));
}

pub fn scratch_tree(tag: &str) -> PathBuf {
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

pub struct Fixture {
    pub root: PathBuf,
}

impl Fixture {
    pub fn new(tag: &str) -> Self {
        Self {
            root: scratch_tree(tag),
        }
    }

    pub fn file(&self, relative: &str, content: &str) -> &Self {
        write_file(&self.root, relative, content);
        self
    }

    pub fn run(&self) -> Output {
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

pub const PREFLIGHT_GH_STUB: &str =
    include_str!("../fixtures/bot_review_gate/preflight_gh_stub.sh");

/// A live-path fixture: the validator runs without `--offline`, `gh` resolves
/// to the stub, and every page the validator requests is served from `pages/`.
pub struct LiveFixture {
    pub root: PathBuf,
}

impl LiveFixture {
    pub fn new(tag: &str) -> Self {
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

    pub fn page(&self, relative: &str, content: &str) -> &Self {
        write_file(&self.root.join("pages"), relative, content);
        self
    }

    pub fn run(&self) -> Output {
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

pub fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

pub const NARROW_RULESET: &str = r#"{"rules":[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"Security Audit","integration_id":15368}]}}]}"#;

pub const GATE_RULESET: &str = r#"{"rules":[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"Bot Review Gate","integration_id":424242}]}}]}"#;

pub const EXACT_MAIN_POLICY: &str =
    r#"{"total_count":1,"branch_policies":[{"id":1,"name":"main","type":"branch"}]}"#;

pub const ENV_CUSTOM_MAIN: &str = r#"{"deployment_branch_policy":{"custom_branch_policies":true,"protected_branches":false},"protection_rules":[]}"#;

pub fn validated_ruleset(app_id: u64) -> String {
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

/// Page slugs the live stub resolves: owner/repo stripped and `/` -> `_`.
pub const ACTIVATION_PAGE: &str = "actions_variables_BOT_REVIEW_GATE_ACTIVATION.json";
pub const APP_ID_PAGE: &str = "actions_variables_BOT_REVIEW_GATE_APP_ID.json";
pub const ENV_PAGE: &str = "environments_bot-review-gate-publisher.json";
pub const ENV_SECRETS_PAGE: &str = "environments_bot-review-gate-publisher_secrets.json";
pub const DDB_SECRETS_PAGE: &str = "dependabot_secrets.json";
pub const DEPLOYMENT_POLICIES_PAGE: &str =
    "environments_bot-review-gate-publisher_deployment-branch-policies.page.1.json";

pub fn valid_active_pages(fixture: &LiveFixture) -> &LiveFixture {
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
