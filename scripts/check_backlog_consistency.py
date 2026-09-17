#!/usr/bin/env python3
"""Read-only consistency checks for the Tributary implementation backlog.

``docs/task.md`` is the repository's countable execution index.  Several of its
invariants are maintained by hand and were previously unenforced:

* every top-level checkbox is one record with a unique stable ID;
* the literal completion counters written in prose match the checkbox state;
* every relative link (and ``#anchor``) resolves inside the checkout;
* each active record maps to a GitHub issue, a Gas City bead, and (when one
  exists) a pull request, with no merged-but-unreconciled record and no stale
  review head.

This module is deliberately **read-only**.  It never edits the index, never
closes a parent record, never assigns a worker, and never talks to GitHub or
the Gas City ledger.  A caller may pass an optional ledger snapshot
(``--ledger``); the checker then only *reports* missing mappings,
merged-but-unreconciled records, and stale review heads.  Acting on those
findings stays with the operator and the authoritative ledgers.

Usage::

    python3 scripts/check_backlog_consistency.py
    python3 scripts/check_backlog_consistency.py --ledger path/to/snapshot.json

Exit status is 0 when every check passes and 1 when any check fails.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Sequence
from urllib.parse import unquote

REPOSITORY = Path(__file__).resolve().parent.parent
TASK_INDEX_NAME = "docs/task.md"

# One countable record: a top-level checkbox whose title begins with a bold
# stable ID (for example ``- [x] **P1.5-A** — ...`` or ``- [ ] **R1 — ...**``).
RECORD_PATTERN = re.compile(r"^- \[(?P<mark>[ xX])\] \*\*(?P<id>[^*\s]+)")

# Inline Markdown links, images and link reference definitions.  Reference-style
# *inline* links (`[text][label]`) are not used in this repository; definitions
# are validated so a future relative definition cannot rot silently.
INLINE_LINK = re.compile(
    r"(?P<bang>!?)\[(?P<text>[^\]]*)\]\((?P<target>[^)\s]+)(?:\s+\"[^\"]*\")?\)"
)
DEFINITION_LINK = re.compile(r"^\[(?P<label>[^\]]+)\]:\s*(?P<target>\S+)")
HEADING = re.compile(r"^(?P<hashes>#{1,6})\s+(?P<title>.*?)\s*#*\s*$")
ABSOLUTE_TARGET = re.compile(r"^[a-zA-Z][a-zA-Z0-9+.-]*:")
PERCENT_COUNTER = re.compile(
    r"\*\*(?P<complete>\d+)/(?P<total>\d+)\s*\((?P<percent>\d+(?:\.\d+)?)%\)\*\*"
)

# Prose counters, anchored on stable phrasing so a wording change fails loudly
# instead of silently skipping the check.
COUNTER_PATTERNS = (
    (
        "overall",
        re.compile(
            r"Current status:\s*\*\*(?P<complete>\d+)/(?P<total>\d+)\s*"
            r"\((?P<percent>\d+(?:\.\d+)?)%\)\*\*"
        ),
    ),
    (
        "baseline",
        re.compile(r"retained baseline is\s*\*\*(?P<complete>\d+)/(?P<total>\d+)\*\*"),
    ),
    (
        "corrective",
        re.compile(r"with\s*\*\*(?P<complete>\d+)/(?P<total>\d+)\*\*\s+new corrective"),
    ),
    (
        "engineering",
        re.compile(
            r"and\s*\*\*(?P<complete>\d+)/(?P<total>\d+)\*\*\s+engineering records complete"
        ),
    ),
)

# Directories that never contain tracked documentation worth checking.
SKIPPED_DIRECTORIES = frozenset({".git", "target", "node_modules", "dist"})


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


def iter_content_lines(text: str) -> Iterable[tuple[int, str]]:
    """Yield ``(line_number, line)`` outside fenced code blocks.

    Fenced code blocks frequently contain shell snippets whose ``#`` comment
    lines would otherwise be mistaken for headings and whose bracketed text
    would be mistaken for links.
    """
    fence: str | None = None
    for number, line in enumerate(text.splitlines(), start=1):
        stripped = line.lstrip()
        if fence is None:
            if stripped.startswith("```") or stripped.startswith("~~~"):
                fence = stripped[:3]
            else:
                yield number, line
            continue
        if stripped.startswith(fence):
            fence = None


def parse_records(text: str) -> list[Record]:
    """Return every countable checkbox record in document order."""
    records: list[Record] = []
    for number, line in iter_content_lines(text):
        match = RECORD_PATTERN.match(line)
        if match:
            records.append(
                Record(
                    identifier=match.group("id"),
                    complete=match.group("mark").lower() == "x",
                    line=number,
                )
            )
    return records


def slugify(heading: str) -> str:
    """Approximate GitHub's heading anchor slug for a Markdown heading."""
    text = heading.strip().lower()
    text = re.sub(r"[`*_]", "", text)
    text = re.sub(r"[^\w\- ]", "", text, flags=re.UNICODE)
    return text.replace(" ", "-")


def document_anchors(path: Path) -> set[str]:
    """Return the set of anchors GitHub exposes for *path*."""
    counts: dict[str, int] = {}
    text = path.read_text(encoding="utf-8", errors="replace")
    for _, line in iter_content_lines(text):
        match = HEADING.match(line)
        if match:
            slug = slugify(match.group("title"))
            counts[slug] = counts.get(slug, 0) + 1
    anchors: set[str] = set()
    for slug, count in counts.items():
        for index in range(count):
            anchors.add(slug if index == 0 else f"{slug}-{index}")
    return anchors


def collect_markdown_files(root: Path) -> list[Path]:
    """Return the tracked Markdown files to link-check.

    Tracked files are preferred because they exclude build output and scratch
    trees; a plain recursive walk is the fallback for a tarball or a synthetic
    test tree that is not a Git checkout.
    """
    try:
        result = subprocess.run(
            ["git", "-C", str(root), "ls-files", "*.md"],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        result = None
    if result is not None and result.returncode == 0:
        tracked = [root / name for name in result.stdout.split("\n") if name]
        if tracked:
            return sorted(tracked)
    return sorted(
        path
        for path in root.rglob("*.md")
        if not SKIPPED_DIRECTORIES.intersection(path.relative_to(root).parts)
    )


def iter_link_targets(line: str) -> Iterable[str]:
    """Yield every link target written on one content line."""
    for match in INLINE_LINK.finditer(line):
        if not match.group("bang"):
            yield match.group("target")
    definition = DEFINITION_LINK.match(line)
    if definition:
        yield definition.group("target")


def split_target(target: str) -> tuple[str | None, str]:
    """Split a link target into ``(path, fragment)``.

    ``None`` means the target is external and must be skipped.  An empty path
    means the fragment points inside the containing file.
    """
    target = target.strip()
    if target.startswith("<") and target.endswith(">"):
        target = target[1:-1]
    if target.startswith("#"):
        return "", target[1:]
    if not target or ABSOLUTE_TARGET.match(target):
        return None, ""
    path_part, _, fragment = target.partition("#")
    return unquote(path_part), fragment


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
                f"unique-id: record ID '{identifier}' is defined {len(lines)} times ({rendered})"
            )
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


def check_counters(text: str, records: Sequence[Record]) -> list[str]:
    """Report prose counters that disagree with the checkbox state or arithmetic."""
    normalized = re.sub(r"\s+", " ", text)
    expected = derived_counts(records)
    problems: list[str] = []

    for label, pattern in COUNTER_PATTERNS:
        match = pattern.search(normalized)
        if match is None:
            problems.append(
                f"counter: could not find the {label} completion counter; "
                "if the wording changed, update COUNTER_PATTERNS"
            )
            continue
        complete = int(match.group("complete"))
        total = int(match.group("total"))
        want_complete, want_total = expected[label]
        if (complete, total) != (want_complete, want_total):
            problems.append(
                f"counter: {label} says {complete}/{total} but the index contains "
                f"{want_complete}/{want_total} completed records"
            )
        stated = match.groupdict().get("percent")
        if stated is not None:
            computed = round(want_complete / want_total * 100, 1) if want_total else 0.0
            if abs(float(stated) - computed) > 0.05:
                problems.append(
                    f"counter: {label} states {stated}% but {want_complete}/{want_total} "
                    f"rounds to {computed}%"
                )

    # Every explicit percentage, including the archived remediation counter that
    # this checker does not recount, must at least be arithmetically consistent.
    for match in PERCENT_COUNTER.finditer(text):
        complete = int(match.group("complete"))
        total = int(match.group("total"))
        stated = float(match.group("percent"))
        computed = round(complete / total * 100, 1) if total else 0.0
        if abs(stated - computed) > 0.05:
            problems.append(
                f"counter: '{complete}/{total} ({stated}%)' is arithmetically "
                f"inconsistent (rounds to {computed}%)"
            )
    return problems


def check_links(root: Path, markdown_files: Sequence[Path]) -> list[str]:
    """Report relative link targets and ``#anchors`` that do not resolve."""
    problems: list[str] = []
    anchor_cache: dict[Path, set[str]] = {}

    for path in markdown_files:
        relative = relative_display(root, path)
        try:
            text = path.read_text(encoding="utf-8")
        except OSError as error:  # pragma: no cover - unreadable checkout file
            problems.append(f"link: {relative} is unreadable ({error})")
            continue
        for number, line in iter_content_lines(text):
            for target in iter_link_targets(line):
                path_part, fragment = split_target(target)
                if path_part is None:
                    continue
                target_path = (path if not path_part else path.parent / path_part).resolve()
                if not target_path.exists():
                    problems.append(
                        f"link: {relative}:{number}: broken link target '{target}'"
                    )
                    continue
                if fragment and target_path.suffix.lower() == ".md":
                    anchors = anchor_cache.get(target_path)
                    if anchors is None:
                        anchors = document_anchors(target_path)
                        anchor_cache[target_path] = anchors
                    if fragment not in anchors:
                        problems.append(
                            f"link: {relative}:{number}: missing anchor '#{fragment}' in "
                            f"{relative_display(root, target_path)}"
                        )
    return problems


def _short(sha: str) -> str:
    """Render a Git object name compactly for diagnostics."""
    return sha[:12] if len(sha) > 12 else sha


def check_ledger(
    snapshot: object, records: Sequence[Record], task_index: Path
) -> list[str]:
    """Report mapping problems from an optional read-only ledger snapshot.

    The snapshot is a JSON object::

        {
          "records": {
            "Q7": {
              "issue": 276,
              "bead": "tr-bps4d",
              "pr": 999,
              "head_sha": "abc123",
              "reviewed_sha": "def456",
              "merged": false
            }
          }
        }

    Every active (unchecked) record must appear with an issue and a bead; a
    record missing either is reported.  ``merged`` on an unchecked record is a
    merged-but-unreconciled report, and ``reviewed_sha`` differing from
    ``head_sha`` is a stale review head.  Completed records may be omitted.
    """
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
        if entry.get("active", not record.complete):
            if not entry.get("bead"):
                problems.append(f"ledger: active record '{identifier}' has no bead mapping")
            if not entry.get("issue"):
                problems.append(f"ledger: active record '{identifier}' has no issue mapping")
        head = entry.get("head_sha")
        reviewed = entry.get("reviewed_sha")
        if head and reviewed and head != reviewed:
            problems.append(
                f"ledger: record '{identifier}' has a stale review head "
                f"(reviewed {_short(str(reviewed))} != head {_short(str(head))})"
            )
        if entry.get("merged") and not record.complete:
            problems.append(
                f"ledger: record '{identifier}' is merged but still unchecked in "
                f"{task_index.name} (merged-but-unreconciled)"
            )

    for record in records:
        if not record.complete and record.identifier not in entries:
            problems.append(
                f"ledger: active record '{record.identifier}' has no mapping entry"
            )
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
    problems.extend(check_unique_ids(records, root, task_index))
    problems.extend(check_counters(text, records))
    problems.extend(check_links(root, markdown_files))
    if ledger is not None:
        snapshot = json.loads(ledger.read_text(encoding="utf-8"))
        problems.extend(check_ledger(snapshot, records, task_index))
    return records, markdown_files, problems


def parse_args(argv: Sequence[str] | None) -> argparse.Namespace:
    """Parse command-line arguments."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=Path,
        default=REPOSITORY,
        help="repository root (default: the checkout containing this script)",
    )
    parser.add_argument(
        "--task-index",
        type=Path,
        default=None,
        help=f"task index to check (default: <root>/{TASK_INDEX_NAME})",
    )
    parser.add_argument(
        "--ledger",
        type=Path,
        default=None,
        help="optional read-only ledger snapshot JSON for mapping checks",
    )
    parser.add_argument(
        "--quiet",
        action="store_true",
        help="suppress the passing summary line",
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    """Run the checker; return a process exit status."""
    args = parse_args(argv)
    root = args.root.resolve()
    task_index = (args.task_index or root / TASK_INDEX_NAME).resolve()
    if not task_index.is_file():
        print(f"backlog consistency: missing task index {task_index}", file=sys.stderr)
        return 2
    ledger = args.ledger.resolve() if args.ledger else None
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
            f"{len(markdown_files)} markdown files, 0 problems"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
