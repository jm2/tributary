# Dependency update policy

Tributary keeps Dependabot enabled for the root Cargo package, the independent
fuzz workspace, and GitHub Actions. The policy separates routine updates from
changes which need coordinated repair:

- Compatible Cargo and Actions patch/minor updates may use native GitHub
  auto-merge, but only after every required branch-protection check is green.
- `sea-orm` and `sea-orm-migration` always share one Dependabot group and must
  retain matching manifest requirements and resolved versions.
- Cargo major updates remain reviewed changes and normally arrive
  individually; the coupled SeaORM pair is the intentional grouped exception.
- Exact `dtolnay/rust-toolchain` updates remain enabled and have their own
  group. They are never automatically merged, even when GitHub classifies the
  Rust release bump as SemVer-minor.
- The fuzz crate has its own Dependabot entry because `fuzz/Cargo.toml`
  intentionally declares a separate workspace and owns `fuzz/Cargo.lock`.

## Root Cargo update repair

A root Cargo update may also change production dependencies inherited by the
fuzz workspace. Pull-request CI compares the root lock against the exact base
SHA, loads that base's `fuzz/Cargo.lock`, and enforces absolute
root-authoritative equality for every shared production-direct dependency in
the submitted `fuzz/Cargo.lock`. When the base fuzz lock needs a direct
transition, CI applies the same bounded base-fuzz-to-head proof used by the
writer; raw lock edits cannot bypass it merely because the direct versions now
match.

GasCity should handle that expected failure in a trusted isolated worktree:

```sh
python3 scripts/sync_fuzz_lock.py write --base-ref <exact-base-sha>
python3 scripts/sync_fuzz_lock.py check --base-ref <exact-base-sha>
```

The write command uses targeted `cargo update --precise` operations. It does
one path-package re-resolution first when the root dependency declaration
changed. That permits a new direct major to coexist with an older major still
required transitively. It then requires exact transition readback, rejects any
package-identity drift outside the exact old and resulting fuzz closures, and
compares every changed dependency edge by its resolved `(name, version)`
identity. Formatting-only disambiguation is harmless, but a semantic rebind
must have either an exact authorized parent or a complete exact old/new target
surface; crate-name coincidence grants no authority. The Tributary path record
may move only the exact requested direct transitions. The independent fuzz
resolver may select a different compatible transitive version inside its exact
new closure. Identities also present in the current root must match its
immutable source/checksum metadata, while locked Cargo fetch verifies
resolver-only identities. A broad resolver rewrite, failed command, failed
materialization, or failed proof restores the original fuzz lock.

The command does not commit, push, approve, or merge. The Repairer must verify
the resulting lock diff, commit the repair to the existing Dependabot branch,
and require the complete CI matrix. Graph rewrites which cannot be proven by
this bounded version-selection policy fail closed for manual repair.
`--offline` is available for a pre-populated Cargo cache; normal repair runs
may use the registry to obtain the exact versions already selected in the root
lock.

Transitive-only root-lock updates are deliberately not projected into the
independent fuzz resolver: its graph can legitimately select a different
compatible version. The dedicated `/fuzz` Dependabot entry and locked fuzz CI
own those updates. A security update affecting both lockfiles must therefore
be raised or repaired in both rather than inferred from coincident package
names. Likewise, when the exact base fuzz lock already needs no direct repair,
an ordinary fuzz-only Dependabot update remains in that independent lane and
is not forced through a root-transition closure.

## Rust toolchain and MSRV repair

The two numeric `dtolnay/rust-toolchain@X.Y.0` refs are the Dependabot signal
for a Rust release proposal. On that PR, GasCity should run:

```sh
python3 scripts/sync_rust_toolchain.py --from-actions
python3 scripts/sync_rust_toolchain.py --check
```

This synchronizes the Cargo MSRV, exact MSRV and coverage toolchains, cache
keys, versioned step labels, and current README commands. The CI job/check name
remains the stable `MSRV` so future compiler bumps do not rename the hosted
context. The bump is feasible only when
the full Linux, macOS, Windows, Flatpak, fuzz, audit, coverage, and repository
policy matrix passes. The repository enforces consistency and prevents native
auto-merge; GasCity must separately provide independent semantic review and
the normal Refinery exact-SHA merge gate before merging it.

Rust 1.92 is today's declared floor, not a permanent pin. Dependabot remains
enabled for `dtolnay/rust-toolchain`; each feasible release proposal goes
through this dedicated coordinated, non-auto-merge lane.

## Deployment gate migration

This change deliberately does not mutate GitHub rulesets or the local GasCity
configuration. At the time of adoption, the live `main` ruleset does not
require either the old versioned MSRV context or the new stable `MSRV` context.
Before enabling routine Dependabot auto-merge, maintainers must make the live
required-check set match the policy promised here, including `MSRV`. GasCity's
Tributary hosted-check configuration must also replace `MSRV (1.92)` with
`MSRV` in the same deployment. Future Rust bumps then keep the stable context
and need no additional gate rename.

## Workflow security boundary

This Dependabot auto-merge workflow does not check out pull-request code while
holding a write token. (Other repository workflows, including semantic review,
have separate permission and trust boundaries and are not covered by that
claim.) The workflow uses `pull_request`, verifies the actor, PR author, and
repository, and enables GitHub's native guarded auto-merge without checking
out the branch.

Its first job has read-only pull-request access. It verifies the event's exact
head before and after paginated changed-file enumeration, requires the observed
file count, rejects current or previous names for the privileged workflow, and
then revalidates the head immediately before and after running the pinned
metadata action with read authority. Per-PR concurrency cancels stale runs as
defense in depth. The separate write-capable job contains no third-party
action: it revalidates that exact head immediately before asking GitHub to
enable auto-merge with an atomic expected-head guard.
Thus a same-count H1/H2 race or mixed-path self-update fails closed rather than
executing an H1 action ref with write authority.

Lockfile and toolchain repair intentionally remain GasCity Repairer operations
instead of a `pull_request_target` writer. This keeps untrusted dependency or
pull-request content out of a privileged execution context.

GitHub references: [Dependabot options
reference](https://docs.github.com/en/code-security/reference/supply-chain-security/dependabot-options-reference)
and [Automating Dependabot with GitHub
Actions](https://docs.github.com/en/code-security/tutorials/secure-your-dependencies/automate-dependabot-with-actions).
