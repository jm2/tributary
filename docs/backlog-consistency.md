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
2. **Literal counters.** The completion counters written in prose must equal the
   mechanically derived checkbox counts: the overall `N/M`, the retained
   baseline, the corrective `R` family, and the engineering `Q` family. Every
   explicit percentage in the file, including the archived remediation counter
   that this check does not recount, must at least be arithmetically consistent.
3. **Internal links and anchors.** Every relative Markdown link target must
   exist, and every `#anchor` into another Markdown file must match a heading.
4. **Issue/bead/PR mappings** (optional, see below). Active records must map to
   a GitHub issue and a Gas City bead; merged-but-unreconciled records and
   stale review heads are surfaced.

## Running it

```sh
python3 scripts/check_backlog_consistency.py
python3 scripts/test_check_backlog_consistency.py
```

The checker exits `0` when everything passes and `1` when any check fails; each
failure prints a `[FAIL]` line naming the rule and the location.

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

- `bead` and `issue` are required for every active (unchecked) record; a record
  missing either, or absent from the snapshot, is reported.
- `merged: true` on a record that is still unchecked is reported as
  *merged-but-unreconciled*.
- `reviewed_sha` that differs from `head_sha` is reported as a *stale review
  head*.
- Completed records may be omitted. `pr`, `head_sha`, and `reviewed_sha` are
  optional and only used for the reports above.

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
- **Broken link or anchor** — repair the target path, or update the heading so
  the anchor matches.
- **Mapping finding** — resolve it in GitHub and the Gas City ledger first; the
  snapshot is only a report of that state.
