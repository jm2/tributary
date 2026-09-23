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
This gate is **not** the full machine enforcement of the all-green operator
policy below: that 2026-09-03 policy remains in force for the operator, and
what the live gate enforces of it is an enforcement gap, not a retirement —
the gate is name-sensitive in exactly one place (the required ∪ city gating
set) and reworks on any failure anywhere, while advisory checks and bot
reviews still rest on review discipline ("Enforcement status" below).

Verification against the live rig, including the exact ruleset-required
check contexts and fresh dry-run decisions, is recorded in
[ci-gate-verification-2026-09-17.md](ci-gate-verification-2026-09-17.md).

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
them. Until the ruleset is widened, the gap between the policy and the
machine gate persists: the live reconciler (verified 2026-09-17; absence
parking added 2026-09-18) scans every check on the head for failure
conclusions, but its pending set is only the ruleset-required contexts plus
the two city gates, with an expected gating context absent from the rollup
parked as well — an unfinished advisory
check (CodeRabbit, Windows (aarch64), Desktop Metadata, `SHA256 Checksums`)
no longer delays it, and the all-green rule above remains operator policy
enforced by review discipline, not by the repository refusing the merge.

**Routine auto-merge stays off until the live gate matches the policy.** The
`dependabot-automerge` workflow enables GitHub native auto-merge on clean
patch dependency PRs, and native auto-merge waits only for the
ruleset's required checks. So while the ruleset is narrower than the policy,
auto-merge can land a dependency PR while a bot check is pending or failing.
Widening the gate is a precondition for trusting auto-merge, not an optional
follow-up; do not enable or rely on it before then.

**Closing the gap (the machine gate).** Widen "Require CI before merge
(main)" to require the full policy set: the Coverage (Linux x86_64),
Windows (aarch64), Desktop Metadata, and SHA256 Checksums jobs, the CodeQL
Analyze jobs, Codacy Static Code Analysis, the CodeRabbit status context,
and a repo-owned `bot-review-gate` check — eleven additions to the seven
checks the ruleset already requires, coordinated with the dependency-updates
gate migration so both documents demand the same context set. The GitHub
reviews API returns a bot's full review history, and a later review never
deletes an earlier one, so the gate evaluates exactly one review per bot:
its latest submitted review. Submitted is the operative word: the
[reviews API](https://docs.github.com/en/rest/pulls/reviews) lists
submitted reviews only — a review still in pending state has no
`submitted_at` timestamp and is visible solely to the credential that
created it, so the gate can never observe another integration's draft
review and must not claim to. Whether a re-review is in flight is
therefore knowable only through a gate-visible, trusted handshake: a
re-review request addressed to the bot, acknowledged by a bot-owned
gate-visible signal — the bot's status context or a machine-readable
comment — bound to the current head SHA and the specific review attempt.
An outstanding handshake at the current head invalidates that bot's
earlier clean result at the same head: the prior approval stops counting
until the new attempt is submitted at this head. A missing or untrusted
handshake signal fails closed — the bot counts as unproven at this head,
exactly like a bot with no acceptable review, never as "no re-review in
progress".

The gate fails closed unless all three of the following hold: the latest
submitted review of every in-scope bot has reached an acceptable
conclusion — supersession within a bot's own history is
conclusion-sensitive, because an outstanding change request survives
comments: a newer APPROVED review or an explicit dismissal of the
change-request review through the API clears it, a later COMMENTED review
alone never does, and a CHANGES_REQUESTED latest review blocks even when
its threads are resolved; that latest review was submitted against the
pull request's current head SHA — a review of an older head does not
count and leaves the bot unproven at this head; and no actionable bot
review thread remains unresolved. All three inputs are queried via the
API, so review conclusions, review head SHAs, and review threads become
machine-readable merge evidence.

In-scope is fixed by enumeration, not by observation: the gate
configuration carries an explicit list of the bot identities expected to
review every pull request on this repository, each listed by exact bot
login — the review-posting integrations the operator has authorized, grown
only by the same reviewed change that enables the bot — and every listed
identity must have an acceptable review at the current head. A review-only
bot that never posts — an outage, a rate limit, a rename — leaves no
review record, and every condition above would otherwise pass vacuously;
absence therefore fails closed exactly like a rejected review. The only
sanctioned waiver is an operator-documented reviewer substitution: a
rate-limited reviewer explicitly listed in the repository's substitution
policy, covered by a substitute approval bound to the same evaluated
head. A substitution entry never waives CI, never covers an identity the
policy does not list, and never overrides an unresolved thread or
outstanding change request.

That is a repository-settings and workflow change: it goes through its own
bead and full CI validation, never an out-of-band ruleset edit, and it must
be validated against a live pull request before the refinery treats the
widened gate as authoritative.

### Addressing Codacy/CodeRabbit findings

When a bot leaves findings on your pull request:

1. Read the bot's review comments on the PR (the Reviews tab and the inline
   comments).
2. Fix the valid findings in your worktree.
3. Push the fixes to the same `polecat/<bead-id>` branch — never a side
   branch or a second PR. The bots re-review the new head automatically.
4. Repeat until every bot check and review is green. The refinery gate is
   fail-closed: a requested re-review invalidates the bot's earlier clean
   result at that head until the new review is submitted, and until then
   the bot counts as unproven — the merge simply waits.

Operational review is performed out of band by Gas City's locally configured
GLM 5.3 reviewer. Its admission and evidence belong to the rollout manifests,
not GitHub Actions or this repository. A reviewer outage must never become an
unsatisfiable branch-close gate.

The former local-only `review_bots`, `review_timeout_seconds`, and
`review_workflow` overrides are retired and must remain absent from the
pack and city configuration. Any future hosted reviewer is a new change that
requires explicit authorization, a finite timeout, a credential and threat
model, and regression coverage.
