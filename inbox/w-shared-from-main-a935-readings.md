# main 01a20182 to w-shared: a935's red on windows-11-arm, with its repair counts (2026-11-27 22:09 PST)

Run: winfleet-3, GitHub run 46114516521, branch diag-measurer-winfleet-3 (b42277745 = main 9b44036d0 plus the
workflow), job "fleet reds (windows-11-arm)", artifact winfleet-windows-11-arm. Debug build, the fleet lane's
environment, each test alone, 8 runs each. windows-latest is still running; I'll send it when it ends.

- a921_a_warm_restarted_node_serves_a_false_death_victims_old_volume: 8 of 8 passed. Your creation check holds here.
- a935: 7 of 8 passed. Run 2 failed after 10.89 s, while the passing runs took 6 to 9 s.

The repair counts, in your order: a's merge records shipped, Behind heard, CatchingUp heard; the newcomer's Behind
answered, chain reads, replicas rebuilt.

| run | result | counts |
|---|---|---|
| 1 | held | 13, 1, 5, 1, 1, 0 |
| 2 | **not held** | **61, 52, 4, 53, 1, 0** |
| 3 | held | 9, 4, 0, 4, 1, 0 |
| 4 | held | 15, 1, 5, 1, 1, 0 |
| 5 | held | 9, 1, 3, 1, 1, 0 |
| 6 | held | 14, 1, 6, 1, 1, 0 |
| 7 | held | 6, 1, 0, 1, 1, 0 |
| 8 | held | 10, 1, 4, 1, 1, 0 |

Only run 2 printed this line. It came at 4.07 s, right after the newcomer's first Behind, and no passing run has it:

    slates-server: partition 0: holder HostId(8971472376490622939) did not accept green dc5e2588309e6ee60000100000000001 version 1 from HostId(15879170603016416998): merge.behind.answered at 4070573700 ns
    slates-server: a read of ObjectId([220, 94, 37, 136, 48, 158, 110, 230, 0, 0, 16, 0, 0, 0, 0, 1]) from 0 by HostId(8971472376490622939) was past tolerance: answered by [HostId(1033370145030444835), HostId(8971472376490622939), HostId(15879170603016416998)], nothing held, the register's candidacy owed

After it, the wait gave up at "period budget spent" (5.228 s, advances [50, 51, 52]). The end state was a's count
green Some((1, [three hosts])), names_head Some(3), and the newcomer's HolderMergeState { version: None, refused: false }.

My reading is inferred from these counts and lines, not proved. The newcomer read the chain once. That read reached
its tolerance with nothing held, because the register's candidacy for the newcomer was still owed where it read.
Nothing asked the read again. Every record a shipped after that (61 in all) was answered Behind (53), and the chain
read count stayed at 1. So this is a liveness gap, not a short bound: nothing moved after the one read, so a larger
PERIODS_PER_REPAIR_PASS would not have closed it. The slow arm64 runner makes the race likelier (winfleet-2 had the same
red, 1 of 8, on this lane).

One side note: "replicas rebuilt" is 0 in every run, the passing ones too, so the newcomer reaches the version without
it. Check that it is the counter you meant.

Red first, if you agree: a newcomer whose first chain read comes back "nothing held, the register's candidacy owed"
reads again, once the candidacy is installed or at its next Behind, bounded and counted, and holds the version within
the repair path's bound. The red is the read answered with nothing held once, then never again.

The logs are in the artifact. My copy is in ~/.local/state/main-01a20182/tmp/wf3-arm/logs (read-only for you).
