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
