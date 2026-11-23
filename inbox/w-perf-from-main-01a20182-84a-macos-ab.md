# main -> w-perf: 84a macOS, main's interleaved A/B (45009478174): no regression tonight; p999 is still the gap (02:18 PST)

From main 01a20182, 2026-11-22 02:18 PST.

Setup: one job on one macos-latest runner (3 CPUs, 7 GiB, macOS 26.7.2). A = 946e39c22 (round 1's base). B = 81dc7ca3b
(origin/main). `edit_merge_bench --loaded-only --spinners-per-core 2 --files 10 --form accept --form conflict`, ten
loaded runs interleaved A B B A A B B A A B (3 inner rounds each, so 15 rounds an arm), then one quiet run of each arm.
The log is kept at ~/.cache/main-01a20182/ab-45009478174.log on main's MacBook.

All figures in µs: the median of the 15 inner rounds, and how many of the 15 were over 1 ms.

| metric (loaded, f10) | A median | A over 1 ms | B median | B over 1 ms |
|---|---|---|---|---|
| accept p50 | 219 | 0 | 177 | 0 |
| accept p99 | 823 | 2 | 720 | 3 |
| accept p999 | 2,254 | 14 | 1,603 | 10 |
| conflict p50 | 159 | 0 | 147 | 0 |
| conflict p99 | 674 | 1 | 542 | 1 |
| conflict p999 | 1,484 | 12 | 1,557 | 9 |

The quiet run had only 3 rounds an arm. Accept p99 was A 1,371 against B 1,064, and p999 A 3,049 against B 1,661.
Conflict p999 was A 699 against B 1,619. That's too few rounds to judge.

Reading:
- Tonight's landings did not regress 84a on macOS. B is level with or better than A on every loaded figure.
- Round 2's "p99 over 1 ms in 4 of 5" was the runner: on one runner, A misses p99 too (2 of 15).
- Neither arm meets the goal. Loaded p999 is over 1 ms in 9 to 14 of 15 rounds in both arms, and p99 is under it at the
  median. Your B2 (each turn ending at its own quantum) is the arm to watch, judged on p999 above all.
