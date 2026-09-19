# Dependency update policy

Tributary keeps Dependabot enabled for the root Cargo package, the independent
fuzz workspace, and GitHub Actions. The policy separates routine updates from
changes which need coordinated repair:

- Compatible Cargo and Actions patch/minor updates are merged through the
  normal reviewed path. **Native GitHub auto-merge enablement is staged off in
  this change** and is not re-introduced here: the
  `dependabot-automerge.yml` workflow is now a strictly read-only readiness
  *diagnostic* that inspects the live `main` rulesets and reports whether they
  require the complete policy check set (see "Deployment gate migration"
  below). It contains no merge request, no auto-merge enablement, and no write
  permission of any kind. Re-enabling unattended merge requires a separate
  reviewed change with live freshness/rollout evidence; a passing inspection is
  a diagnostic, never merge authority. A green required-check set alone never
  authorizes a merge while any other check or bot review is pending or failing.
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

## Security audit boundaries

`cargo audit` at the repository root sees only the production lock. The fuzz
crate is a separate workspace with its own `fuzz/Cargo.lock`, and its resolver
may legitimately select different transitive versions, so lock coherence proves
nothing about whether that graph was security-audited. CI audits both graphs
explicitly with `scripts/audit_lockfiles.py`:

- each graph is scanned with its own lockfile passed via `--file`, from its own
  directory, so cargo-audit cannot silently fall back to the root lock;
- each graph takes only its own `[advisories].ignore` list — the root
  `.cargo/audit.toml` for the production lock and `fuzz/.cargo/audit.toml` for
  the fuzz lock — so an exception justified for one graph never suppresses a
  finding in the other;
- the JSON report is validated before it is trusted: the scanned dependency
  count must match the requested lockfile, the applied ignore set must match
  that graph's scoped exceptions, and no vulnerabilities may remain.

`scripts/test_audit_lockfiles.py` keeps this honest with fixture tests proving
the fuzz lock is actually selected and that a fuzz-only finding fails the audit
even while the root audit stays green.

A security advisory affecting both graphs must still be reviewed and repaired
for each independently: an exception (or a fix) in one lock does not carry to
the other.

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

The #225 documentation change itself deliberately did not mutate GitHub
rulesets or the local GasCity configuration; the MSRV migration it
anticipated has since landed through separate reviewed changes. The live
`main` ruleset ("Require CI before merge (main)") requires the stable `MSRV`
context alongside Security Audit, Linux (x86_64), Linux (aarch64),
macOS (aarch64), Windows (x86_64), and Flatpak (Linux), and GasCity's
Tributary hosted-check configuration already emits `MSRV` rather than the old
versioned `MSRV (1.92)` context. Future Rust bumps therefore keep the stable
context and need no additional gate rename.

The `dependabot-automerge.yml` workflow is a read-only readiness diagnostic:
it reads the active rulesets that apply to `main`, reports whether they require
the complete policy check set below with `require-conversation-resolution`, and
fails loudly when the rollout is incomplete. It does **not** enable GitHub
native auto-merge — that write path is deliberately absent from this staged
change — so a routine Dependabot patch/minor PR is merged through the normal
reviewed path, and a passing inspection never merges anything. The
machine-readable rollout prerequisites live in
`.github/bot-review-gate-rollout.json`, and the operator-facing, strictly
read-only validator is `scripts/preflight_bot_review_gate.sh`; external
activation itself belongs to the operator validation bead tr-rcvys.

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
| --- | --- |
| Coverage (Linux x86_64) | GitHub Actions (15368) |
| Desktop Metadata | GitHub Actions (15368) |
| SHA256 Checksums | GitHub Actions (15368) |
| Windows (aarch64) | GitHub Actions (15368) |
| Bot Review Gate | dedicated gate-publisher App (repository variable `BOT_REVIEW_GATE_APP_ID`) |
| CodeQL | GitHub Advanced Security (57789) |
| Analyze (python) | GitHub Advanced Security (57789) |
| Analyze (rust) | GitHub Advanced Security (57789) |
| Analyze (actions) | GitHub Advanced Security (57789) |
| Codacy Static Code Analysis | Codacy (56611) |
| CodeRabbit | unbound (commit-status context, no app id) |

`Bot Review Gate` is reported by the repository-owned
`.github/workflows/bot-review-gate-publisher.yml` — the trusted publisher —
and refreshed by the announcer `.github/workflows/bot-review-gate.yml`. The
split is one half of the enforcement boundary: GitHub executes a
`pull_request`-triggered workflow's pull-request revision, so the announcer
deliberately contributes nothing but the refresh events (it evaluates
nothing and holds no token scopes, and its job check-run carries a different
name). The publisher is triggered by the announcer's `workflow_run`
completions, and GitHub always executes a `workflow_run` workflow's
default-branch revision — so a pull request cannot alter the evaluation or
publication logic, cannot no-op the gate under the required name, and can
only suppress its own refresh. A suppressed (or dying) refresh is not
itself fail-safe, though: check runs attach to the head SHA and the latest
completed run under the required name decides, so a head that already
carries a green verdict would keep it. The publisher therefore never exits
without publishing at a known head — every discovery-level refusal (a
failed discovery query, an announcement with no candidate pull request, an
announcement whose every candidate was skipped) publishes the superseding
failing verdict at the announced head. An unreported required check blocks
the merge only at a head that has never been evaluated.

The split alone does not make the context unforgeable, though: every
Actions workflow job — including anything a pull request adds to a
PR-controlled workflow, or renames the announcer's job into — publishes its
check runs under the shared GitHub Actions integration (15368), and
required-check matching is by check-run name on the head SHA. The required
context is therefore also identity-bound: the publisher's workflow token
holds no `checks` grant at all and structurally cannot publish, and the
verdict is posted only with a minted installation token of the dedicated
bot-review-gate-publisher GitHub App — so the check run is authored by that
App's integration, which no pull-request-controlled job can produce a check
run under. The ruleset entry and the Dependabot readiness diagnostic both
require the context from that App's id (the repository variable
`BOT_REVIEW_GATE_APP_ID`, never 15368), and the publisher refuses to
publish anything if the App credentials are missing — the head's verdict
keeps its previous value and the failed job is the re-run signal. The
publisher binds its verdict to
the announcing run's head commit, re-derives the associated pull requests
through the API from that exact commit (the event's branch-derived
pull-request fields are never trusted), selects only pull requests actually
headed by that commit (a stacked descendant that merely contains it is not
evaluated — evaluating one would refuse on its head mismatch on every
refresh), never checks out any commit, and publishes exactly ONE
shared verdict per announced head (check runs attach to commits, not pull
requests, so a head shared by several open pull requests gets one
fail-if-any-fail verdict instead of competing per-PR publications whose
last write would decide for all of them; when no candidate exists or every
candidate was skipped, the one shared verdict is the superseding
not-evaluated failure described above). It fails while, at one exact
pull-request head, any of the following holds: a bot-started review thread is
not explicitly resolved (GitHub's "outdated" flag never substitutes for
resolution — moving code is not addressing a finding); a bot reviewer's
latest decisive review requests changes (only a CHANGES_REQUESTED, APPROVED
or DISMISSED review carries a conclusion, and a change request blocks even
when it opened no inline thread); or a bot reviewer's latest review predates
the head being evaluated (review evidence must be bound to the commit being
merged). A later approving or dismissing review by the same bot clears that
bot's conclusion; a comment-only review never does, and reviews submitted
after a dismissal are bound to the evaluated head like any other evidence.
Both the review-thread
and the review queries are paginated
completely, every page is validated, and the published result is bound to the
exact head, which is re-verified immediately before the green result. The
CodeQL
`Analyze (…)` contexts follow the languages configured in the CodeQL default
setup; adding or removing a language changes those contexts and must update
this ruleset and the Dependabot readiness diagnostic in the same reviewed
change.

### Staged activation (default off)

The publisher ships **inert**. Its first job reads the trusted,
repository-owned activation variable `BOT_REVIEW_GATE_ACTIVATION` — set only by
trusted administrators, never by a pull request — before the publishing job
can request its protected environment or mint an App token. The variable is
unset by default; while it is unset or `inactive`, the publishing job is
skipped entirely, no App-authored check run is published, and no merge
authority is claimed. Any value other than unset, `inactive`, or `active`
fails the activation job closed rather than being silently treated as
inactive.

Staging is kept safe by structural absence, not by a forged success: the
activation job holds no token scopes and publishes nothing, so an inactive
refresh leaves the required `Bot Review Gate` context absent. If the live
`main` ruleset already requires that App-bound context while the publisher is
inactive, its absence blocks the merge; the workflow deliberately does not
publish a workflow-token success to paper over that gap. Once the variable is
explicitly `active`, a missing or wrong credential, binding, or API failure
fails the publishing job — an active gate never silently skips.
`scripts/preflight_bot_review_gate.sh` validates the three phases (staged
inactive, active-but-incomplete, externally validated) strictly read-only.

### Refreshing the gate after thread resolution

Resolving a review thread fires no GitHub Actions event
(`pull_request_review_thread` is a webhook event, not an Actions trigger), so
the gate uses only documented Actions triggers and refreshes through:
pushes (`synchronize`), review submissions, edits, and dismissals
(`pull_request_review`), new review comments
(`pull_request_review_comment`), review requests addressed to a reviewer and
their withdrawal (`pull_request` `review_requested`/`review_request_removed`
— the events behind the re-review handshake that invalidates the reviewer's
earlier clean result at the head; note GitHub does not guarantee that an
Actions run fires when the requester or requested reviewer is a bot or App,
the common case for the gate's trusted reviewers, so these two types are
best-effort and the targeted dispatch below remains the guaranteed refresh
path for a re-review request that fired no announcer run), a pull request
closing (`pull_request` `closed` — a sibling sharing the head leaving the
candidate set fires no other event, so the closed type re-announces the
commit and the publisher's open-only recompute publishes the fresh shared
verdict for the remaining open pull requests), a targeted
`gh workflow run bot-review-gate.yml --ref <head branch> -f pr_number=<n>`
(`workflow_dispatch`), or a plain check re-run — every announcer completion
fires the publisher, which re-evaluates the current
state. Duplicate announcer refreshes queue rather than cancel: a cancelled
run is terminal and would leave a red `Bot Review Gate Trigger` check at the
live head that no later event re-runs, so the announcer serializes its
per-pull-request refreshes and the publisher's own per-head concurrency
collapses the burst. A dispatch must target the
pull request's head branch: the publisher binds its verdict to the
announcing run's head commit (`workflow_run.head_sha`), so a run announced
from any other ref is refused exactly like a stale head — no evaluation can
be bound to one pull request's required check from another commit. The
publisher's failure output
prints the dispatch path with the pull request number filled in.

Reopening a previously resolved thread fires no Actions event either —
thread state changes are webhook-only, and `pull_request_review_comment` fires
only for new comments — so once the Bot Review Gate has gone green at a head,
a reopened bot thread cannot refresh the check through any trigger. Merge-time
enforcement for that gap is GitHub's native **require-conversation-resolution**
setting: the rollout below requires the live `main` ruleset to carry it in the
same change that widens the required checks, so GitHub itself re-blocks the
merge the moment any resolved review thread is reopened, independent of every
check result. After a reopening, the check is still refreshed through the
usual re-run or documented dispatch path so its published evidence describes
the reopened state; the native rule is what makes the reopened state
merge-blocking in the interim.

### Reviewer rate-limit substitution (operator fallback)

When a required bot reviewer is stuck behind reviewer rate limiting, the
operator may document it in a repository-owned policy file on `main`,
`.github/bot-review-substitution.json`:

```json
{
  "substitute_reviewer": "chatgpt-codex-connector[bot]",
  "rate_limited_reviewers": [
    {
      "login": "coderabbitai[bot]",
      "reason": "reviewer rate-limited",
      "evidence": "https://github.com/jm2/tributary/pull/237#issuecomment-...",
      "documented_at": "2026-09-08T20:00:00Z"
    }
  ]
}
```

For a listed reviewer ONLY, the Bot Review Gate then waives the stale-evidence
violation when — and only when — every one of the following holds at the exact
head being evaluated:

1. the policy file exists on `main`, is well-formed, and lists that reviewer
   (the file is read from `main`, so a pull request can never edit its own
   waiver; a missing file disables the waiver entirely, and a malformed file
   warns and disables it);
2. the policy's `substitute_reviewer` has an APPROVED review bound to the
   exact evaluated head — a comment-only review is never an approval, and an
   approval at any other commit is precisely the stale evidence the gate
   refuses;
3. every review thread is resolved (bot and human alike);
4. no author has an outstanding change request (bot and human alike; the
   latest decisive review per author decides);
5. the violation being waived is `stale_bot_review_evidence` — unresolved
   review threads and outstanding change requests always block, and the
   waiver never extends to a reviewer the policy does not list.

Every granted waiver is printed by the check with its evidence link, and the
green result still carries the exact-head binding and the final publication
re-check. This substitution covers reviewer availability only: it does not
weaken the all-checks policy, does not touch human change requests, and does
not re-enable non-Dependabot auto-merge.

### Rollout order and live validation

0. **(tr-rcvys)** Register the two minimal GitHub Apps named in
   `.github/bot-review-gate-rollout.json`:

   - `tributary-bot-review-gate-publisher` with only `checks: write`, installed
     on this repository, with its credentials stored **exclusively** as secrets
     of the protected environment `bot-review-gate-publisher` (names
     `BOT_REVIEW_GATE_APP_ID` and `BOT_REVIEW_GATE_PRIVATE_KEY`); the
     environment is locked to **custom deployment branch policies naming
     exactly the `main` branch** (type `branch`; no tags, no other branches, no
     wildcards), with no required reviewers and no wait timer. The exact-main
     deployment branch policy — not the `protected_branches` flag, which
     authorizes *every* protected branch rather than an exact allowlist — is
     what makes the credentials reachable exactly where they are trusted —
     `workflow_run` completions from default-branch content — and nowhere else.
     Never store them as repository-level Actions secrets: every
     same-repository pull-request workflow can read those, which is the exact
     forged-verdict exposure the identity-bound context exists to prevent.
   - `tributary-ruleset-reader` with only `Administration: read`, whose
     credentials are stored as repository-level **Dependabot secrets** (Settings
     → Secrets and variables → Dependabot → Repository secrets) named
     `RULESET_READER_APP_ID` and `RULESET_READER_APP_PRIVATE_KEY`. Dependabot
     secrets, not Actions secrets, are the only secret store a
     Dependabot-triggered run can read; credentials stored as plain Actions
     secrets reach the job empty and the pinned
     `actions/create-github-app-token` action fails closed.

   Then set the repository variables `BOT_REVIEW_GATE_APP_ID` (the publisher's
   numeric App id) and `BOT_REVIEW_GATE_ACTIVATION`. Run
   `scripts/preflight_bot_review_gate.sh`: while activation is off it must
   report a consistent **staged-inactive** phase.
1. Land the gate pair (this change): the announcer
   `.github/workflows/bot-review-gate.yml` and the trusted publisher
   `.github/workflows/bot-review-gate-publisher.yml`, so the
   `Bot Review Gate` check can report on pull requests from default-branch
   content before it is marked required. The publisher ships **staged inert**:
   until `BOT_REVIEW_GATE_ACTIVATION` is explicitly `active`, the publishing
   job is skipped before it can request its environment or mint a token, no
   App-authored verdict is published, and no merge authority is claimed. Any
   value other than unset, `inactive`, or `active` fails closed. While the
   ruleset is still narrow, the gate is advisory and routine Dependabot
   auto-merge remains staged off.
2. **(tr-rcvys)** Edit ruleset 17650907 to add every context in the table above
   with the listed app binding (`CodeRabbit` unbound), and switch on **require
   conversation resolution** in the same edit — the Actions-trigger gap for
   reopened threads (see "Refreshing the gate after thread resolution") is
   enforced by that native rule, so it must be live before the widened gate is
   treated as authoritative. Verify the saved ruleset actually lists all
   eighteen required checks — the seven the ruleset already required (Security
   Audit, Linux (x86_64), Linux (aarch64), macOS (aarch64), Windows (x86_64),
   Flatpak (Linux), MSRV) plus the eleven additions in the table above — and
   that require-conversation-resolution is enabled; the save, not the intent,
   is what is enforced. Only after the ruleset is saved **and** the publisher
   is explicitly `active` does the gate become authoritative.
3. **(tr-rcvys)** Set `BOT_REVIEW_GATE_ACTIVATION=active`, re-run
   `scripts/preflight_bot_review_gate.sh` (expect a validated, active result),
   then validate against a live pull request: confirm all widened checks report
   on that PR, address and resolve any actionable bot review threads, exercise
   the refresh path by confirming the gate re-runs green at the same head after
   the last resolution (via the automatic triggers or the documented
   re-run/dispatch path), and confirm the `Dependabot auto-merge` readiness
   diagnostic reaches its "readiness met" report. That report is live proof
   that the widened ruleset is active and agrees with the manifest; it is a
   diagnostic, not an enablement. Do not claim enforcement is active until this
   verification has passed on GitHub's side of the settings.
4. **(separate reviewed change)** Only after (1)–(3) are verified may a separate
   change re-introduce unattended Dependabot auto-merge enablement with live
   freshness/rollout evidence. Until then, routine Dependabot auto-merge stays
   off and manual operator merges remain the supported path.

Until that rollout completes, the ruleset is still narrower than the
repository's all-checks policy: it does not yet require Coverage, CodeQL,
Codacy Static Code Analysis, the CodeRabbit status context, or bot-review
gating. Routine Dependabot auto-merge stays off, and the enablement path is
staged out of the workflow entirely (see "Closing the gap (the machine gate)"
in docs/refinery-config.md). Neither a configured manifest nor a prior green
head proves active enforcement: only a live, freshly published App-authored
verdict bound to the evaluated head counts, and the activation flag plus saved
ruleset are what make the gate authoritative.

The stable `MSRV` context above is part of the required set for the same
reason it was introduced — that migration has already landed, as described at
the start of this section: GasCity's Tributary hosted-check configuration
emits `MSRV` in place of the old versioned `MSRV (1.92)` expectation. Future
Rust bumps keep the stable context and need no additional gate rename.

## Workflow security boundary

The Dependabot readiness workflow is strictly read-only. It uses
`pull_request` (NOT `pull_request_target`) and never checks out pull-request
code. Its first job has read-only pull-request access: it verifies the event's
exact head before and after paginated changed-file enumeration, requires the
observed file count, rejects current or previous names for the privileged
workflow, and revalidates the head immediately before and after running the
pinned metadata action with read authority. Per-PR concurrency cancels stale
runs as defense in depth.

The inspection job holds `contents: read` and `pull-requests: read`, and
contains exactly one action — the pinned GitHub-org
`actions/create-github-app-token` (v2.2.1), which signs a JWT and exchanges it
for an installation token without executing any repository code. That token
carries `administration: read` and nothing else: reading rulesets requires the
`administration` permission, which the workflow `GITHUB_TOKEN` cannot hold (it
is not a valid `GITHUB_TOKEN` scope; declaring it makes GitHub reject the
workflow at validation). The job reads the rulesets that apply to `main` via
the branch-rules endpoint, reports readiness, and uses the token for nothing
else.

There is no write job, no merge request, and no auto-merge enablement in this
workflow: a narrowed ruleset can no longer silently widen what native
auto-merge waits on, because nothing here enables native auto-merge at all.
The publisher's App private key is stored only as a secret of the protected
`bot-review-gate-publisher` environment — never as a repository Actions
secret — and the publisher refuses to fall back to the shared workflow token.

Lockfile and toolchain repair intentionally remain GasCity Repairer operations
instead of a `pull_request_target` writer. This keeps untrusted dependency or
pull-request content out of a privileged execution context.

GitHub references: [Dependabot options
reference](https://docs.github.com/en/code-security/reference/supply-chain-security/dependabot-options-reference)
and [Automating Dependabot with GitHub
Actions](https://docs.github.com/en/code-security/tutorials/secure-your-dependencies/automate-dependabot-with-actions).
