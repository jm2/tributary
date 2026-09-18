# Backlog consistency check

[`docs/task.md`](task.md) is the countable execution index for Tributary. A few
of its invariants are maintained by hand, and until this check existed nothing
enforced them. The 2026-09-09 review found the direct cost: the prose counter
said `15/39` while the literal checkbox count was `16/39`
([proposal](backlog-review-proposal-2026-09-09.md)).

[`scripts/check_backlog_consistency.py`](../scripts/check_backlog_consistency.py)
is a read-only, dependency-free checker for those invariants. It runs in the CI
`audit` job and can be run locally at any time.

## What it checks

1. **Unique stable IDs.** Every top-level checkbox carries one stable ID
   (`R1`, `P1.5-A`, `Q7`, ...). A duplicate is a failure that names both lines.
   A top-level checkbox *without* a bold stable ID is reported as malformed
   rather than silently skipped, with its source line, before the counters
   and mappings are derived.
2. **Literal counters.** The completion counters written in prose must equal the
   mechanically derived checkbox counts: the overall `N/M`, the retained
   baseline, the corrective `R` family, the engineering `Q` family, and the
   archived remediation counter. The archived counter is recounted from its
   source document `task-remediation-2026-07.md`: status-summary boxes, the
   global-validation-gate section, and withdrawn (struck-through) false-finding
   boxes are excluded, mirroring that document's own accounting, and a
   checkbox under any other heading is reported as unclassifiable rather than
   silently ignored. Every explicit percentage in the file must also be
   arithmetically consistent. Counter patterns are matched against the prose
   only: fenced code blocks hold syntax examples, not progress, so an example
   counter inside a fence is neither selected nor arithmetically validated.
3. **Internal links and anchors.** Every relative Markdown link target must
   exist inside the repository root — a target that resolves outside the root
   (including through a symlink) is reported — and every `#anchor` into
   another Markdown file must match a heading. Anchors are matched exactly:
   GitHub renders heading anchors lowercased, and browsers match fragments to
   element IDs case-sensitively, so a case-mismatched fragment is broken on
   GitHub even though a case-insensitive reader might resolve it. Only
   *tracked* Markdown files are enumerated in a Git checkout
   (`git ls-files`); in a non-Git root the checker falls back to a recursive
   walk, where untracked scratch files are part of the tree by definition.
4. **Issue/bead/PR mappings** (optional, see below). Active records must map to
   a GitHub issue, a Gas City bead, and a pull request; merged-but-unreconciled
   records and stale review heads are surfaced.

## Running it

```sh
python3 scripts/check_backlog_consistency.py
python3 scripts/test_check_backlog_consistency.py
python3 scripts/test_check_backlog_consistency_rework.py
```

The checker exits `0` when everything passes and `1` when any check fails; each
failure prints a `[FAIL]` line naming the rule and the location. Exit `2` is a
usage error raised before any check runs: the task index (`--task-index`, or
`<root>/docs/task.md` by default) does not exist, or the `--ledger` snapshot
path does not exist. Each usage error prints a single diagnostic on stderr.
The two test modules split the suite so each stays under the 500-line limit
enforced by static analysis: the main module covers the checker invariants and
the pass paths, and the rework module covers the corrective findings F1-F6.

## Optional ledger snapshot

The issue/bead/PR mapping lives in the live Gas City ledger and GitHub, not in
the repository, so the mapping check is opt-in. Pass a read-only JSON snapshot:

```sh
python3 scripts/check_backlog_consistency.py --ledger path/to/snapshot.json
```

```json
{
  "records": {
    "Q7": {
      "issue": "https://github.com/jm2/tributary/issues/276",
      "bead": "tr-bps4d",
      "pr": 999,
      "head_sha": "0123456789abcdef",
      "reviewed_sha": "fedcba9876543210",
      "merged": false
    }
  }
}
```

- `bead`, `issue`, and `pr` are required for every active (unchecked) record; a
  record missing any of them, or absent from the snapshot, is reported.
- `"pr": null` is the explicit *not yet published* representation and passes;
  omitting the `pr` key is reported as a missing mapping. A published `pr` (a
  positive integer or digit string) must carry `head_sha` evidence.
- `merged: true` on a record that is still unchecked is reported as
  *merged-but-unreconciled*.
- `reviewed_sha` that differs from `head_sha` is reported as a *stale review
  head*.
- The task index decides whether a record is active: an `active` snapshot flag
  that disagrees with the index is reported and never downgrades the required
  fields.
- Completed records may be omitted. `head_sha` and `reviewed_sha` are optional
  and only used for the reports above.

## Guarantees and non-goals

The checker is strictly read-only. It never edits `docs/task.md`, never closes
or completes a parent record, never assigns a worker, and never contacts GitHub
or the Gas City ledger. Reporting a merged-but-unreconciled record is not
acceptance: a merged child must not automatically complete its parent, and
acting on a finding stays with the operator and the authoritative ledgers.
There is deliberately no scheduler or automatic synchronizer here.

## Fixing a failure

- **Duplicate ID or counter mismatch** — correct `docs/task.md` so the prose
  matches the checkboxes, or fix the checkbox state. Do not weaken the counter
  to hide a discrepancy.
- **Archived counter mismatch** — one side moved: either the archived
  `task-remediation-2026-07.md` checkboxes changed, or the prose counter in
  `docs/task.md` is stale. Recount from the archived source (status-summary
  boxes, the validation gate, and withdrawn boxes excluded) and update the
  prose counter and its percentage together. An *unclassifiable checkbox*
  finding means a box lives under a heading that is neither a `P0`-`P3` task
  section nor a documented exclusion — move it or document the exclusion.
- **Broken link or anchor** — repair the target path, or update the heading so
  the anchor matches.
- **Mapping finding** — resolve it in GitHub and the Gas City ledger first; the
  snapshot is only a report of that state. Use `"pr": null` for an active
  record whose pull request has not been published yet.
