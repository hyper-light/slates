# The cadence mix test held a recovery's snapshot decode to the tail's budget (2026-11-18, A-243's test; §4.8)

Found on CI: run 44260193629 (main 30014e930), the macOS gates job, re-routed to g251 by the supervisor at 14:15Z.
`a_tail_whose_records_change_size_still_replays_within_its_recovery_budget` (`crates/db/tests/snapshot_cadence.rs`)
failed its eighth check: "a tail of 60988 bytes replayed in 11605000 ns at the median, past the 10000000 ns budget, with
no snapshot yet (the commits measured 7939184 ns of its work)". Decided a test fault, not the product's: the trigger
fired as the design states it.

## Description

The test fills a segment with large records until the snapshot cadence settles, switches to small records, and times a
recovery of the tail after every eighth of a budget's replay, until the next snapshot. Each check held a whole
recovery's cost to the 10 ms budget. A recovery is the snapshot's decode plus the tail's replay, and the design bounds
it by "the snapshot's decode plus the larger of the budget and the replay of a snapshot's size of tail"
(`slates_db::replay::snapshot_due`): the budget is the tail's share, not the decode's.

## Evidence

- CI run 44260193629, checks 1 to 8 (tail bytes, median of three recoveries): 4,684 B in 1.72 ms; 8,140 B in 4.69 ms;
  10,372 B in 2.13 ms; 16,492 B in 3.09 ms; 23,188 B in 2.52 ms; 34,780 B in 3.05 ms; 49,108 B in 5.20 ms; 60,988 B in
  11.6 ms. The commits measured 0.61 to 7.94 ms for checks 2 to 8.
- Check 1's near-empty tail cost 1.72 ms: the decode and a recovery's fixed work. Past it, check 8's tail cost 9.9 ms,
  inside the budget; the commits had measured 7.9 ms, below the trigger.
- The runner's noise: check 2 (8,140 B) cost more than checks 3 to 6 with up to four times the bytes, and check 8's
  per-byte replay past check 1 was 162 ns against check 7's 71 ns while the commits' per-byte measure moved 94 to 130 ns.

## Fix

- A baseline recovery of the settled segment before any small record (the snapshot's decode and a recovery's fixed
  work) is timed, and each check's median is judged past it.
- Each check times five recoveries (`CHECK_RECOVERIES` = `RECOVERIES`), and its noise allowance is the spread of their
  inner quartiles plus the baseline's: the second and the fourth cost, so the fastest and the slowest stand outside it.
  The full range was tried first and rejected: one recovery preempted at load 37 widened it to 8.2 ms and let a trigger
  three times late pass (13.9 ms past the baseline).
- The checks are paced by the whole recovery's cost, so they still step at most an eighth of the budget apart.

## Runs (Mac Studio d1323898, 2026-11-18)

- With the full range as the allowance: the test 6 of 6 and its file 3 of 3 green; red with the trigger moved to 1.5x
  the budget 0 of 3 (on this machine the commits measure about 1.7x the replay's own work, 9.9 ms against 5.7 ms at
  the last check, so a 1.5x trigger still replays within the budget); 2x 2 of 3 failed; 3x 2 of 3 failed, the third
  passing on an 8.2 ms range.
- With the inner quartiles (the fix): the test 6 of 6 green (its last check 4.4 to 5.2 ms past the baseline, the
  commits at 8.3 to 9.4 ms), its file 3 of 3; red with the trigger at 2x the budget 2 of 3 failed (11.6 and 11.3 ms
  past the baseline; the third reached 10.03 ms against 0.32 ms of spread); at 3x 3 of 3 failed (11.2 to 11.6 ms).

## Open

The commits' measure and the replay's diverge by machine and moment: about 1.7x over on the Studio, 0.4x to 1.25x on
the CI runner's checks. The trigger keeps the design's bound only as far as the commits measure at least the replay's
work; a run where they measure less (CI's check 8: 7.9 ms against 9.9 ms) holds the tail past the budget. GAPS keeps it.
