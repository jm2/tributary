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
multi-selection drag onto local regular playlists — has landed on `main` (PR #182) and introduced
the single tracklist `DragSource` and the typed in-app drag payload. That record is the foundation
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
available*. The availability analysis at the end is therefore normative, not commentary: Policy
E's prerequisites are now all on `main` and its thin slice is implementable there as its own
record; Policy D's transfer machinery exists only on the corrected PR #175 branch; and Policy R's
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
5. **`GDK_ACTION_COPY` only.** The export drag offers exactly one action: copy. `GDK_ACTION_MOVE`
   and `GDK_ACTION_LINK` are prohibited. MOVE would promise that Tributary deletes each source
   file after the receiving file manager copies it — but Tributary observes no completion event
   once the URI list leaves the process, so it could never know when (or whether) it was allowed
   to discharge that promise, and a drag gesture carries no delete authority over library files
   in any case. LINK would ask the receiver to fabricate a symlink or alias pointing into the
   library — a live pointer outside the retained root authority that outlives the validation this
   policy performs at drag-prepare time, and a disclosure the boundary above does not cover.
   The `DragSource` action set must therefore be `GDK_ACTION_COPY` alone; a receiver that cannot
   copy is not an export target.

### Transfer

None performed by Tributary. No write authority, no lease, no planner, no progress, no rollback.
The app's obligation ends at the drag-prepare boundary.

### Mechanics and one explicit hazard

The single PR #182 `DragSource` publishes a union content provider: the in-app payload first, and
the export format (`GdkFileList`, which GTK serializes to `text/uri-list`) only when the
all-or-nothing gate passes. The union is built at drag-prepare time, so the gate is evaluated per
drag, and the source's action set is `GDK_ACTION_COPY` alone per the authority constraint above.
In-app drop targets match the payload type; file managers match `text/uri-list`; neither sees
formats the other would act on.

**Hazard (binding):** the tracklist has exactly **one gesture-owning `DragSource`** — ever, on
`main`, alongside any follow-up record. Export rides that source (via the union provider) or it
does not ship; a second, export-only source on the same widget is forbidden. The reason is
structural, not incidental: GTK does not promise a stable, documented arbitration order between
multiple drag sources competing for one widget's gesture, so no wiring order — addition order
included — may be relied on to keep a parallel source harmless. Correctness must come from the
single-owner invariant itself, not from assumptions about event-controller sequencing; any
feature that appears to need a second source is wired as another content provider or drop target
on the existing surface instead.

### Availability and implementation gate

The export target semantics — local files with live-resolvable identities — exist on `main`, and
the drag surface they ride has landed with them (PR #182). No in-flight prerequisite remains: a
`main`-only implementation is now possible without fabricating a parallel drag source, which the
hazard above forbids. The slice remains **deferred as its own small record** rather than part of
this document, and is implementable directly against `main`: one union provider with a
`GDK_ACTION_COPY`-only action set, the all-or-nothing gate, and focused tests (all-local
selection offers the full URI list; any remote or unresolvable candidate suppresses the export
format while the in-app payload keeps working; paths are revalidated at prepare time; the
offered drag action is COPY and nothing else).

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
  reparse traversal; no mount crossing. Each write is staged as a sibling temporary file created
  beneath the retained root, and the final rename to the destination name is performed **through
  the retained authority, not by the slice**: preparing the write binds the destination parent
  directory through the retained root (root-level writes bind the root itself, nested writes bind
  the parent via the same relative-directory open the read path uses), so the boundary check that
  authorises the rename is the read path's own check. The mount boundary is revalidated
  immediately before and immediately after the rename; a binder swap, unmount, or remount between
  staging and commit fails closed and cannot authorise a partial publish. Rejection rolls back
  with no success event. The staged handle is flushed and closed before the rename or removal —
  on Windows a rename of a file with an open handle fails with a sharing violation, so the handle
  must never outlive the write phase.
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
  drop; the new copy takes a disambiguated sibling name chosen at planning time. `Skip`,
  `Overwrite`, and `Fail` remain typed planner policies; a per-drop override UI is future work
  and must not default to `Overwrite`. The planner resolves each conflict against the live
  destination and records the resolution on the copy stage, so the executor never re-decides a
  conflict mid-transfer.
- **Capacity — accounted once across all source-root requests.** A selection may span several
  library roots, and grouping gives each root its own transfer request against the same
  destination volume. The planner enforces a capacity budget per request against that request's
  own total, so per-request checks against the full free space would each pass while the groups
  collectively overcommit the volume. The slice therefore accounts capacity **once**: it plans
  every source-root group first, sums the planned totals, and rejects the whole drop against a
  single budget equal to the destination volume's free space before any group executes. No byte
  is written by any group unless the aggregate fits.
- **Progress and cancellation:** the executor's stage/byte progress feeds a visible transfer
  surface (toast-level at minimum), and cancellation is cooperative via a `CancellationObserver`
  — checked between stages and per buffered chunk, with uncommitted staged files rolled back
  before the executor returns.
- **Candidate gate:** same all-or-nothing rule as Policy E — any remote candidate makes the
  selection non-device-copyable (a remote track has no file to copy; upload pipelines are a
  different record).

### Availability and implementation gate

The target UI seam exists on `main` (device rows already carry mount points, and the removable
controller owns their lifecycle events), but the transfer machinery still lives in the open
PR #175, corrected on its branch (planner/executor module split, parent-bound commits,
staged-handle close-before-rename). A `main`-only implementation would have to grow a private
copy path outside the retained lease model — exactly the duplication the backlog forbids.
**Implementation is gated on #175 landing**: a per-row drop target on device rows, candidate
grouping per source root, planning with the layout/capacity/conflict policy above, once-per-drop
aggregate capacity accounting across the groups, execution with progress and the lifecycle-wired
cancellation, plus focused tests (Preserve conflicts never overwrite; aggregate capacity over the
free-space budget is rejected with no group executed; a mount swap between staging and commit
fails closed instead of publishing; unmount mid-transfer rolls back committed stages in reverse
order; and a rejected transfer emits no success event).

## Availability matrix (normative)

- **E — file-manager export.** Target semantics today: available for local candidates (files
  exist; resolvable through retained authority). Prerequisite: PR #182, the drag surface it must
  extend — **landed on `main`**, so this prerequisite is satisfied. Disposition: deferred — the
  thin union-provider slice (COPY-only action set included) is implementable now as its own
  record.
- **R — remote-row.** Target semantics today: not available on any backend (no server write
  authority; read-only playlist lane; MPD is output-only). Prerequisite: a refined server
  playlist-write authority record. Disposition: out of scope — policy recorded; revisit via a new
  refined issue.
- **D — device-copy.** Target semantics today: not on `main` (the planner/executor exists only on
  the corrected PR #175 branch, which this document's normative claims track). Prerequisite:
  PR #175 (planner + write authority). Disposition: deferred — implement the wiring slice, with
  once-per-drop aggregate capacity accounting, when #175 lands.

No implementation ships with this document. Shipping a parallel drag source or a private copy
loop on `main` would create guaranteed conflicts with the landed drag surface (E) or the
in-flight transfer planner (D), and would duplicate authority models this project has already
paid to get right. Each deferred slice above is small by construction precisely because this
document fixes its policy decisions now.

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

- **Policy E slice:** drag-prepare union provider + gate (UI layer, on the landed #182 surface).
  No new authority types; the gate reuses the resolver's existing exact-file validation; the
  source's action set narrows to `GDK_ACTION_COPY`.
- **Policy D slice:** device-row drop target + grouping + planner/executor invocation + aggregate
  capacity accounting + progress surface + lifecycle cancellation wiring (UI + a thin coordination
  module, on #175). No new authority types; `MountedWriteAuthority` and the planner are consumed
  as merged from #175, whose commits bind every rename through the retained parent/root authority.
- **Policy R slice:** exists only after a refined server write-authority issue lands its lane;
  the drop wiring then composes that lane. This document grants it nothing today.
