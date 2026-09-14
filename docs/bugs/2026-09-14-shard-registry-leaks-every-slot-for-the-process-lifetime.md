# The shard registry leaked every slot, kick descriptor and context for the process lifetime

Date: 2026-09-14
Area: `crates/rt/src/registry.rs` (the process-wide shard registry), `crates/rt/src/driver.rs`
(`Kick`), `crates/rt/src/{kqueue,epoll,uring}.rs` (the kick descriptor), `crates/rt/src/runtime.rs`
(`Runtime::shutdown`, `LocalRuntime`), `crates/rt/src/shard.rs`, `crates/mem/src/slab.rs`
Severity: banned item 8 (unbounded growth) in the runtime's core: a process that starts runtimes
repeatedly ran out of registry slots at the 895th shard and leaked one kernel descriptor per shard;
in the in-process fleet suite (~35 tests × 2–5 daemons × 1–5 shards) this was the accumulated state
that made late tests fail under CPU oversubscription
(`docs/bugs/2026-09-14-fleet-suite-accumulates-per-test-leaks-under-oversubscription.md`, which
classified the effect and left the resource unnamed).

## Symptom

`crates/rt/tests/reclaim.rs`, written first:

- `a_shut_down_runtimes_slot_is_reclaimed_so_more_runtimes_than_slots_may_run_in_turn`: start and
  shut down 1,025 one-shard runtimes in turn. Before: `runtime 894 refused after 894 shut-down
  runtimes: TooManyShards { max: 1024 }` (the test binary's other shards had taken the rest).
- `a_shut_down_runtime_closes_every_descriptor_it_opened`: 64 start/shutdown cycles of a two-shard
  runtime. Before: `descriptors leaked across 64 two-shard cycles: 8 before, 220 after` — one kqueue
  per shard, plus the test's own, never closed.

## Root cause

`registry::register` took the next id from a monotonic counter and `Box::leak`ed the entry into a
1024-slot static table; the module doc justified it ("entries are never freed, so a waker that
outlives its shard kicks a closed driver … one runtime in production"). Around it, `kqueue::prepare`
/ `uring::prepare_eventfd` leaked the kick descriptor, `ShardContext::build` leaked the context, and
`connect_pairs` leaked every pair ring. The leak was doing a job — a foreign waker holding a shard id
must never touch freed memory — but at the cost of a bound on shards *ever created* rather than
shards *alive*, and of a descriptor per shard for the process lifetime.

## Fix (the design; no product behaviour changed for a live shard)

Registry slots are generational: each of the 1024 slots holds a `generation` word — odd while free,
even while a shard holds it — and an `AtomicPtr` to its entry. `register` claims the lowest free
slot with one `compare_exchange` (free → the next even value), so concurrent runtimes never share a
slot; `unregister` (called by `Runtime::shutdown` after every thread joined, and by `LocalRuntime`'s
drop) closes the kick descriptor and turns the generation odd. The entry is **retired, not freed**:
it stays allocated until the slot's next registration replaces it — after the generation moved past
every reader — so a waker that outlives its shard reads valid memory whose kick is closed and whose
control channel is disconnected (both inert), or finds the slot free and is counted (`stale_wakes`).
Memory is bounded at one entry per slot for the process lifetime; nothing grows with the number of
runtimes started.

What a stale waker can still do is reach a *new* shard that reused the slot. That wake is refused by
the new shard's task arena: the retired entry records the highest task generation the old shard
issued (`note_arena_generation`, at the shard loop's exit) and the new shard's arena starts its
generations past it (`Slab::with_generation_base`; the arena's generations are monotonic per slot for
the process lifetime), so a word minted for the old shard names a generation the new arena has not
reached and fails the existing handle check. Proven by
`a_wake_minted_for_a_dead_shard_is_refused_by_the_slots_new_holder`: the second runtime provably
reused the same id, the stale wake reached a live slot (the stale-wake counter did not move), and the
new holder completed exactly its own task.

The kick descriptor is owned by the entry (`driver::KickFd`: an `OwnedFd` behind a `closed` flag;
every kick and every driver use goes through `with`/`fd`, which check the flag first, so a stale
kick never writes to a descriptor number a later open reused). The pair rings are owned by the source
shard's entry and lent to its peers (`lend_pair_ring`), retired with it — every shard of a runtime
unregisters only after every thread of it joined, so no borrower outlives a ring. The drivers take
their kick from the slot (`DriverSeed` now receives the `Kick`) instead of a leaked `&'static`.

Unsafe budget: `slates-rt` 49 → 60, each site named in `unsafe-budget.toml` (the `KickFd` cell under
its flag; the entry's `'static` borrows under the generation protocol). A loom model of the slot
protocol is owed; `registrations_wakes_and_unregistrations_interleave_without_a_fault` (16 threads ×
200 register/wake/unregister cycles against shared slots) is the stress stand-in.

## Verification

`cargo test -p slates-rt --test reclaim` → 3/3 (the two failing-first tests above and the stale
waker); `cargo test -p slates-rt` all green; `cargo test -p slates-mem --lib` 37/37; `cargo clippy -p
slates-rt -p slates-mem --all-targets -- -D warnings` clean; `cargo xtask check` unsafe budget ok
(rt 60/60); the workspace compiles. The fleet suite on the merged tree is recorded in the commit.

## Sibling sweep

- `SimRuntime` registers the same way and now unregisters through `Runtime::shutdown`'s path for
  its shards; its `SimShared` clock is still `Box::leak`ed per simulation (small, bounded per
  simulation; owed to the same treatment).
- `daemon.rs`'s `fleet_progress` atomic, `fleet.rs`'s identity, the demultiplexer sockets and the
  shard contexts themselves: **closed the same day** —
  `docs/bugs/2026-09-14-shard-context-and-fleet-sockets-leak-per-boot.md`.
