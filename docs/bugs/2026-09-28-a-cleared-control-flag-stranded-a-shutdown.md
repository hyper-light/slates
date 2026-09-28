# A cleared control flag stranded a shutdown

Date: 2026-09-28. Scope: the runtime's control path (`crates/rt/src/parking.rs` `ControlFlag`, used by
`registry::send_control_to` and `shard::drain_control`). Found by a hung in-process fleet suite on this
machine (Apple silicon).

## Symptom

A full `cargo test -p slates-server --test fleet` run stopped making progress at 15:42 and was still stuck at
17:17. Every other test waited on the suite's serializing mutex. The one holding it,
`a_root_learner_fetches_the_committed_region_membership_over_the_transport`, was inside `Daemon::stop` →
`Runtime::shutdown`, joining a shard thread (`sample` of the process, kept in the session scratchpad):

```
fleet.rs:3409  Daemon::stop → Runtime::shutdown → JoinHandle::join → __ulock_wait
slates-shard-N  ShardContext::park → Parking::park_unless_pending → wait_in_driver → KqueueDriver::wait → kevent
```

The shard had been sent `Control::Shutdown` and was parked in its driver with no deadline. Alone, the test
passed 20 of 20 runs (2–4 s each), so the hang was a rare interleaving, not a logic error in the test.

## Root cause

A sender pushes a control message, then publishes the shard's control-pending flag, then kicks the shard only
if it has announced parking. The shard cleared the flag before draining its channel with a `load(Acquire)`
followed by a separate `store(false, Release)`, and senders published with a plain `store(true, SeqCst)`.
Both halves were wrong under the language's memory model, and loom finds each:

1. **The clear was not atomic** (loom: failed at interleaving 207 with only this half wrong). The shard's load
   can read an earlier sender's mark; its store of `false` then lands after a later sender's mark in the
   flag's modification order and erases it. If the drain does not see that later sender's message — the
   drain's acquire loads may execute before the preceding store-release of `false` completes, which Apple
   silicon's RCpc `ldapr` permits — the message stays in the channel with the flag cleared. That sender's
   kick had coalesced into the wake the shard already took, so the shard parks with no deadline, for good.
2. **The publish was a plain store** (loom: failed at interleaving 1 with only this half wrong). A plain store
   from a second sender ends the first sender's release sequence (C++20 `[intro.races]`: a release sequence
   continues only through read-modify-writes). The shard's acquire of the second mark then synchronizes with
   the second sender only, and nothing orders the first sender's message before the drain; neither sender
   kicked (the shard was not yet parked), so the shard parks with a message it never drained.

A `Shutdown` stranded this way leaves the shard parked, and `Runtime::shutdown` joins it forever.

## Fix

The flag is now a type, `parking::ControlFlag`, with both halves as read-modify-writes:
- `publish` is `swap(true, SeqCst)`: consecutive publications form one release sequence, so the shard's
  acquire of the latest mark synchronizes with every sender whose mark it absorbs.
- `take` is `swap(false, AcqRel)`: it reads the latest mark in the modification order, so a concurrent
  publication is either absorbed (and its message ordered before the drain) or lands after and stays set.
- `rearm` (after a batch-limited drain) and `is_pending` (the park re-check) are unchanged in meaning.

Cost: one atomic RMW per control message on the sender, on a path used for spawns, cancellations, activity
changes and shutdown — never per task wake.

## Test

`parking::loom_tests::a_control_message_published_while_the_shard_drains_is_never_lost`: two senders each push,
publish and kick-if-parked; the shard takes, drains, and parks unless pending. It drives the real
`ControlFlag` and `Parking` code.

| `publish` | `take` | loom |
|---|---|---|
| store | load, then store | deadlock at interleaving 1 |
| store | swap | deadlock at interleaving 1 |
| swap | load, then store | deadlock at interleaving 207 |
| swap | swap | **5,204 interleavings, no loss**; the shard waited in some (non-vacuous) |

The full loom suite (`RUSTFLAGS="--cfg loom" cargo test -p slates-mem -p slates-rt -p slates-ipc --lib
--release loom`) passes, and the runtime's tests pass.

## Sibling sweep

- The foreign wake ring (`registry::send_foreign`) publishes through the ring itself, whose per-slot sequence
  numbers carry the synchronization; its parking model (`a_word_published_while_the_shard_parks_is_never_lost`)
  already covers it.
- The daemon's doorbell flag (`DOORBELL_RANG`, `crates/server/src/daemon.rs`) is cleared by `swap(false,
  AcqRel)` and has one publisher (the doorbell thread), so neither shape arises; the doorbell protocol itself
  has fenced loom models (`crates/ipc/src/doorbell.rs`).
- The simulated driver's `kicked` flag (`crates/rt/src/sim.rs`) is cleared by a `swap` and set by a plain
  store, which is sound only because the simulated fabric runs every shard on one thread (D-20); it is not a
  cross-thread protocol.
- No other flag in `crates/rt` or `crates/ipc` is cleared by a load-then-store pair (swept by `grep` for
  `load(…Acquire)` and `store(false`).
