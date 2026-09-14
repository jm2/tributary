//! Tests for the publisher's default-off staged activation gate.
//!
//! The staging contract lives in the workflow, not only in prose: the
//! `activation` job reads the trusted repository activation variable and only
//! an explicit `active` value lets the publishing job run. These tests execute
//! the activation script extracted from the workflow (so the tested bytes are
//! the bytes Actions executes) and pin the job wiring that keeps the
//! environment and the App token unreachable while staged inactive.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_yaml::Value;

use crate::harness::BOT_REVIEW_GATE_PUBLISHER_YAML;

fn publisher_workflow() -> Value {
    serde_yaml::from_str(BOT_REVIEW_GATE_PUBLISHER_YAML).expect("publisher workflow must parse")
}

fn activation_script(workflow: &Value) -> String {
    let steps = workflow["jobs"]["activation"]["steps"]
        .as_sequence()
        .expect("the activation job must declare steps");
    assert_eq!(
        steps.len(),
        1,
        "the activation job must stay a single credential-free step"
    );
    assert!(
        steps[0].get("uses").is_none(),
        "the activation step must be inline and action-free"
    );
    steps[0]["run"]
        .as_str()
        .expect("the activation step must inline its script")
        .to_owned()
}

fn scratch_root() -> PathBuf {
    if let Ok(target_tmp) = std::env::var("CARGO_TARGET_TMPDIR") {
        return PathBuf::from(target_tmp);
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/scratch")
}

fn run_activation(activation: Option<&str>) -> (Output, String) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = scratch_root().join(format!(
        "activation-fixture-{}-{serial}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("activation scratch must be creatable");

    let script_path = root.join("activation.sh");
    std::fs::write(&script_path, activation_script(&publisher_workflow()))
        .expect("activation script must be writable");
    let output_path = root.join("github-output");
    std::fs::write(&output_path, "").expect("GITHUB_OUTPUT file must be writable");

    let mut command = Command::new("bash");
    command.arg(&script_path).env("GITHUB_OUTPUT", &output_path);
    if let Some(value) = activation {
        command.env("ACTIVATION", value);
    } else {
        command.env_remove("ACTIVATION");
    }
    let output = command
        .output()
        .expect("the activation script must run under bash");
    let github_output = std::fs::read_to_string(&output_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&root);
    (output, github_output)
}

fn assert_activation(value: Option<&str>, expected_active: bool) {
    let (output, github_output) = run_activation(value);
    assert!(
        output.status.success(),
        "activation value {value:?} must be accepted: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        github_output.contains(&format!("active={expected_active}")),
        "activation value {value:?} must emit active={expected_active}, got:\n{github_output}"
    );
}

#[test]
fn activation_is_default_off_and_only_active_enables_publication() {
    assert_activation(None, false);
    assert_activation(Some(""), false);
    assert_activation(Some("inactive"), false);
    assert_activation(Some("active"), true);
}

#[test]
fn inactive_activation_publishes_nothing_and_claims_no_authority() {
    let (output, github_output) = run_activation(Some("inactive"));
    assert!(github_output.contains("active=false"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("staged inactive"),
        "the inactive path must state that publication is staged off:\n{stdout}"
    );
    assert!(
        stdout.contains("no merge authority"),
        "the inactive path must disclaim merge authority:\n{stdout}"
    );

    // The activation step must stay credential-free: it can never publish a
    // check run, so an inactive refresh leaves the required context absent.
    let script = activation_script(&publisher_workflow());
    for forbidden in ["gh api", "check-runs", "GATE_TOKEN", "secrets."] {
        assert!(
            !script.contains(forbidden),
            "the activation step must not touch {forbidden}"
        );
    }
}

#[test]
fn unrecognized_activation_value_fails_closed_instead_of_skipping() {
    let (output, github_output) = run_activation(Some("yes-please"));
    assert!(
        !output.status.success(),
        "an unrecognized activation value must fail the run, not silently skip"
    );
    assert!(
        !github_output.contains("active=true"),
        "an unrecognized activation value must never activate publication"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must be unset, 'inactive', or 'active'"),
        "the failure must explain the accepted values:\n{stderr}"
    );
}

#[test]
fn publishing_job_is_gated_behind_the_activation_output() {
    let workflow = publisher_workflow();
    let activation = &workflow["jobs"]["activation"];
    assert!(
        activation["outputs"]["active"]
            .as_str()
            .is_some_and(|value| value.contains("steps.activation.outputs.active")),
        "the activation job must expose its decision as a job output"
    );
    assert!(
        activation.get("environment").is_none(),
        "the activation job must request no environment"
    );
    assert_eq!(
        activation["permissions"],
        serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        "the activation job must hold no token scopes"
    );

    let publish = &workflow["jobs"]["publish"];
    assert_eq!(
        publish["needs"].as_str(),
        Some("activation"),
        "the publishing job must depend on the activation decision"
    );
    assert_eq!(
        publish["if"].as_str(),
        Some("needs.activation.outputs.active == 'true'"),
        "the publishing job must run only when activation is explicitly active"
    );
    assert_eq!(
        publish["environment"].as_str(),
        Some("bot-review-gate-publisher"),
        "the protected environment stays attached to the gated publishing job"
    );
}

#[test]
fn active_mode_requires_complete_credentials_and_never_falls_back() {
    let workflow = publisher_workflow();
    let steps = workflow["jobs"]["publish"]["steps"]
        .as_sequence()
        .expect("publishing steps must be a sequence");
    assert_eq!(
        steps.len(),
        2,
        "the publishing job must stay the token mint plus the inline publication"
    );
    assert!(
        steps[0].get("continue-on-error").is_none() && steps[1].get("continue-on-error").is_none(),
        "no step may swallow a credential failure with continue-on-error"
    );
    let publish = steps[1]["run"]
        .as_str()
        .expect("the publication step must inline its script");
    assert!(
        publish.contains("[ -z \"${GATE_TOKEN:-}\" ]"),
        "publication must fail closed when the minted App identity is missing"
    );
}
