# Initial scan, command admission, and shutdown latency

This note records the behavior contract introduced for the R9 issue
("Initial scan can indefinitely delay command draining and window close",
GitHub #256). It covers the library engine's startup scan, the UI command
admission boundary, and how window close still drains admitted work.

## Problem

The library engine finished the whole initial traversal and parse before it
serviced any UI command. A slow or stalled filesystem operation (a large or
removable/network root, or a kernel call that cannot be interrupted) therefore
delayed admitted rating and playback-history edits until the window closed, and
the command FIFO grew without bound while it waited. Window close enqueued a
`Flush` marker and waited for its acknowledgement while the window was disabled,
so the same stall delayed shutdown.

## Budgets and admission policy

- **Command FIFO capacity** — `src/ui/library_commands.rs::COMMAND_FIFO_CAPACITY`
  bounds the number of admitted-but-unserviced library commands. Producers get
  an explicit `CommandAdmissionOutcome` (`Accepted`, `Closed`, `Overloaded`)
  instead of silently accumulating a backlog; overload is surfaced by each
  caller (rating popover, root-trust prompt, Rhythmbox migration, history).
- **Reserved shutdown slot** — one slot in the bounded FIFO is permanently
  reserved for the terminal `Flush` marker. Ordinary admission may not consume
  it, so a saturated backlog can never make graceful close impossible.
- **Read-only settle budget** —
  `src/local/engine.rs::SCAN_READONLY_SETTLE_BUDGET` bounds how long a read-only
  scan job may hold shutdown once cancellation is observed. `spawn_blocking`
  cannot cancel a kernel call already inside `readdir`/`open`/`read`, so the
  isolation contract is: abandon the read-only handle after the budget, and
  treat the abandoned observation as incomplete.

## Command service during the initial scan

`LibraryEngine::run` drives the initial scan and command service from one task
with `service_commands_while_scanning`. While the scan future is pending on a
blocking discovery worker, the command branch is polled and admitted mutations
settle. Both branches share one task, so a scan mutation and a command mutation
are never in flight at the same time and the catalogue stays single-writer.

The `Flush` marker is the reserved drain: by the time the loop receives it,
every earlier admitted command has settled in FIFO order, so the loop waits for
the (cancelled) scan to reach settlement before acknowledging. The scanner's
already-admitted durable mutations complete; its abandoned read-only work does
not block the acknowledgement.

## Durable-mutation admission boundary

The read-only parser deliberately settles inside its grace after cancellation,
because its kernel call cannot be interrupted. `admit_scan_mutation` is the
explicit boundary that prevents a post-cancellation parser completion from
starting a new upsert and its authority probes:

- Pre-admission read-only work (pre-parse and post-parse root revalidation, and
  the destructive-phase authority preflight) is shutdown-aware. An abandoned
  probe refuses the mutation that depended on it.
- Once admitted, a durable mutation is awaited to settlement. It is never
  cancelled or dropped, so the reserved `Flush` drain cannot acknowledge work
  that did not commit. The commit guards therefore pass no cancellation.
- Cancellation is re-checked before the destructive phase and again after its
  preflight. A cancelled scan fails closed: every root is marked incomplete, so
  `is_complete`, reconciliation authority, and stale deletion all refuse, and
  no catalogue row is deleted.

## Restart and overflow

The existing filesystem-watcher overflow reconciliation and root-marker
reconciliation are unchanged. On restart, the scan re-derives root authority
from persisted state, and a cancelled or overloaded run leaves the persisted
catalogue exactly as the last committed transaction left it.

## Regressions

`src/local/engine.rs` tests cover: command service while read-only traversal is
held (`startup_services_an_admitted_rating_while_discovery_is_held`), the
reserved drain waiting for held discovery
(`reserved_flush_drain_waits_for_held_discovery_to_settle`), the post-parse
cancellation admission boundary
(`post_parse_cancellation_refuses_the_durable_upsert`,
`parser_settling_inside_the_grace_is_refused_at_the_mutation_boundary`),
restart reconciliation of the refused work
(`restart_after_post_parse_cancellation_reconciles_the_track`), cancelled
scans preserving stale rows
(`cancelled_initial_scan_admits_no_mutations_and_preserves_stale_tracks`), and
the bounded admission overload path
(`src/ui/library_commands.rs::admission_reports_overload_without_consuming_the_shutdown_slot`).
