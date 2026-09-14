# A refused client admission leaked its id, the refusal never reached the client, and one process-wide doorbell flag lost rings between daemons

Date: 2026-09-14
Area: `crates/server/src/daemon.rs` (`control_loop`, `Admission`, `release_client_id`, the doorbell
flag table), `crates/server/src/verbs.rs` (`reap_client`, `shard_report`), `crates/ipc/src/rendezvous.rs`
(both platforms' `accept_one` and `connect`; `IpcError::HandoffLost`), `crates/ipc/src/protocol.rs`
(`ShardReport::tasks_refused`), the CLI and MCP status printers
Severity: banned item 9 in the daemon's admission — three ways a refusal was lost — plus a lost wake
between daemons sharing a process. These are the parts of the KIND lane's fifth defect
(`docs/bugs/2026-09-14-fleet-tasks-outside-the-task-budget-poison-client-admission.md`) that the
fleet's task share (`5de244d`) did not cover: with the share the arena no longer fills, but the paths a
full arena took stayed as they were.

## Symptoms

1. **The refusal never reached the client.** A connect past the daemon's client bound was refused
   inside `make_region` (`TooManyClients`), but `accept_pending` returned the error to a loop that
   dropped it: on Linux the peer socket closed without a handoff and the client read "region layout:
   short handoff message"; on macOS and Windows the claim slot stayed `CLAIMED` and the client waited
   out its whole claim wait to report the daemon unavailable. Failing first
   (`a_connect_past_the_client_bound_is_refused_typed_at_the_rendezvous`, a one-shard daemon with a
   bound of one): the second connect answered
   `DaemonUnavailable { why: "the daemon did not answer the claim within the claim wait" }` after
   **1.002 s**.
2. **A refused seat leaked the id.** The client's id was recorded in the control shard's live set and
   the seat task moved to its shard; a seat task the arena refused was dropped unrun (`shard.rs`
   `handle_control`: counted, no return path), or the shard's client table refused the slot — either
   way the id stayed live, the client reconnected under it, was assigned a fresh one and refused
   `SessionTaken`, and the bound was consumed two ids at a time until the node refused every client.
3. **A burst admitted past the bound.** An accept round serves every pending connection before it
   returns, and the id entered the live set only after the round — so a bound of one admitted a second
   client whose connect landed inside the first's round (the typed-refusal test admitted two, 275 µs).
4. **One doorbell flag for every daemon in the process.** `DOORBELL_RANG` was one static `AtomicBool`:
   with several daemons in one process (the daemon test suite runs seven), one daemon's control poller
   swapped off the ring meant for another, whose accept round then never ran — the typed-refusal test
   failed 3 of 4 parallel runs with the claim unanswered for the full wait, and passed alone every time.

## Fixes

- **Typed refusal on the wire.** Linux: `accept_one` answers a `TooManyClients` refusal with a handoff
  naming client `REFUSED_CLIENT` (0, never assigned), the bound in the length word and no descriptors;
  `connect` decodes it to `IpcError::TooManyClients { limit }`. macOS/Windows: the daemon writes the
  bound into the slot's length word and marks it `REFUSED`; the client reads the bound, marks the slot
  `DONE` and returns the same error. `accept_pending` counts a capacity refusal (`capacity_refused`)
  and serves the next pending connection — a full daemon keeps answering.
- **The id is reserved at admission** (inside `make_region`, after the bound check), so a burst inside
  one round is counted; a region that cannot be created gives the reservation back at once. A handoff
  that fails after admission is reported with its id (`IpcError::HandoffLost { client_id, cause }`) so
  the daemon releases it, counted `HANDOFF_LOST` and logged once; any other accept-round failure is
  counted `ACCEPTS_FAILED` and logged once, never dropped.
- **An `Admission` guard** moves into the seat task and, dropped unseated (the task refused and dropped
  unrun, or the table full), gives the id back through `release_client_id` — directly on the control
  shard, else as a forget task on it, with a refused forget counted `RELEASE_LOST` and logged once —
  and counts `HANDOFF_LOST`. The reaper's release goes through the same helper. Gotcha recorded in the
  code: the terminal step is a method (`seat`), because an `async move` block captures a `Copy` field it
  assigns by copy and would leave the guard behind in the accept loop (the 2021 disjoint-capture rule) —
  the guard then dropped unseated at once and gave back a seated client's id; the test caught it.
- **One doorbell flag per daemon**: a cache-line-aligned table indexed by the daemon's control shard
  (its registry id, unique per live daemon), so daemons in one process never consume each other's rings.
- **`tasks_refused` in the shard report** (from the runtime's `admission_refused`), printed by
  `slates status` and the MCP shard block: a refused admission is a visible bug signal.

## Tests

- `a_connect_past_the_client_bound_is_refused_typed_at_the_rendezvous` (daemon suite): the second
  connect at a bound of one answers `TooManyClients { limit: 1 }` in under the refusal-answer bound, the
  status counts one refused client, the first client still runs verbs. Failing first as above.
- `crates/ipc/tests/rendezvous.rs::a_client_process_refused_at_the_bound_is_told_so_typed`: across real
  processes, the listener refuses every region and the child sees the typed refusal with the bound;
  green on macOS (5/5) and on Linux in the `rust:1.98` container (5/5).
- `daemon::tests::an_admission_dropped_unseated_gives_its_id_back`: on a runtime shard, a guard dropped
  unseated removes its id from the live set and counts one lost handoff; a seated one keeps its id.

## Verification

`cargo test -p slates-server --test daemon` 7/7, six runs in a row under the parallel suite (the doorbell
table); `cargo test -p slates-ipc` all green here and the rendezvous suite 5/5 in the `rust:1.98`
container; `--lib` 64/64; the CLI (27) and MCP suites; `cargo clippy --workspace --all-targets -D
warnings`, fmt and `cargo xtask check` clean. Validated on a wiped `target/` (2026-09-14 17:25–17:35):
20/20 fast suites, the gates, and the fleet suite **38/38 in 203.09 s** under the 300 s stall detector.

## Sibling sweep

- A `make_region` failure other than the bound (a region that cannot be created) still leaves the
  macOS/Windows claim slot to the client's claim wait; it is counted and logged once on the daemon
  (`ACCEPTS_FAILED`). A general refusal code in the slot is the next step if it is ever seen.
- The runtime's general form — a `Control::Spawn` on a full arena drops the moved future without telling
  the sender — stands; the guard is the pattern for any future that owns a resource the sender must know
  about, and the fleet's task share keeps the arena from filling in the first place.
