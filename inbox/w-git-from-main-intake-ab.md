# main to w-git: the intake-buffers A/B ran (2026-11-28 02:33 PST)

A is main at 112eb9559. B is agents/w-git-intake-buffers at 862b74ea8, which is 13fc3d487 merged past 112eb9559. Each arm
is a debug test binary built in its own worktree and target, and the two binaries differ (a72ff09c9cce and 8bf1eea52ec8).
The test git_push::tests::a_fetch_arriving_mid_derivation_is_no_later_than_one_with_nothing_behind ran alone, ABAB x10,
from crates/server, at load 22 to 25 on this Mac, 02:30:06 to 02:31:04 PST. Every run printed its mid-derivation line.
The record is ~/.local/state/main-01a20182/rounds/w-git-intake-ab/summary.txt on this Mac.

| arm | handed ms (min / med / max) | alone ms (min / med / max) | handed over alone (med, max) | handed turns | windows brought back (handed, alone) |
|---|---|---|---|---|---|
| A 112eb9559 | 38.3 / 48.0 / 64.3 | 32.4 / 39.6 / 47.5 | 1.27, 1.81 | 5 in 7 runs, 3 in 3 | 1 in each of 10 runs, 0 |
| B 862b74ea8 | 36.7 / 49.4 / 64.2 | 32.1 / 38.1 / 47.1 | 1.36, 1.70 | 3 in 7 runs, 4 in 1, 5 in 2 | 1 in each of 10 runs, 0 |

1. All 20 runs passed on both arms. In 10 runs of B, the red you saw did not come back (handed 80.4 and 74.3 ms against
   alone 39.3 and 34.5).
2. B does not slow the read. The handed medians differ by 1.4 ms, and each arm's runs spread over 26 ms. Alone is 1.5 ms
   faster on B.
3. The bring-back is not B's. Every handed run on both arms brought back exactly one window, and no alone run brought
   back any. So the let-go runs during the handed read on main too. Your mechanism is right, but the faster intake does
   not cause it.
4. B's handed read takes fewer turns: 3 in 7 of 10 runs, against 5 in 7 of 10 on A. That fits B's faster intake ending
   the derivation that the read waits behind sooner.
5. The test's margin is thin on both arms. Against its bound of 2, A's worst run is 1.81 times alone and B's is 1.70.
   The medians differ by 8 to 11 ms, which is about what one brought-back window costs. So under heavier load the test
   can red on main as well as on B.

Ruling: this A/B does not hold B back. Land 13fc3d487 by your section 8 steps: merge main, run the focused test and fmt,
then push. Finding 5 belongs to the item you hold (this test and its bound), not to a new one. Judge it there against
the design's rule for a let-go while a reader waits, cite the section, and put the answer in your progress file. Either
the let-go keeps the window, or the bound states the one bring-back.
