# A fleet node's own tasks are outside its task budget, and a refused client admission poisons the node

**Date:** 2026-09-14. **Found by:** the KIND lane (`cargo xtask kind all --keep`, the five-replica scale
step, run started 15:55 CDT). **Status:** root cause confirmed by the runtime code path, the live pod's
descriptors and the refusal counters (below); the failing test is written
(`a_fleet_node_under_a_containers_memory_bound_still_admits_a_client`, `crates/server/tests/fleet.rs`,
committed `#[ignore]` with this record as its reason); the fix is specified below and **pending** — the box
was reserved for main's validation when this record was written, so no build or test could run.

## Description

`helm upgrade --install … --set replicas=5` (15:57:36 CDT): four pods Ready within seconds, `slates-4`
`0/1` for the whole 300 s bound with 0 restarts. Its readiness probe (`/slates status`) was refused five
times, 5 s apart, with

    slates: session held by a live client; assigned 2 / 4 / 6 / 8 / 10

and then, twenty times over the remaining 4m30s and on every `kubectl exec slates-4 -- /slates status`
afterwards (exit 4):

    slates: channel: region layout: short handoff message

The daemon in that pod was alive and fully meshed: `slates-3`'s `fleet_members` held slates-4's id
`14075004087422500318` with `fleet_peers_probed 4`; slates-4's own log had nothing past its boot lines. In
its process namespace (`kubectl debug --target=slates`, `ss -xap`): only the LISTEN socket
`@slates-rv-default/65532` — **no accepted client socket at all** — plus 12 UDP sockets (the fleet's
planes). Healthy pods reported `clients_refused 0`. The 3-replica run of 14:53 CDT that stalled 14 min on
`slates-2 0/1` (recorded in `docs/wip/kind-lane.md` as "cause unrecorded") has the same signature.

## Root cause

Two defects compose.

**1. The task budget omits the fleet.** `crates/server/src/config.rs` derives
`tasks_per_shard = clients_per_shard × TASKS_PER_CLIENT + LOOP_TASKS_PER_SHARD` = `10 × 2 + 5 = 25` in the
pod (its boot log). `LOOP_TASKS_PER_SHARD` is documented as "the server loop, the reap loop, the control
loop and the heartbeat, plus a spare". What the control shard actually spawns: those loops plus the NFS
serve loop and transient host tasks, and — in a fleet — `run_membership`'s tasks (`crates/server/src/
fleet.rs`): two demultiplexer receive loops, two accept loops, one record-plane coordinator, **two dial
tasks per peer** (`probe_peer`, `establish_record_link`) and, for every session a peer dials in, a serve
task per plane, bounded by the demultiplexer at `SESSIONS_PER_PEER = 2` per peer per plane — `5 + 6 ×
peers`, 29 at four peers. Demand at five replicas is above 34 for a budget of 25. At three replicas it is
about 22 plus transients: a re-dial's replacement serve task tips it now and then — the intermittent
14-minute stall. Every fleet spawn is `if let Ok(task) = futures::spawn(..)`, so the refused ones vanish
silently; the arena simply ends up full.

**2. A refused client admission poisons the node.** `control_loop` (`crates/server/src/daemon.rs`)
records the client's id in the control shard's `handed` set and then seats the client by a
`Control::Spawn` to its shard — a future that owns the `ClientSlot` parts (the region end and the
**control socket**). `Shard::handle_control` (`crates/rt/src/shard.rs`) on a full arena does
`counters.admission_refused += 1` and **drops the future** — no log, no return path. Dropping it closes
the control socket, so the client's liveness check reads the daemon as gone after its reply deadline and
reconnects **under its own id**; `handed` still holds that id (the reaper only walks slots that were
inserted), so `assign` hands out a fresh one and the client refuses `SessionTaken { assigned }`: ids 1 → 2,
3 → 4, … each probe burning two of `bound = clients_per_shard × shards = 10`. At ten, `TooManyClients` is
returned from `make_region`, `accept_pending` returns `Err`, the loop's `if let Ok(accepted)` **swallows
it**, and the peer socket is dropped without a handoff — the client reads 0 bytes: "region layout: short
handoff message", for the life of the daemon. The ids are never released: the node serves its fleet and
refuses every local client, permanently.

Why no earlier harness saw it: on a laptop `clients_per_shard` is in the hundreds (memory-derived), so the
client-only budget dwarfs the fleet's tasks; the pod's 1 GiB Guaranteed QoS bound makes it ten. The
in-process test reproduces the pod by setting `memory.limit` to 1 GiB and giving the node one peer that
dials in (its serve tasks take the slots the transient host tasks free) plus as many silent peers as the
client-only budget has slots.

## Impact

Any fleet node whose `5 + 6 × peers` plus its own loops exceeds `clients_per_shard × 2 + 5`: at the
lane's 1 GiB pod, four peers always, two peers intermittently. The node keeps the fleet's quorum but its
readiness never passes and no local verb reaches it — the operator sees a layout error for what is a
capacity refusal. From the failing test's run: with the arena full the daemons' shutdown did not complete
either (the test hung past 300 s after its reply deadline) — to be confirmed with the fix in place.

## Exact edits (the fix, pending)

1. `crates/server/src/fleet.rs`: name the fleet's task shape — `FLEET_LOOP_TASKS` (2 receive loops, 2
   accept loops, 1 coordinator), `DIAL_TASKS_PER_PEER` (2), `SERVE_PLANES` (2), `SESSIONS_PER_PEER` (2,
   existing) — and `pub(crate) fn tasks_per_shard(peers) = FLEET_LOOP_TASKS + peers × (DIAL_TASKS_PER_PEER
   + SERVE_PLANES × SESSIONS_PER_PEER)` (zero with no peers: `run_membership` returns before spawning).
2. `crates/server/src/config.rs`: recount `LOOP_TASKS_PER_SHARD` to the control shard's actual loops (the
   NFS serve loop and a transient host among them); `with_fleet` re-derives `tasks_per_shard` and
   `timers_per_shard` as `clients_per_shard × TASKS_PER_CLIENT + LOOP_TASKS_PER_SHARD +
   fleet::tasks_per_shard(peers)` and pushes the derived values to the boot log (`derivations`).
3. `crates/server/src/daemon.rs` `control_loop`: the admission future's moved state carries a drop guard
   that, unless the slot was inserted, releases the id from `handed` (directly on the control shard;
   through a forget task otherwise), counts `HANDOFF_LOST` and logs the first occurrence — so a task dropped
   unrun, or an insert the shard's table refused, never leaks the id; `accept_pending`'s errors are logged
   on first occurrence instead of swallowed; every `if let Ok(task) = futures::spawn(..)` in `daemon.rs`
   and `fleet.rs` logs its refusal once (the runtime already counts it).
4. `crates/ipc/src/rendezvous.rs` (Linux): a `TooManyClients` refusal is sent as a typed handoff — client
   id 0 (never assigned; a hello of 0 means "fresh"), the bound in the length word, no descriptors — and
   `connect` decodes it to `IpcError::TooManyClients { limit }`, so a probe reports the capacity refusal.
5. `ShardReport.tasks_refused` (`crates/ipc/src/protocol.rs`) filled from the runtime's
   `admission_refused` (`registry::with_current(|ctx| ctx.counters())`) in `verbs::shard_report`, printed
   by the CLI's shard line and the MCP shard block — the counted bug signal reaching `slates status`.

Then: the test above un-ignored and green; the fleet suite; `cargo test -p slates-ipc` on Linux (the typed
refusal) in the `rust:1.98` container; the image rebuilt and the lane re-run through the five-replica scale
and the netem profiles.

## Siblings (reported, not fixed here)

- macOS/Windows bootstrap path (`rendezvous.rs`, the claim slots): a `make_region` refusal returns before
  the slot's word is set, leaving it `CLAIMED`; the client waits to its own deadline — untyped there too.
- `verbs::reap_client`: the forget task sent to the control shard is `let _ = send_control(..)`; a refused
  spawn there leaks the id the same way (the budget fix makes it unreachable; the drop guard pattern
  applies).
- `daemon.rs`: `if let Ok(task) = futures::spawn(reap_loop())` (and the serve and heartbeat loops): a
  daemon whose reaper failed to spawn never reaps and says nothing.
- Runtime, the general form: `Control::Spawn` on a full arena drops a future handed by move without telling
  the sender (banned item 9). The drop guard is the local mitigation; a sender-notified refusal is the
  runtime-level fix to decide.
