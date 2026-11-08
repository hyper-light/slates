# A shard's recovery ran in one poll, and the watchdog withheld the beat (2026-11-07)

## Description

A shard's whole start, its recovery included, ran as one poll of one task on the shard (`init_shard` inside the
`spawn_on` of `Daemon::start`). The A-228 watchdog withholds the daemon's beat once a shard has run a liveness window of
CPU without completing a step, until the step ends. So a recovery that needed more than a window of CPU on one shard
stopped the beat for its CPU less a window. The anchor kills a daemon whose beat is older than a window plus its own
lateness, and every successor ran the same recovery: a restart loop the start judgment (2026-11-07,
`docs/bugs/2026-11-07-the-anchor-killed-every-start-a-busy-machine-starved.md`) could not break.

## Root cause, measured

- **One poll.** `crates/server/tests/recovery.rs`
  `a_daemon_keeps_its_beat_through_a_recovery_longer_than_a_window_and_recovers_everything` holds this crate's own
  source tree (77 files) as a snapshot and creates greens over it (A-217: each walks its origin again from the
  snapshot at every recovery), doubling them and restarting until a recovery reaches the least CPU an unsliced daemon
  cannot beat through (two windows and two beats). On main 4811191a, this Mac: 1, 2, 4 and 8 greens recovered in 0.23,
  0.39, 0.65 and 1.26 s of CPU and kept the beat; 16 greens took 2.36 s, the watchdog logged "shard 1 is spinning: 1130
  ms of CPU without a step", and the beat went 2.20 s without advancing (the bound: a window plus a beat, 1.1 s).
- **Recovery grew faster than its state.** The records' sort recomputed each record's lineage depth at every
  comparison, and the depth listed the whole catalog to find its own bound: 132-141 ms of a 1,001-volume recovery's CPU
  on this Mac (step 1 of the slicing plan, `~/.cache/slates-agents-scratch/liveness/recovery-units.md`). A merge state of
  versions and open works recovered in 0.43 s at 256 units, 1.44 s at 512 and 5.52 s at 1,024 (the test's first
  design, agents/liveness-slice3 @ 8011d73a).
- **Two snapshot decodes.** Recovery decoded both snapshot slots whole and dropped the older; of a 1,001-volume
  partition's 184-218 ms snapshot read, one decode was waste.

## Exact edits

- `crates/rt/src/shard.rs`, `futures.rs`, `queue.rs`, `error.rs`: `futures::spawn_alone`, a task the shard runs alone
  until its future ends. Each step polls it and no other task; the rest stay queued in order (a task polled in the same
  batch as the one that begins the hold is put back at the front, `LocalQueue::put_back`) and run once it ends, as
  behind one long poll. Every yield still ends a step, so the shard's pulse and inbox go on. A second hold is refused
  `RtError::ShardHeld`; a hold whose task is gone is let go at the next step.
- `crates/server/src/daemon.rs`: each shard's start runs alone (`start_shard`), so nothing else reaches the shard's
  state before it is installed, and `init_shard` is async. The boot log's line now reads "shard N initialized in T µs,
  C µs of its CPU, in S slices of U units; the longest slice W µs, the most CPU in one X µs, ending at unit K (a slice's
  budget B µs)".
- `crates/server/src/recovery_slices.rs` (new): the recovery's slices, each half the live step quantum
  (`DaemonConfig::copy_slice_ns`), checked against the clock after every unit; the thread's CPU read once a slice.
- `crates/server/src/verbs.rs`: `rebuild_recovered` is async and yields between units: each record rebuilt, a green's
  replay a unit at a time (`GreenReplay`: its held origin's walk, its engine built or restored at its checkpoint, each
  chain entry), each logged FUSE write replayed (Linux). The order, the fences and every count are unchanged; the sync
  callers (a takeover's rebuild) run the same steps to their end. The records' sort computes each key once
  (`sort_by_cached_key`) and the lineage depth's bound is the catalog's count, not a list of it.
- `crates/server/src/merge_service.rs`: `HeldWalk`, the held origin's walk a unit at a time over main's `OriginWalk`
  (a directory's listing or a file's read), the same walk `walk_held_origin` runs to its end.
- `crates/db/src/replay.rs`: the newest snapshot slot is decoded first, by the sequence its encoding begins with, and
  the older only when the newer does not decode; the same choice as before, at one decode.

## Tests

- The test above fails on main for its stated reason (the beat 2.20 s without advancing) and passes with the fix: 16
  greens recovered in 2.51 s of CPU on shard 1 in 1,320 slices, the most CPU in one slice 152 ms (one green's engine
  built over its origin, one unit), the beat never older than 495 ms; every green's every file read back.
- `crates/rt/tests/alone.rs`: a task alone runs through its yields while a task queued before it, one polled in the
  very batch that started it, one spawned from another thread and one spawned by the task itself all wait, then run in
  their order; each of its polls is in a step of its own; a second hold is refused; a shutdown cancels the holder and
  those behind it. Red with the hold not set (the order assertion) and with the batch's put-back removed. A simulated
  shard's version runs under Miri (CI's lane now names `--test alone`), red with the hold not set.
- `crates/rt/src/queue.rs`: `take_slot` and `put_back` keep the queue's order and collapse a repeated wake.
- `crates/db/tests/model.rs` `a_newest_snapshot_that_does_not_decode_leaves_recovery_the_older_one`: a newer slot whose
  sequence reads highest but does not decode leaves recovery the older one; red when only the newest is tried.

## Left open

- The catalog's replay (`slates_db::replay::recover`) is still one unit: 206-261 ms of CPU for a 1,001-volume partition
  on this Mac. It stays under a window until a partition several times that size.
- A green's engine built over its origin (`Green::with_origin`, or `restore` at a checkpoint) is one unit: 152 ms here
  for a 77-file origin in a debug build.
- Sibling, reported: after a restart, a pinned overlay's snapshot taken before it is refused `ConsistentBaseUnavailable`
  as a base for a new green, since the rebuilt base plane holds no listings (`crates/vfs/src/coverage.rs`
  `snapshot_is_complete`); recovery still walks existing greens' held origins from it.
