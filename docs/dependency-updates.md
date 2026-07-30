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
SHA and fails if a changed shared direct dependency remains stale in
`fuzz/Cargo.lock`.

GasCity should handle that expected failure in a trusted isolated worktree:

```sh
python3 scripts/sync_fuzz_lock.py write --base-ref <exact-base-sha>
python3 scripts/sync_fuzz_lock.py check --base-ref <exact-base-sha>
```

The write command uses targeted `cargo update --precise` operations. It does
not commit, push, approve, or merge. The Repairer must verify that only the
expected lock state changed, commit the repair to the existing Dependabot
branch, and require the complete CI matrix. Graph rewrites which cannot be
expressed as a version substitution fail closed for manual repair.
`--offline` is available for a pre-populated Cargo cache; normal repair runs
may use the registry to obtain the exact versions already selected in the root
lock.

## Rust toolchain and MSRV repair

The two numeric `dtolnay/rust-toolchain@X.Y.0` refs are the Dependabot signal
for a Rust release proposal. On that PR, GasCity should run:

```sh
python3 scripts/sync_rust_toolchain.py --from-actions
python3 scripts/sync_rust_toolchain.py --check
```

This synchronizes the Cargo MSRV, exact MSRV and coverage toolchains, cache
keys, job labels, and current README commands. The bump is feasible only when
the full Linux, macOS, Windows, Flatpak, fuzz, audit, coverage, and repository
policy matrix passes. It then needs independent semantic review and the normal
Refinery exact-SHA merge gate; the Dependabot auto-merge workflow will not
merge it.

## Workflow security boundary

No workflow checks out pull-request code while holding a write token. The
auto-merge workflow uses `pull_request`, verifies the actor, PR author, and
repository, fetches only GitHub-provided Dependabot metadata, and enables
GitHub's native guarded auto-merge without checking out the branch.

Lockfile and toolchain repair intentionally remain GasCity Repairer operations
instead of a `pull_request_target` writer. This keeps untrusted dependency or
pull-request content out of a privileged execution context.

GitHub references: [Dependabot options
reference](https://docs.github.com/en/code-security/reference/supply-chain-security/dependabot-options-reference)
and [Automating Dependabot with GitHub
Actions](https://docs.github.com/en/code-security/tutorials/secure-your-dependencies/automate-dependabot-with-actions).
