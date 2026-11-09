# The cadence test's approach predicted the next record from the last one (2026-11-09, A-243's test; §4.8)

Found by raft-chaos's gates: `a_fresh_partitions_longest_tail_replays_within_its_recovery_budget`
(`crates/db/tests/snapshot_cadence.rs`) failed "a snapshot came before the approach stopped 3 times" on the shared Mac,
4 of 4 at load about 190, then on main's own db code at load 48 to 64 (18:06 to 18:12 UTC). A test fault, not the
product's: the trigger fired as it should.

## Description

The test approaches the snapshot trigger by appending small records, and stops one record short of it, so the
recovery it then times replays the longest tail the trigger allows. The trigger fires once the tail's replay work, as
each commit times its append and apply on the wall clock, reaches the recovery budget (10 ms here) and the tail is at
least the last snapshot's size. The approach predicted the next record from the last one alone: stop when
`tail + 2 × last` would reach the budget.

A record whose commit is preempted measures the preemption as replay work. On the shared Mac under load, single
records measured 1 to 8 ms against about 5 us for the rest. So a spike crossed the prediction and the snapshot came
first; the test restarts the approach after such a snapshot, three times, and then gives up.

## Measured

- An instrumented approach, this Mac, load 49 to 71 (11 runs, the single test and the whole binary): the largest record
  of an approach measured 61 us to 5.1 ms. Early snapshots came with the tail at 2.07, 3.87, 7.66 and 9.96 ms while
  the largest record seen before them was 78 us to 1.5 ms, so the records that brought them took at least 6.1, 7.9,
  2.3 and 0.04 ms.
- Linux does not show it: on kiln-a094a12d (32 CPUs) under 64 spinners, load 24 to 68, main's test passed 10 of 10 and
  the fix 10 of 10; no early snapshot failed a run.
- Red/green on this Mac under 36 spinners (two a CPU, bounded, stopped by pid), the whole `snapshot_cadence` binary
  as the gates run it:
  - main: failed 3 of 6, at load 69, 104 and 112; passed at 55, 82 and 92;
  - the fix: passed 6 of 6, at load 116 to 143.

## Fix

The approach predicts the next record from the largest one it has seen since the settling snapshots, across its
restarts, not from the last one alone. An early snapshot is itself evidence: the record that brought it took at least
the room the tail had left (`budget − tail`), so that lower bound joins the largest, and a restarted approach stops
short of a spike that size. The window is the approach's own records, which the trigger sums; no count is chosen.

## What the fix costs

Under heavy load the approach stops early. A record measured at half the budget or more makes the prediction reach the
budget at once, so the approach ends after a record: in 3 of the 6 green runs (load 135 to 143) the tail was 144 bytes
and its recovery 34 to 48 us. Those runs still check that recovery fits the budget, but they no longer test the
longest tail the trigger allows. The test prints the largest record it saw, so a run that stopped early says so. On an
unloaded runner the records stay near 5 us and the approach ends near the trigger (the tail 0.58 to 0.67 of the budget
on CI's Linux runners, A-243's record).

## Not fixed here

The trigger times replay work on the wall clock, so preemption counts as replay work. That makes a loaded partition
snapshot early, which is safe (recovery stays within the budget) but costs snapshots, and it is why no prediction made
from the commits' own measure can approach the trigger reliably under load. Timing the append and apply in the
committing thread's CPU time (A-232's class: a budget judged in the shard's own time) would remove the spikes from both
the trigger and this test. That is a product change to `Db::charge_replay_work`, raft's to decide.
