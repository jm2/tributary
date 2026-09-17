# P2.3-C acceptance evidence — presentation refinements (#29, PR #179)

Recorded: 2026-09-17 (acceptance reconciliation leg `tr-3asjp`). Revision 2,
same day: the display-backed evidence was regenerated with the GTK 4 Broadway
daemon and fail-closed positive runs after refinery rejection of the original
record (its reproduction pointed the GTK 4 test binary at the GTK 3
`broadwayd`, so the widget contract skipped silently behind a green suite).
Revision 3, same day: the refinery's repeat review reproduced that the
revision-2 harness block discarded cargo's own exit status (`cargo | tee`
followed by `SUITE=$?` reports tee's status, masking an injected exit 101 as
success), never required the contract ok-line it described, proceeded after
the readiness loop exhausted its timeout, and reaped the daemon only on the
success path. The reproduction below was rewritten fail-closed end to end
and then validated with a display-backed success run plus four failure
injections against the published text (harness-validation section).

This record supplies the visual/accessibility acceptance evidence for the merged
presentation refinements. PR #179 (`polecat/tr-ewq` → `main`, merge commit
`ca803ea5dffa4ce81065fe127a264badb0ed567b`, merged 2026-09-09) is the accepted
design; nothing in this leg re-litigates or re-implements it.

The widget-contract test reports `ok` both when it executes and when it skips on a
headless machine, so a green suite alone does not establish widget acceptance.
This record therefore pairs (a) a **negative control** proving the skip gate
works, with (b) **display-backed runs** in which the contract assertions
genuinely execute, and (c) an **operator manual matrix** for the checks that
require a physical desktop session.

## Disposition of the original #29 asks

| Issue ask | Merged disposition |
|-----------|--------------------|
| Remove track separation lines | `show_row_separators(false)` on the tracklist; Adwaita's dense-list vertical rhythm (`.data-table .cell { min-height: 24px }`) replaces the grid look (`src/ui/tracklist.rs`, `src/ui/style.css`). |
| Drop section separators, differentiate by colour (GNOME file browser style) | Browser panes keep a single 1px gutter whose contrast is toned to `opacity: 0.15` so it reads as a gutter, not a divider (`style.css` documents the HIG rationale and the standard-CSS constraint). Disabled panes hand their gutter to the next surviving pane; no gutter dangles at a window edge (`src/ui/preferences.rs`). |
| Item counts darker / lower opacity | Counts render as a dedicated dimmed caption label (`.browser-count { opacity: 0.75 }` on top of `dim-label caption numeric`), scoped so the header-bar scrubber is unaffected; zero-count rows render no "(0)" (`src/ui/browser.rs`). |
| Left align all columns (especially time) | Text columns (Title, Artist, Album, Genre, Composer, Date Modified) are left-aligned. Numeric columns (`#`, Time, Year, Bitrate, Sample Rate, Plays) remain right-aligned (`src/ui/tracklist.rs` `add_sorted_column` call sites), the standard treatment for numeric magnitude comparison; the literal "left align all" ask is therefore only partially adopted, and column-level visual judgment on that divergence belongs to the operator's physical review. |
| (not asked, part of the same merge) | Status bar aligns via `.statusbar-box` (4px/12px padding, centered content) instead of per-widget margins; screen readers announce each browser row as one utterance ("Artist Name, (12)") at the `GtkListItem` boundary. |

User documentation reconciliation: the `[Unreleased]` → *Changed* →
"Browser and tracklist presentation" CHANGELOG entry covers all of the above;
the AppStream description makes no presentation-specific claims, so nothing
there is stale.

## Automated evidence (display-backed)

Revision 2 (2026-09-17). The original record started `broadwayd`, which on
Fedora is the **GTK 3** daemon (gtk3-3.24.52); the matching GTK 4 daemon is
`gtk4-broadwayd` (gtk4 4.22.5). With the GTK 3 daemon, GTK 4 initialization
fails and `widget_test_session::acquire` (`src/ui/mod.rs`) silently skips
every widget assertion while the suite still reports `ok` — the original
"widget contract executed" claim was unverifiable, and the refinery rejected
it. The procedure below was re-executed end to end on this host. Every
positive run now fails closed twice: in code — `TRIBUTARY_WIDGET_TESTS_FAIL_CLOSED`
makes both acquisition skip paths assert instead of skipping — and in the
harness, which captures cargo's own exit status (no pipeline sits between
cargo and the status check), aborts on either skip diagnostic, requires the
consolidated contract's ok-line, fails when daemon readiness times out, and
reaps the daemon through an EXIT trap on every exit path.

Two further traps this regeneration demonstrated first-hand, both now baked
into the procedure:

- The daemon's socket lives under `$XDG_RUNTIME_DIR`, so the daemon and the
  test process must export the **same** runtime directory. With the daemon
  isolated but the test process left on the ambient one, GTK initialization
  failed and (pre-fail-closed) the contract skipped behind an exit-0 suite —
  retained as the worked example `suite.log` in the fingerprint list below.
- Pick a Broadway display whose HTTP port (8080+N) is free: stale daemons
  from earlier sessions held `:5` and `:9` on this host; these runs use `:6`.

Exact reproduction (fails closed at every step):

```sh
command -v gtk4-broadwayd >/dev/null || exit 1  # GTK 4 daemon; never the GTK 3 broadwayd

EVIDENCE=$(mktemp -d "${TMPDIR:-/var/tmp}/p23c-evidence.XXXXXX")
mkdir -m 700 -p "$EVIDENCE/runtime"
export XDG_RUNTIME_DIR="$EVIDENCE/runtime"      # shared by daemon AND test process
export TRIBUTARY_WIDGET_TESTS_FAIL_CLOSED=1     # skip paths assert instead of skipping

BROADWAY_PID=
cleanup() {                     # every exit path reaps exactly OUR daemon
  rc=$?
  [ -n "$BROADWAY_PID" ] && kill "$BROADWAY_PID" 2>/dev/null
  [ -n "$BROADWAY_PID" ] && wait "$BROADWAY_PID" 2>/dev/null
  exit "$rc"
}
trap cleanup EXIT

gtk4-broadwayd :6 >"$EVIDENCE/daemon.log" 2>&1 &
BROADWAY_PID=$!
READY=0
for _ in $(seq 1 40); do                        # readiness: wait for the socket
  ls "$XDG_RUNTIME_DIR"/broadway*.socket >/dev/null 2>&1 && { READY=1; break; }
  kill -0 "$BROADWAY_PID" 2>/dev/null || exit 1 # daemon died: fail
  sleep 0.25
done
[ "$READY" -eq 1 ] || exit 1                    # readiness timeout: fail, never proceed

DISPLAY=:6 GDK_BACKEND=broadway BROADWAY_DISPLAY=:6 \
  cargo test --all-targets -- --nocapture >"$EVIDENCE/suite.log" 2>&1
SUITE=$?                        # cargo's own status: no pipe, nothing masks it

# Harness-side fail-closed: green-but-unexercised is a failure even at exit 0.
grep -q "no display session" "$EVIDENCE/suite.log" && SUITE=1
grep -q "GTK unavailable"     "$EVIDENCE/suite.log" && SUITE=1
grep -Fq "test ui::browser::tests::gtk_widget_contracts_hold_on_one_session ... ok" \
  "$EVIDENCE/suite.log" || SUITE=1              # the contract must have executed
printf 'suite status: %s (log: %s/suite.log)\n' "$SUITE" "$EVIDENCE"
exit "$SUITE"
```

The acquisition helper prints two and only two skip diagnostics
(`src/ui/mod.rs`); a positive run counts as evidence only when cargo exits 0,
`TRIBUTARY_WIDGET_TESTS_FAIL_CLOSED` was set, neither diagnostic appears
anywhere in the log, and the log contains
`test ui::browser::tests::gtk_widget_contracts_hold_on_one_session ... ok`.
The published harness enforces all of that mechanically: cargo's exit status
is captured directly (nothing sits between cargo and `SUITE=$?`), either skip
diagnostic forces `SUITE=1`, the exact contract ok-line is required, a
readiness timeout or early daemon death exits nonzero, and the EXIT trap
terminates and waits for the daemon this invocation started on every path.

Results at this head (source tree identical to `730fc36e`; only this document
changed afterwards):

| Run | Result |
|-----|--------|
| Headless negative control, `TRIBUTARY_WIDGET_TESTS_FAIL_CLOSED` unset | Prints `gtk widget contracts test: no display session ($WAYLAND_DISPLAY/$DISPLAY unset); skipping.`, reports `ok`, exit 0 — the deliberate skip-gate proof and the preserved headless-CI behavior. This is the control, not acceptance evidence. |
| Headless negative control, `TRIBUTARY_WIDGET_TESTS_FAIL_CLOSED=1` | Prints `FAIL-CLOSED positive run: no display session ...; refusing to skip.` and **exits 101** — the code-level gate refuses to let a positive run pass unexercised. |
| Display-backed full suite (`gtk4-broadwayd :6`, shared runtime dir, fail-closed set) | exit 0; **0** skip diagnostics; contract ok-line present. Top-level libtest totals: `src/lib.rs` unittests 20 + `src/main.rs` unittests 1871 (incl. the consolidated widget contract) + `tests/packaging_metadata.rs` 30 = **1921 passed / 0 failed / 0 ignored**. The additional `test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1870 filtered out` line is an internal subprocess re-run of the test binary spawned by `daap::client::tests::protected_daap_and_subsonic_streams_play_to_eos`; it is not a top-level suite and is not counted again. |
| Display-backed, `GTK_THEME=Adwaita-dark` | Contract filter run: exit 0, no skip diagnostics, contract executed. |
| Display-backed, `GTK_THEME=Raleigh` (non-Adwaita legacy theme) | Contract filter run: exit 0, no skip diagnostics, contract executed — the a11y contracts are property-level, not theme-dependent. |
| Display-backed, `GDK_SCALE=2` (200% scaling) | Contract filter run: exit 0, no skip diagnostics, contract executed. |

Raw logs retained off-repo under
`${TMPDIR:-/var/tmp}/tr3asjp-evidence.fXit0F/`, sha256-fingerprinted for
verification (`suite.log` is the retained worked example of the trap: green
suite, exit 0, one `GTK unavailable` skip — caught by the fail-closed greps):

```
b96d924e0899c4e9e94ab7a9e769c855235eb899e0c184e849013d9b28147513  daemon.log
21b5c1a73cc0d4666e57b1b5bd69ecf7a3d56d85aa00e1c6c1e9235c584ace54  suite.log
3d251cf2aa0302450be6ecaff44658ae00d8993eaa34a4458c79935ed0fdccf6  negative-control-r2.log
e9938c42520993ffb3a3b3d2fed8fe2c1f44b8b96daaed2c550464fa0607fb09  negative-control-failclosed.log
c31eba1f27a3619f15db773eb7348ae13574af21e8dd9442f55feeac225bc1ce  suite-positive-r2.log
52f6fcb8301b406e80b130da3c922f56fb9153e7f24a52140f5d383bffeb5c77  variant-r2-GDK_SCALE-2.log
8db76c53d0d0442797b788178cd1d7491d23d93a6c9ac8ab2614223a66a6d0ff  variant-r2-GTK_THEME-Adwaita-dark.log
3b395db9057cae180120827db29be24747a138ade84c853012dbb4a2f399fd8c  variant-r2-GTK_THEME-Raleigh.log
```

## Harness validation (revision 3)

The refinery repeat review rejected the revision-2 harness block itself
(R1): the reproduction extracted the exact published text, injected a
`cargo` function returning 101, and the block still exited 0 — `SUITE=$?`
after `cargo | tee` reports tee's status, and nothing required the
contract ok-line. The block above was rewritten (direct log redirect so
nothing sits between cargo and the status, required contract ok-line,
explicit readiness-timeout failure, EXIT-trap daemon cleanup) and then
validated against the published text exactly as the reviewer probed it:
extracted verbatim from this document, with only a prepended shell
function or environment for each injection. Run on this host 2026-09-17,
`gtk4-broadwayd :6`, shared isolated runtime dir, fail-closed set.

| Run | Outcome |
|-----|---------|
| Success: published block as written, full suite | **exit 0** in 164 s; contract ok-line present; **0** skip diagnostics; top-level totals 20 + 1871 + 30 = 1921 passed / 0 failed — same profile as the r2 display-backed row above. |
| Injection: `cargo` returns 101 with a compilation-error line (the R1 repro) | **exit 1** (the r2 block returned 0 here); suite log contains only the injected failure, no contract, no suite. |
| Injection: `cargo` exits 0 with a green-suite log missing the contract ok-line | **exit 1** — the required-contract check fails a green-but-unexercised run even with cargo at 0 and no skip diagnostics. |
| Injection: `cargo` exits 0 with the `GTK unavailable` skip diagnostic (plus contract ok-line, to isolate this layer) | **exit 1** — the zero-skip-diagnostic greps still catch a green suite that never exercised the contract. |
| Injection: `gtk4-broadwayd` replaced by a process that never creates the socket | **exit 1 after ~10 s** (readiness loop exhausts instead of falling through); the injected daemon process is reaped by the EXIT trap — no stray daemon or socket remains. |

Raw harness-validation logs retained off-repo under
`${TMPDIR:-/var/tmp}/tr3asjp-harness-r3/`, sha256-fingerprinted
(`published-block.sh` is the verbatim extraction of the fenced block
above at the time of the runs):

```
82a0bb9b4a220cc9345787287e64e62241a3b0e0422df7ee290f0cf586b1d69b  published-block.sh
24f74795b160f186dbbde2c5180ba87d6bda2032c1fb88aa80e0ef76c70849ea  v1-suite.log
b553eb1cedf8be53aab7f0ef85915d13c17f5c5412b7637c4c7cfe84a92297fa  v1-daemon.log
7488fc28f4f208e6e3256d6135f12798896fb92f4ade51f19bb40d8e5a1996b9  v2-suite.log
4346ae6acd7100b4f077871d9ad0e878d50b715a3bb7b94e1306aaf2989f472b  v3-suite.log
0439df4823e159c3c27fc50de5f1420a5e316a40880d2b7e033f65ea40706d72  v4-suite.log
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  v5-runner.out
```

The V5 daemon-timeout run leaves no suite log by design (it exits before
cargo starts); its runner transcript (`v5-runner.out`) and the generated
injection scripts are retained alongside the logs. Physical/Orca/
high-contrast validation remains operator-owned and is not claimed by any
of these runs.

What the consolidated contract (`ui::browser::tests::
gtk_widget_contracts_hold_on_one_session`) actually asserts while a GTK session
is up: combined accessible label "Label, (Count)" published on the
`GtkListItem` row boundary; child labels presentational so the row is one
utterance; zero-count rows announce the bare label and render no count; unbind
resets accessible label and visible texts on recycled rows; the context-menu
popover attaches a visible scrolling child with one button per enabled action;
browser gutter separators join visible panes around hidden ones; tracklist
drags start only from the data row area.

## Operator manual matrix (physical validation)

Automated runs cannot reach these; they remain operator-owned and are not
claimed here. Suggested checks on a desktop session:

- **Screen reader**: Orca announces a focused browser row as a single
  utterance ("Artist Name, (12)") and a zero-count row without "(0)".
- **Keyboard-only operation**: browse panes, focus tracklist, open the context
  menu (Menu key), activate/leave the popover without a pointer.
- **High contrast**: GNOME HighContrast / large-text — count caption stays
  legible at `opacity: 0.75`; gutter remains visible but not distracting.
- **Fractional scaling** (125%/150%): rows, gutter (1px), and status bar stay
  crisp; no clipped counts at long labels.
- **Long translations**: switch to a long-string locale and verify browser-row
  ellipsization and count non-overlap.
- **Screenshot refresh**: `data/screenshot.png` (referenced by the AppStream
  screenshot URL) predates #179 and still shows the old presentation; capture a
  replacement with the new look at release time.
