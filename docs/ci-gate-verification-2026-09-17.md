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
  completion gate (769 lines at review SHA
  d49cbfeebfb10b367657247819b3bab2933b2a26297cf1cfbe01c8cb772b0177, read in
  full).
- `.gc/operations/orders/tributary-reconcile.toml` — the order that executes
  the gate.
- Direct probes of the live `reconcile.py` functions (`decide`,
  `rejection_rounds`, `exhausted_rounds`, `notify_rounds`), run against
  fixtures under `${TMPDIR:-/var/tmp}` with no production mutation and no
  `--apply` side effects. Probe inputs and outputs are quoted where a
  behavioral claim depends on them. The behavioral regression suite
  `.gc/operations/test_reconcile.py` (83 tests) is cited by test name where
  it pins the probed behavior.
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
- **Failure-first scan.** Before any incompleteness is considered, every
  check in the rollup is scanned for a failure-class conclusion (FAILURE,
  ERROR, CANCELLED, TIMED_OUT, ACTION_REQUIRED, STARTUP_FAILURE). The first
  one found routes the bead back to the polecat pool as `rework` with the
  failing check named in `rejection_reason` — regardless of its position in
  the rollup and regardless of how many other checks are still queued or
  unknown. GitHub's rollup order is not a priority order: a queued job must
  not hide a completed failure and strand its corrective work. Direct probe
  of the live `decide()` — rollup `[Security Audit: IN_PROGRESS/queued,
  Windows: COMPLETED/FAILURE]` returns `('rework', 'Windows: FAILURE')`;
  reversing the rollup order returns the same. Regression test:
  `test_pending_check_never_masks_failed_job_in_either_rollup_order`.
- **Pending/unknown scan, gating checks.** Only after the failure scan finds
  nothing are unfinished results considered, and only for *gating* checks —
  the live ruleset-required contexts plus the city gates (`Codacy Static
  Code Analysis`, `Coverage (Linux x86_64)`). A gating check whose
  **status** is not COMPLETED parks as `pending` (`<name>: not completed`);
  a gating check whose **conclusion** is outside the pass set (SUCCESS,
  NEUTRAL, SKIPPED) parks as `pending` (`<name>: unknown or pending result
  …`). An unfinished *optional* check is skipped: the ruleset would not
  block the merge on it either, and a failed optional check was already
  caught by the failure-first scan. If no gating check concluded SUCCESS,
  the bead parks as `pending` ("no successful checks (all skipped/neutral)").
- **Status vs conclusion.** `COMPLETED` is the check *status* field;
  SUCCESS/FAILURE/etc. are *conclusions*. A conclusion value is never
  "COMPLETED" — probe: a gating check with status `COMPLETED` and an empty
  conclusion is pending, not passed:
  `('pending', "Security Audit: unknown or pending result ''")`.
- **Pending is a remainder, not a blanket state.** `decide()` evaluates,
  before checks are even observed: scope and PR-identity guards, exact-head
  and branch/base match, review holds and draft disposition (→ `hold`),
  operator audit reject at this head (→ `hold`), unresolved
  changes-requested reviews (→ `rework`), and independent approval evidence
  (→ `review`). A held, draft, changes-requested, or failure-flagged PR
  never reads as merely pending, whatever its checks are doing.

Because there is no timer, the failure mode tr-3h7 documented — a green
branch rejected as "pending at deadline" while slow checks were still
in flight — is structurally eliminated: a slow branch simply stays pending
until the checks conclude, provided no earlier condition (hold, draft,
changes-requested, review evidence, a failure conclusion) applies — pending
is what remains when nothing worse was observed. Pending state is parked
with `merge_result = pull_request_pending` and `gc.routed_to = human`
(`reconcile_one`); the next reconciliation pass re-examines it with no
operator action required.

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
- A draft PR is a **hold**, not a pending. The direct live probe returns
  `('hold', 'draft or unknown draft status; human disposition required')`,
  and `reconcile_one` stores that disposition as `merge_result = blocked`
  with `gc.routed_to = human` while preserving `status = open` — the bead is
  parked for human disposition; it is not recorded as waiting on CI and the
  PR itself is never touched.
- Unknown check conclusions on gating checks park; they are never treated as
  passing (see Finding 2 for the gating/optional distinction).
- **Corrective review alongside a hold is an explicit opt-in, not a hold
  bypass.** For an open bead with `review.corrections_while_held = "true"`,
  a merge hold coexists with corrective-review routing:
  - a new unresolved code-finding thread at the current head yields action
    `review` — `unresolved review thread at <path>:<line>; merge hold
    remains`;
  - an unreviewed corrective handoff (`merge_result = review_required` with
    no current-head approval) yields `review` — "corrective handoff needs
    independent review; merge hold remains";
  - missing, incomplete, or stale-head review evidence yields `pending`
    ("complete current-head review evidence is missing") until current-head
    evidence exists;
  - clean current-head evidence yields `hold` again — the underlying hold
    was never cleared.
  Probed I/O (live `decide()`, held + opted-in draft bead): missing evidence
  → `pending`; stale head `cccc…` → `pending`; clean current evidence →
  `('hold', 'unresolved review hold; corrective work does not authorize
  completion')`; unresolved thread at `preflight.sh:114` → `review`,
  reason ending `; merge hold remains`. Behavioral pins:
  `test_opted_in_draft_gets_new_findings_reviewed_without_clearing_hold`
  (draft stays draft, `review_hold` untouched, refinery assigned with
  `merge_result = review_required`) and
  `test_held_review_poll_fetches_current_evidence_and_returns_to_hold_when_clean`.
  A `review` action assigns the refinery to adjudicate the finding; it never
  authorizes merging a held or draft PR.

## Finding 5 — correction routing: counters, acknowledgement, and a live coverage gap

Correction routing is bounded by `reconcile.py` constants and directory
counting. Three things are verified here: what a "round" actually counts,
what the acknowledgement metadata actually does, and a deployment gap in
which most live rejection directories never advance any counter.

**What a round counts.** `rejection_rounds(bead_id)` counts directories
matching the glob `refinery-*-<bead-id>` under
`.gc/operations/reviews/` — the directory name must **end** with
`-<bead-id>` (fnmatch `*` also swallows inner dashes, so
`refinery-20260915-0ac3fcc-tr-t3a` counts for `tr-t3a`). A directory named
with the bead id *first* (`refinery-<bead>-<date>`) does **not** count. Direct probe of the live
function against a temporary root containing four directories —
`refinery-tr-probe-20260917`, `refinery-20260917-tr-probe`,
`refinery-20260917T1430-tr-probe`, `refinery-20260917-tr-other` — returns:

- `rejection_rounds('tr-probe') = 2` — only the two bead-last names
  (`refinery-20260917-tr-probe`, `refinery-20260917T1430-tr-probe`) count;
  the bead-first name `refinery-tr-probe-20260917` does not;
- `rejection_rounds('tr-other') = 1`;
- `rejection_rounds('tr-absent') = 0` — a bead with no bead-last directory
  has zero recorded rounds regardless of what else the root contains.

**Operator finding (live coverage gap).** At the time of this revision
(2026-09-17), the live reviews root held 130 `refinery-*` directories, and
81 of them did **not** match the counted glob — timestamp-only names such
as `refinery-20260911T045659Z` match no bead at all, and bead-first or
suffix-annotated names (`refinery-tr-…-<date>`,
`refinery-<date>-tr-<bead>-validation`) count for nothing. Directories
that do count for a bead include `refinery-20260915-tr-xaj` and the
`refinery-<date>-tr-t3a` family. Consequence: these directories prove that
reviews happened, but most of them advanced no round counter, so per-bead
round counts can undercount actual rejection history. Finite routing is
verified as implemented (below); the claim "all live rounds are bounded and
counted" is **not** supported by the directory evidence. Recording the
mismatch here is the operator finding; renaming directories or changing the
glob is a live-policy change outside this verification's scope.

**Nominal constants (source-verified).**

- `NOTIFY_ROUNDS = 6`, `PARK_AFTER_ROUNDS = 12`, `HARD_POOL = None` (with
  the hard-pool threshold constant `HARD_POOL_AFTER = 3` inert while
  `HARD_POOL` is None); only the standard polecat pool is used for rework.
- Park: `exhausted_rounds()` returns the round count only when
  `count >= 12` **and** `count > acknowledged`; otherwise 0. When it fires,
  `park_exhausted` sets a `review_hold` (prefix "Parked by
  tributary-reconcile"), turns `recovery.rework_authorized` off, routes the
  bead to the human, and records `recovery.rounds_parked`. This is the
  spend valve; the park is itself a hold, so later ticks keep it at `hold`.
- `recovery.rounds_acknowledged = <n>` metadata does **not** raise both
  thresholds by `n`. It changes the comparison: the park requires strictly
  more unacknowledged rounds, and the recorded park count is the full
  count, not `12 + n`. Probed I/O of the live function (acknowledged = 12):
  count 12 → `0` (not parked), count 13 → `13`, count 23 → `23`;
  acknowledged = 0: count 11 → `0`, count 12 → `12`.
- Notify: `notify_rounds()` emits the single informational mail when
  `count >= 6` **and** `count > acknowledged` **and**
  `recovery.rounds_notified < 6`, then records `recovery.rounds_notified`.
  Counts at or below the acknowledged count notify nothing; once notified,
  a source is never re-notified. Routing to the polecat pool continues
  throughout — this is visibility, not a gate. Probed I/O (live function,
  `--apply`-less): acknowledged = 6 at count 6 → no notification; already
  notified at 6 → no re-notification; first crossing of 6 → exactly one
  `notify_rounds` event.
- Foreign or operator-owned holds are never overwritten by the routing
  machinery (`park_exhausted` keeps a foreign hold as `keep_foreign_hold`);
  release paths are explicit (operator acknowledgement covering the round
  count, or a refinery hold whose reason is superseded by a later head).

The earlier draft of this report cited directory names such as
`refinery-tr-xaj-…` and `refinery-tr-47yad-…` as evidence that "the round
machinery [is] operating"; per the counting rule above, bead-first names
like those are not counted rounds, and a `polecat-*-rework-*` directory was
never a refinery rejection round at all. The corrected claim is the scoped
one: the counter, park, and acknowledgement logic is verified at the
function level with the probes above; live directory coverage is partial
(operator finding), so directory counts understate rejection history.

## Finding 6 — preserved holds (verified live)

Hold semantics in the live gate, unchanged and enforced:

- `review_hold` metadata (unless explicitly CLEARED-prefixed, the legacy
  discipline) or a `hold:` label parks the bead as `blocked`, routed to the
  human. The gate never infers clearance from a successful check, a later
  commit, or the words "hold cleared" in notes.
- An operator audit verdict of `reject` at the current head parks the bead
  (`hold`, "operator self-review found unresolved defects at this head; see
  audit.report").
- A repair authorization allows corrective coding; it never clears a hold —
  "corrective work does not authorize completion" is the exact live
  decision string.
- Holds are evaluated before checks, so a held bead stays held regardless
  of CI color. The one exception-shaped path is the explicit
  `review.corrections_while_held` opt-in described in Finding 4, which can
  raise a `review` action for new findings or an unreviewed corrective
  handoff *while the merge hold remains* — it never clears the hold,
  undrafts the PR, or authorizes a merge.

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
- The live gate has **no CI timeout**: a failure conclusion routes rework
  immediately — even while other checks are still pending, in either rollup
  order — and pending is the fail-closed remainder: a slow PR parks
  indefinitely until its gating checks conclude, unless an earlier
  condition (hold, draft, changes-requested, review evidence) applies
  first.
- Exact check contexts: seven ruleset-required contexts (listed above),
  while the completion gate enforces the operator's stricter all-green
  policy over every observed check on the exact head.
- Failure-first non-green behavior is implemented as documented and probed
  against the live functions. Correction routing constants (6 → notify,
  12 → park) are verified at the function level, with the acknowledgement
  semantics corrected above; live directory counting undercounts rejection
  history (operator finding), so directory evidence alone does not bound
  live rounds. Preserved holds — including the ordinary draft/hold
  disposition and the corrective-review opt-in — are implemented and probed
  as described.

## Corrections in this revision (audit R1/R2/R3 mapping)

- **R1 → Finding 2 (and the Conclusion's pending bullet).** Replaced the
  "only after every check is COMPLETED are conclusions evaluated" claim
  with the live failure-first scan (probe:
  `('rework', 'Windows: FAILURE')` in both rollup orders; regression test
  `test_pending_check_never_masks_failed_job_in_either_rollup_order`);
  corrected status (`COMPLETED`) vs conclusion (`SUCCESS`/`FAILURE`/…)
  terminology; qualified every blanket "stays pending" claim with the
  earlier hold/review/changes-requested/failure conditions that win first.
- **R2 → Finding 5 (and the Conclusion's routing bullet).** Documented the
  real counted naming pattern (`refinery-*-<bead>`, bead id last) with
  probed input/output; corrected the acknowledgement claim
  (`count >= 12 and count > acknowledged`, not "thresholds raised by n";
  probed 0/13/23 at acknowledged = 12); documented `notify_rounds`
  suppression; recorded the 81 uncounted live review directories (of 130 at
  revision time) as an operator finding instead of claiming all live rounds
  are bounded.
- **R3 → Finding 4 (and Finding 6's hold bullet).** Corrected the draft
  disposition from `pending` to `hold` with the exact live reason string
  and `reconcile_one` storage (`merge_result = blocked`,
  `gc.routed_to = human`, `status = open` preserved); documented the
  `review.corrections_while_held` corrective-review opt-in (new findings →
  `review`, unreviewed handoff → `review`, missing evidence → `pending`,
  clean evidence → back to `hold`) with probed I/O and the behavioral
  tests `test_opted_in_draft_gets_new_findings_reviewed_without_clearing_hold`
  and `test_held_review_poll_fetches_current_evidence_and_returns_to_hold_when_clean`,
  without implying it authorizes merging or weakens holds.
