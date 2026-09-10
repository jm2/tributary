//! Shared harness for the bot review gate decision tests: the extracted
//! publisher script, a stubbed `gh` on PATH, and a throwaway sandbox per run.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub const BOT_REVIEW_GATE_YAML: &str = include_str!("../../.github/workflows/bot-review-gate.yml");
pub const BOT_REVIEW_GATE_PUBLISHER_YAML: &str =
    include_str!("../../.github/workflows/bot-review-gate-publisher.yml");
pub const GH_STUB_SCRIPT: &str = include_str!("../fixtures/bot_review_gate/gh_stub.sh");
pub const GATE_REPOSITORY: &str = "jm2/tributary";
pub const GATE_PR_NUMBER: &str = "42";
pub const HEAD_SHA: &str = "1111111111111111111111111111111111111111";
pub const OTHER_SHA: &str = "2222222222222222222222222222222222222222";

/// The publisher workflow's `EXPECTED_BOT_REVIEWERS` step env, mirrored here
/// so the extracted script runs under the same environment Actions gives it:
/// the trusted expected-reviewer set, fixed by enumeration on the default
/// branch. The policy authorizes exactly one review integration —
/// `CodeRabbit` — so out-of-band reviewers stay deliberately absent.
pub const EXPECTED_BOT_REVIEWERS: &str = "coderabbitai";

pub fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bot_review_gate")
}

/// The publisher's evaluation script: the bytes Actions executes from the
/// trusted default-branch workflow. The job's first step mints the
/// gate-publisher App token (an action); the tested script is the inline
/// publication step.
pub fn gate_run_script() -> String {
    let workflow: serde_yaml::Value = serde_yaml::from_str(BOT_REVIEW_GATE_PUBLISHER_YAML)
        .expect("bot review gate publisher workflow must parse");
    let steps = workflow["jobs"]["publish"]["steps"]
        .as_sequence()
        .expect("the publisher workflow must define its job steps");
    let run = steps
        .iter()
        .find_map(|step| step["run"].as_str())
        .expect("the publisher job must inline its run script");
    assert!(
        !run.trim().is_empty(),
        "the publisher run script must not be empty"
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

/// A throwaway directory holding the extracted publisher script, the `gh`
/// stub on PATH, and the copied fixture pages for one run.
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

    /// Runs the publisher with the announcing run's head commit.
    ///
    /// `event_name` records which announcer event produced the run (all
    /// announcer completions fire the publisher identically); the binding
    /// input is always `announcer_head_sha` — the announcing run's head, the
    /// only commit the publisher is willing to evaluate.
    pub fn run(&self, event_name: &str, announcer_head_sha: Option<&str>) -> Output {
        self.run_with_gate_token(event_name, announcer_head_sha, Some("stub-gate-token"))
    }

    /// Runs the publisher with an overridden `EXPECTED_BOT_REVIEWERS` value
    /// (the workflow pins it by enumeration; the override exists to exercise
    /// the set's own failure modes — an absent reviewer, an empty set).
    pub fn run_with_expected_reviewers(
        &self,
        event_name: &str,
        announcer_head_sha: Option<&str>,
        expected_reviewers: &str,
    ) -> Output {
        self.run_inner(
            event_name,
            announcer_head_sha,
            Some("stub-gate-token"),
            Some(expected_reviewers),
        )
    }

    /// Runs the publisher with the gate-publisher App identity withheld,
    /// as when the mint step failed: the script must refuse to publish the
    /// required context rather than degrade to the shared workflow token.
    pub fn run_without_gate_token(
        &self,
        event_name: &str,
        announcer_head_sha: Option<&str>,
    ) -> Output {
        self.run_with_gate_token(event_name, announcer_head_sha, None)
    }

    fn run_with_gate_token(
        &self,
        event_name: &str,
        announcer_head_sha: Option<&str>,
        gate_token: Option<&str>,
    ) -> Output {
        self.run_inner(event_name, announcer_head_sha, gate_token, None)
    }

    fn run_inner(
        &self,
        event_name: &str,
        announcer_head_sha: Option<&str>,
        gate_token: Option<&str>,
        expected_reviewers: Option<&str>,
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
            .env("EVENT_HEAD_SHA", announcer_head_sha.unwrap_or(""))
            // The completion event's announcing run id, as the workflow
            // passes it; the publisher falls back to the run record only
            // when the event head is missing, and a scenario without an
            // actions-run.json fixture answers that fallback with an
            // API-style failure.
            .env("EVENT_RUN_ID", "4711")
            .env("RUNNER_TEMP", self.root.join("runner-temp"))
            .env("GH_STUB_PAGES", self.root.join("pages"))
            .env("GH_STUB_STATE", self.root.join("state"))
            // The workflow applies this step env by enumeration; a test
            // override replaces it (including with an empty string, to
            // exercise the empty-set failure), the default mirrors the
            // workflow.
            .env(
                "EXPECTED_BOT_REVIEWERS",
                expected_reviewers.unwrap_or(EXPECTED_BOT_REVIEWERS),
            );
        if let Some(token) = gate_token {
            // The publication path must be authenticated as the
            // gate-publisher App; the script refuses to publish without it.
            command.env("GATE_TOKEN", token);
        }
        command
            .output()
            .expect("the publisher script must be runnable under bash")
    }

    /// Every check-run API call the publisher made during the run, in call
    /// order: one line per `CHECK-RUN target=... name=... head_sha=...
    /// status=... conclusion=...` record the stub logged. The opening POST
    /// targets `.../check-runs`, the terminal PATCH `.../check-runs/<id>`.
    pub fn check_runs(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("state/check-runs.log")).map_or_else(
            |_| Vec::new(),
            |log| log.lines().map(str::to_owned).collect(),
        )
    }

    /// The one shared verdict: asserts the refresh opened the required
    /// context as a single in-progress check-run at a head and finalized THAT
    /// run (the terminal PATCH targets the created run's id) with a
    /// conclusion, and returns the finalizing record for verdict assertions.
    pub fn opened_and_finalized_verdict(&self) -> String {
        let check_runs = self.check_runs();
        assert_eq!(
            check_runs.len(),
            2,
            "the refresh must open the context once and finalize that one run:\n{check_runs:?}"
        );
        let (open, finish) = (check_runs[0].clone(), check_runs[1].clone());
        assert!(
            open.contains("target=repos/jm2/tributary/check-runs ")
                && open.contains("status=in_progress")
                && !open.contains("conclusion="),
            "the refresh must open the required context as an in-progress run:\n{open}"
        );
        assert!(
            open.contains("name=Bot Review Gate")
                && finish.contains("target=repos/jm2/tributary/check-runs/1 ")
                && finish.contains("status=completed")
                && finish.contains("conclusion="),
            "the terminal update must finalize the run the refresh opened:\n{open}\n{finish}"
        );
        // The head field sits mid-line on the opening POST and end-of-line
        // on the terminal PATCH, so the extraction must stop at the next
        // field boundary instead of consuming the rest of the line.
        let head_of = |line: &str| -> String {
            line.split("head_sha=")
                .nth(1)
                .unwrap_or_default()
                .split(' ')
                .next()
                .unwrap_or_default()
                .to_owned()
        };
        assert_eq!(
            head_of(&open),
            head_of(&finish),
            "the opened and finalized runs must bind the same head:\n{open}\n{finish}"
        );
        finish
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

pub fn run_scenario(scenario: &str, event_name: &str, announcer_head_sha: Option<&str>) -> Output {
    let sandbox = GateSandbox::new(scenario);
    sandbox.use_scenario(scenario);
    sandbox.run(event_name, announcer_head_sha)
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
