# Drag-and-drop export, remote-row, and device-copy policies

- Status: design only — P2.2 drop-policy design record; no implementation ships with this
  document. Each policy names the exact prerequisite that gates its implementation slice.
- Decision date: 2026-09-03
- Tracking issue: [#46](https://github.com/jm2/tributary/issues/46)
- Backlog entry: [`task.md`](task.md) (P2.2, design-and-availability record)
- Roadmap rationale: [`roadmap.md`](roadmap.md) (Audio-experience plan item 1, and the issue
  table: "file export, remote rows, and device copies as distinct policies")
- Related records: [`source-lifecycle.md`](architecture/source-lifecycle.md) (source identity,
  exact-session authority lanes), [`subsonic-playlist-sync.md`](subsonic-playlist-sync.md)
  (server-native playlist reads are the only server playlist authority accepted today)

## Context

Issue #46 asks for Rhythmbox-style drag and drop: select tracks in Tributary and drop them on a
file browser, on Tributary's own playlist rows, or elsewhere. The basic record — accessible
multi-selection drag onto local regular playlists — is implemented in PR #182 and introduces the
single tracklist `DragSource` and the typed in-app drag payload. That record is the foundation
this document builds on.

Three further drop destinations exist in the product space, and they are **not** variations of
one behavior. Each has a different authority question (what is the app allowed to disclose or
mutate, and through which retained boundary) and a different transfer question (who moves the
bytes, through which planner, with which conflict, capacity, progress, cancellation, and rollback
contract). This document therefore records them as three separate policies:

- **Policy E — file-manager export drops**: drag out of the application into an operating-system
  file manager.
- **Policy R — remote-row drops**: drop onto a remote source row in the sidebar.
- **Policy D — device-copy drops**: drop onto a mounted removable-device row in the sidebar.

The backlog record requires implementing *only the variants whose target semantics are actually
available*. The availability analysis at the end is therefore normative, not commentary: today,
none of the three can land a code slice on `main` without its named prerequisite, and Policy R's
target semantics do not exist in any backend.

## Shared drag surface (from the basic record)

One `DragSource` lives on the tracklist column view (PR #182). On drag prepare it snapshots the
selection into a typed GTK payload of `PlaylistAddCandidate` values:

- `Local(MediaKey)` — a catalogue identity resolvable to a concrete file through the local
  library's retained root and exact-file authority; and
- `Remote { media_key, session_epoch, catalogue_generation }` — a pathless registry identity
  bounded to one source session epoch and catalogue generation. A remote candidate has **no**
  local file, ever: remote sources publish pathless catalogues by the source-lifecycle decision.

Sidebar rows that accept drops (editable regular playlist rows in PR #182) declare the payload
type they match and resolve the destination from drop coordinates. All policies below extend this
one drag surface; none of them may install a second, parallel `DragSource` on the tracklist
(recorded as an explicit hazard under Policy E).

## Policy E — file-manager export drops

### Semantics

The user drags tracks from the tracklist into an external file-manager window. The **receiving
application performs the copy**; Tributary only discloses file identities. Tributary writes
nothing, holds no destination authority, and observes no completion event — once the URI list
leaves the process, the transfer is the operating system's.

### Authority

Read-only disclosure of existing local file identity. The constraints:

1. **Local candidates only.** Remote candidates are pathless by the source-lifecycle decision and
   are never exportable. There is no URL-export fallback: handing a remote stream locator to a
   file manager would leak session-bound, possibly credentialed URLs into a foreign application.
2. **Resolvable now.** A candidate is exportable only if its file resolves through the same
   retained local root and exact-file authority that playback consumes — the path is validated
   against the live mount at drag-prepare time, so renamed, moved, or disappeared files are never
   offered.
3. **All-or-nothing per drag.** If any selected candidate is remote or unresolvable, the export
   format is omitted from the drag entirely. File managers copy everything a drag offers; a
   silent partial export would mislead. The selection remains draggable for in-app targets, so
   the user loses nothing and gets no false signal. (A future per-candidate picker is out of
   scope until requested.)
4. **Disclosure boundary.** A `file://` URI handed to the OS discloses the local library's
   absolute paths. This is accepted deliberately: the library roots are user-configured, the
   gesture is explicit, and the payload contains paths only — no credentials, no session epochs,
   no remote locators, no catalogue metadata beyond the filename.

### Transfer

None performed by Tributary. No write authority, no lease, no planner, no progress, no rollback.
The app's obligation ends at the drag-prepare boundary.

### Mechanics and one explicit hazard

The single PR #182 `DragSource` publishes a union content provider: the in-app payload first, and
the export format (`GdkFileList`, which GTK serializes to `text/uri-list`) only when the
all-or-nothing gate passes. The union is built at drag-prepare time, so the gate is evaluated per
drag. In-app drop targets match the payload type; file managers match `text/uri-list`; neither
sees formats the other would act on.

**Hazard (binding):** a second, export-only `DragSource` on the tracklist must never be added —
not on `main`, not alongside #182. GTK consults event controllers on a widget in reverse
addition order, so a parallel source would shadow the playlist drag (or be shadowed by it)
depending on wiring order. Export rides the existing source or it does not ship.

### Availability and implementation gate

The export target semantics — local files with live-resolvable identities — exist on `main`.
The drag surface they must ride, however, is introduced by the still-open PR #182. A `main`-only
implementation would have to fabricate a parallel drag source, which the hazard above forbids, or
reimplement #182. **Implementation is therefore gated on #182 landing**: one union provider, the
gate, and focused tests (all-local selection offers the full URI list; any remote or unresolvable
candidate suppresses the export format while the in-app payload keeps working; paths are
revalidated at prepare time).

## Policy R — remote-row drops

### Semantics considered

Dropping tracks on a remote source row could mean two things: enqueue on that server, or write
the tracks into a server-native playlist. Neither target semantic exists in any backend today:

- No adapter exposes a queue-submission authority reachable from a sidebar row. Remote rows
  navigate to their catalogue; playback is a pull model fed by resolved catalogue identities, not
  a push target.
- No adapter implements a server playlist write. The accepted Subsonic lane
  ([`subsonic-playlist-sync.md`](subsonic-playlist-sync.md)) is read-only by contract: one-time
  Import Copy and read-only Pull Mirror, explicitly scoped so it "does not consult the accepted
  music catalogue and cannot turn a returned track ID into display, stream, artwork, rating, or
  history authority." A drop is the opposite direction — local catalogue identities to the
  server — and would be a new authority, not an extension of the read lane.
- MPD in Tributary is an audio output backend, not a music source; there is no MPD library row to
  drop on.

### Authority required before this can exist

A future server-side playlist write must flow through the same shape as the read lane: an
exact-session authority lane (adapter + session epoch + session lease, pre/post network
revalidation), capability detection per backend, typed unavailability elsewhere, and admission
through a coordinator rather than a direct adapter call from the drop handler. A drop handler
holding a raw adapter reference would bypass every ordering guarantee the source-lifecycle and
Record E decisions established; that is rejected up front.

### Availability

**Not available on any backend. Implementation is out of scope for this record.** The policy is
recorded so that when a server playlist-write authority is proposed (a new refined issue per the
scope protocol), the drop wiring, the candidate conversion question (remote candidates may map to
server identities; local candidates have no server identity and would require upload semantics —
a much larger authority), and the unavailability UX are designed in that context, not improvised
in the UI layer.

## Policy D — device-copy drops

### Semantics

The user drops tracks on a mounted removable-device row in the sidebar; Tributary copies the
files onto that volume. Unlike Policy E, **Tributary performs the transfer** and therefore owns
the full authority, conflict, capacity, progress, cancellation, and rollback contract.

### Authority

The 2026-07-14 root trust decision and the transfer-planner record (PR #175) define the model;
this policy reuses it verbatim and must not rebuild it:

- **Destination:** one `MountedWriteAuthority` acquired on the device mount — a retained
  root-and-marker lease. Every mutation resolves beneath that single retained root; no symlink or
  reparse traversal; no mount crossing. Each write is staged as a sibling temporary file and
  committed by atomic rename. Authorities are revalidated after catalogue resolution and again
  immediately before commit; a binder swap, unmount, or remount between staging and commit fails
  closed. Rejection rolls back with no success event. Windows staging handles omit delete-sharing.
- **Source:** each candidate's library root read authority (`MountedRootAuthority`), opened
  through the same retained-exact-file boundary playback uses. A selection may span multiple
  library roots; transfers are grouped per source root so each group holds exactly one retained
  read authority.
- **Lifecycle:** the write authority is retained for the duration of one transfer and discarded
  at completion or failure. The device lifecycle hooks the removable-media controller already
  observes (pre-unmount, mount removed, retirement) must cancel the transfer's cooperative
  cancellation token; the executor's reverse-order rollback then removes only files this transfer
  committed, revalidating before each removal.

### Transfer

Through the PR #175 planner and executor, not a bespoke copy loop:

- **Destination layout:** everything lands under a mount-relative `Tributary/` transfer root,
  preserving each file's library-root-relative path. This keeps the device self-describing,
  collision-poor across repeated drops, and trivially reviewable. Rejected alternatives: copying
  into the mount root (sprawl, collisions on repeated drops) and renaming from parsed metadata
  (adds the tag parser as a transfer dependency and fabricates names the library does not
  guarantee).
- **Conflict policy:** default `Preserve` — an existing device file is never overwritten by a
  drop; the new copy takes a disambiguated name. `Skip`, `Overwrite`, and `Fail` remain typed
  planner policies; a per-drop override UI is future work and must not default to `Overwrite`.
- **Capacity:** the plan carries a capacity budget equal to the destination volume's free space;
  a plan whose total bytes exceed it is rejected before any byte is written.
- **Progress and cancellation:** the executor's stage/byte progress feeds a visible transfer
  surface (toast-level at minimum), and cancellation is cooperative via a `CancellationObserver`
  — checked between stages and per buffered chunk, with uncommitted staged files rolled back
  before the executor returns.
- **Candidate gate:** same all-or-nothing rule as Policy E — any remote candidate makes the
  selection non-device-copyable (a remote track has no file to copy; upload pipelines are a
  different record).

### Availability and implementation gate

The target UI seam exists on `main` (device rows already carry mount points, and the removable
controller owns their lifecycle events), but the transfer machinery lives in the still-open PR
#175. A `main`-only implementation would have to grow a private copy path outside the retained
lease model — exactly the duplication the backlog forbids. **Implementation is gated on #175
landing**: a per-row drop target on device rows, candidate grouping per source root, planning
with the layout/capacity/conflict policy above, execution with progress and the lifecycle-wired
cancellation, plus focused tests (Preserve conflicts never overwrite, capacity rejection is
pre-write, unmount mid-transfer rolls back committed stages in reverse order, and a rejected
transfer emits no success event).

## Availability matrix (normative)

| Policy | Target semantics today | Implementation prerequisite | Disposition |
| --- | --- | --- | --- |
| E — file-manager export | Available for local candidates (files exist; resolvable through retained authority) | PR #182 (the drag surface it must extend) | Deferred — implement the thin union-provider slice when #182 lands |
| R — remote-row | Not available on any backend (no server write authority; read-only playlist lane; MPD is output-only) | A refined server playlist-write authority record | Out of scope — policy recorded; revisit via a new refined issue |
| D — device-copy | Not on `main` (planner exists only in PR #175) | PR #175 (planner + write authority) | Deferred — implement the wiring slice when #175 lands |

No implementation ships with this document. Shipping a parallel drag source or a private copy
loop on `main` would create guaranteed conflicts with the in-flight prerequisite PRs and would
duplicate authority models this city has already paid to get right. Each deferred slice above is
small by construction precisely because this document fixes its policy decisions now.

## Non-goals

- Cut/paste (clipboard) export and the `x-special/gnome-copied-files` target — a separate record
  if requested.
- Import drops (dragging files *into* Tributary from a file manager) — served by the existing
  open-files path; drag wiring for it is a separate record.
- Playlist drops — covered by the basic record, PR #182.
- MTP/Android devices — separate discovery and transfer record (PR #178), which consumes the
  same planner under Policy D's authority shape.
- Cross-library-root moves or deduplication during export/copy.
- Per-candidate include/exclude pickers for mixed selections.

## Implementation boundaries (for the deferred slices)

Each slice changes only the layer named here and validates its own focused regressions:

- **Policy E slice:** drag-prepare union provider + gate (UI layer, on #182). No new authority
  types; the gate reuses the resolver's existing exact-file validation.
- **Policy D slice:** device-row drop target + grouping + planner/executor invocation + progress
  surface + lifecycle cancellation wiring (UI + a thin coordination module, on #175). No new
  authority types; `MountedWriteAuthority` and the planner are consumed as merged.
- **Policy R slice:** exists only after a refined server write-authority issue lands its lane;
  the drop wiring then composes that lane. This document grants it nothing today.
