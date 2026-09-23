#!/usr/bin/env python3
"""Unit tests for the Dependabot auto-merge workflow and Rust toolchain sync."""

from __future__ import annotations

import os
import shutil
# Tests execute checked-in scripts with argv-only subprocess calls.
import subprocess  # nosec B404
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import sync_rust_toolchain

REPOSITORY = Path(__file__).resolve().parents[1]


def workflow_run_script(job: str, step_name: str) -> str:
    """Extract one literal Bash `run: |` body from the checked-in workflow."""
    workflow = (
        REPOSITORY
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
                cwd=REPOSITORY,
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


class RustToolchainPolicyTests(unittest.TestCase):
    def test_candidate_comes_from_exact_toolchain_manifest_release(self):
        self.assertEqual(
            sync_rust_toolchain.candidate_from_toolchain(
                '[toolchain]\nchannel = "1.93.0"\nprofile = "minimal"\n'
            ),
            "1.93",
        )

    def test_nonrelease_toolchain_channel_fails_closed(self):
        for channel in ["stable", "1.93", "1.93.1"]:
            with self.subTest(channel=channel):
                with self.assertRaises(sync_rust_toolchain.PolicyError):
                    sync_rust_toolchain.candidate_from_toolchain(
                        f'[toolchain]\nchannel = "{channel}"\n'
                    )

    def test_action_pins_must_be_full_matching_master_commits(self):
        valid = (
            f"uses: dtolnay/rust-toolchain@{'a' * 40} # master\n"
            f"uses: dtolnay/rust-toolchain@{'a' * 40} # master\n"
        )
        self.assertEqual(sync_rust_toolchain.exact_action_pins(valid), ["a" * 40] * 2)
        invalid_sources = [
            valid.replace("a" * 40, "a" * 12, 1),
            valid.replace("a" * 40, "b" * 40, 1),
            valid.replace("# master", "# 1.93.0", 1),
        ]
        for source in invalid_sources:
            with self.subTest(source=source):
                with self.assertRaises(sync_rust_toolchain.PolicyError):
                    sync_rust_toolchain.exact_action_pins(source)

    def test_toolchain_proposal_synchronizes_without_changing_action_commit(self):
        action_sha = "b" * 40
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = root / "Cargo.toml"
            toolchain_manifest = root / "rust-toolchain.toml"
            ci = root / "ci.yml"
            readme = root / "README.md"
            manifest.write_text(
                '[package]\nname = "fixture"\nversion = "0.0.0"\n'
                'rust-version = "1.92"\n'
            )
            toolchain_manifest.write_text(
                '[toolchain]\nchannel = "1.93.0"\nprofile = "minimal"\n'
            )
            ci.write_text(
                "# rustc 1.92 is the supported floor\n"
                "msrv:\n"
                "    name: MSRV\n"
                "  steps:\n"
                "    - name: Install Rust toolchain (1.92)\n"
                f"      uses: dtolnay/rust-toolchain@{action_sha} # master\n"
                "      with:\n"
                "        toolchain: 1.92.0\n"
                "coverage:\n"
                "  steps:\n"
                "    - name: Install coverage toolchain\n"
                f"      uses: dtolnay/rust-toolchain@{action_sha} # master\n"
                "      with:\n"
                "        toolchain: 1.92.0\n"
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
                sync_rust_toolchain.TOOLCHAIN_MANIFEST,
                sync_rust_toolchain.CI,
                sync_rust_toolchain.README,
            )
            try:
                sync_rust_toolchain.MANIFEST = manifest
                sync_rust_toolchain.TOOLCHAIN_MANIFEST = toolchain_manifest
                sync_rust_toolchain.CI = ci
                sync_rust_toolchain.README = readme
                target = sync_rust_toolchain.candidate_from_toolchain()
                sync_rust_toolchain.synchronize(target)
                sync_rust_toolchain.check_consistency()
            finally:
                (
                    sync_rust_toolchain.MANIFEST,
                    sync_rust_toolchain.TOOLCHAIN_MANIFEST,
                    sync_rust_toolchain.CI,
                    sync_rust_toolchain.README,
                ) = original

            self.assertIn('rust-version = "1.93"', manifest.read_text())
            self.assertIn("    name: MSRV\n", ci.read_text())
            self.assertEqual(ci.read_text().count(action_sha), 2)
            self.assertEqual(
                ci.read_text().count("toolchain: 1.93.0"),
                2,
            )
            self.assertIn('channel = "1.93.0"', toolchain_manifest.read_text())
            self.assertNotIn("1.92", ci.read_text() + readme.read_text())

    def test_manual_set_updates_the_toolchain_manifest_too(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = root / "Cargo.toml"
            toolchain_manifest = root / "rust-toolchain.toml"
            ci = root / "ci.yml"
            readme = root / "README.md"
            action_sha = "c" * 40
            manifest.write_text('[package]\nrust-version = "1.92"\n')
            toolchain_manifest.write_text('[toolchain]\nchannel = "1.92.0"\n')
            ci.write_text(
                "# rustc 1.92 floor\n"
                "    name: MSRV\n"
                "- name: Install Rust toolchain (1.92)\n"
                f"  uses: dtolnay/rust-toolchain@{action_sha} # master\n"
                "  with:\n    toolchain: 1.92.0\n"
                f"  uses: dtolnay/rust-toolchain@{action_sha} # master\n"
                "  with:\n    toolchain: 1.92.0\n"
                "key: coverage-1.92.0-llvm-cov-fixture\n"
            )
            readme.write_text(
                "Rust 1.92+\n"
                "rustup toolchain install 1.92.0\n"
                "cargo +1.92.0 llvm-cov\n"
                "coverage is pinned to Rust 1.92.0\n"
            )
            original = (
                sync_rust_toolchain.MANIFEST,
                sync_rust_toolchain.TOOLCHAIN_MANIFEST,
                sync_rust_toolchain.CI,
                sync_rust_toolchain.README,
            )
            try:
                sync_rust_toolchain.MANIFEST = manifest
                sync_rust_toolchain.TOOLCHAIN_MANIFEST = toolchain_manifest
                sync_rust_toolchain.CI = ci
                sync_rust_toolchain.README = readme
                sync_rust_toolchain.synchronize(
                    "1.93", update_toolchain_manifest=True
                )
            finally:
                (
                    sync_rust_toolchain.MANIFEST,
                    sync_rust_toolchain.TOOLCHAIN_MANIFEST,
                    sync_rust_toolchain.CI,
                    sync_rust_toolchain.README,
                ) = original

            self.assertIn('channel = "1.93.0"', toolchain_manifest.read_text())
            self.assertEqual(ci.read_text().count(action_sha), 2)


if __name__ == "__main__":
    unittest.main()
