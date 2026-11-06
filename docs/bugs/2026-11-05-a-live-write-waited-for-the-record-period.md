# A live-shipped write waited for the record period (about 100 ms)

**Found** 2026-11-05 by `measure_what_live_shipping_adds_to_a_stable_nfs_write` (`crates/cli/tests/live_fleet.rs`):
p999 9.7–95 ms and max about 100 ms for a stable 4 KiB NFS write to a live-shipped volume, against 0.6 ms for a plain
one. A second machine (kiln-a094a12d, old code) showed the same: max 100,527.5 µs.

**Root cause**, two waits for the next record period (100 ms), each found by its counter (the test now prints every
`fleet.live.*` counter of the owner):

1. `send_from_control` took the holder's record session with `fleet::take_sessions`. When the session was out on loan
   (to the previous write's dispatch or to a straggler), it got none and sent nothing: 2,724 sends found no session and
   all 2,724 were unanswered, in one 2,000-write run. The write's entry then went with whichever later dispatch carried
   it, at worst the period's re-ship.
2. With `CREDIT` (two) entries in flight, a publication cannot become an entry (`fleet.live.refused`: 270 of 2,285).
   It marks the volume as owing a whole record, which only the next publication or the period appended. The last
   turned-away write of a burst waited for the period.

The unit's CPU cap was ruled out: `cpu.stat` showed `nr_throttled` 0 across a run.

**Impact.** No acknowledged write was lost and none was answered early: the tail was latency only.

**Edits** (`crates/server/src/live_ship.rs`):
- `borrow_session`: a send waits for a lent session one timer tick at a time, within the liveness budget. With no link
  at all it is still left to the period. `unanswered` went to 0.
- `fold_into`: the reply that frees the credit appends the owed whole record at once (`owes_whole_within_credit`,
  shared with the period).

**Evidence.** Worst live write 86–276 ms before, 0.47–1.87 ms after, at load 8–30 (`docs/wip/BENCHMARKS.md`, "The
100 ms tail, explained and removed").

**No red test.** The bound is a latency, measured by the gated measure test. Its count of writes over 50 ms went from
5 to 0.
