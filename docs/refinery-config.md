# Refinery config (tributary)

This rig's hosted CI ([`.github/workflows/ci.yml`](../.github/workflows/ci.yml))
regularly runs 20-53 minutes wall-clock. The slow end is dominated by the
Coverage job and the cross-compiled aarch64 matrix tail; the fast end of a
clean run is ~20 minutes (measured 1211s on a typical green PR, run
30173146972). Anything under 1200s to a readable verdict is the lower bound.

## Hosted-CI gate history and the live deadline-free gate

The upstream refinery pack (`mol-refinery-patrol`) once defaulted a hosted-CI
gate deadline, `ci_timeout_seconds`, to 900s (15 minutes). Honoring that
literally rejected branches whose checks were still running — by the deadline
the `Coverage` and cross-compile jobs were routinely in-flight, so a green
branch read as "pending at deadline" and was rejected for the wrong reason
(bead tr-3h7). This rig first raised the deadline to 3600s under
`[rigs.formula_vars]` in the city config; the 2026-09-08 rendered-workflow
snapshot still records that override, alongside `ci_poll_seconds = "30"` and
an eleven-entry `hosted_required_checks_json` allowlist.

**That mechanism is retired.** None of those `ci_*` variables exists in any
live layer today — not in `city.toml`, not in the rig-local refinery formula
override (2026-09-03), not in the pinned upstream pack — and the upstream
keys they tuned were removed from the pack as well. The live completion gate
is the guarded reconciler `.gc/operations/reconcile.py`, run every five
minutes by the `tributary-reconcile` order. It polls GitHub's
`statusCheckRollup` per open PR head and is **deadline-free and fail-closed**:
any failure-class conclusion on any observed check routes the bead back to
the polecat pool as rework, and an empty rollup never reads as green. The
gate also compares the check names it observes against the complete gating
set — the main-branch ruleset's required contexts plus the two city gates
(`Codacy Static Code Analysis`, `Coverage (Linux x86_64)`) — and parks the
bead as pending, naming the absent contexts, whenever an expected gating
check never materialized as a run at all: a rollup that simply lacks an
expected check is not a passing one. Checks still running park the bead as
pending — but only when the unfinished check is a *gating* one.
Hold-class dispositions are evaluated ahead of any CI state and are never
converted into pending by it: a draft PR (or unknown draft status), an
unresolved review hold, or an operator audit reject at the head parks the
bead as `hold` for human disposition. The one exception is the explicit
per-source `review.corrections_while_held = "true"` opt-in: an open held or
draft bead inside corrective review whose current-head review evidence is
missing, incomplete, or stale parks as `pending` until that evidence
exists, and a new unresolved finding routes to the refinery as `review` —
the hold itself remains in force throughout and is never cleared by that
routing.
Queued advisory jobs such as `SHA256 Checksums` — which GitHub's
per-account Actions concurrency cap can hold for an hour or more — do not
delay review or an operator landing that the ruleset itself would allow.
There is no deadline at which a pending branch can be wrongly rejected.
The repository's own merge gate is the `main` ruleset described in "Merge
gate" below. The reconciler still counts Codacy as a city gate, which is
stricter than repository policy (Codacy is advisory); align it before Gas City
is resumed.

Verification against the live rig, including the exact ruleset-required
check contexts and fresh dry-run decisions, is recorded in
[ci-gate-verification-2026-09-17.md](ci-gate-verification-2026-09-17.md).

## Reviewer policy

Repository-owned AI review workflows are deliberately absent from both forks.
That describes what these repositories run themselves; it is not an exemption
from review. Third-party GitHub App integrations (Codacy, CodeQL, CodeRabbit,
and any other bot) post genuine checks and reviews on every pull request, and
those checks and reviews count.

**Merge policy (2026-09-24, #339): a pull request is merge-ready when every
required check below is green at its head and its review is done.** Codacy
Static Code Analysis, CodeQL and CodeRabbit are advisory: read what they
report and fix real defects, but their style metrics (method length,
cyclomatic complexity, Markdown line length) never block a merge. Codacy
allows zero new findings per pull request, so large changes almost always
trip it.

### Merge gate (2026-09-24, #339)

The `main` ruleset "Require CI before merge (main)" requires eleven GitHub
Actions checks: Security Audit, Linux (x86_64), Linux (aarch64), macOS
(aarch64), Windows (x86_64), Windows (aarch64), Flatpak (Linux), MSRV, GTK
Display Gate (Linux x86_64), Desktop Metadata, and Coverage (Linux x86_64).
Coverage fails below the line percentage in
[`coverage-baseline.txt`](../coverage-baseline.txt), raised as the README
describes.

- **Branches need not be up to date with `main`** (strict mode is off). A
  full CI run takes one to three hours on the shared runner pool, so
  re-running it every time `main` moves would starve the queue. Batches of
  pull requests are merged as merge trains built on current `main`, and the
  CI run on `main` after each merge catches the rest.
- **Admins may bypass only through a pull request.** Nothing reaches `main`
  by a direct push, so every change runs CI. An admin can still merge a pull
  request past a stuck or broken check, and should say why on the pull
  request. The refinery's `merge_strategy=direct` would push to `main`
  directly, so it must switch to pull-request merges before Gas City is
  resumed.
- **Dependabot patch updates auto-merge** (`dependabot-automerge.yml`).
  GitHub's native auto-merge waits for the eleven required checks.

### Addressing Codacy/CodeRabbit findings

When a bot leaves findings on your pull request:

1. Read the bot's review comments on the PR (the Reviews tab and the inline
   comments).
2. Fix the valid findings in your worktree.
3. Push the fixes to the same `polecat/<bead-id>` branch — never a side
   branch or a second PR. The bots re-review the new head automatically.
4. Findings that are only style metrics (see "Merge policy" above) may be
   left as they are; say so in a reply instead of reshaping working code.

Operational review is performed out of band by Gas City's locally configured
GLM 5.3 reviewer. Its admission and evidence belong to the rollout manifests,
not GitHub Actions or this repository. A reviewer outage must never become an
unsatisfiable branch-close gate.

The former local-only `review_bots`, `review_timeout_seconds`, and
`review_workflow` overrides are retired and must remain absent from the
pack and city configuration. Any future hosted reviewer is a new change that
requires explicit authorization, a finite timeout, a credential and threat
model, and regression coverage.
