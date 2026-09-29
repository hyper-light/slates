# An idle shard spun for good once a client had connected (2026-09-29)

## Description

With the CPU placement fixed (A-41), the KIND lane's idle five-replica fleet still spent a large share
of its CPU doing nothing. Each pod's one shard used 28–62 s of CPU in its first 190 s: 15–32 % of a core,
with no request in flight, only the readiness probe's `slates status` every 5 s (probe3 on
`slates:placement`, 2026-09-29).

## Root cause

`activate` in `crates/server/src/daemon.rs` sent `Control::Active(true)` to a client's shard at the
handoff. Nothing ever sent `Active(false)`: not the client's departure, not its reap. The runtime's loop
(`crates/rt/src/shard.rs`, `run`) spins its idle window before every park while the flag is set. So from a
shard's first client on, every idle moment ended in a full idle window of spinning, however long after the
client had gone. On a fleet pod those idle moments are many: the heartbeat, the probe and record periods
and the reap sweep.

The design words the rule as §4.7's "Shards poll rings while any client has activity within the measured
idle window". The serve loop implements that rule for ring clients: it keeps polling for the window after
its last served request, which is what lets a client skip the doorbell. The runtime's flag added a second
spin, unbounded by any activity.

A solo daemon in plain Docker under the same two-CPU quota showed the step cleanly: 0.07–0.09 % of a CPU
idle before any client, 1.6–1.8 % for good after one `slates status` had come and gone and been reaped
(`clients_reaped: 1`).

## Impact

Every daemon that had ever served a client spun before each park for the rest of its life.

- On a laptop that is a daemon burning CPU with no agent attached.
- On a fleet node it was up to a third of a core per idle shard, which competes with the node's other
  work.
- Under a CPU quota it spends the quota the node's real work needs.

## Exact edits

- `crates/rt/src/shard.rs`: the `active` flag is replaced by `activity_ns`, the shard clock time the server
  last noted client work (`ShardContext::note_activity`). The loop spins only until that activity's window
  ends (`spin_after_activity`): the window is measured from the activity, not from each idle moment.
- `crates/rt/src/control.rs`, `runtime.rs`: `Control::Active` and `Runtime::set_active` are removed.
- `crates/server/src/daemon.rs`: the handoff's `activate` and `ACTIVATION_LOST` are removed. The serve loop
  notes activity when a round served work. `Daemon::idle_spins` is new, an observation of each shard's idle
  spins.
- `crates/server/src/verbs.rs` (`run_forwarded`), `crates/server/src/nfs.rs` (each call a connection
  answers) and `crates/bridge-virtiofs/src/serve.rs` (each service pass) note client work, so a burst of
  forwarded verbs, mount calls or guest requests is caught spinning.
- Tests and bench: `crates/rt/tests/tcp.rs` and `pollers.rs` open the window by noting activity; the
  registry tests send `Shutdown` where they sent `Active`; `rt_bench` notes each spawned task and ping-pong
  turn.

## Evidence

- Failing test first: `crates/server/tests/daemon.rs`
  `an_idle_daemon_parks_once_its_clients_windows_have_passed` failed with 12 idle spins on the control shard
  and 2 on the other in one quiet second (`[23, 4]` against `[11, 2]`). It passes now, spins unchanged
  across the stretch.
- `crates/rt/tests/idle_spin.rs` checks the window by use: no spin without activity, spins inside the
  window (the non-vacuity count), none after it.
- Suites: runtime all, server library 122, daemon 15, client all, fleet 59 of 59 (257 s, host load 10–12),
  CLI 13 of 13.
- Solo daemon in Docker (`--cpus=2`, image `slates:a42`): 0.09–0.16 % of a CPU before any client and
  0.08–0.11 % after one. Before the fix it was 1.6–1.8 % after one.
- KIND, the same idle five-replica wan fleet over 180 s: each shard used 2.3–3.6 s of CPU in about 190 s,
  against 28–62 s before, with 0 lapses and 0 restarts.

## Sibling found while measuring, open

The provisioning histogram (`cargo run --release -p slates-client --example provision_bench`, the R9
gate) aborts before it measures: `Refused(BudgetExceeded { available: 1478 })` at its create-destroy loop,
after 51 "a volume was not imaged, skipped: ESTALE" lines. It does the same at `fd7b7b0`, before this
change, so the cause is elsewhere; it is recorded in GAPS and fixed next.
