# Every shard context, fleet serve socket, identity and progress word was leaked per runtime start

Date: 2026-09-14
Area: `crates/rt/src/registry.rs` (the slot protocol; `reclaim_context`, `with_entry`,
`try_send_foreign`, `note_exited`), `crates/rt/src/shard.rs` (`ShardContext::build`/`keep`/`send_to`,
`Pulse::beat`), `crates/rt/src/runtime.rs`, `crates/rt/src/sim.rs`, `crates/transport/src/demux.rs`,
`crates/server/src/fleet.rs`, `crates/server/src/daemon.rs`, `crates/server/src/doorbell.rs`
Severity: banned item 8 (unbounded growth) in the runtime and the fleet: every runtime start leaked, per
shard, the context — a task arena, run queue and timer wheel each sized to the shard's task budget
(2,032 KiB resident for a two-shard daemon-sized runtime, measured below) — and every fleet daemon boot
leaked its two serve sockets (so a stopped in-process daemon's ports were never free again), its TLS
identity, its coordinator progress word and its doorbell stop flag. This is the remainder of the
accumulated suite state classified in `docs/bugs/2026-09-14-shard-registry-leaks-every-slot-for-the-process-lifetime.md`
("the demultiplexer socket is … the largest remaining per-daemon leak") and the reason the restart
tests had to dial a restarted node at a *different* address pair.

## Symptoms

- `crates/rt/tests/memory.rs::a_shut_down_runtimes_context_heap_is_given_back` (written first): warm up
  one two-shard runtime sized like a daemon (4,096 tasks per shard), then start and shut down 32 more in
  turn, reading the process's resident size through `ps`. Before: **growth 56,016 KiB over 32 cycles,
  0 contexts reclaimed** (one footprint of 2,080 KiB per cycle — the leak exactly). After: growth
  **752 KiB** against a footprint of 2,032 KiB, 66 contexts reclaimed (2026-09-14 15:47, this box).
- `crates/server/tests/fleet.rs::a_stopped_daemons_serve_ports_are_freed_so_its_restart_binds_the_same_addresses`
  (by use): a two-node fleet forms; B stops; a fresh B starts on B's exact addresses. Before (the same
  test run on the pre-fix tree, `5de244d`, 2026-09-14 15:58): the mesh never re-formed and the
  restart's refusals read `{"fleet.bind": 1}` — failed after 304.38 s (a full period budget). After:
  no `fleet.bind`, the mesh re-forms to the restart, ok in 4.41 s.

## Root cause

`ShardContext::build` ended in `Box::leak` ("a shard lives for the process; the leak is what lets every
reference be a plain `&'static` with no unsafe code"). Around it, `Demux::start` leaked each
demultiplexer (holding its socket), `fleet::run_membership` leaked the identity, `Daemon::start` leaked
the coordinator's progress atomic, and `DoorbellThread::start` leaked its stop flag — each justified as
"once per boot, the daemon is process-lifetime". A process that starts daemons repeatedly (the suite,
~35 tests × 2–5 daemons) grew without bound, and the sockets made a same-address restart impossible.

## Fix

**The context is owned by its slot and freed by its own thread.** `build` hands the slot the raw box
pointer (`attach_context`); the owning thread, once its loop has returned (`Runtime`'s worker after
`run`; `LocalRuntime` and `SimRuntime` in `Drop`), calls `reclaim_context`, which clears the thread's
current-context cell, marks the entry exited, and frees the box. That is sound because **no other
thread ever dereferences a context**: a waker is a packed word routed by shard id through the registry
*entry* (which stays retired-not-freed), cross-shard rings are owned by entries, and the current-context
cell is thread-local. The `'static` the tasks see is a promise the loop's exit keeps.

**Per-shard singletons live on the context** (`ShardContext::keep`): a value the shard owns for its
life, handed out as `&'static`, dropped with the context after every task, last kept first. The
demultiplexer (`Demux::start` now refuses off a shard thread), the fleet identity, and — by the same
principle, without an allocation at all — the coordinator's progress count, which moved onto the
registry `Pulse` (`beat`/`progress`), where an observer reads it as it read the pulse. The doorbell's
stop flag became a channel whose dropped sender is the signal. `SimRuntime` now unregisters and
reclaims its shards (before, a simulation never gave its slots back).

**Two faults in the slot protocol, found by the registry stress test once contexts were freed** (both
timing-dependent; the same binary passed one validation run and hung or crashed the next):

1. A foreign send to a full ring spun until the holder drained it — and a holder that had unregistered,
   or left its loop, never would (a livelock; the test hung 10/10 bounded runs). `send_foreign` is now
   `try_send_foreign` turns that re-read the target under the reader count each time and stop when its
   holder has exited (`Entry::exited`, set at the loop's exit) or its slot is free — counted stale, never
   spun on. A shard's pair-ring send (`send_to`) does the same and, while it waits, **drains its own
   inbound rings** (`drain_while_waiting`), so two shards saturating each other's rings release each
   other instead of each waiting for the other for good; the stress test models its threads as shards
   the same way (a consumer that blocks in a producer loop without consuming was the deadlock).
2. A reader descheduled between loading a slot's entry pointer and pushing to its ring dereferenced an
   entry a re-registration had freed (`SIGSEGV` on the freed ring's head word). Every foreign reader now
   goes through `with_entry`, which counts itself on the slot (`Slot::readers`) around the read; a
   registration that replaces the slot's entry frees the previous one only once the count is zero — a
   sequentially consistent fence on both sides (the parking protocol's discipline) guarantees one side
   sees the other. Slots are cache-line aligned (`#[repr(align(128))]`) since every waker now writes a
   slot's count.

Unsafe budget: `slates-rt` 60 → 64, each site named in `unsafe-budget.toml`.

## Verification

`cargo test -p slates-rt` all green (registry stress test 25/25 bounded runs; `tests/reclaim.rs` 3/3
serialized — its three tests measure process-global state and now hold a harness lock, since under
parallel threads another test's runtime took the slot the stale-waker test expects reused, 3 of 10
runs); `cargo test -p slates-transport` 88 + 5 + 1 + 10; `cargo test -p slates-server --lib` 48/48,
`--test daemon` 6/6; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo xtask check`
ok. Validated on a wiped `target/` (2026-09-14 16:23–16:29, load average 10–17 with the KIND lane's
four-pod cluster and one of its fleet tests alive on the box): 20/20 fast suites, the three gates, and
the fleet suite **37/37 in 203.13 s** under the 300 s stall detector (`validate.sh leak-fix`).

## Sibling sweep

- `SimShared` (the simulation clock and per-shard flags) is still leaked per simulation: a retired
  entry's `Kick::Sim` points at the flag and may be kicked by a stale waker until the slot's next
  registration, so it must outlive the entry — the kick-descriptor treatment is owed to it (small: a
  few atomics per simulation).
- `driver.rs`'s `test_kick` and `parking.rs`'s loom model leak in test code only.
- `xshard` pending-call tables and `state::install` are per-thread and taken down with the state.
- The old restart tests keep dialing the restart at a fresh address pair on purpose (old B's last
  datagrams in flight must not reach the restart); their doc comments no longer claim the ports are
  leaked.
