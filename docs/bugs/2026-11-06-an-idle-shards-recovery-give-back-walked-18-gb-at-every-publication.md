# An idle shard's recovery give-back walked 18 GB at every publication (2026-11-06, A-226)

## Description

On macOS, a paced edit and merge (`edit_merge_bench --quiet-only --pause-ns 2000000`, so that each request meets
parked shards) had a p99 of 5.7 to 21 ms and a max of 30 to 74 ms, against a p50 of 140 to 170 µs. The tail was blamed
on the doorbell and the park/kick protocol (BENCHMARKS, "The macOS doorbell"): split by the doorbell thread's own
stamps, the late calls spent 17 to 31 ms between the doorbell's wake and the shard's read of the slot.

It was not a lost or held wake. A temporary diagnostic, not committed, stamped the doorbell's kick on each parked
shard's parking word and logged, from the shard's own clock, every park whose kick-to-return passed 1 ms. It also
logged every poll longer than 1 ms with the thread's CPU time across the poll and the polled future's type name. Over
one paced run of 3,000 calls, 12 parks came back from their kick 1.1 to 15 ms late, in waits the kernel never put to
sleep (asked timeout 0), so the host had descheduled the thread. Separately, 46 of 57 long polls were the reap loop,
each 18 to 42 ms long with the thread on the CPU the whole time (wall 42,068 µs, CPU 42,055 µs). The cause, narrowed
down section by section: `purge_if_idle` → `return_dead_recovery`, at 18 to 21 ms a call, marking 18,325,143,552
bytes reusable each time.

## Root cause

A-117 gives back an idle shard's recovery memory that no restart reads: the checkpoint slot older than the committed
one, and the delta log past its tail. It gave back each region whole, its full length. Its guard re-armed the give-back
at every publication (`recovery_returned_at` against the journal's generation). Only the pages written since the last
give-back can be resident, but the give-back walked the regions' whole length: about 18 GB of address space a shard on
adas-macbook-pro-2 (Apple M5 Max, 18 cores, 128 GiB). On macOS that walk is an `madvise(MADV_FREE_REUSABLE)` over the range, 18 to 21 ms of the shard's
CPU in one poll. A merge publishes, so every reap tick after a merge paid it. Any request that reached the shard
meanwhile waited behind it.

## Impact

- macOS: about one reap tick per shard per second held its shard 18 to 42 ms (the walk, plus the descheduling it
  invited). That was the p99 and max of every paced form, whatever the green held.
- Linux: the same walk is a hole punch over the same length. Its cost was not measured here; the counter
  `recovery.returned_bytes` counted the whole range on every platform (91,625,930,752 bytes over five shards for a
  one-file delta in the new test).
- Correctness: none. A give-back only ever returned pages no restart reads.

## Exact edits

- `crates/vfs/src/checkpoint_log.rs`: `Journal` records, for each region, how far it may have been written since it
  was last given back (`committed_written`, `stale_written`, `log_written`). A journal opened over memories it did not
  write treats them as written throughout (`UNKNOWN_EXTENT`), so its first give-back returns them whole, once.
  `dead_ranges` stops each range there. A new `given_back` records what the object took. The checkpoint transition is
  one function, `checkpointed`.
- `crates/server/src/daemon.rs`: `return_dead_recovery` gives back the bounded ranges and records them. A range the
  object refuses stays owed. The generation guard is gone, since an empty range costs nothing, and so is
  `ShardState::recovery_returned_at` (`crates/server/src/state.rs`). New test support:
  `Daemon::give_back_recovery_on_every_shard`.
- Tests: `a_give_back_returns_only_recovery_memory_written_since_the_last` (`crates/server/tests/recovery.rs`) was red
  before the change (91,625,930,752 bytes given back after a one-file delta, against 0) and is green after. The oracle
  `a_give_back_names_every_dead_written_byte_and_stops_at_the_last` (`crates/vfs/tests/checkpoint_log.rs`) runs
  generated histories of checkpoints, appends, give-backs and restarts over memories that record every byte written.
  After every step it requires the named ranges to cover every written byte no restart reads, to stay inside what no
  restart reads, and to end at the last such write. It also requires a restart to recover the full image. Mutation
  checks: dropping the longer-image rule from `checkpointed` left byte 32,767 of a stale slot resident, and zeroing
  `log_written` at a checkpoint left log bytes resident; the oracle failed on both.

## Siblings (reported, not changed)

- `crates/server/src/write_log.rs` `WriteLog::purge` (Linux, the FUSE write log, A-63/A-105) gives back the log's whole
  `capacity` once per emptying, re-armed by any append. It has the same shape. Its capacity and its cost per give-back
  are not measured yet.
