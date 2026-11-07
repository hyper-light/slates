# A partition's snapshot cadence never left its boot guess (2026-11-07, §4.8 "Recovery", Phase 2 task 2, D-11; A-243)

Found by agents/loadtest-scale sampling the 10,000-client rung after the verdict fix
(`2026-11-07-a-verdict-walked-every-delta-since-its-base.md`). Design agreed with raft, the db owner.

## Description

A partition snapshots itself (re-encodes the whole partition into the segment and trims the log) once the log since
the last snapshot reaches `bytes_between_snapshots = recovery budget × replay throughput`. That throughput was measured
once, at recovery, from the log the boot replayed.
- A daemon booted on a fresh segment replays nothing, so its throughput was 0, taken as one byte a microsecond.
- That gave a 1 MB cadence, for the daemon's whole life.
- So every 1 MB of log re-encoded the whole partition: snapshot work per byte of log was the partition's size / 1 MB.

At 10,000 works the green's owner partition holds every work's log (A-207), every client's completion records and
every volume, and its log fills constantly. The shard spent its time re-encoding.

## Measured

- `sample` of the daemon in the 10k rung after the verdict fix (this Mac, release without LTO, kernel/, 10,000
  clients):
  - the owner shard was idle 0 of 2,241 samples;
  - about 1,812 of its samples were in `Db::commit → snapshot`: the partition's encode about 940, the publish into the
    segment about 590, and record clones about 280.
- `crates/db/tests/snapshot_cadence.rs`: a fresh partition built to about 8 MiB, then 16 MiB of small records.
  - Red on CI run 42385009677: the snapshots wrote past `2 × log + partition` (about 128 MiB expected for 16 MiB of
    log).
  - Green with the fix on both Linux runners: run 42385399630 (the first version), then run 42387993912 (raft's
    version, with a log ring about the partition's size too), with the db and server suites, the CLI status test and
    clippy.
- The local 10k rung, 900 s bound, on this Mac at load 11 to 139:
  - before: 2,197 of 10,000 clients done and 56,375 submits;
  - after (the first version of the cadence): 3,128 done and 92,542 submits.

## Root cause

The cadence was derived from a throughput measured only at boot, and a boot that replays nothing measures nothing.

## Fix

- At every snapshot, the cadence is re-derived (`SnapshotPolicy::at_snapshot`) as
  `max(budget × the apply throughput measured since the last snapshot, the snapshot's size)`.
  - The apply throughput is timed around the partition's apply in every commit: the work a replay repeats, measured
    at no extra pass (raft's choice over the encode or a decode).
  - The boot replay's rate stands until one is measured.
- The second term is the doubling rule: a snapshot comes only once the log has absorbed what it costs, so snapshot
  work stays within about twice the log written.
- The decision is the commit's, never the apply's.
- A cadence a caller sets (`Db::set_policy`, the model tests) is pinned.
- `Db::stats()` (`DbStats`) gives the figures, and `slates status` prints them per shard: snapshots, bytes written,
  longest duration, log appended, cadence, and the measured apply rate.
- Recovery's bound changes: decode(P) + replay(max(budget bytes, P)). That is the budget while the partition's
  snapshot decodes within it, and about twice its own cost beyond. It is stated in §4.8.

## Impact

Snapshot cost grew with the partition on every megabyte of log, on the committing shard's turn: the throughput wall of
any partition large enough, here the green's owner at 10,000 works.

## Siblings, reported, not fixed here

- The snapshot still runs whole on the committing shard's turn. An 8 MiB encode stalls that shard for its duration,
  now counted (`longest_ns`) so a late beat can be matched to it. Slicing it is the follow-on.
- A partition far past the budget wants an incremental snapshot (dirty records only), which changes the format and
  recovery.
