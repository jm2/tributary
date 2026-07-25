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

## Reviewer wait-for-comments gate (LOCAL-ONLY)

A second piece of upstream refinery behavior was miscalibrated for this rig,
recorded by bead `tr-xqu`.

The withdrawn upstream PR `#235` ("gate refinery closes on review, hosted CI,
and bot comments", commits `4ad09b3` / `720bd22`) hardcoded a reviewer wait
gate that listed `gemini-code-assist[bot]` and `chatgpt-codex-connector[bot]`.
On `jm2/tributary`, the Gemini Code Assist GitHub App announced its sunset
on 2026-07-23 and now posts an identical tombstone comment on every PR; the
withdrawn gate treated that comment as unaddressable, so every branch was
rejected forever and merge throughput froze. PR `#235` was withdrawn and
replaced by upstream PR `#239`, which deliberately excludes any reviewer gate
on automated reviewer comments. The live pack at the city's pinned sha
(`9bb06e00`) does not contain the gate — verified: no `review_bots` reference
in the shipped refinery prompt, formula, or assets.

The operator wants a wait-for-review-comments rule restored, but:

1. **LOCAL-ONLY**, not upstream-bound. The reviewer policy is a deployment
   preference (one operator's choice of which comments are blocking), not a
   Gastown defect. It lives in the local pack stack
   (`file:///home/jmulesa/gascity-packs//gastown`), is listed in
   `LOCAL-ONLY.md` at the packs repo root, and must never ride into an
   upstream PR.

2. **Keyed on the live Claude reviewer.** The `claude-review.yml` workflow
   became live on `jm2/tributary` (commit `68ed2b0`, "ci: add automatic
   Claude PR review", 2026-07-25 18:42 EDT). The first run on a real PR
   (PR `#178`, head `polecat/tr-t3i` at `ce09d0c`, run `30178620793`)
   completed at 2026-07-25 23:05:09Z and posted under the login
   **`claude[bot]`** — the GitHub App is installed, so the workflow posts
   as the App rather than as `github-actions[bot]` (which would be the
   `claude-code-action` fallback when the App is absent). `review_bots` is
   pinned to that exact value.

3. **Bounded**, not "wait forever". The original Gate 3 in PR `#235` had no
   bound, which is exactly the deadlock the decommissioned Gemini bot
   produced. The restored gate has four terminal conditions, none of which
   can stall indefinitely:

   - **Reviewer comments present** → evaluate them via the gate's normal
     path (resolved thread, in-thread substantive non-bot reply, or an
     APPROVED review).
   - **`Claude PR Review` check-run concluded, no comments** → proceed.
     This is the operator's stated case ("especially if CI has completed
     and after that there's still no comments") and keys on the
     *strongest available signal* (the workflow's check-run conclusion,
     not a clock). Stamp `review_gate_result=no-comments` and proceed; do
     not burn the full timeout here, that is pure latency on every clean
     PR.
   - **Absolute timeout** → proceed loudly, stamping what was waited on
     and for how long. Backstop for the case where the review workflow
     never registers a check-run at all (never triggered, workflow
     disabled, or the event was missed). Must be tuned to real CI
     duration, not the `900` default — Tributary's CI regularly runs
     `20-53` minutes (Coverage + cross-compiled `aarch64` tail); the
     `ci_timeout_seconds = "3600"` override above provides that margin
     and the same bound should bound the reviewer wait on this rig.
   - **Review workflow failed or errored** → do NOT block. Report loudly
     and proceed. A broken reviewer must not freeze the queue. Same
     lesson as the CI gate: an absolute gate that cannot distinguish
     "reviewer says no" from "reviewer is broken" converts every reviewer
     outage into a fleet-wide deadlock.

### What this rig overrides

The actual gate code lives in the local pack stack at:

- `gastown/agents/refinery/prompt.template.md` — the Gate block, plus the
  `review_bots` reference.
- `gastown/formulas/mol-refinery-patrol.toml` — the bound vars
  (`review_bots`, `review_timeout_seconds`, `review_workflow`).

The rig-side clone does not own those files. They are edited in the local
pack fork (`~/gascity-packs`, pinned via
`source = "file:///home/jmulesa/gascity-packs//gastown"` in `city.toml`),
and the city picks them up on next controller start. Per `LOCAL-ONLY.md`,
those edits ride only into local rigs and never into an upstream PR.

The pin values for this rig:

| var | value | meaning |
|---|---|---|
| `review_bots` | `claude[bot]` | Read off PR `#178` run `30178620793` on 2026-07-25. App is installed, so this is the Claude GitHub App login, not the `github-actions[bot]` `claude-code-action` fallback. |
| `review_timeout_seconds` | `3600` | Same one-hour budget as `ci_timeout_seconds` above. Anything tighter risks a false positive on a long CI run that is still queued for review. |
| `review_workflow` | `Claude PR Review` | The check-run whose conclusion is the strongest "reviewer ran" signal. Bound to the `.github/workflows/claude-review.yml` workflow on `jm2/tributary main` and `jm2/kisakcod master`. |

If the App is ever uninstalled and the workflow falls back to
`github-actions[bot]`, the rig-side owner of these vars must rerun the
discovery step (push to any PR, read the first real review's author) and
update `review_bots`. Guessing that value is how the original unsatisfiable
gate got built.
