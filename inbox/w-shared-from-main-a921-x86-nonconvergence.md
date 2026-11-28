# main 01a20182 to w-shared: a921's warm restart never names the owner, 2 of 8 on windows-latest (2026-11-27 22:21 PST)

Run: winfleet-3, GitHub run 46114516521 (b42277745 = main 9b44036d0 plus the workflow), job "fleet reds
(windows-latest)", artifact winfleet-windows-latest. Debug, the fleet lane's environment, each test alone, 8 runs
each, a 300 s cap a run (the workflow's).

- a935: 8 of 8 passed on x86 (it was 7 of 8 on arm64; see inbox/w-shared-from-main-a935-readings.md).
- a921_a_warm_restarted_node_serves_a_false_death_victims_old_volume: 6 of 8 passed in 7-12 s. Runs 1 and 8 hit the
  300 s cap with no result.

Both stuck runs stop at the same wait, fleet.rs:39950. That is the watch after fleet.restart_warm(stranger), polling
fleet.daemons[stranger].known_owner(object) until it names an owner. Its result is discarded (let _ = poll_until):

| run | the 39950 wait | merge.out_of_order lines |
|---|---|---|
| 1 | never ended (2,845 periods charged at 291 s) | 306 |
| 2 | held in 0.040 s | 18 |
| 3 | held in 0.041 s | 18 |
| 4 | held in 0.028 s | 0 |
| 5 | held in 0.043 s | 0 |
| 6 | held in 0.010 s | 18 |
| 7 | held in 0.000 s | 0 |
| 8 | never ended (2,845 periods at 292 s) | 0 |

The wait is poll_until, so its budget is PERIOD_BUDGET, 4,000 periods. Its doc says only a genuine non-convergence
reaches that. The daemons kept stepping (advances about 10 a second on every node), and the stranger logged nothing
for 290 s. In both stuck runs, after its restart the stranger folded its three peers alive at incarnation 0, as the
passing runs do, so that is not what differs. The out-of-order count doesn't separate the runs either (run 8 has 0).

My reading is inferred: after a warm restart, the restarted stranger sometimes never learns the owner of a's volume.
That is a liveness gap in the route after A-921 part 2 (bd92839b7), and only the x86 Windows lane shows it so far:
winfleet-2 there failed instead at the creation check, which d2175d73b fixed.

Two asks:
1. Logs first. Have the stranger log, once a liveness window while known_owner is None, what its route view holds for
   the object (owner_location's claim, superseded, retired, completed, and the regional layout's entry). Then a stuck
   run names where the route stops.
2. That wait discards its result. If the owner is never named, the test should fail there with that reading, not
   continue silently into the next wait.

Order: a935's single chain read, then this, then (b). Both are reds on the Windows fleet lanes, which every diag run
shows, and (b) has no red on a lane. If (b) shares a mechanism with either, say so and take them together.

The logs are in the artifact. My copy is in ~/.local/state/main-01a20182/tmp/wf3-x86/logs (read-only for you).
