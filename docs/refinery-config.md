# Refinery config (tributary)

This rig's hosted CI ([`.github/workflows/ci.yml`](../.github/workflows/ci.yml))
regularly runs 20-53 minutes wall-clock. The slow end is dominated by the
Coverage job and the cross-compiled aarch64 matrix tail; the fast end of a
clean run is ~20 minutes (measured 1211s on a typical green PR, run
30173146972). Anything under 1200s to a readable verdict is the lower bound.

The upstream refinery pack (`mol-refinery-patrol`) defaults the hosted-CI
gate deadline `ci_timeout_seconds` to 900s (15 minutes). Honoring that
literally rejects branches whose checks are still running — by the deadline
the `Coverage` and cross-compile jobs are routinely in-flight, so a green
branch reads as "pending at deadline" and is rejected for the wrong reason.
On a red branch, the eventual verdict would have been a rejection anyway, but
the timer would have blamed the wrong check.

## What this rig overrides

Configured under `[rigs.formula_vars]` in the city config (the rig-side
clone does not own this value — `city.toml` does):

```
ci_timeout_seconds = "3600"   # 1 hour, comfortable margin over the tail
```

Everything else stays at the upstream defaults:

| var | value | meaning |
|---|---|---|
| `ci_gate` | `true` | "pending is never green" — fail-closed when checks are still running. **Preserved.** |
| `ci_poll_seconds` | `60` | Poll GitHub for check-runs every minute. **Preserved.** |
| `ci_zero_check_grace_seconds` | `300` | Allow the workflow runs to materialize before declaring zero-on-this-branch. **Preserved.** |
| `ci_timeout_seconds` | `3600` | **Raised from 900**: covers the observed ~20-53 min tail with margin. |

The change is the deadline alone; the fail-closed policy is intentionally
left intact. A bead (tr-3h7) opened the issue and another polecat can land
the city-config edit through the usual refinery handoff if the change
hasn't already been applied out-of-band.

## Reviewer policy

Repository-owned AI review workflows are deliberately absent from both forks.
That describes what these repositories run themselves; it is not an exemption
from review. Third-party GitHub App integrations (Codacy, CodeQL, CodeRabbit,
and any other bot) post genuine checks and reviews on every pull request, and
those checks and reviews count.

**Operator policy (2026-09-03): a pull request is merge-ready only when every
check and every bot review is green.** That includes the hosted CI jobs above
(test/lint/clippy, Coverage, the cross-compiled aarch64 matrix), Codacy Static
Code Analysis, CodeQL, Coverage, CodeRabbit, and any other bot or status
integration that posts on the pull request — regardless of whether branch
protection marks the check "required". A pending or failing bot check blocks
merge exactly like a red CI job; "not required" is not an exemption.

### Enforcement status: policy gate vs. live ruleset (as of 2026-09-04)

The all-green rule above is refinery policy, not yet a machine-enforced
repository rule. The live default-branch ruleset ("Require CI before merge
(main)") requires exactly seven GitHub Actions status checks — Security
Audit, Linux (x86_64), Linux (aarch64), macOS (aarch64), Windows (x86_64),
Flatpak (Linux), and MSRV — and no reviews. Coverage, CodeQL, Codacy Static
Code Analysis, CodeRabbit, Windows (aarch64), and every bot review are
advisory as far as the repository is concerned: GitHub will merge without
them. Until the ruleset is widened, the all-green rule is enforced by the
refinery's own fail-closed check polling (which observes every check on the
head, required or not) and by review discipline — not by the repository
refusing the merge.

**Routine auto-merge stays off until the live gate matches the policy.** The
`dependabot-automerge` workflow enables GitHub native auto-merge on clean
patch/minor dependency PRs, and native auto-merge waits only for the
ruleset's required checks. So while the ruleset is narrower than the policy,
auto-merge can land a dependency PR while a bot check is pending or failing.
Widening the gate is a precondition for trusting auto-merge, not an optional
follow-up; do not enable or rely on it before then.

**Closing the gap (the machine gate).** Widen "Require CI before merge
(main)" to require the full policy set: the Coverage and Windows (aarch64)
jobs, the CodeQL Analyze jobs, Codacy Static Code Analysis, the CodeRabbit
status context, and a repo-owned `bot-review-gate` check that fails while
the pull request has unresolved actionable bot review threads (queried via
the API, so review threads become machine-readable merge evidence). That is
a repository-settings and workflow change: it goes through its own bead and
full CI validation, never an out-of-band ruleset edit, and it must be
validated against a live pull request before the refinery treats the widened
gate as authoritative.

### Addressing Codacy/CodeRabbit findings

When a bot leaves findings on your pull request:

1. Read the bot's review comments on the PR (the Reviews tab and the inline
   comments).
2. Fix the valid findings in your worktree.
3. Push the fixes to the same `polecat/<bead-id>` branch — never a side
   branch or a second PR. The bots re-review the new head automatically.
4. Repeat until every bot check and review is green. The refinery gate is
   fail-closed ("pending is never green"), so a re-review still running at
   poll time simply keeps the merge waiting.

Operational review is performed out of band by Gas City's locally configured
GLM 5.3 reviewer. Its admission and evidence belong to the rollout manifests,
not GitHub Actions or this repository. A reviewer outage must never become an
unsatisfiable branch-close gate.

The former local-only `review_bots`, `review_timeout_seconds`, and
`review_workflow` overrides are retired and must remain absent from the
pack and city configuration. Any future hosted reviewer is a new change that
requires explicit authorization, a finite timeout, a credential and threat
model, and regression coverage.
