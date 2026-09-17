# Live CI-gate verification — O2 (2026-09-17)

Task O2 asked for verification of the documented 3600-second CI timeout, the
exact check contexts, pending non-green behavior, finite correction routing,
and preserved holds against the **live** rig configuration — repository
documentation alone is not deployment evidence. This report records what is
actually deployed and what it actually decided on live pull requests.

Everything below was gathered read-only: configuration files, the reconciliation
source, one dry-run execution, and GitHub API observations. No city, rig, or
pack configuration was changed by this verification.

## Method

- `/home/jmulesa/ai-city/city.toml` — the live city configuration (the
  `[rigs.tributary]` and `[rigs.formula_vars]` sections).
- The three formula layers for `mol-refinery-patrol`: the rig-local live copy
  (`.gc/operations/formulas/mol-refinery-patrol.toml`), the city formulas
  directory, and the pinned upstream pack commit
  (`sha:aab8030d397c211be6a4d460e9ce8de39e867a09`, "pr-pipeline: refuse an
  oversized PR review before spending", pinned in `city.toml` imports).
- `.gc/operations/reconcile.py` — the guarded reconciler that is the live
  completion gate (735 lines, read in full).
- `.gc/operations/orders/tributary-reconcile.toml` — the order that executes
  the gate.
- `python3 .gc/operations/reconcile.py` executed **without `--apply`** on
  2026-09-17: a dry run of the live gate against real beads and open PRs.
- GitHub API (read-only): the default-branch ruleset and the open-PR rollup
  the gate itself observes.
- The historical rendered-workflow snapshot
  `.gc/operations/reviews/2026-09-08/beads.before.json`, retained as evidence
  of what the superseded configuration once was.

## Finding 1 — the documented 3600-second timeout is not in any live layer

`docs/refinery-config.md` used to state that the rig raises the upstream
pack default `ci_timeout_seconds` from 900 to 3600 under
`[rigs.formula_vars]` in the city config. That was true when written (bead
tr-3h7, closed after the 2026-09-08 measurement of a 1211 s green run) and
the 2026-09-08 rendered-workflow snapshot still records
`gc.var.ci_timeout_seconds = "3600"`, `ci_poll_seconds = "30"`, and an
eleven-entry `hosted_required_checks_json` allowlist.

None of that exists live today:

- `city.toml` `[rigs.formula_vars]` for tributary carries only
  `run_tests`, `binding_prefix`, and the build/lint/setup/test/typecheck
  commands. No `ci_*` key.
- The rig-local live refinery formula override has no `ci_*` variables; it is
  a 2026-09-03 override that switched the rig to PR-only operation and
  delegated CI verification to the reconciler.
- The city formulas directory copy has no `ci_*` variables.
- The pinned pack commit's `formulas/mol-refinery-patrol.toml` has no
  `ci_*` variables either — the upstream keys the old override tuned were
  themselves removed upstream.

`grep -rn "ci_timeout\|ci_poll\|ci_gate\|ci_zero"` across every live layer
returns nothing. The 3600-second deadline — and the deadline concept itself —
was retired with the agent-driven hosted-CI polling when the deterministic
reconciler superseded it.

## Finding 2 — the live gate has no CI deadline at all

The live completion gate is `.gc/operations/reconcile.py`, executed by the
`tributary-reconcile` order every five minutes (`interval = "5m"`,
`timeout = "4m"`, `exec = ... reconcile.py --apply`). Its check evaluation
(`decide()`) is purely deadline-free and fail-closed:

- The GitHub `statusCheckRollup` is observed for every open PR head.
- An empty or missing rollup parks the bead as `pending` — "an empty set is
  not a passing gate".
- Any check result outside the known conclusion set parks as `pending`
  (unknown results never read as green).
- Any check whose conclusion is not COMPLETED parks as `pending`
  (`{name}: not completed`).
- Only after every check is COMPLETED are conclusions evaluated: any
  failure-class conclusion (FAILURE, ERROR, CANCELLED, TIMED_OUT,
  ACTION_REQUIRED, STARTUP_FAILURE) routes the bead back to the polecat pool
  as `rework` with the failing check named in `rejection_reason`; otherwise
  the bead is merge-candidate only if at least one check is SUCCESS.

Because there is no timer, the failure mode tr-3h7 documented — a green
branch rejected as "pending at deadline" while slow checks were still
in flight — is structurally eliminated: a slow branch simply stays pending
until the checks conclude. Pending state is parked with
`merge_result = pull_request_pending` and `gc.routed_to = human`; the next
reconciliation pass re-examines it with no operator action required.

The only timeouts in the live gate are the 90-second bound on each `gh`/`gc`
subprocess call and the four-minute ceiling of the order itself — both are
fail-closed (an observation error invalidates stored readiness evidence
instead of implying a verdict).

## Finding 3 — exact check contexts

Three distinct sets exist, with different roles:

1. **Branch-protection ruleset (machine-enforced).** The default-branch
   ruleset "Require CI before merge (main)" (ruleset id 17650907,
   enforcement `active`, verified live via the GitHub API on 2026-09-17)
   requires exactly seven status check contexts:

   - Security Audit
   - Linux (x86_64)
   - Linux (aarch64)
   - macOS (aarch64)
   - Windows (x86_64)
   - Flatpak (Linux)
   - MSRV

   Coverage, CodeQL, Codacy, CodeRabbit, Windows (aarch64), Desktop
   Metadata, and SHA256 Checksums are advisory as far as the repository is
   concerned: GitHub would merge without them.

2. **The refinery completion gate (policy-enforced).** The reconciler
   observes **every** check on the head — required or advisory, CI job or
   bot check — and requires all of them completed and green (at least one
   SUCCESS, zero failure conclusions). There is no name allowlist anywhere
   in the live gate; the eleven-entry `hosted_required_checks_json`
   allowlist visible in the 2026-09-08 snapshot belonged to the retired
   agent gate, not to this one.

3. **Operator all-green policy (2026-09-03, documented in
   `docs/refinery-config.md`).** Set 2 is the enforcement of set 3: the
   operator's rule that a PR is merge-ready only when every check and every
   bot review is green, "required" or not.

The reconciler additionally refuses completion unless semantic review
evidence matches the exact current head SHA: an approval or bot review
belonging to an older head leaves the bead pending (see the fresh dry-run
evidence below).

## Finding 4 — pending non-green behavior (fail-closed, verified)

Beyond the check rules above, the live gate is fail-closed in every
adjacent path:

- Readiness verdicts are re-verified against a fresh GitHub snapshot
  immediately before any terminal mutation; a changed head in between
  invalidates readiness and skips the action.
- Any observation error (API failure, subprocess timeout) invalidates stored
  readiness evidence repo-wide for that pass rather than implying a verdict.
- A draft PR parks (`pending`) with a human-disposition reason.
- Unknown check conclusions park; they are never treated as passing.

## Finding 5 — finite correction routing (verified in source and live)

Correction rounds are finite and bounded by `reconcile.py` constants:

- A "round" is one refinery rejection directory
  (`.gc/operations/reviews/refinery-<bead>-<date>`); routing state advances
  per source bead.
- `NOTIFY_ROUNDS = 6`: after six rounds the operator receives a single
  informational mail; routing to the polecat pool continues.
- `PARK_AFTER_ROUNDS = 12`: after twelve rounds the bead is parked for the
  human — `review_hold` set, rework authorization off, no further
  automatic rework routing. This is the spend valve.
- `recovery.rounds_acknowledged = <n>` metadata on a bead raises both
  thresholds by `n`, recording operator acknowledgement of progress.
- The hard pool rerouting mechanism is disabled (`HARD_POOL = None` since
  2026-09-16); only the standard polecat pool is used for rework.
- Foreign or operator-owned holds are never overwritten by the routing
  machinery; release paths are explicit (operator acknowledgement, or a
  refinery hold whose reason is superseded by a later head).

Live review directories for 2026-09-17 (`refinery-tr-xaj-…`,
`refinery-tr-47yad-…`, `refinery-20260917T1430-tr-dkk`, …) and the
`polecat-tr-3asjp-rework-20260917` directory show the round machinery
operating on real work.

## Finding 6 — preserved holds (verified live)

Hold semantics in the live gate, unchanged and enforced:

- `review_hold` metadata (unless explicitly CLEARED-prefixed, the legacy
  discipline) or a `hold:` label parks the bead as `blocked`, routed to the
  human. The gate never infers clearance from a successful check, a later
  commit, or the words "hold cleared" in notes.
- An operator audit verdict of `reject` at the current head parks the bead.
- A repair authorization allows corrective coding; it never clears a hold —
  "corrective work does not authorize completion" is the exact live
  decision string.
- Holds are evaluated before checks, so a held bead stays held regardless
  of CI color.

## Fresh execution evidence (2026-09-17, dry run)

`python3 .gc/operations/reconcile.py` (no `--apply`) on 2026-09-17 observed
16 open PRs and produced zero errors. Representative live decisions:

- `tr-t3a` (PR #270) → **hold**: "unresolved review hold; corrective work
  does not authorize completion" — preserved operator hold.
- `tr-4q8` (PR #231) → **hold**: "operator self-review found unresolved
  defects at this head; see audit.report" — preserved audit hold.
- `tr-uv3lp` (PR #272) → **pending**: "github-advanced-security: latest bot
  review belongs to an older head" — fail-closed exact-head review evidence.
- Multiple beads → `wait_dependencies` / `keep_workflow` — prerequisite and
  workflow bookkeeping routing working as designed.

The gate was thereby observed deciding real pull requests exactly as its
source describes, with no deadline parameter and no check-name allowlist in
play.

## Conclusion

- The documented 3600-second `ci_timeout_seconds` override is **not live**
  in any configuration layer; it was superseded (along with the deadline
  concept) by the deterministic reconciler. The stale claim in
  `docs/refinery-config.md` is corrected by the commit that adds this
  report.
- The live gate has **no CI timeout**: pending checks park a PR
  indefinitely and fail-closed until the checks conclude.
- Exact check contexts: seven ruleset-required contexts (listed above),
  while the completion gate enforces the operator's stricter all-green
  policy over every observed check on the exact head.
- Pending non-green behavior, finite correction routing (6 → notify,
  12 → park), and preserved holds are all implemented as documented and
  were observed live.
