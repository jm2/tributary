//! Fixture-driven decision tests for the bot review gate
//! (`.github/workflows/bot-review-gate.yml`).
//!
//! The gate is the machine-readable merge evidence for the repository's
//! all-green policy, and its audit required the decisions themselves — not
//! just the workflow text — to be tested. These tests extract the run script
//! from the workflow YAML (so the tested bytes are the bytes Actions
//! executes), run it under bash with a stub `gh` that serves recorded API
//! pages from `tests/fixtures/bot_review_gate/<scenario>/`, and assert the
//! pass/fail decision and the reported reason codes for each policy
//! situation: unresolved threads (outdated included), outstanding change
//! requests, stale review evidence, pagination completeness, head binding,
//! and the documented refresh paths.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BOT_REVIEW_GATE_YAML: &str = include_str!("../.github/workflows/bot-review-gate.yml");
const GH_STUB_SCRIPT: &str = include_str!("fixtures/bot_review_gate/gh_stub.sh");
const GATE_REPOSITORY: &str = "jm2/tributary";
const GATE_PR_NUMBER: &str = "42";
const HEAD_SHA: &str = "1111111111111111111111111111111111111111";
const OTHER_SHA: &str = "2222222222222222222222222222222222222222";

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bot_review_gate")
}

fn gate_run_script() -> String {
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

fn copy_tree(source: &Path, destination: &Path) {
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
struct GateSandbox {
    root: PathBuf,
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
    fn new(tag: &str) -> Self {
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

    fn use_scenario(&self, scenario: &str) {
        let source = fixtures_root().join(scenario);
        assert!(
            source.is_dir(),
            "scenario {scenario} must have fixtures under {}",
            fixtures_root().display()
        );
        copy_tree(&source, &self.root.join("pages"));
    }

    fn run(&self, event_name: &str, event_head_sha: Option<&str>) -> Output {
        self.run_with_github_sha(event_name, event_head_sha, None)
    }

    /// Runs the gate with an explicit `GITHUB_SHA`, the runner-injected tip
    /// of the ref an event (notably `workflow_dispatch`) executed against.
    fn run_with_github_sha(
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

fn report(output: &Output) -> String {
    format!(
        "exit: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn run_scenario(scenario: &str, event_name: &str, event_head_sha: Option<&str>) -> Output {
    let sandbox = GateSandbox::new(scenario);
    sandbox.use_scenario(scenario);
    sandbox.run(event_name, event_head_sha)
}

fn assert_blocked(output: &Output, expected_stdout: &[&str], expected_stderr: &str) {
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

#[test]
fn clean_evidence_at_the_evaluated_head_passes() {
    let output = run_scenario("clean", "pull_request", Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "resolved threads plus a current bot approval must pass:\n{}",
        report(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("clean at {HEAD_SHA}")),
        "the published result must be bound to the exact evaluated head:\n{}",
        report(&output)
    );
    assert!(
        stdout.contains("review threads evaluated: 2") && stdout.contains("reviews evaluated: 2"),
        "the published result must report the paginated evidence it evaluated:\n{}",
        report(&output)
    );
}

#[test]
fn unresolved_current_bot_thread_blocks() {
    let output = run_scenario("unresolved-current-thread", "pull_request", Some(HEAD_SHA));
    assert_blocked(
        &output,
        &["UNRESOLVED BOT REVIEW THREAD", "u/current-thread"],
        "not clean",
    );
}

#[test]
fn unresolved_outdated_bot_thread_still_blocks() {
    // The audit's P1b finding: filtering unresolved threads by
    // `isOutdated == false` dropped findings whenever their diff location
    // moved. Movement is not resolution, so an outdated unresolved thread
    // must still block, with its outdatedness disclosed.
    let output = run_scenario("unresolved-outdated-thread", "pull_request", Some(HEAD_SHA));
    assert_blocked(
        &output,
        &[
            "UNRESOLVED BOT REVIEW THREAD",
            "outdated",
            "u/outdated-thread",
        ],
        "not clean",
    );
}

#[test]
fn human_threads_stay_outside_the_bot_evidence_gate() {
    let output = run_scenario("human-thread-only", "pull_request", Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "only bot-started threads are the gate's scope:\n{}",
        report(&output)
    );
}

#[test]
fn outstanding_bot_change_request_blocks_without_a_thread() {
    // The audit's P1a finding: a bot can request changes without opening an
    // inline thread, and a threads-only query cannot see it.
    let output = run_scenario("outstanding-change-request", "pull_request", Some(HEAD_SHA));
    assert_blocked(
        &output,
        &["OUTSTANDING BOT CHANGE REQUEST", "u/cr-review"],
        "not clean",
    );
}

#[test]
fn superseding_bot_approval_clears_a_change_request() {
    let output = run_scenario("change-request-superseded", "pull_request", Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "the author's latest review decides: a later current-head approval supersedes the change request:\n{}",
        report(&output)
    );
}

#[test]
fn dismissing_a_bot_change_request_clears_it() {
    let output = run_scenario("change-request-dismissed", "pull_request", Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "a formally dismissed review leaves no outstanding conclusion:\n{}",
        report(&output)
    );
}

#[test]
fn comment_after_a_dismissal_is_still_bound_to_the_head() {
    // A formal dismissal clears the dismissed conclusion, but it exempts
    // only itself from the head binding: a review submitted after the
    // dismissal is ordinary evidence again, so a comment left at a
    // since-superseded head must report stale evidence instead of passing.
    let output = run_scenario(
        "change-request-dismissed-then-stale-comment",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert_blocked(
        &output,
        &["STALE BOT REVIEW EVIDENCE", OTHER_SHA, HEAD_SHA],
        "not clean",
    );
}

#[test]
fn current_comment_after_a_dismissal_passes() {
    let output = run_scenario(
        "change-request-dismissed-then-current-comment",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert!(
        output.status.success(),
        "a dismissal followed by a current-head review provides current evidence:\n{}",
        report(&output)
    );
}

#[test]
fn comment_only_bot_review_does_not_clear_a_change_request() {
    // GitHub clears Request-changes only on a later approval or a formal
    // dismissal. A comment-only review carries no conclusion, so a bot
    // commenting at the evaluated head must not lift its outstanding
    // change request — ranking every review state together would let the
    // newest comment silence the block.
    let output = run_scenario(
        "change-request-then-commented",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert_blocked(
        &output,
        &["OUTSTANDING BOT CHANGE REQUEST", "u/ctc-cr"],
        "not clean",
    );
}

#[test]
fn stale_bot_review_evidence_blocks() {
    // A bot whose latest review predates the evaluated head provides no
    // evidence about the commit being merged.
    let output = run_scenario("stale-approval", "pull_request", Some(HEAD_SHA));
    assert_blocked(
        &output,
        &["STALE BOT REVIEW EVIDENCE", OTHER_SHA, HEAD_SHA],
        "not clean",
    );
}

#[test]
fn dispatch_on_a_ref_other_than_the_head_is_refused() {
    // A workflow_dispatch run is attached to the dispatched ref's tip
    // (GITHUB_SHA) while pr_number only names the pull request to evaluate.
    // A dispatch on any other ref — another branch, or main — would attach
    // this pull request's required check to a run whose evidence was
    // evaluated at a different commit, so the gate must refuse it before
    // evaluating anything.
    let sandbox = GateSandbox::new("dispatch-cross-bound");
    sandbox.use_scenario("clean");
    let output = sandbox.run_with_github_sha("workflow_dispatch", None, Some(OTHER_SHA));
    assert_blocked(&output, &[], "is not pull request head");
}

#[test]
fn dispatch_on_the_head_branch_publishes_evidence_at_that_head() {
    // The documented refresh path dispatches on the head branch, whose tip
    // equals the pull request head, so the run must proceed and publish its
    // result bound to exactly that head.
    let sandbox = GateSandbox::new("dispatch-head-bound");
    sandbox.use_scenario("clean");
    let output = sandbox.run_with_github_sha("workflow_dispatch", None, Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "the documented dispatch refresh path must pass on the head branch:\n{}",
        report(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("clean at {HEAD_SHA}")),
        "the dispatched run must publish evidence at the pull request head:\n{}",
        report(&output)
    );
}

#[test]
fn reviews_beyond_the_first_pagination_page_reach_the_decisions() {
    let output = run_scenario("review-pagination-boundary", "pull_request", Some(HEAD_SHA));
    assert_blocked(
        &output,
        &["OUTSTANDING BOT CHANGE REQUEST", "u/pg-cr"],
        "not clean",
    );
}

#[test]
fn incomplete_thread_pagination_fails_closed() {
    let output = run_scenario(
        "thread-pagination-incomplete",
        "pull_request",
        Some(HEAD_SHA),
    );
    assert_blocked(
        &output,
        &[],
        "Review-thread response omitted the pull request; failing closed.",
    );
}

#[test]
fn failed_graphql_query_fails_closed() {
    let output = run_scenario("query-failure", "pull_request", Some(HEAD_SHA));
    assert_blocked(&output, &[], "Review-thread query failed; failing closed.");
}

#[test]
fn head_moved_during_evaluation_fails_closed() {
    let output = run_scenario("head-moved", "pull_request", Some(HEAD_SHA));
    assert_blocked(&output, &[], "Pull request head moved to");
}

#[test]
fn workflow_dispatch_derives_the_head_from_the_pull_request() {
    // workflow_dispatch carries no pull-request context, so the head comes
    // from the pull request record itself, bound to the dispatched ref's
    // tip — the documented resolution refresh path dispatches on the head
    // branch, whose tip is the pull request head.
    let sandbox = GateSandbox::new("dispatch");
    sandbox.use_scenario("dispatch");
    let output = sandbox.run_with_github_sha("workflow_dispatch", None, Some(HEAD_SHA));
    assert!(
        output.status.success(),
        "a dispatched refresh of clean evidence must pass:\n{}",
        report(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("clean at {HEAD_SHA}")),
        "the dispatched result must bind to the recorded head:\n{}",
        report(&output)
    );
}

#[test]
fn non_main_base_fails_closed() {
    let output = run_scenario("non-main-base", "workflow_dispatch", None);
    assert_blocked(&output, &[], "not main");
}

#[test]
fn stale_event_head_fails_closed() {
    let output = run_scenario("event-head-stale", "pull_request", Some(OTHER_SHA));
    assert_blocked(&output, &[], "no longer matches pull request head");
}

#[test]
fn thread_failure_prints_the_documented_refresh_paths() {
    // Thread resolution fires no Actions event, so the failure output must
    // point at the supported refresh paths: a check re-run or a targeted
    // workflow_dispatch on the pull request's head branch.
    let output = run_scenario("unresolved-current-thread", "pull_request", Some(HEAD_SHA));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("gh workflow run bot-review-gate.yml --ref <head branch>")
            && stderr.contains(&format!("pr_number={GATE_PR_NUMBER}")),
        "the failure must print the dispatch refresh path:\n{}",
        report(&output)
    );
}
