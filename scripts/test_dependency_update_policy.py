#!/usr/bin/env python3
"""Focused unit tests for the dependency-update repair helpers."""

from __future__ import annotations

import os
import shutil
# Tests execute checked-in scripts with argv-only subprocess calls.
import subprocess  # nosec B404
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import sync_fuzz_lock
import sync_rust_toolchain


def workflow_run_script(job: str, step_name: str) -> str:
    """Extract one literal Bash `run: |` body from the checked-in workflow."""
    workflow = (
        sync_fuzz_lock.REPOSITORY
        / ".github"
        / "workflows"
        / "dependabot-automerge.yml"
    ).read_text()
    lines = workflow.splitlines()
    job_marker = f"  {job}:"
    step_marker = f"      - name: {step_name}"
    try:
        job_start = lines.index(job_marker)
        step_start = lines.index(step_marker, job_start)
        run_start = lines.index("        run: |", step_start) + 1
    except ValueError as error:
        raise AssertionError(
            f"cannot locate {job}/{step_name} literal run body"
        ) from error

    body: list[str] = []
    for line in lines[run_start:]:
        if line and not line.startswith("          "):
            break
        body.append(line[10:] if line else "")
    if not body:
        raise AssertionError(f"{job}/{step_name} has an empty run body")
    return "\n".join(body) + "\n"


def lock(direct: list[str], versions: dict[str, list[str]]) -> dict:
    packages = [
        {
            "name": "tributary",
            "version": "0.5.1",
            "dependencies": direct,
        }
    ]
    for name, releases in versions.items():
        packages.extend(
            {
                "name": name,
                "version": release,
                "source": "registry+https://github.com/rust-lang/crates.io-index",
            }
            for release in releases
        )
    return {"version": 4, "package": packages}


class DependabotAutomergeRaceTests(unittest.TestCase):
    # The fixture intentionally keeps creation, execution, and evidence capture
    # together so every workflow race test uses the same hermetic boundary.
    def run_workflow_script(
        self,
        script: str,
        *,
        heads: str,
        expected_head: str = "H1",
        merge_head: str = "H1",
    ) -> tuple[subprocess.CompletedProcess[str], str, str]:
        #lizard forgives
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            fake_bin = root / "bin"
            fake_bin.mkdir()
            state = root / "state"
            calls = root / "calls"
            output = root / "output"
            fake_gh = fake_bin / "gh"
            fake_gh.write_text(
                """#!/usr/bin/env python3
import os
import pathlib
import sys

arguments = sys.argv[1:]
state_path = pathlib.Path(os.environ["FAKE_GH_STATE"])
calls_path = pathlib.Path(os.environ["FAKE_GH_CALLS"])
with calls_path.open("a") as calls:
    calls.write(repr(arguments) + "\\n")

if arguments and arguments[0] == "api":
    if any("/files?" in argument for argument in arguments):
        print("Cargo.lock\\t")
        raise SystemExit(0)
    heads = os.environ["FAKE_GH_HEADS"].split(",")
    index = int(state_path.read_text()) if state_path.exists() else 0
    print(heads[min(index, len(heads) - 1)])
    state_path.write_text(str(index + 1))
    raise SystemExit(0)

if arguments[:2] == ["pr", "merge"]:
    guard_index = arguments.index("--match-head-commit") + 1
    guarded_head = arguments[guard_index]
    raise SystemExit(0 if guarded_head == os.environ["FAKE_MERGE_HEAD"] else 42)

raise SystemExit(64)
"""
            )
            fake_gh.chmod(0o755)
            environment = {
                **os.environ,
                "PATH": f"{fake_bin}:{os.environ['PATH']}",
                "RUNNER_TEMP": str(root),
                "GITHUB_OUTPUT": str(output),
                "GITHUB_REPOSITORY": "jm2/tributary",
                # This value is synthetic and unique to the temporary fixture.
                "GH_TOKEN": f"test-only-{root.name}",
                "PR_NUMBER": "7",
                "PR_URL": "https://github.invalid/jm2/tributary/pull/7",
                "EXPECTED_HEAD_SHA": expected_head,
                "EXPECTED_CHANGED_FILES": "1",
                "FAKE_GH_STATE": str(state),
                "FAKE_GH_CALLS": str(calls),
                "FAKE_GH_HEADS": heads,
                "FAKE_MERGE_HEAD": merge_head,
            }
            bash = shutil.which("bash")
            if bash is None:
                raise AssertionError("bash is required for workflow policy tests")
            # The test intentionally executes a literal checked-in workflow
            # body inside a temporary directory with a fake GitHub CLI.
            completed = subprocess.run(  # nosec B603  # nosemgrep
                [bash],
                input=script,
                cwd=sync_fuzz_lock.REPOSITORY,
                env=environment,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            return (
                completed,
                calls.read_text() if calls.exists() else "",
                output.read_text() if output.exists() else "",
            )

    def test_same_count_h1_h2_file_inspection_fails_closed(self):
        script = workflow_run_script(
            "inspect_changed_files",
            "Deny privileged workflow changes",
        )
        completed, calls, output = self.run_workflow_script(
            script,
            heads="H1,H2",
        )

        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("changed during file inspection", completed.stderr)
        self.assertEqual(calls.count("/files?"), 1)
        self.assertNotIn("safe=true", output)

    def test_server_guard_rejects_h2_after_final_h1_readback(self):
        script = workflow_run_script(
            "dependabot-automerge",
            "Enable exact-head auto-merge for patch & minor updates",
        )
        completed, calls, _ = self.run_workflow_script(
            script,
            heads="H1",
            merge_head="H2",
        )

        self.assertEqual(completed.returncode, 42)
        self.assertIn("'--match-head-commit', 'H1'", calls)


class FuzzLockPolicyTests(unittest.TestCase):
    def test_git_reader_requires_an_exact_commit_sha(self):
        with self.assertRaises(sync_fuzz_lock.PolicyError):
            sync_fuzz_lock.load_toml_from_git("--help", "Cargo.lock")

    def test_changed_shared_production_dependency_requires_exact_fuzz_version(self):
        base = lock(["tokio"], {"tokio": ["1.52.0"]})
        current = lock(["tokio"], {"tokio": ["1.53.0"]})
        fuzz = lock(["tokio"], {"tokio": ["1.52.0"]})
        manifest = {"dependencies": {"tokio": "1"}}

        self.assertEqual(
            sync_fuzz_lock.required_transitions(base, current, fuzz, manifest),
            [sync_fuzz_lock.Transition("tokio", "1.52.0", "1.53.0")],
        )

    def test_existing_shared_production_mismatch_requires_alignment(self):
        root = lock(["tokio"], {"tokio": ["1.53.0"]})
        fuzz = lock(["tokio"], {"tokio": ["1.52.0"]})
        manifest = {"dependencies": {"tokio": "1"}}

        self.assertEqual(
            sync_fuzz_lock.required_transitions(root, root, fuzz, manifest),
            [sync_fuzz_lock.Transition("tokio", "1.52.0", "1.53.0")],
        )

    def test_changed_root_dev_dependency_is_not_inherited_by_fuzz(self):
        base = lock(["toml"], {"toml": ["1.1.2"]})
        current = lock(["toml"], {"toml": ["1.1.3"]})
        fuzz = lock([], {})
        manifest = {"dev-dependencies": {"toml": "1"}}

        self.assertEqual(
            sync_fuzz_lock.required_transitions(base, current, fuzz, manifest),
            [],
        )

    def test_alias_uses_the_actual_package_name(self):
        manifest = {
            "dependencies": {
                "gtk": {"version": "0.11", "package": "gtk4"},
                "tokio": {"version": "1"},
            },
            "target": {
                "cfg(target_os = 'windows')": {
                    "dependencies": {
                        "windows-sys": {"version": "0.61"},
                    }
                }
            },
        }
        self.assertEqual(
            sync_fuzz_lock.production_dependency_names(manifest),
            {"gtk4", "tokio", "windows-sys"},
        )

    def test_removed_production_dependency_fails_closed(self):
        base = lock(["tokio"], {"tokio": ["1.52.0"]})
        current = lock([], {})
        fuzz = lock(["tokio"], {"tokio": ["1.52.0"]})
        with self.assertRaises(sync_fuzz_lock.PolicyError):
            sync_fuzz_lock.required_transitions(base, current, fuzz, {})

    def test_new_direct_major_can_coexist_with_required_transitive_old_major(self):
        base = lock(["sha2"], {"sha2": ["0.10.9"]})
        current = lock(
            ["sha2 0.11.0"],
            {"sha2": ["0.10.9", "0.11.0"]},
        )
        stale_fuzz = lock(["sha2"], {"sha2": ["0.10.9"]})
        repaired_fuzz = lock(
            ["sha2 0.11.0"],
            {"sha2": ["0.10.9", "0.11.0"]},
        )
        old_manifest = {"dependencies": {"sha2": "0.10"}}
        new_manifest = {"dependencies": {"sha2": "0.11"}}
        transitions = sync_fuzz_lock.required_transitions(
            base, current, stale_fuzz, new_manifest
        )

        self.assertEqual(
            transitions,
            [sync_fuzz_lock.Transition("sha2", "0.10.9", "0.11.0")],
        )
        self.assertNotEqual(
            sync_fuzz_lock.production_dependency_declarations(old_manifest),
            sync_fuzz_lock.production_dependency_declarations(new_manifest),
        )
        self.assertEqual(
            sync_fuzz_lock.required_transitions(
                base, current, repaired_fuzz, new_manifest
            ),
            [],
        )
        sync_fuzz_lock.validate_bounded_package_changes(
            base,
            current,
            stale_fuzz,
            repaired_fuzz,
            transitions,
        )

    def test_bounded_repair_rejects_unrelated_package_version_drift(self):
        base = lock(["sha2"], {"sha2": ["0.10.9"], "unrelated": ["1.0.0"]})
        current = lock(
            ["sha2 0.11.0"],
            {"sha2": ["0.10.9", "0.11.0"], "unrelated": ["1.0.0"]},
        )
        stale_fuzz = lock(
            ["sha2"],
            {"sha2": ["0.10.9"], "unrelated": ["1.0.0"]},
        )
        unsafe_fuzz = lock(
            ["sha2 0.11.0"],
            {"sha2": ["0.10.9", "0.11.0"], "unrelated": ["2.0.0"]},
        )
        with self.assertRaises(sync_fuzz_lock.PolicyError):
            sync_fuzz_lock.validate_bounded_package_changes(
                base,
                current,
                stale_fuzz,
                unsafe_fuzz,
                [sync_fuzz_lock.Transition("sha2", "0.10.9", "0.11.0")],
            )

    def test_bounded_repair_rejects_fuzz_only_dependency_surface_drift(self):
        root = lock(
            ["sha2"],
            {"sha2": ["0.11.0"], "old-edge": ["1.0.0"], "new-edge": ["1.0.0"]},
        )
        stale_fuzz = lock(
            ["sha2"],
            {
                "sha2": ["0.10.9"],
                "old-edge": ["1.0.0"],
                "new-edge": ["1.0.0"],
                "unrelated": ["1.0.0"],
            },
        )
        unsafe_fuzz = lock(
            ["sha2 0.11.0"],
            {
                "sha2": ["0.10.9", "0.11.0"],
                "old-edge": ["1.0.0"],
                "new-edge": ["1.0.0"],
                "unrelated": ["1.0.0"],
            },
        )
        stale_fuzz["package"][-1]["dependencies"] = ["old-edge"]
        unsafe_fuzz["package"][-1]["dependencies"] = ["new-edge"]

        with self.assertRaises(sync_fuzz_lock.PolicyError):
            sync_fuzz_lock.validate_bounded_package_changes(
                root,
                root,
                stale_fuzz,
                unsafe_fuzz,
                [sync_fuzz_lock.Transition("sha2", "0.10.9", "0.11.0")],
            )

    def test_bounded_repair_allows_feature_edge_inside_target_closure(self):
        base = lock(
            ["facade 1.0.0"],
            {
                "facade": ["1.0.0"],
                "shared": ["1.0.0"],
                "old-edge": ["1.0.0"],
            },
        )
        current = lock(
            ["facade 1.1.0"],
            {
                "facade": ["1.1.0"],
                "shared": ["1.0.0"],
                "old-edge": ["1.0.0"],
                "new-edge": ["1.0.0"],
            },
        )
        stale_fuzz = lock(
            ["facade 1.0.0"],
            {
                "facade": ["1.0.0"],
                "shared": ["1.0.0"],
                "old-edge": ["1.0.0"],
            },
        )
        repaired_fuzz = lock(
            ["facade 1.1.0"],
            {
                "facade": ["1.1.0"],
                "shared": ["1.0.0"],
                "old-edge": ["1.0.0"],
                "new-edge": ["1.0.0"],
            },
        )
        for fixture in (base, stale_fuzz):
            fixture["package"][1]["dependencies"] = ["shared"]
            fixture["package"][2]["dependencies"] = ["old-edge"]
        for fixture in (current, repaired_fuzz):
            fixture["package"][1]["dependencies"] = ["shared"]
            fixture["package"][2]["dependencies"] = ["old-edge", "new-edge"]

        sync_fuzz_lock.validate_bounded_package_changes(
            base,
            current,
            stale_fuzz,
            repaired_fuzz,
            [sync_fuzz_lock.Transition("facade", "1.0.0", "1.1.0")],
        )

    def test_bounded_repair_rejects_unrelated_existing_version_rebind(self):
        base = lock(
            ["sha2 0.10.9"],
            {"sha2": ["0.10.9"], "shared": ["1.0.0", "2.0.0"]},
        )
        current = lock(
            ["sha2 0.11.0"],
            {
                "sha2": ["0.10.9", "0.11.0"],
                "shared": ["1.0.0", "2.0.0"],
            },
        )
        stale_fuzz = lock(
            ["sha2 0.10.9"],
            {
                "sha2": ["0.10.9"],
                "shared": ["1.0.0", "2.0.0"],
                "unrelated": ["1.0.0"],
            },
        )
        stale_fuzz["package"][-1]["dependencies"] = ["shared 1.0.0"]
        unsafe_fuzz = lock(
            ["sha2 0.11.0"],
            {
                "sha2": ["0.10.9", "0.11.0"],
                "shared": ["1.0.0", "2.0.0"],
                "unrelated": ["1.0.0"],
            },
        )
        unsafe_fuzz["package"][-1]["dependencies"] = ["shared 2.0.0"]

        with self.assertRaises(sync_fuzz_lock.PolicyError):
            sync_fuzz_lock.validate_bounded_package_changes(
                base,
                current,
                stale_fuzz,
                unsafe_fuzz,
                [sync_fuzz_lock.Transition("sha2", "0.10.9", "0.11.0")],
            )

    def test_bounded_repair_rejects_same_name_target_outside_exact_closures(self):
        base = lock(
            ["facade 1.0.0"],
            {
                "facade": ["1.0.0"],
                "shared": ["1.0.0", "9.0.0"],
                "unrelated": ["1.0.0"],
            },
        )
        current = lock(
            ["facade 1.1.0"],
            {
                "facade": ["1.0.0", "1.1.0"],
                "shared": ["1.0.0", "9.0.0"],
                "unrelated": ["1.0.0"],
            },
        )
        stale_fuzz = lock(
            ["facade 1.0.0"],
            {
                "facade": ["1.0.0"],
                "shared": ["1.0.0", "9.0.0"],
                "unrelated": ["1.0.0"],
            },
        )
        stale_fuzz["package"][-1]["dependencies"] = ["shared 1.0.0"]
        unsafe_fuzz = lock(
            ["facade 1.1.0"],
            {
                "facade": ["1.0.0", "1.1.0"],
                "shared": ["1.0.0", "9.0.0"],
                "unrelated": ["1.0.0"],
            },
        )
        unsafe_fuzz["package"][-1]["dependencies"] = ["shared 9.0.0"]

        with self.assertRaises(sync_fuzz_lock.PolicyError):
            sync_fuzz_lock.validate_bounded_package_changes(
                base,
                current,
                stale_fuzz,
                unsafe_fuzz,
                [sync_fuzz_lock.Transition("facade", "1.0.0", "1.1.0")],
            )

    def test_check_mode_rejects_raw_rebind_after_direct_transition_is_repaired(self):
        base = lock(
            ["facade 1.0.0"],
            {
                "facade": ["1.0.0"],
                "shared": ["1.0.0", "9.0.0"],
                "unrelated": ["1.0.0"],
            },
        )
        current = lock(
            ["facade 1.1.0"],
            {
                "facade": ["1.0.0", "1.1.0"],
                "shared": ["1.0.0", "9.0.0"],
                "unrelated": ["1.0.0"],
            },
        )
        base_fuzz = lock(
            ["facade 1.0.0"],
            {
                "facade": ["1.0.0"],
                "shared": ["1.0.0", "9.0.0"],
                "unrelated": ["1.0.0"],
            },
        )
        base_fuzz["package"][-1]["dependencies"] = ["shared 1.0.0"]
        submitted_fuzz = lock(
            ["facade 1.1.0"],
            {
                "facade": ["1.0.0", "1.1.0"],
                "shared": ["1.0.0", "9.0.0"],
                "unrelated": ["1.0.0"],
            },
        )
        submitted_fuzz["package"][-1]["dependencies"] = ["shared 9.0.0"]

        with self.assertRaises(sync_fuzz_lock.PolicyError):
            sync_fuzz_lock.validate_submitted_fuzz_update(
                base,
                current,
                base_fuzz,
                submitted_fuzz,
                {"dependencies": {"facade": "1"}},
            )

    def test_fuzz_only_update_remains_independent_when_base_needs_no_repair(self):
        root = lock(
            ["facade 1.1.0"],
            {
                "facade": ["1.1.0"],
                "shared": ["1.0.0", "2.0.0"],
                "fuzz-only": ["1.0.0"],
            },
        )
        base_fuzz = lock(
            ["facade 1.1.0"],
            {
                "facade": ["1.1.0"],
                "shared": ["1.0.0", "2.0.0"],
                "fuzz-only": ["1.0.0"],
            },
        )
        base_fuzz["package"][-1]["dependencies"] = ["shared 1.0.0"]
        updated_fuzz = lock(
            ["facade 1.1.0"],
            {
                "facade": ["1.1.0"],
                "shared": ["1.0.0", "2.0.0"],
                "fuzz-only": ["1.0.0"],
            },
        )
        updated_fuzz["package"][-1]["dependencies"] = ["shared 2.0.0"]

        requested, remaining = sync_fuzz_lock.validate_submitted_fuzz_update(
            root,
            root,
            base_fuzz,
            updated_fuzz,
            {"dependencies": {"facade": "1"}},
        )
        self.assertEqual(requested, [])
        self.assertEqual(remaining, [])

    def test_bounded_repair_rejects_same_version_source_rewrite(self):
        base = lock(["sha2"], {"sha2": ["0.10.9"]})
        current = lock(
            ["sha2 0.11.0"],
            {"sha2": ["0.10.9", "0.11.0"]},
        )
        stale_fuzz = lock(
            ["sha2"],
            {"sha2": ["0.10.9"], "unrelated": ["1.0.0"]},
        )
        unsafe_fuzz = lock(
            ["sha2 0.11.0"],
            {
                "sha2": ["0.10.9", "0.11.0"],
                "unrelated": ["1.0.0"],
            },
        )
        unsafe_fuzz["package"][-1]["source"] = "git+https://invalid.example/rewrite"

        with self.assertRaises(sync_fuzz_lock.PolicyError):
            sync_fuzz_lock.validate_bounded_package_changes(
                base,
                current,
                stale_fuzz,
                unsafe_fuzz,
                [sync_fuzz_lock.Transition("sha2", "0.10.9", "0.11.0")],
            )

    def test_bounded_repair_rejects_new_root_identity_source_and_checksum_mismatch(
        self,
    ):
        base = lock(["facade 1.0.0"], {"facade": ["1.0.0"]})
        current = lock(
            ["facade 1.1.0"],
            {"facade": ["1.0.0", "1.1.0"]},
        )
        stale_fuzz = lock(["facade 1.0.0"], {"facade": ["1.0.0"]})
        unsafe_fuzz = lock(
            ["facade 1.1.0"],
            {"facade": ["1.0.0", "1.1.0"]},
        )
        for package in current["package"]:
            if package["name"] == "facade" and package["version"] == "1.1.0":
                package["checksum"] = "authoritative"
        for package in unsafe_fuzz["package"]:
            if package["name"] == "facade" and package["version"] == "1.1.0":
                package["source"] = "git+https://invalid.example/facade"
                package["checksum"] = "tampered"

        with self.assertRaises(sync_fuzz_lock.PolicyError):
            sync_fuzz_lock.validate_bounded_package_changes(
                base,
                current,
                stale_fuzz,
                unsafe_fuzz,
                [sync_fuzz_lock.Transition("facade", "1.0.0", "1.1.0")],
            )

    def test_bounded_repair_allows_exact_new_fuzz_resolver_identity(self):
        base = lock(
            ["facade 1.0.0"],
            {"facade": ["1.0.0"], "component": ["1.0.0"]},
        )
        base["package"][1]["dependencies"] = ["component 1.0.0"]
        current = lock(
            ["facade 1.1.0"],
            {"facade": ["1.1.0"], "component": ["1.2.0"]},
        )
        current["package"][1]["dependencies"] = ["component 1.2.0"]
        stale_fuzz = lock(
            ["facade 1.0.0"],
            {"facade": ["1.0.0"], "component": ["1.0.0"]},
        )
        stale_fuzz["package"][1]["dependencies"] = ["component 1.0.0"]
        repaired_fuzz = lock(
            ["facade 1.1.0"],
            {"facade": ["1.1.0"], "component": ["1.1.0"]},
        )
        repaired_fuzz["package"][1]["dependencies"] = ["component 1.1.0"]

        sync_fuzz_lock.validate_bounded_package_changes(
            base,
            current,
            stale_fuzz,
            repaired_fuzz,
            [sync_fuzz_lock.Transition("facade", "1.0.0", "1.1.0")],
        )

    def test_bounded_repair_allows_nonsemantic_edge_disambiguation(self):
        base = lock(["sha2"], {"sha2": ["0.10.9"]})
        current = lock(
            ["sha2 0.11.0"],
            {"sha2": ["0.10.9", "0.11.0"]},
        )
        stale_fuzz = lock(
            ["sha2"],
            {"sha2": ["0.10.9"], "consumer": ["1.0.0"]},
        )
        stale_fuzz["package"][-1]["dependencies"] = ["sha2"]
        repaired_fuzz = lock(
            ["sha2 0.11.0"],
            {
                "sha2": ["0.10.9", "0.11.0"],
                "consumer": ["1.0.0"],
            },
        )
        repaired_fuzz["package"][-1]["dependencies"] = ["sha2 0.10.9"]

        sync_fuzz_lock.validate_bounded_package_changes(
            base,
            current,
            stale_fuzz,
            repaired_fuzz,
            [sync_fuzz_lock.Transition("sha2", "0.10.9", "0.11.0")],
        )

    def test_bounded_repair_rejects_unrequested_path_package_rebind(self):
        base = lock(
            ["sha2 0.10.9", "tokio 1.0.0"],
            {"sha2": ["0.10.9"], "tokio": ["1.0.0", "2.0.0"]},
        )
        current = lock(
            ["sha2 0.11.0", "tokio 1.0.0"],
            {
                "sha2": ["0.10.9", "0.11.0"],
                "tokio": ["1.0.0", "2.0.0"],
            },
        )
        stale_fuzz = lock(
            ["sha2 0.10.9", "tokio 1.0.0"],
            {"sha2": ["0.10.9"], "tokio": ["1.0.0", "2.0.0"]},
        )
        unsafe_fuzz = lock(
            ["sha2 0.11.0", "tokio 2.0.0"],
            {
                "sha2": ["0.10.9", "0.11.0"],
                "tokio": ["1.0.0", "2.0.0"],
            },
        )

        with self.assertRaises(sync_fuzz_lock.PolicyError):
            sync_fuzz_lock.validate_bounded_package_changes(
                base,
                current,
                stale_fuzz,
                unsafe_fuzz,
                [sync_fuzz_lock.Transition("sha2", "0.10.9", "0.11.0")],
            )

    def test_bounded_repair_allows_consumer_rebind_to_reviewed_target(self):
        base = lock(["facade"], {"facade": ["1.0.0"], "consumer": ["1.0.0"]})
        base["package"][-1]["dependencies"] = ["facade"]
        current = lock(
            ["facade 1.1.0"],
            {"facade": ["1.0.0", "1.1.0"], "consumer": ["1.0.0"]},
        )
        current["package"][-1]["dependencies"] = ["facade 1.1.0"]
        stale_fuzz = lock(
            ["facade"],
            {"facade": ["1.0.0"], "consumer": ["1.0.0"]},
        )
        stale_fuzz["package"][-1]["dependencies"] = ["facade"]
        repaired_fuzz = lock(
            ["facade 1.1.0"],
            {"facade": ["1.0.0", "1.1.0"], "consumer": ["1.0.0"]},
        )
        repaired_fuzz["package"][-1]["dependencies"] = ["facade 1.1.0"]

        sync_fuzz_lock.validate_bounded_package_changes(
            base,
            current,
            stale_fuzz,
            repaired_fuzz,
            [sync_fuzz_lock.Transition("facade", "1.0.0", "1.1.0")],
        )

    def test_bounded_repair_allows_target_dependency_closure(self):
        base = lock(
            ["facade"],
            {"facade": ["1.0.0"], "component": ["1.0.0"]},
        )
        base["package"][1]["dependencies"] = ["component"]
        current = lock(
            ["facade"],
            {"facade": ["1.1.0"], "component": ["1.1.0"]},
        )
        current["package"][1]["dependencies"] = ["component"]
        stale_fuzz = lock(
            ["facade"],
            {"facade": ["1.0.0"], "component": ["1.0.0"]},
        )
        stale_fuzz["package"][1]["dependencies"] = ["component"]
        repaired_fuzz = lock(
            ["facade"],
            {"facade": ["1.1.0"], "component": ["1.1.0"]},
        )
        repaired_fuzz["package"][1]["dependencies"] = ["component"]

        sync_fuzz_lock.validate_bounded_package_changes(
            base,
            current,
            stale_fuzz,
            repaired_fuzz,
            [sync_fuzz_lock.Transition("facade", "1.0.0", "1.1.0")],
        )

    def test_transitive_only_root_lock_change_is_deliberately_not_projected(self):
        base = lock(
            ["tokio"],
            {"tokio": ["1.53.1"], "transitive": ["1.0.0"]},
        )
        current = lock(
            ["tokio"],
            {"tokio": ["1.53.1"], "transitive": ["1.1.0"]},
        )
        fuzz = lock(
            ["tokio"],
            {"tokio": ["1.53.1"], "transitive": ["1.0.0"]},
        )
        self.assertEqual(
            sync_fuzz_lock.required_transitions(
                base,
                current,
                fuzz,
                {"dependencies": {"tokio": "1"}},
            ),
            [],
        )


class RustToolchainPolicyTests(unittest.TestCase):
    def test_candidate_requires_both_exact_action_refs_to_agree(self):
        digest = "a" * 40
        source = """
        uses: dtolnay/rust-toolchain@DIGEST # 1.93.0
        uses: dtolnay/rust-toolchain@stable
        uses: dtolnay/rust-toolchain@DIGEST # 1.93.0
""".replace("DIGEST", digest)
        self.assertEqual(
            sync_rust_toolchain.candidate_from_actions(source),
            "1.93",
        )

    def test_mixed_exact_action_refs_fail_closed(self):
        digest = "a" * 40
        source = """
        uses: dtolnay/rust-toolchain@DIGEST # 1.92.0
        uses: dtolnay/rust-toolchain@DIGEST # 1.93.0
""".replace("DIGEST", digest)
        with self.assertRaises(sync_rust_toolchain.PolicyError):
            sync_rust_toolchain.candidate_from_actions(source)

    def test_mixed_exact_action_digests_fail_closed(self):
        source = f"""
        uses: dtolnay/rust-toolchain@{'a' * 40} # 1.93.0
        uses: dtolnay/rust-toolchain@{'b' * 40} # 1.93.0
"""
        with self.assertRaises(sync_rust_toolchain.PolicyError):
            sync_rust_toolchain.candidate_from_actions(source)

    def test_dependabot_action_proposal_synchronizes_every_contract(self):
        proposed_digest = "b" * 40
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = root / "Cargo.toml"
            ci = root / "ci.yml"
            readme = root / "README.md"
            manifest.write_text(
                '[package]\nname = "fixture"\nversion = "0.0.0"\n'
                'rust-version = "1.92"\n'
            )
            ci.write_text(
                "# rustc 1.92 is the supported floor\n"
                "msrv:\n"
                "    name: MSRV\n"
                "  steps:\n"
                "    - name: Install Rust toolchain (1.92)\n"
                f"      uses: dtolnay/rust-toolchain@{proposed_digest} # 1.93.0\n"
                "coverage:\n"
                "  steps:\n"
                "    - name: Install coverage toolchain\n"
                f"      uses: dtolnay/rust-toolchain@{proposed_digest} # 1.93.0\n"
                "  key: coverage-1.92.0-llvm-cov-fixture\n"
            )
            readme.write_text(
                "Rust 1.92+\n"
                "rustup toolchain install 1.92.0\n"
                "cargo +1.92.0 llvm-cov\n"
                "coverage is pinned to Rust 1.92.0\n"
            )

            original = (
                sync_rust_toolchain.MANIFEST,
                sync_rust_toolchain.CI,
                sync_rust_toolchain.README,
            )
            try:
                sync_rust_toolchain.MANIFEST = manifest
                sync_rust_toolchain.CI = ci
                sync_rust_toolchain.README = readme
                target = sync_rust_toolchain.candidate_from_actions(ci.read_text())
                sync_rust_toolchain.synchronize(target)
                sync_rust_toolchain.check_consistency()
            finally:
                (
                    sync_rust_toolchain.MANIFEST,
                    sync_rust_toolchain.CI,
                    sync_rust_toolchain.README,
                ) = original

            self.assertIn('rust-version = "1.93"', manifest.read_text())
            self.assertIn("    name: MSRV\n", ci.read_text())
            self.assertEqual(ci.read_text().count(proposed_digest), 2)
            self.assertNotIn("1.92", ci.read_text() + readme.read_text())


if __name__ == "__main__":
    unittest.main()
