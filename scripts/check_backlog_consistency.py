#!/usr/bin/env python3
"""Read-only consistency checks for the Tributary implementation backlog."""
# docs/task.md is the repository's countable execution index.  Several of its
# invariants are maintained by hand and were previously unenforced:
#
# * every top-level checkbox is one record with a unique stable ID;
# * the literal completion counters written in prose match the checkbox state,
#   including the archived remediation counter, which is recounted from its
#   archived source document;
# * every relative link (and #anchor) resolves inside the checkout;
# * each active record maps to a GitHub issue, a Gas City bead, and a pull
#   request ("pr": null meaning "not yet published"), with no
#   merged-but-unreconciled record and no stale review head.
#
# This module is deliberately READ-ONLY.  It never edits the index, never
# closes a parent record, never assigns a worker, and never talks to GitHub or
# the Gas City ledger.  A caller may pass an optional ledger snapshot
# (--ledger); the checker then only *reports* missing mappings,
# merged-but-unreconciled records, and stale review heads.  Acting on those
# findings stays with the operator and the authoritative ledgers.
#
# Usage:
#     python3 scripts/check_backlog_consistency.py
#     python3 scripts/check_backlog_consistency.py --ledger path/to/snapshot.json
#
# Exit status is 0 when every check passes, 1 when any check fails, and 2
# when a supplied file (the task index or the ledger snapshot) is missing.

from __future__ import annotations

import argparse
import json
import re
import subprocess  # nosec B404 - index-authoritative git ls-files, fixed argv
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

from backlog_markdown import (
    HEADING,
    document_anchors,
    iter_content_lines,
    iter_link_targets,
    split_target,
)

REPOSITORY = Path(__file__).resolve().parent.parent
TASK_INDEX_NAME = "docs/task.md"
# The archived remediation counter is recounted from this source document.
ARCHIVED_INDEX_NAME = "docs/task-remediation-2026-07.md"

# One countable record: a top-level checkbox whose title begins with a bold
# stable ID (for example ``- [x] **P1.5-A** — ...`` or ``- [ ] **R1 — ...**``).
RECORD_PATTERN = re.compile(r"^- \[(?P<mark>[ xX])\] \*\*(?P<id>[^*\s]+)")

# Any top-level checkbox, regardless of title shape.  A checkbox that does
# not also match RECORD_PATTERN is not a countable record and must be
# reported instead of silently omitted.
CHECKBOX_PATTERN = re.compile(r"^- \[(?P<mark>[ xX])\]")

PERCENT_COUNTER = re.compile(
    r"\*\*(?P<complete>\d+)/(?P<total>\d+)\s*\((?P<percent>\d+(?:\.\d+)?)%\)\*\*"
)

# Prose counters, anchored on stable phrasing so a wording change fails loudly
# instead of silently skipping the check.
COUNTER_PATTERNS = (
    ("overall", re.compile(
        r"Current status:\s*\*\*(?P<complete>\d+)/(?P<total>\d+)\s*"
        r"\((?P<percent>\d+(?:\.\d+)?)%\)\*\*")),
    ("baseline", re.compile(
        r"retained baseline is\s*\*\*(?P<complete>\d+)/(?P<total>\d+)\*\*")),
    ("corrective", re.compile(
        r"with\s*\*\*(?P<complete>\d+)/(?P<total>\d+)\*\*\s+new corrective")),
    ("engineering", re.compile(
        r"and\s*\*\*(?P<complete>\d+)/(?P<total>\d+)\*\*\s+engineering records complete")),
)

# The archived remediation counter is anchored on its own stable phrasing.
# Unlike the active counters it is validated against a mechanical recount of
# the archived source document, not merely for arithmetic.
ARCHIVED_COUNTER_PATTERN = re.compile(
    r"archived\s+remediation[^*]*remains\s*\*\*(?P<complete>\d+)/(?P<total>\d+)\s*"
    r"\((?P<percent>\d+(?:\.\d+)?)%\)\*\*"
)

# One checkbox in the archived remediation source.  Unlike the active index,
# archived boxes carry plain titles instead of bold stable IDs.
ARCHIVED_BOX_PATTERN = re.compile(r"^- \[(?P<mark>[ xX])\]")

# Directories that never contain tracked documentation worth checking: VCS
# metadata, build output, and scratch/operations trees.
SKIPPED_DIRECTORIES = frozenset({".git", ".gc", "target", "node_modules", "dist"})


@dataclass(frozen=True)
class Record:
    """One countable checklist record from the task index."""

    identifier: str
    complete: bool
    line: int


def relative_display(root: Path, path: Path) -> str:
    """Render *path* relative to *root* when possible, else absolute."""
    try:
        return str(path.relative_to(root))
    except ValueError:
        return str(path)


def parse_records(text: str) -> list[Record]:
    """Return every countable checkbox record in document order."""
    records: list[Record] = []
    for number, line in iter_content_lines(text):
        match = RECORD_PATTERN.match(line)
        if match:
            records.append(Record(
                identifier=match.group("id"),
                complete=match.group("mark").lower() == "x",
                line=number,
            ))
    return records


def check_record_shapes(text: str, task_index: Path) -> list[str]:
    """Report top-level checkboxes that are not countable stable-ID records."""
    # A top-level checkbox without the bold stable-ID prefix is silently
    # skipped by parse_records (it is not a countable record), which would
    # let a malformed or missing ID hide from the counters and the mappings.
    # Report it with its source line so the omission is loud.
    display = task_index.name
    problems: list[str] = []
    for number, line in iter_content_lines(text):
        if CHECKBOX_PATTERN.match(line) and not RECORD_PATTERN.match(line):
            problems.append(
                f"record: {display}:{number}: top-level checkbox has no bold "
                f"stable ID and is not a countable record: {line.strip()!r}")
    return problems


def _walked_markdown_files(root: Path) -> list[Path]:
    """Walk *root* and return every Markdown file worth link-checking."""
    # A recursive walk with a fixed skip list keeps this a pure-Python, static
    # operation: no subprocess, no shell, no PATH dependence.  The skip list
    # covers VCS metadata, build output and scratch/operations trees.  This is
    # the documented fallback for non-Git synthetic/tarball roots, where no
    # index exists to define the tracked file set.
    return sorted(
        path
        for path in root.rglob("*.md")
        if not SKIPPED_DIRECTORIES.intersection(path.relative_to(root).parts)
    )


def _tracked_markdown_files(root: Path) -> list[Path] | None:
    """Return index-authoritative Markdown files, or ``None`` outside a Git checkout."""
    # In a Git checkout the index decides which files are tracked; an
    # untracked scratch file must not fail validation.  The Git invocation is
    # safe and statically defined: a fixed argument-vector command, no shell,
    # no user-supplied input beyond the checkout root.
    if not (root / ".git").exists():
        return None
    try:
        completed = subprocess.run(  # nosec B603, B607 - fixed argv, no shell
            ["git", "-C", str(root), "ls-files", "-z", "--", "*.md"],
            capture_output=True, check=False, timeout=60,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if completed.returncode != 0:
        return None
    listing = completed.stdout.decode("utf-8", errors="surrogateescape")
    return sorted(root / relative for relative in listing.split("\0") if relative)


def collect_markdown_files(root: Path) -> list[Path]:
    """Return every Markdown file below *root* that is worth link-checking."""
    tracked = _tracked_markdown_files(root)
    if tracked is not None:
        return tracked
    return _walked_markdown_files(root)


def check_unique_ids(records: Sequence[Record], root: Path, task_index: Path) -> list[str]:
    """Report every stable ID that is defined more than once."""
    locations: dict[str, list[int]] = {}
    for record in records:
        locations.setdefault(record.identifier, []).append(record.line)
    display = relative_display(root, task_index)
    problems: list[str] = []
    for identifier, lines in locations.items():
        if len(lines) > 1:
            rendered = ", ".join(f"{display}:{line}" for line in lines)
            problems.append(
                f"unique-id: record ID '{identifier}' is defined "
                f"{len(lines)} times ({rendered})")
    return problems


def derived_counts(records: Sequence[Record]) -> dict[str, tuple[int, int]]:
    """Return ``label -> (complete, total)`` for each counted record family."""
    baseline = [r for r in records if not re.match(r"^[RQ]\d+$", r.identifier)]
    corrective = [r for r in records if re.match(r"^R\d+$", r.identifier)]
    engineering = [r for r in records if re.match(r"^Q\d+$", r.identifier)]

    def pair(items: Sequence[Record]) -> tuple[int, int]:
        return sum(1 for item in items if item.complete), len(items)

    return {
        "overall": pair(records),
        "baseline": pair(baseline),
        "corrective": pair(corrective),
        "engineering": pair(engineering),
    }


def _counter_search_text(text: str) -> str:
    """Return the fenced-block-free text that counter patterns may search."""
    # Counters are prose, so they are searched on the same filtered view the
    # record parser uses: a fenced code block holds syntax examples, not
    # progress, and must never be selected as the counter nor reported as an
    # arithmetic inconsistency.
    return "\n".join(line for _, line in iter_content_lines(text))


def check_counters(text: str, records: Sequence[Record]) -> list[str]:
    """Report prose counters that disagree with the checkbox state or arithmetic."""
    searchable = _counter_search_text(text)
    normalized = re.sub(r"\s+", " ", searchable)
    expected = derived_counts(records)
    problems: list[str] = []

    for label, pattern in COUNTER_PATTERNS:
        match = pattern.search(normalized)
        if match is None:
            problems.append(
                f"counter: could not find the {label} completion counter; "
                "if the wording changed, update COUNTER_PATTERNS")
            continue
        complete = int(match.group("complete"))
        total = int(match.group("total"))
        want_complete, want_total = expected[label]
        if (complete, total) != (want_complete, want_total):
            problems.append(
                f"counter: {label} says {complete}/{total} but the index "
                f"contains {want_complete}/{want_total} completed records")
        stated = match.groupdict().get("percent")
        if stated is not None:
            computed = round(want_complete / want_total * 100, 1) if want_total else 0.0
            if abs(float(stated) - computed) > 0.05:
                problems.append(
                    f"counter: {label} states {stated}% but "
                    f"{want_complete}/{want_total} rounds to {computed}%")

    # Every explicit percentage must at least be arithmetically consistent.
    # The archived remediation counter additionally gets a mechanical recount
    # against its source document in check_archived_counter.  The scan runs on
    # the filtered prose: a fenced example such as ``**1/3 (50.0%)**`` is
    # documentation, not a stated progress figure.
    for match in PERCENT_COUNTER.finditer(searchable):
        complete = int(match.group("complete"))
        total = int(match.group("total"))
        stated = float(match.group("percent"))
        computed = round(complete / total * 100, 1) if total else 0.0
        if abs(stated - computed) > 0.05:
            problems.append(
                f"counter: '{complete}/{total} ({stated}%)' is arithmetically "
                f"inconsistent (rounds to {computed}%)")
    return problems


def _archived_section_flags(title: str | None) -> tuple[bool, bool]:
    """Return ``(in_summary, in_gate)`` for one archived-document heading."""
    lowered = (title or "").lower()
    return lowered.startswith("how to use"), "global validation" in lowered


def _is_excluded_archived_box(line: str, in_summary: bool, in_gate: bool) -> bool:
    """Return whether an archived checkbox is one of the documented exclusions."""
    # Status-summary boxes, global-validation gate boxes and withdrawn false
    # findings are all documented in the archive as non-task boxes.
    if in_summary or in_gate:
        return True
    return "~~" in line and "withdrawn" in line.lower()


def _collect_archived_boxes(text: str) -> tuple[int, int, dict[str, int]]:
    """Count the in-scope archived checkboxes of the remediation source."""
    # Returns ``(complete, total, unclassified)`` where *unclassified* maps
    # each section title that holds boxes outside the P0-P3 task sections and
    # the documented exclusions to its box count.
    complete = 0
    total = 0
    unclassified: dict[str, int] = {}
    in_summary = False
    in_gate = False
    section: str | None = None
    for _, line in iter_content_lines(text):
        heading = HEADING.match(line)
        if heading:
            section = heading.group("title").strip()
            in_summary, in_gate = _archived_section_flags(section)
            continue
        match = ARCHIVED_BOX_PATTERN.match(line)
        if match is None:
            continue
        if _is_excluded_archived_box(line, in_summary, in_gate):
            continue
        if section is None or not re.match(r"^P[0-3]\b", section):
            key = section or "<top>"
            unclassified[key] = unclassified.get(key, 0) + 1
            continue
        total += 1
        if match.group("mark").lower() == "x":
            complete += 1
    return complete, total, unclassified


def derive_archived_counts(path: Path) -> tuple[int, int, list[str]]:
    """Mechanically recount the archived remediation checkboxes."""
    # The archived counter describes the in-scope task checkboxes of the
    # archived remediation document.  That document's prose documents the
    # exclusions, which this recount applies mechanically:
    #
    # * the status-summary boxes in the "How to use this file" section are
    #   section summaries, not task progress;
    # * every box in the "Global validation gate" section is a gate, not a
    #   task;
    # * a struck-through (``~~...~~``) box marked "Withdrawn" is a retracted
    #   false finding.
    #
    # Every other top-level checkbox under a ``P0``-``P3`` section is in
    # scope.  A checkbox under any other heading is unclassifiable and is
    # reported as a structural problem instead of being silently ignored.
    # Returns ``(complete, total, problems)``.
    display = path.name
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:  # pragma: no cover - unreadable checkout file
        return 0, 0, [
            f"counter: archived remediation source {display} is unreadable ({error})"]
    complete, total, unclassified = _collect_archived_boxes(text)
    problems: list[str] = []
    for title, count in sorted(unclassified.items()):
        problems.append(
            f"counter: {display} has {count} checkbox(es) under section "
            f"'{title}', which is neither a P0-P3 task section nor a documented "
            "exclusion; the archived recount cannot classify them")
    return complete, total, problems


def check_archived_counter(text: str, root: Path) -> list[str]:
    """Report drift between the archived counter prose and its source boxes."""
    # Like the active counters, the archived counter is searched on the
    # filtered prose: a fenced example of the counter wording is
    # documentation and must not be selected as the counter.
    match = ARCHIVED_COUNTER_PATTERN.search(_counter_search_text(text))
    if match is None:
        missing = (
            "counter: could not find the archived remediation counter; "
            "if the wording changed, update ARCHIVED_COUNTER_PATTERN"
        )
        return [missing]
    source = root / ARCHIVED_INDEX_NAME
    if not source.is_file():
        return [f"counter: archived remediation source {ARCHIVED_INDEX_NAME} is missing"]
    problems: list[str] = []
    complete, total, structural = derive_archived_counts(source)
    problems.extend(structural)
    stated_complete = int(match.group("complete"))
    stated_total = int(match.group("total"))
    if (stated_complete, stated_total) != (complete, total):
        problems.append(
            f"counter: archived remediation says {stated_complete}/{stated_total} "
            f"but {ARCHIVED_INDEX_NAME} contains {complete}/{total} in-scope "
            "task checkboxes")
    stated = float(match.group("percent"))
    computed = round(complete / total * 100, 1) if total else 0.0
    if abs(stated - computed) > 0.05:
        problems.append(
            f"counter: archived remediation states {stated}% but "
            f"{complete}/{total} rounds to {computed}%")
    return problems


def _check_link_target(
    root: Path,
    source: Path,
    number: int,
    target: str,
    anchor_cache: dict[Path, set[str]],
) -> list[str]:
    """Report one relative link target (and ``#anchor``) that does not resolve."""
    problems: list[str] = []
    path_part, fragment = split_target(target)
    if path_part is None:
        return problems
    relative = relative_display(root, source)
    target_path = (source if not path_part else source.parent / path_part).resolve()
    try:
        target_path.relative_to(root)
    except ValueError:
        # ``resolve`` fully normalizes ``..`` segments and symlinks, so a
        # target that lands outside *root* is an escape (including a symlink
        # that points out of the checkout), not a valid in-tree reference.
        problems.append(
            f"link: {relative}:{number}: link target '{target}' resolves outside "
            f"the repository root ({relative_display(root, target_path)})")
        return problems
    if not target_path.exists():
        problems.append(f"link: {relative}:{number}: broken link target '{target}'")
        return problems
    if fragment and target_path.suffix.lower() == ".md":
        anchors = anchor_cache.get(target_path)
        if anchors is None:
            anchors = document_anchors(target_path)
            anchor_cache[target_path] = anchors
        if fragment not in anchors:
            problems.append(
                f"link: {relative}:{number}: missing anchor '#{fragment}' in "
                f"{relative_display(root, target_path)}")
    return problems


def check_links(root: Path, markdown_files: Sequence[Path]) -> list[str]:
    """Report relative link targets and ``#anchors`` that do not resolve."""
    problems: list[str] = []
    anchor_cache: dict[Path, set[str]] = {}

    for path in markdown_files:
        try:
            text = path.read_text(encoding="utf-8")
        except OSError as error:  # pragma: no cover - unreadable checkout file
            problems.append(f"link: {relative_display(root, path)} is unreadable ({error})")
            continue
        for number, line in iter_content_lines(text):
            for target in iter_link_targets(line):
                problems.extend(
                    _check_link_target(root, path, number, target, anchor_cache))
    return problems


def _short(sha: str) -> str:
    """Render a Git object name compactly for diagnostics."""
    return sha[:12] if len(sha) > 12 else sha


def _pr_evidence(entry: dict) -> tuple[str, object]:
    """Classify the ``pr`` mapping of a ledger entry for one record."""
    # Returns a ``(state, value)`` pair where *state* is one of:
    #
    # * ``"missing"`` - the ``pr`` key is absent.  Silence is not a published
    #   representation of "no pull request"; it is a missing mapping.
    # * ``"none"`` - the key is explicitly ``null``, the documented
    #   not-yet-published representation.
    # * ``"invalid"`` - present but not a positive PR identifier.
    # * ``"published"`` - a usable pull-request identifier.
    if "pr" not in entry:
        return "missing", None
    value = entry["pr"]
    if value is None:
        return "none", None
    if isinstance(value, bool):
        return "invalid", value
    if isinstance(value, int) and value > 0:
        return "published", value
    if isinstance(value, str) and value.isdigit() and int(value) > 0:
        return "published", value
    return "invalid", value


def _check_active_flag(identifier: str, entry: dict, active: bool) -> list[str]:
    """Report a snapshot ``active`` flag that disagrees with the index state."""
    if "active" not in entry or bool(entry["active"]) == active:
        return []
    state_word = "active" if active else "complete"
    drift = (
        f"ledger: record '{identifier}' snapshot flag "
        f"active={entry['active']} disagrees with the index state "
        f"({state_word}); the index decides")
    return [drift]


def _check_active_mapping(identifier: str, entry: dict) -> list[str]:
    """Report missing bead, issue and pull-request mapping fields of one record."""
    # The record is index-derived active.  ``"pr": null`` is the documented
    # not-yet-published representation; a published ``pr`` (a positive
    # integer or digit string) must carry ``head_sha`` evidence for the
    # stale-review check in _check_entry_state.
    problems: list[str] = []
    if not entry.get("bead"):
        problems.append(f"ledger: active record '{identifier}' has no bead mapping")
    if not entry.get("issue"):
        problems.append(f"ledger: active record '{identifier}' has no issue mapping")
    state, value = _pr_evidence(entry)
    if state == "missing":
        problems.append(
            f"ledger: active record '{identifier}' has no pr mapping "
            "(use null for 'not yet published')")
    elif state == "invalid":
        problems.append(
            f"ledger: active record '{identifier}' has an invalid pr "
            f"mapping ({value!r})")
    elif state == "published" and not entry.get("head_sha"):
        problems.append(
            f"ledger: active record '{identifier}' references PR {value} "
            "without head_sha evidence")
    return problems


def _check_entry_state(
    identifier: str, entry: dict, record: Record, task_index: Path
) -> list[str]:
    """Report a stale review head and merged-but-unreconciled record state."""
    problems: list[str] = []
    head = entry.get("head_sha")
    reviewed = entry.get("reviewed_sha")
    if head and reviewed and head != reviewed:
        problems.append(
            f"ledger: record '{identifier}' has a stale review head "
            f"(reviewed {_short(str(reviewed))} != head {_short(str(head))})")
    if entry.get("merged") and not record.complete:
        problems.append(
            f"ledger: record '{identifier}' is merged but still unchecked in "
            f"{task_index.name} (merged-but-unreconciled)")
    return problems


def _check_unmapped_records(
    records: Sequence[Record], entries: dict[str, object]
) -> list[str]:
    """Report index-active records that have no snapshot entry at all."""
    return [
        f"ledger: active record '{record.identifier}' has no mapping entry"
        for record in records
        if not record.complete and record.identifier not in entries
    ]


def check_ledger(
    snapshot: object, records: Sequence[Record], task_index: Path
) -> list[str]:
    """Report mapping problems from an optional read-only ledger snapshot."""
    # The snapshot is a JSON object of the shape::
    #
    #     {
    #       "records": {
    #         "Q7": {
    #           "issue": 276,
    #           "bead": "tr-bps4d",
    #           "pr": 999,
    #           "head_sha": "abc123",
    #           "reviewed_sha": "def456",
    #           "merged": false
    #         }
    #       }
    #     }
    #
    # Every active (unchecked) record must appear with an issue, a bead, and
    # a pull-request mapping.  ``"pr": null`` is the explicit
    # not-yet-published representation; omitting the ``pr`` key is reported
    # as a missing mapping.  A published ``pr`` must carry ``head_sha``
    # evidence, which the stale-review check validates.
    #
    # The task index is authoritative for checked state: an ``active``
    # snapshot flag that disagrees with the index is reported and never
    # downgrades the required-field checks.  ``merged`` on an active record
    # is a merged-but-unreconciled report, and ``reviewed_sha`` differing
    # from ``head_sha`` is a stale review head.  Completed records may be
    # omitted.
    if not isinstance(snapshot, dict) or not isinstance(snapshot.get("records"), dict):
        return ["ledger: snapshot must be an object with a 'records' object"]
    entries: dict[str, object] = snapshot["records"]
    by_id = {record.identifier: record for record in records}
    problems: list[str] = []

    for identifier, entry in sorted(entries.items()):
        record = by_id.get(identifier)
        if record is None:
            problems.append(f"ledger: mapping references unknown record ID '{identifier}'")
            continue
        if not isinstance(entry, dict):
            problems.append(f"ledger: mapping for '{identifier}' must be an object")
            continue
        # The task index decides whether a record is active.  A snapshot
        # ``active`` flag may not silently override it: the required-field
        # checks below always run on the index-derived state, and a flag that
        # contradicts the index is itself reportable drift.
        active = not record.complete
        problems.extend(_check_active_flag(identifier, entry, active))
        if active:
            problems.extend(_check_active_mapping(identifier, entry))
        problems.extend(_check_entry_state(identifier, entry, record, task_index))

    problems.extend(_check_unmapped_records(records, entries))
    return problems


def run_checks(
    root: Path,
    task_index: Path,
    ledger: Path | None = None,
) -> tuple[list[Record], list[Path], list[str]]:
    """Run every configured check and return the records, files and problems."""
    text = task_index.read_text(encoding="utf-8")
    records = parse_records(text)
    markdown_files = collect_markdown_files(root)

    problems: list[str] = []
    # Malformed records are reported before the counters and mappings they
    # would silently skew: a checkbox that parse_records skips still has to
    # surface as a finding.
    problems.extend(check_record_shapes(text, task_index))
    problems.extend(check_unique_ids(records, root, task_index))
    problems.extend(check_counters(text, records))
    problems.extend(check_archived_counter(text, root))
    problems.extend(check_links(root, markdown_files))
    if ledger is not None:
        try:
            snapshot = json.loads(ledger.read_text(encoding="utf-8"))
        except (OSError, ValueError) as error:
            # A missing or malformed snapshot is a reportable failure, not an
            # uncaught traceback: the checker's contract is a `[FAIL]` line.
            problems.append(f"ledger: cannot read snapshot {ledger} ({error})")
        else:
            problems.extend(check_ledger(snapshot, records, task_index))
    return records, markdown_files, problems


def parse_args(argv: Sequence[str] | None) -> argparse.Namespace:
    """Parse command-line arguments."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=REPOSITORY,
                        help="repository root (default: the checkout containing this script)")
    parser.add_argument("--task-index", type=Path, default=None,
                        help=f"task index to check (default: <root>/{TASK_INDEX_NAME})")
    parser.add_argument("--ledger", type=Path, default=None,
                        help="optional read-only ledger snapshot JSON for mapping checks")
    parser.add_argument("--quiet", action="store_true",
                        help="suppress the passing summary line")
    return parser.parse_args(argv)


def _resolve_ledger_path(args: argparse.Namespace) -> Path | None:
    """Resolve the optional ledger snapshot path from parsed CLI arguments."""
    return args.ledger.resolve() if args.ledger else None


def main(argv: Sequence[str] | None = None) -> int:
    """Run the checker; return a process exit status."""
    args = parse_args(argv)
    root = args.root.resolve()
    task_index = (args.task_index or root / TASK_INDEX_NAME).resolve()
    if not task_index.is_file():
        print(f"backlog consistency: missing task index {task_index}", file=sys.stderr)
        return 2
    ledger = _resolve_ledger_path(args)
    if ledger is not None and not ledger.is_file():
        print(f"backlog consistency: missing ledger snapshot {ledger}", file=sys.stderr)
        return 2

    records, markdown_files, problems = run_checks(root, task_index, ledger)
    if problems:
        for problem in problems:
            print(f"[FAIL] {problem}", file=sys.stderr)
        print(f"backlog consistency: {len(problems)} problem(s)", file=sys.stderr)
        return 1
    if not args.quiet:
        complete = sum(1 for record in records if record.complete)
        print(
            f"backlog consistency: {len(records)} records ({complete} complete), "
            f"{len(markdown_files)} markdown files, 0 problems")
    return 0


if __name__ == "__main__":
    sys.exit(main())
