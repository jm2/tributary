//! Shared harness for the bot review gate decision tests: the extracted
//! workflow script, a stubbed `gh` on PATH, and a throwaway sandbox per run.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub const BOT_REVIEW_GATE_YAML: &str = include_str!("../../.github/workflows/bot-review-gate.yml");
pub const GH_STUB_SCRIPT: &str = include_str!("../fixtures/bot_review_gate/gh_stub.sh");
pub const GATE_REPOSITORY: &str = "jm2/tributary";
pub const GATE_PR_NUMBER: &str = "42";
pub const HEAD_SHA: &str = "1111111111111111111111111111111111111111";
pub const OTHER_SHA: &str = "2222222222222222222222222222222222222222";

pub fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bot_review_gate")
}

pub fn gate_run_script() -> String {
    let workflow: serde_yaml::Value =
        serde_yaml::from_str(BOT_REVIEW_GATE_YAML).expect("bot review gate workflow must parse");
    let steps = workflow["jobs"]["bot-review-gate"]["steps"]
        .as_sequence()
        .expect("the gate workflow must define its job steps");
    let run = steps
        .first()
        .expect("the gate job must define its evaluation step")["run"]
        .as_str()
        .expect("the gate evaluation step must inline its run script");
    assert!(
        !run.trim().is_empty(),
        "the gate run script must not be empty"
    );
    run.to_owned()
}

pub fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).unwrap_or_else(|error| {
        panic!(
            "fixture destination {} must be creatable: {error}",
            destination.display()
        )
    });
    for entry in std::fs::read_dir(source).unwrap_or_else(|error| {
        panic!("fixture dir {} must be readable: {error}", source.display())
    }) {
        let entry = entry.expect("fixture entry must be readable");
        let target = destination.join(entry.file_name());
        if entry
            .file_type()
            .expect("fixture entry type must be readable")
            .is_dir()
        {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap_or_else(|error| {
                panic!(
                    "fixture {} must be copyable: {error}",
                    entry.path().display()
                )
            });
        }
    }
}

/// A throwaway directory holding the extracted gate script, the `gh` stub on
/// PATH, and the copied fixture pages for one run.
pub struct GateSandbox {
    pub root: PathBuf,
}

/// Crate-owned scratch root for sandbox trees.
///
/// Deriving scratch space from the system temp directory is flagged by
/// Codacy's "`temp_dir` should not be used for security operations" advisory,
/// and the zero-new-issues policy makes that advisory blocking. The gate
/// fixtures never need a system-wide temp directory — every staged byte
/// derives from in-repo fixtures — so the sandbox lives under the cargo
/// target tree instead: it stays gitignored, co-located with the build, and
/// removable by `cargo clean`.
fn scratch_root() -> PathBuf {
    if let Ok(target_tmp) = std::env::var("CARGO_TARGET_TMPDIR") {
        return PathBuf::from(target_tmp);
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/scratch")
}

impl GateSandbox {
    pub fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = scratch_root().join(format!(
            "bot-review-gate-fixture-{}-{tag}-{serial}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        for component in ["bin", "pages", "runner-temp", "state"] {
            std::fs::create_dir_all(root.join(component))
                .unwrap_or_else(|error| panic!("sandbox {component} must be creatable: {error}"));
        }
        std::fs::write(root.join("bin/gh"), GH_STUB_SCRIPT).expect("gh stub must be writable");
        let stub = root.join("bin/gh");
        let mut permissions = std::fs::metadata(&stub)
            .expect("gh stub must exist")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&stub, permissions).expect("gh stub must be executable");

        Self { root }
    }

    pub fn use_scenario(&self, scenario: &str) {
        let source = fixtures_root().join(scenario);
        assert!(
            source.is_dir(),
            "scenario {scenario} must have fixtures under {}",
            fixtures_root().display()
        );
        copy_tree(&source, &self.root.join("pages"));
    }

    pub fn run(&self, event_name: &str, event_head_sha: Option<&str>) -> Output {
        self.run_with_github_sha(event_name, event_head_sha, None)
    }

    /// Runs the gate with an explicit `GITHUB_SHA`, the runner-injected tip
    /// of the ref an event (notably `workflow_dispatch`) executed against.
    pub fn run_with_github_sha(
        &self,
        event_name: &str,
        event_head_sha: Option<&str>,
        github_sha: Option<&str>,
    ) -> Output {
        let script_path = self.root.join("gate.sh");
        std::fs::write(&script_path, gate_run_script()).expect("gate script must be writable");

        let system_path = std::env::var("PATH").unwrap_or_default();
        let path = format!("{}:{system_path}", self.root.join("bin").display());

        let mut command = Command::new("bash");
        command
            .arg(&script_path)
            .env("PATH", path)
            .env("GH_TOKEN", "stub-token")
            .env("GITHUB_REPOSITORY", GATE_REPOSITORY)
            .env("GITHUB_EVENT_NAME", event_name)
            .env("PR_NUMBER", GATE_PR_NUMBER)
            .env("EVENT_HEAD_SHA", event_head_sha.unwrap_or(""))
            .env("RUNNER_TEMP", self.root.join("runner-temp"))
            .env("GH_STUB_PAGES", self.root.join("pages"))
            .env("GH_STUB_STATE", self.root.join("state"));
        if let Some(sha) = github_sha {
            command.env("GITHUB_SHA", sha);
        }
        command
            .output()
            .expect("the gate script must be runnable under bash")
    }
}

impl Drop for GateSandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub fn report(output: &Output) -> String {
    format!(
        "exit: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

pub fn run_scenario(scenario: &str, event_name: &str, event_head_sha: Option<&str>) -> Output {
    let sandbox = GateSandbox::new(scenario);
    sandbox.use_scenario(scenario);
    sandbox.run(event_name, event_head_sha)
}

pub fn assert_blocked(output: &Output, expected_stdout: &[&str], expected_stderr: &str) {
    assert!(
        !output.status.success(),
        "the gate must block, but it passed:\n{}",
        report(output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for fragment in expected_stdout {
        assert!(
            stdout.contains(fragment),
            "the gate report must contain {fragment}:\n{}",
            report(output)
        );
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected_stderr),
        "the gate failure must explain itself with {expected_stderr}:\n{}",
        report(output)
    );
}
