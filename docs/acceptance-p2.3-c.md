# P2.3-C acceptance evidence — presentation refinements (#29, PR #179)

Recorded: 2026-09-17 (acceptance reconciliation leg `tr-3asjp`).

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
| Left align all columns (especially time) | Text columns (Title, Artist, Album, Genre, Composer, Date Modified) are left-aligned. Numeric columns (`#`, Time, Year, Bitrate, Sample Rate, Plays) remain right-aligned — the HIG re-evaluation deliberately kept numeric magnitude comparison readable, amending this ask (`src/ui/tracklist.rs` `add_sorted_column` call sites). |
| (not asked, part of the same merge) | Status bar aligns via `.statusbar-box` (4px/12px padding, centered content) instead of per-widget margins; screen readers announce each browser row as one utterance ("Artist Name, (12)") at the `GtkListItem` boundary. |

User documentation reconciliation: the `[Unreleased]` → *Changed* →
"Browser and tracklist presentation" CHANGELOG entry covers all of the above;
the AppStream description makes no presentation-specific claims, so nothing
there is stale.

## Automated evidence (display-backed)

Host: headless Linux (no X/Wayland session), GTK 4 broadway backend via
`broadwayd`, using the repository cargo guard wrapper. Exact reproduction:

```sh
broadwayd :5 &                                     # listens on $XDG_RUNTIME_DIR/broadway6.socket
DISPLAY=:5 GDK_BACKEND=broadway BROADWAY_DISPLAY=:5 \
  cargo test --all-targets                         # full suite, display-backed
# negative control — prove the gate skips without a display session:
env -u DISPLAY -u WAYLAND_DISPLAY \
  cargo test --all-targets gtk_widget_contracts -- --nocapture
```

| Run | Result |
|-----|--------|
| Headless negative control (`env -u DISPLAY -u WAYLAND_DISPLAY`) | Prints `gtk widget contracts test: no display session ($WAYLAND_DISPLAY/$DISPLAY unset); skipping.` — contract **not** exercised (test still reports `ok`; this is why the display-backed runs matter). |
| Display-backed, default theme, full suite | **1920 passed / 0 failed / 0 ignored** (lib 1869 + 20 unit; main binary incl. the consolidated widget contract: 1; packaging 30). Widget contract executed (not skipped). |
| Display-backed, `GTK_THEME=Adwaita-dark` | Widget contract passes. |
| Display-backed, `GTK_THEME=Raleigh` (non-Adwaita legacy theme) | Widget contract passes — the a11y contracts are property-level, not theme-dependent. |
| Display-backed, `GDK_SCALE=2` (200% scaling) | Widget contract passes. |

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
