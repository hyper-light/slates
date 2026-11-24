# The store owner's stop joined blind on a handle still held (2026-11-23)

## Description
A fleet test of A-727 (d1a-3) hung 75 minutes on the MacBook. A 10 s sample of the hung process showed its harness in
`GitFleet::stop_store` → `StoreOwner::stop` → `pthread_join`, waiting on the store owner's thread while a stopped
daemon's shards still held a `StoreHandle`. The production daemon's serve (`crates/api/src/role/daemon.rs`) calls the
same stop after `daemon.stop()`.

## Root cause
`StoreOwner::stop` let go of the daemon's own handle and joined the owner's thread, but the owner's loop ended only when
its request channel closed, that is once every handle was gone. Any handle a task, a shard's state or a test still held
kept the owner serving and the join waiting, with no bound and no word: a blind join (banned item 9's spirit: a wait
nothing owns the end of).

## Impact
A daemon's stop could hang for as long as a handle outlived its shards (a handle leaked into a detached task, a test's
fleet harness holding the stopped daemons). Nothing was lost: the owner kept serving and checkpointed only at its end,
so a hung stop also left the store uncheckpointed until the process was killed, and a restart replayed its log.

## Exact edits
- `crates/store/src/owner.rs`: a `Request::Stop`. `StoreOwner::stop` sends it from its plain thread (`blocking_send`;
  the owner gone already is fine; on a shard it answers `NotOnShard`, since a shard never blocks), then joins. The owner
  ends its loop at the stop, lets its open landing go and writes its checkpoint, so the join waits for the request it is
  serving and the checkpoint, never for a handle. Its receiver's drop closes the queue: every request queued or sent
  after the stop is refused `Gone`, the queued ones dropping with their answers. An owner whose store could not open
  (`refuse_all`) ends at the stop too. The module's "Ending" paragraph and `OwnerError::NotOnShard`'s doc say so.
- `crates/store/tests/owner.rs`: `a_stop_with_a_handle_still_held_ends_the_owner_and_the_handle_is_refused_gone`:
  a changeset landed, a handle kept, the stop on a plain thread; red first, the stop not answered within 60 s; green,
  answered Ok, the kept handle's request `Gone`, a new owner listing the changeset (the checkpoint written).

## Sweep (main's order: every blind join reported)
No other blind join in shipped code; each join's thread has been told to end, or has ended its work, before the join:
- `crates/server/src/doorbell.rs` `Doorbell::stop`: the stop sender dropped and the waker rung; the thread sees it
  within its wait's bound.
- `crates/server/src/watchdog.rs` `Watchdog::stop`: the stop sender dropped; the thread's next beat sees it. The
  failed-exit writer is joined only after it wrote its line.
- `crates/server/src/landing.rs` barrier: joined only once its outcome arrived (the thread has only to exit).
- `crates/server/src/winfsp.rs` (`reap`, `end_all`): each mount thread stopped first, joined at its last outcome.
- `crates/rt/src/runtime.rs`: `Shutdown` sent to each shard (retried while its control channel is full) before the
  join; `crates/rt/src/retirement.rs`: `Stop` sent first, into a slot kept for it.
- `crates/land/src/os.rs`, `os_windows.rs`: a landing's sync and flush streams, each a bounded device wait.
- `crates/api/src/container_engine.rs`: the child is killed and waited first, so its pipes close and its readers end.
Two discarded results noted for their owners, not changed here: `container_engine.rs` drops each reader's join result
(`let _ = reader.join()`), and `retirement.rs` drops the `Stop` send's (`let _ = commands.try_send(..)`, which its slot
makes room for).
