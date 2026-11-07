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
- raft's review asked that the bound be checked, not only stated. Two tests now time recovery against the budget
  (debug builds on CI's two Linux runners, a 10 ms budget, median of the recoveries, three runs each):
  - `a_fresh_partitions_longest_tail_replays_within_its_recovery_budget` (the longest tail the cadence allows, five
    recoveries). With the rate timed on the apply alone, red: the commits measured 272 to 383 bytes a microsecond
    while the replay ran 13 to 22, so the median recovery took 15.6 to 20.9 times the budget (runs 42389990113 and
    42390241110). Timed on the append and apply: 0.58 to 0.67 of the budget, the recoveries within 6% of each other.
  - `a_tail_whose_records_change_size_still_replays_within_its_recovery_budget` (large records, then small ones, a
    recovery timed after each eighth of a budget's replay, median of three). With the cadence extrapolated from the
    last stretch of log, red (run 42390792946): the small records' tail passed the budget at about 121 KB (aarch64)
    and 213 to 217 KB (x86_64), with no snapshot due before 6.75 to 8.59 MB. In the cadence test the large records
    had measured 442 to 455 bytes a microsecond and the small ones 12 to 24: a 442 MB cadence, about 37 s of replay
    against a 1 s budget.

## Root cause

The cadence was derived from a throughput measured only at boot, and a boot that replays nothing measures nothing.
The two versions after it each extrapolated a rate: one timed on less than a replay does, the other measured on one
stretch of log and applied to the next.

## Fix

- A partition snapshots once its tail, the log since its last snapshot, has taken the recovery budget's worth of
  replay work as its commits measured it, **and** is at least that snapshot's size (`replay::snapshot_due`; the
  conditions are raft's).
  - The work is timed on each commit's append (encode, checksum, copy into the ring) and apply, never on a wait outside
    them: the work a replay repeats per record, the append mirroring its read, checksum and decode, measured at no
    extra pass. It is slower per byte than the replay, and a preempted commit counts its stall, so it errs early.
  - Nothing is extrapolated: the tail's own work decides, whatever its records' sizes.
  - After a boot the tail's work starts at the recovery's own time, the snapshot's decode included, and the floor at the
    recovered snapshot's size.
  - The budget is the operator's input (`Db::set_recovery_budget`, GAPS §5), one second by default.
- The second condition is the doubling rule: a snapshot comes only once the log has absorbed what it costs, so snapshot
  work stays within about twice the log written. Work alone would break that bound.
- The decision is the commit's, never the apply's.
- A cadence a caller sets (`Db::set_policy`, the model tests, the bench) is pinned. Snapshot points under the
  measured trigger follow the clock, so the model's generated histories run under it too, the recovered state
  compared whole after every crash.
- `Db::stats()` (`DbStats`) gives the figures, and `slates status` prints them per shard: snapshots, bytes written,
  longest duration, log appended, the tail's bytes and measured work, the budget and the floor.
- Recovery's bound changes: decode(P) + max(budget, the replay of P bytes of log). That is the budget while the
  partition is small enough, and about twice its own snapshot's cost beyond. It is stated in §4.8.

## Impact

Snapshot cost grew with the partition on every megabyte of log, on the committing shard's turn: the throughput wall of
any partition large enough, here the green's owner at 10,000 works.

## Siblings, reported, not fixed here

- The snapshot still runs whole on the committing shard's turn. An 8 MiB encode stalls that shard for its duration,
  now counted (`longest_ns`) so a late beat can be matched to it. Slicing it is the follow-on.
- A partition far past the budget wants an incremental snapshot (dirty records only), which changes the format and
  recovery.
