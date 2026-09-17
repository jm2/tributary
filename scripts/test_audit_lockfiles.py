#!/usr/bin/env python3
"""Fixture tests for the two-graph lockfile security audit helper.

Every test runs `scripts/audit_lockfiles.py` against a hermetic repository whose
`cargo-audit` is a fake that records each invocation and emits a controlled JSON
report. This proves the helper selects the fuzz lock explicitly, applies only
that graph's scoped exceptions, and fails when a finding exists only in the fuzz
graph instead of hiding behind a green root audit.
"""

from __future__ import annotations

import json
import os
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest import mock

import audit_lockfiles


REPOSITORY = Path(__file__).resolve().parent.parent

# Stand-in cargo-audit: records every call, counts the requested lockfile's
# packages, filters canned findings through the passed ignore list, and emits
# the same JSON shape cargo-audit produces.
FAKE_AUDIT = r'''#!/usr/bin/env python3
import json
import os
import sys
import tomllib
from pathlib import Path

args = sys.argv[1:]
if not args or args[0] != "audit":
    sys.stderr.write("fake cargo-audit expects the audit subcommand\n")
    sys.exit(2)
args = args[1:]


def flag_value(flag):
    return args[args.index(flag) + 1] if flag in args else None


lock = flag_value("--file")
ignores = []
index = 0
while index < len(args):
    if args[index] == "--ignore":
        ignores.append(args[index + 1])
        index += 2
    else:
        index += 1

with Path(os.environ["AUDIT_FIXTURE_CALLS"]).open("a") as calls:
    calls.write(json.dumps({"cwd": str(Path.cwd()), "file": lock, "ignore": ignores}) + "\n")

lock_path = Path(lock).resolve() if lock else Path("Cargo.lock").resolve()
fixture = json.loads(Path(os.environ["AUDIT_FIXTURE_REPORT"]).read_text())
entry = fixture.get(str(lock_path), {})
count = entry.get("count")
if count is None:
    with lock_path.open("rb") as source:
        count = len(tomllib.load(source).get("package", []))
reported = [item for item in entry.get("vulnerabilities", []) if item not in ignores]
report = {
    "database": {"advisory-count": 0},
    "lockfile": {"dependency-count": count},
    "settings": {"ignore": ignores},
    "vulnerabilities": {
        "count": len(reported),
        "list": [{"advisory": {"id": item}} for item in reported],
    },
    "warnings": {"count": 0, "list": []},
}
sys.stdout.write(json.dumps(report))
sys.exit(1 if reported else 0)
'''


def write_lock(path: Path, count: int) -> None:
    lines = ["version = 4"]
    for number in range(count):
        lines += [
            "[[package]]",
            f'name = "fixture-{number}"',
            'version = "1.0.0"',
            'source = "registry+https://github.com/rust-lang/crates.io-index"',
        ]
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(lines) + "\n")


def write_config(path: Path, ignores: list[str]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    rendered = ", ".join(json.dumps(item) for item in ignores)
    path.write_text(f"[advisories]\nignore = [{rendered}]\n")


class AuditLockfilesTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.repository = self.root / "repo"
        self.repository.mkdir()
        self.fake = self.root / "cargo-audit"
        self.fake.write_text(FAKE_AUDIT)
        self.fake.chmod(0o755)
        self.calls = self.root / "calls.jsonl"
        self.report = self.root / "report.json"

    def build_graphs(
        self,
        *,
        root_packages: int = 3,
        fuzz_packages: int = 5,
        root_ignores: list[str] | None = None,
        fuzz_ignores: list[str] | None = None,
        fixture: dict | None = None,
        fuzz_config: bool = True,
    ) -> None:
        root_ignores = root_ignores or []
        fuzz_ignores = fuzz_ignores or []
        write_lock(self.repository / "Cargo.lock", root_packages)
        write_lock(self.repository / "fuzz" / "Cargo.lock", fuzz_packages)
        write_config(self.repository / ".cargo" / "audit.toml", root_ignores)
        if fuzz_config:
            write_config(
                self.repository / "fuzz" / ".cargo" / "audit.toml", fuzz_ignores
            )
        self.report.write_text(json.dumps(fixture or {}))

    def audit(self) -> list[audit_lockfiles.GraphResult]:
        with mock.patch.dict(
            os.environ,
            {
                "AUDIT_FIXTURE_CALLS": str(self.calls),
                "AUDIT_FIXTURE_REPORT": str(self.report),
            },
        ):
            return audit_lockfiles.audit_repository(self.repository, str(self.fake))

    def recorded_calls(self) -> list[dict]:
        if not self.calls.exists():
            return []
        return [
            json.loads(line)
            for line in self.calls.read_text().splitlines()
            if line.strip()
        ]

    def test_selects_each_graph_with_its_own_lock_and_config(self) -> None:
        self.build_graphs(
            root_packages=3,
            fuzz_packages=5,
            root_ignores=["RUSTSEC-ROOT"],
            fuzz_ignores=["RUSTSEC-FUZZ"],
        )

        results = self.audit()

        self.assertTrue(all(result.error is None for result in results), results)
        by_name = {result.name: result for result in results}
        # The two graphs genuinely differ, so a matching count proves which lock
        # cargo-audit actually scanned.
        self.assertEqual(by_name["root"].packages, 3)
        self.assertEqual(by_name["fuzz"].packages, 5)
        self.assertEqual(by_name["root"].lockfile, "Cargo.lock")
        self.assertEqual(by_name["fuzz"].lockfile, "fuzz/Cargo.lock")

        calls = {Path(call["cwd"]).name: call for call in self.recorded_calls()}
        self.assertEqual(
            Path(calls["repo"]["file"]), self.repository / "Cargo.lock"
        )
        self.assertEqual(
            Path(calls["fuzz"]["file"]), self.repository / "fuzz" / "Cargo.lock"
        )
        self.assertEqual(calls["repo"]["cwd"], str(self.repository))
        self.assertEqual(calls["fuzz"]["cwd"], str(self.repository / "fuzz"))
        # Each graph gets only its own scoped exceptions.
        self.assertEqual(calls["repo"]["ignore"], ["RUSTSEC-ROOT"])
        self.assertEqual(calls["fuzz"]["ignore"], ["RUSTSEC-FUZZ"])

    def test_fuzz_only_finding_fails_while_root_stays_green(self) -> None:
        self.build_graphs(
            fixture={
                str((self.repository / "fuzz" / "Cargo.lock")): {
                    "vulnerabilities": ["RUSTSEC-FUZZ-ONLY"]
                }
            }
        )

        results = {result.name: result for result in self.audit()}

        self.assertIsNone(results["root"].error)
        self.assertIsNotNone(results["fuzz"].error)
        self.assertIn("RUSTSEC-FUZZ-ONLY", results["fuzz"].error)

    def test_root_finding_is_reported_for_the_root_graph(self) -> None:
        self.build_graphs(
            fixture={
                str(self.repository / "Cargo.lock"): {
                    "vulnerabilities": ["RUSTSEC-ROOT-ONLY"]
                }
            }
        )

        results = {result.name: result for result in self.audit()}

        self.assertIsNotNone(results["root"].error)
        self.assertIn("RUSTSEC-ROOT-ONLY", results["root"].error)
        self.assertIsNone(results["fuzz"].error)

    def test_root_exception_does_not_cover_a_fuzz_finding(self) -> None:
        # The root graph ignores the advisory, but the fuzz graph has not granted
        # an exception, so the fuzz finding must still fail the audit.
        self.build_graphs(
            root_ignores=["RUSTSEC-2026-0235"],
            fuzz_ignores=[],
            fixture={
                str(self.repository / "fuzz" / "Cargo.lock"): {
                    "vulnerabilities": ["RUSTSEC-2026-0235"]
                }
            },
        )

        results = {result.name: result for result in self.audit()}

        self.assertIsNone(results["root"].error)
        self.assertIsNotNone(results["fuzz"].error)
        self.assertIn("RUSTSEC-2026-0235", results["fuzz"].error)
        calls = {Path(call["cwd"]).name: call for call in self.recorded_calls()}
        # The root exception was never forwarded to the fuzz invocation.
        self.assertEqual(calls["fuzz"]["ignore"], [])

    def test_wrong_scanned_graph_is_rejected(self) -> None:
        # Simulate cargo-audit ignoring --file and scanning the root lock: the
        # report's count no longer matches the requested fuzz lock.
        self.build_graphs(
            root_packages=3,
            fuzz_packages=5,
            fixture={
                str(self.repository / "fuzz" / "Cargo.lock"): {"count": 3}
            },
        )

        results = {result.name: result for result in self.audit()}

        self.assertIsNotNone(results["fuzz"].error)
        self.assertIn("different graph", results["fuzz"].error)

    def test_missing_fuzz_config_fails_closed(self) -> None:
        self.build_graphs(fuzz_config=False)

        results = {result.name: result for result in self.audit()}

        self.assertIsNotNone(results["fuzz"].error)
        self.assertIn("audit.toml", results["fuzz"].error)

    def test_checked_in_workflow_runs_the_helper_and_fuzz_config_exists(self) -> None:
        workflow = (REPOSITORY / ".github" / "workflows" / "ci.yml").read_text()
        self.assertIn("python3 scripts/audit_lockfiles.py", workflow)
        self.assertIn("python3 scripts/test_audit_lockfiles.py", workflow)

        fuzz_graph = audit_lockfiles.graph_specs(REPOSITORY)[1]
        self.assertEqual(
            fuzz_graph.config, REPOSITORY / "fuzz" / ".cargo" / "audit.toml"
        )
        self.assertTrue(fuzz_graph.config.is_file())
        self.assertTrue(fuzz_graph.lockfile.is_file())


if __name__ == "__main__":
    unittest.main()
