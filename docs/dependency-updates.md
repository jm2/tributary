# Dependency update policy

Tributary keeps Dependabot enabled for the root Cargo package, the independent
fuzz workspace, and GitHub Actions. The policy separates routine updates from
changes which need coordinated repair:

- Compatible Cargo and Actions patch/minor updates may use native GitHub
  auto-merge, but only after every required branch-protection check is green.
  Native auto-merge waits only on the checks the live `main` ruleset marks
  required, so the auto-merge workflow reads those rulesets first and refuses
  to enable it until the complete policy check set (see "Deployment gate
  migration" below) is required — routine auto-merge stays off by
  construction until that live rollout is verified.
- `sea-orm` and `sea-orm-migration` always share one Dependabot group and must
  retain matching manifest requirements and resolved versions.
- Cargo major updates remain reviewed changes and normally arrive
  individually; the coupled SeaORM pair is the intentional grouped exception.
- Rust compiler updates are proposed through `.github/rust-toolchain.toml` and
  never auto-merge. Updates to the digest-pinned `dtolnay/rust-toolchain`
  action implementation are a separate, manually reviewed lane.
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

`.github/rust-toolchain.toml` is Dependabot's authoritative signal for a Rust
compiler proposal. It lives below `.github` so it does not override a
developer's selected toolchain merely by entering the repository. On a
`rust-toolchain` ecosystem PR, GasCity should run:

```sh
python3 scripts/sync_rust_toolchain.py --from-toolchain
python3 scripts/sync_rust_toolchain.py --check
```

This synchronizes the Cargo MSRV, explicit MSRV and coverage compiler inputs,
cache keys, versioned step labels, and current README commands. It never
changes the action implementation. The CI job/check name remains the stable
`MSRV` so future compiler bumps do not rename the hosted context. The bump is
feasible only when the full Linux, macOS, Windows, Flatpak, fuzz, audit,
coverage, and repository-policy matrix passes. The repository enforces
consistency and excludes the entire `rust-toolchain` ecosystem from native
auto-merge; GasCity must separately provide independent semantic review and
the normal Refinery exact-SHA merge gate before merging it.

Rust 1.94 is today's declared floor, not a permanent pin. Dependabot remains
enabled for `.github/rust-toolchain.toml`; each feasible compiler proposal goes
through this dedicated coordinated, non-auto-merge lane.

For a maintainer-initiated compiler bump rather than a Dependabot proposal:

```sh
python3 scripts/sync_rust_toolchain.py --set X.Y
```

The two `dtolnay/rust-toolchain@SHA # master` refs instead pin executable
third-party action code. Their SHA must be a commit in the action's permanent
`master` history, as required by that upstream action, and both jobs must use
the same full 40-character commit. GitHub Actions Dependabot may propose a new
master-history commit independently; the dependency-name guard prevents every
such proposal from auto-merging. Review the action-code diff and run the full
matrix, but do not run the compiler synchronizer unless the compiler manifest
also changed through its own reviewed proposal.

## Deployment gate migration

The auto-merge workflow's writer mechanically reads the active rulesets that
apply to `main` and refuses to enable auto-merge until they require the
complete policy check set below. Until that live rollout is done, a routine
Dependabot patch/minor PR simply never gets auto-merge enabled (the workflow
run fails loudly at the precondition) and must be merged through the normal
reviewed path.

The enforcement gap being closed: at the time of this change, the live
ruleset "Require CI before merge (main)" (id 17650907) requires only seven
GitHub Actions checks — Security Audit, Linux (x86_64), Linux (aarch64),
macOS (aarch64), Windows (x86_64), Flatpak (Linux), and MSRV — so Coverage,
CodeQL, Codacy, CodeRabbit, Windows (aarch64), and every bot review remain
advisory, and native Dependabot auto-merge waits only on that narrower set
and could land a dependency PR while a policy check or bot review is still
pending or failing.

### Required ruleset additions (exact live contexts and app bindings)

Check contexts verified live on a green pull request. The app binding is part
of the requirement: a same-named check from a different integration must not
satisfy the gate.

| Context | Integration (app id) |
|---|---|
| Coverage (Linux x86_64) | GitHub Actions (15368) |
| Windows (aarch64) | GitHub Actions (15368) |
| Bot Review Gate | GitHub Actions (15368) |
| CodeQL | GitHub Advanced Security (57789) |
| Analyze (python) | GitHub Advanced Security (57789) |
| Analyze (rust) | GitHub Advanced Security (57789) |
| Analyze (actions) | GitHub Advanced Security (57789) |
| Codacy Static Code Analysis | Codacy (56611) |
| CodeRabbit | unbound (commit-status context, no app id) |

`Bot Review Gate` is reported by the repository-owned
`.github/workflows/bot-review-gate.yml`, which fails while the pull request
has unresolved, non-outdated review threads started by a bot. The CodeQL
`Analyze (…)` contexts follow the languages configured in the CodeQL default
setup; adding or removing a language changes those contexts and must update
this ruleset and the auto-merge precondition in the same reviewed change.

### Rollout order and live validation

1. Land `.github/workflows/bot-review-gate.yml` first (this change) so the
   `Bot Review Gate` check actually reports on pull requests before it can be
   marked required. While the ruleset is still narrow, the gate is advisory
   and the auto-merge precondition keeps routine Dependabot auto-merge off.
2. Edit ruleset 17650907 to add every context in the table above with the
   listed app binding (`CodeRabbit` unbound). Verify the saved ruleset
   actually lists all sixteen required checks — the save, not the intent, is
   what the auto-merge precondition reads.
3. Validate against a live pull request before treating the widened gate as
   authoritative: confirm all widened checks report on that PR, address and
   resolve any actionable bot review threads, then confirm the next
   Dependabot patch/minor PR's "Dependabot auto-merge" run passes the
   "Require the live ruleset to enforce the full policy gate" step and reaches
   the guarded merge enablement. That run passing is live proof both that the
   widened ruleset is active and that the precondition agrees with it. Do not
   claim enforcement is active until this verification has passed on
   GitHub's side of the settings.

The stable `MSRV` context above is part of the required set for the same
reason it was introduced: GasCity's Tributary hosted-check configuration must
replace the old versioned `MSRV (1.92)` expectation with `MSRV` in the same
deployment. Future Rust bumps then keep the stable context and need no
additional gate rename.

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
action: it first reads the active `main` rulesets (read-only `administration`
authority, used for nothing else) and refuses to enable auto-merge until the
full policy check set is required, then revalidates that exact head
immediately before asking GitHub to enable auto-merge with an atomic
expected-head guard.
Thus a same-count H1/H2 race or mixed-path self-update fails closed rather than
executing an H1 action ref with write authority, and a narrowed ruleset can
never silently widen what native auto-merge waits on.

Lockfile and toolchain repair intentionally remain GasCity Repairer operations
instead of a `pull_request_target` writer. This keeps untrusted dependency or
pull-request content out of a privileged execution context.

GitHub references: [Dependabot options
reference](https://docs.github.com/en/code-security/reference/supply-chain-security/dependabot-options-reference)
and [Automating Dependabot with GitHub
Actions](https://docs.github.com/en/code-security/tutorials/secure-your-dependencies/automate-dependabot-with-actions).
