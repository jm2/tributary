# Dependency update policy

Dependabot proposes updates for Rust crates (the root Cargo workspace), the Rust compiler floor
(`.github/rust-toolchain.toml`), and GitHub Actions.

The repository is one Cargo workspace with one `Cargo.lock`. The fuzz harness in `fuzz/` is a
non-default member: plain `cargo build`, `cargo test`, and `cargo clippy` cover only the
application, while cargo-fuzz and `cargo clippy -p tributary-fuzz` build the harness from the
same lock. A Cargo update therefore changes a single lockfile and needs no follow-up repair.
CI's Security Audit runs `cargo audit` against that lock; its advisory exceptions live in
`.cargo/audit.toml`, each with the reason it is inactive and a review date.

Cargo and Actions patch/minor updates are each batched into one group. Cargo majors arrive
individually, except `sea-orm` and `sea-orm-migration`, which always share a group and must keep
matching manifest requirements and locked versions. The `dependabot-automerge.yml` workflow may
enable GitHub's native auto-merge for patch and minor updates only, and routine auto-merge stays
off until the `main` ruleset requires the repository's full all-checks policy (see "Closing the
gap (the machine gate)" in `docs/refinery-config.md`). Compiler proposals and updates to the
pinned `dtolnay/rust-toolchain` and `dependabot/fetch-metadata` actions never auto-merge.

A `rust-toolchain` proposal is completed in a trusted worktree with
`python3 scripts/sync_rust_toolchain.py --from-toolchain` followed by `--check`
(`--set X.Y` for a maintainer-initiated bump). This synchronizes the Cargo `rust-version`, the
MSRV and coverage toolchain pins and cache keys, and the README commands, without changing the
pinned action commit; the CI check keeps the stable name `MSRV`. Both
`dtolnay/rust-toolchain@<sha> # master` pins must name the same full commit from that action's
`master` history. A bump is feasible only when the full CI matrix passes.

The auto-merge workflow runs on `pull_request` and never checks out pull-request code. It
verifies the actor, author, and repository, re-reads the exact head SHA around changed-file
enumeration and the metadata action, refuses any PR that touches the workflow itself, and
enables auto-merge with an expected-head guard, so a head that moves mid-run fails closed.
Repairs run in a trusted worktree, never in a privileged `pull_request_target` job.
