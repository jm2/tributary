#!/usr/bin/env python3
"""Audit the root and fuzz Cargo.lock graphs as separate security boundaries."""

# The fuzz crate is a separate Cargo workspace that owns `fuzz/Cargo.lock`, so
# it is a separate dependency graph with its own scoped advisory exceptions.
# Syncing the two locks (`scripts/sync_fuzz_lock.py`) proves shared production-
# direct versions stay coherent; it does not prove that both graphs were
# security audited. A vulnerable package that exists only in the fuzz graph can
# therefore hide behind a green root audit. See docs/dependency-updates.md
# ("Security audit boundaries") for the policy.
#
# This helper audits each graph on its own terms:
#
# * the graph's own lockfile is passed explicitly with `--file`, and the working
#   directory is the graph directory, so cargo-audit cannot silently fall back
#   to a different lock;
# * the graph's own `.cargo/audit.toml` `[advisories].ignore` list is read here
#   and passed with `--ignore`, so an exception granted to one graph can never
#   suppress a finding in the other;
# * the JSON report is validated before it is trusted: its scanned dependency
#   count must match the requested lockfile, its applied ignore set must match
#   the graph's scoped exceptions, and it must report no remaining
#   vulnerabilities;
# * a nonzero auditor exit status fails its graph even when the emitted report
#   looks clean, and structurally incomplete or malformed vulnerability results
#   are rejected instead of being read as zero findings.
#
# `root` describes the repository's production lock and `fuzz` the independent
# fuzz workspace lock. The command exits non-zero if either graph fails.

from __future__ import annotations

import argparse
import dataclasses
import json
import shutil
# Subprocesses below use explicit argv with no shell.
import subprocess  # nosec B404
import sys
import tomllib
from pathlib import Path
from typing import Any


REPOSITORY = Path(__file__).resolve().parents[1]
DEFAULT_AUDIT_BIN = "cargo-audit"


class AuditError(RuntimeError):
    """One graph could not be audited or did not pass."""


@dataclasses.dataclass(frozen=True)
class Graph:
    """One independently audited dependency graph."""

    name: str
    repository: Path
    directory: Path
    lockfile: Path
    config: Path


@dataclasses.dataclass
class GraphResult:
    """Validated evidence for one audited graph."""

    name: str
    lockfile: str
    packages: int
    ignores: list[str]
    vulnerabilities: list[str]
    error: str | None = None


def graph_specs(repository: Path) -> tuple[Graph, ...]:
    return (
        Graph(
            name="root",
            repository=repository,
            directory=repository,
            lockfile=repository / "Cargo.lock",
            config=repository / ".cargo" / "audit.toml",
        ),
        Graph(
            name="fuzz",
            repository=repository,
            directory=repository / "fuzz",
            lockfile=repository / "fuzz" / "Cargo.lock",
            config=repository / "fuzz" / ".cargo" / "audit.toml",
        ),
    )


def load_ignores(config: Path) -> list[str]:
    """Read the scoped `[advisories].ignore` list from one graph's config."""
    try:
        data = tomllib.loads(config.read_text())
    except OSError as error:
        raise AuditError(f"cannot read {config}: {error}") from error
    except tomllib.TOMLDecodeError as error:
        raise AuditError(f"invalid TOML in {config}: {error}") from error
    advisories = data.get("advisories")
    if not isinstance(advisories, dict):
        raise AuditError(f"{config} is missing the [advisories] table")
    ignores = advisories.get("ignore", [])
    if not isinstance(ignores, list) or not all(
        isinstance(item, str) for item in ignores
    ):
        raise AuditError(f"{config} [advisories].ignore must be a list of ids")
    return ignores


def count_packages(lockfile: Path) -> int:
    """Count `[[package]]` entries, matching cargo-audit's lockfile count."""
    try:
        data = tomllib.loads(lockfile.read_text())
    except OSError as error:
        raise AuditError(f"cannot read {lockfile}: {error}") from error
    except tomllib.TOMLDecodeError as error:
        raise AuditError(f"invalid TOML in {lockfile}: {error}") from error
    packages = data.get("package", [])
    if not isinstance(packages, list):
        raise AuditError(f"{lockfile} has no package list")
    return len(packages)


def scan_command(graph: Graph, audit_bin: str, ignores: list[str]) -> list[str]:
    command = [
        audit_bin,
        "audit",
        "--json",
        "--file",
        str(graph.lockfile),
    ]
    for advisory_id in ignores:
        command += ["--ignore", advisory_id]
    return command


def parse_report(stdout: str, stderr: str) -> dict[str, Any]:
    try:
        report = json.loads(stdout)
    except json.JSONDecodeError as error:
        detail = stderr.strip().splitlines()
        tail = detail[-1] if detail else "no stderr"
        raise AuditError(
            f"cargo-audit did not emit a JSON report ({error}); last stderr: {tail}"
        ) from error
    if not isinstance(report, dict):
        raise AuditError("cargo-audit JSON report is not an object")
    return report


def run_scan(
    graph: Graph, audit_bin: str, ignores: list[str]
) -> subprocess.CompletedProcess[str]:
    """Run the auditor for one graph with its explicit lock and exceptions."""
    command = scan_command(graph, audit_bin, ignores)
    try:
        completed = subprocess.run(  # nosec B603 and nosemgrep (explicit argv, no shell)
            command,
            cwd=graph.directory,
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError as error:
        raise AuditError(f"cannot run {audit_bin!r}: {error}") from error
    return completed


def ensure_requested_lock_was_scanned(
    report: dict[str, Any], graph: Graph, expected_packages: int
) -> None:
    """Reject a report whose dependency count proves a different graph."""
    scanned = report.get("lockfile")
    scanned_count = scanned.get("dependency-count") if isinstance(scanned, dict) else None
    if scanned_count == expected_packages:
        return
    raise AuditError(
        "cargo-audit scanned a different graph: "
        f"requested {graph.lockfile} with {expected_packages} packages, "
        f"but the report covered {scanned_count}"
    )


def ensure_applied_ignores_match(report: dict[str, Any], ignores: list[str]) -> None:
    """Reject a report whose applied ignore set differs from the scoped list."""
    settings = report.get("settings")
    applied = settings.get("ignore", []) if isinstance(settings, dict) else []
    if isinstance(applied, list) and set(applied) == set(ignores):
        return
    raise AuditError(
        f"scoped exceptions did not apply as written: expected {ignores}, "
        f"cargo-audit reported {applied}"
    )


def vulnerability_ids(report: dict[str, Any]) -> list[str]:
    """Extract advisory ids, rejecting malformed or self-contradictory results."""
    vulnerabilities = report.get("vulnerabilities")
    if not isinstance(vulnerabilities, dict):
        raise AuditError("cargo-audit report is missing the vulnerabilities section")
    entries = vulnerabilities.get("list")
    if not isinstance(entries, list):
        raise AuditError(
            "cargo-audit report vulnerabilities.list is missing or not a list"
        )
    ids: list[str] = []
    for entry in entries:
        advisory = entry.get("advisory") if isinstance(entry, dict) else None
        advisory_id = advisory.get("id") if isinstance(advisory, dict) else None
        if not isinstance(advisory_id, str):
            raise AuditError(
                "cargo-audit report has a malformed vulnerability entry: "
                f"{entry!r}"
            )
        ids.append(advisory_id)
    count = vulnerabilities.get("count")
    if not isinstance(count, int) or isinstance(count, bool) or count != len(ids):
        raise AuditError(
            f"cargo-audit report vulnerabilities.count {count!r} does not match "
            f"its {len(ids)} entries: {sorted(ids)}"
        )
    if count:
        raise AuditError(f"unhandled vulnerabilities reported: {sorted(ids)}")
    return ids


def ensure_auditor_succeeded(
    completed: subprocess.CompletedProcess[str], graph: Graph
) -> None:
    """Fail the graph on a nonzero auditor status despite a clean report."""
    if completed.returncode == 0:
        return
    detail = completed.stderr.strip().splitlines()
    tail = detail[-1] if detail else "no stderr"
    raise AuditError(
        f"cargo-audit for graph {graph.name} ({graph.lockfile}) exited with "
        f"status {completed.returncode} while reporting no unhandled "
        f"vulnerabilities; last stderr: {tail}"
    )


def audit_graph(graph: Graph, audit_bin: str) -> GraphResult:
    """Audit one graph and validate the report proves the intended scan."""
    ignores = load_ignores(graph.config)
    if not graph.lockfile.is_file():
        raise AuditError(f"missing lockfile {graph.lockfile}")
    expected_packages = count_packages(graph.lockfile)

    completed = run_scan(graph, audit_bin, ignores)
    report = parse_report(completed.stdout, completed.stderr)

    # Report content is validated before the exit status: a graph that found
    # vulnerabilities must name them even when the auditor also failed, while a
    # failing auditor is never promoted to a green result by its own JSON
    # output.
    ensure_requested_lock_was_scanned(report, graph, expected_packages)
    ensure_applied_ignores_match(report, ignores)
    ids = vulnerability_ids(report)
    ensure_auditor_succeeded(completed, graph)

    return GraphResult(
        name=graph.name,
        lockfile=str(graph.lockfile.relative_to(graph.repository)),
        packages=expected_packages,
        ignores=ignores,
        vulnerabilities=ids,
    )


def audit_repository(
    repository: Path, audit_bin: str = DEFAULT_AUDIT_BIN
) -> list[GraphResult]:
    """Audit every graph, returning evidence even when one graph fails."""
    results: list[GraphResult] = []
    for graph in graph_specs(repository):
        try:
            results.append(audit_graph(graph, audit_bin))
        except AuditError as error:
            results.append(
                GraphResult(
                    name=graph.name,
                    lockfile=str(graph.lockfile.relative_to(graph.repository)),
                    packages=0,
                    ignores=[],
                    vulnerabilities=[],
                    error=str(error),
                )
            )
    return results


def format_result(result: GraphResult) -> str:
    if result.error is not None:
        return f"FAIL {result.name}: {result.error}"
    return (
        f"OK   {result.name}: lockfile={result.lockfile} "
        f"packages={result.packages} ignores={result.ignores} vulnerabilities=0"
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--audit-bin",
        default=DEFAULT_AUDIT_BIN,
        help="cargo-audit executable to run (default: %(default)s)",
    )
    parser.add_argument(
        "--repository",
        type=Path,
        default=REPOSITORY,
        help="repository root containing Cargo.lock and fuzz/",
    )
    arguments = parser.parse_args(argv)

    if shutil.which(arguments.audit_bin) is None and not Path(
        arguments.audit_bin
    ).is_file():
        print(
            f"error: {arguments.audit_bin!r} is not on PATH; "
            "install it with `cargo install cargo-audit --locked`",
            file=sys.stderr,
        )
        return 1

    results = audit_repository(arguments.repository, arguments.audit_bin)
    failed = False
    for result in results:
        print(format_result(result))
        failed = failed or result.error is not None
    if failed:
        print(
            "error: at least one dependency graph failed its security audit",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
