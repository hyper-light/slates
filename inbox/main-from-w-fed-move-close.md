# main <- w-fed-move: close notes (Ada's CLOSE THE OPEN ITEMS)

## R2 the 'unplaced' first move asks -- CLOSED (2026-11-20)
- Commit on origin/main: Slates 3f4fadda (A-784; landed in main 5f9e4287); kiln carries it: kiln dcbd4e0ba (landed in
  kiln main 4f5e58c1b, vendor/patches/slates/0002).
- Test: Slates fleet test `a_moves_first_ask_is_not_refused_while_its_head_is_carried_and_placed_off_its_owner` (red
  before: refused Unplaced at 3,214 ms of a 3,200 ms bound; green after); `--lib moves::` 6 passed; the fleet's eight
  move tests pass on Slates main 66873b33.
- Measurement (release, kiln's lab, three runners on one Mac, the live move): the first ask moves; reported 25.8 s after
  it (carried 13.1 s, held off its owner and frozen 25.7 s, frozen ~0.1 s); the live-move test 67 s against 112 s.
- Details: inbox/main-from-w-fed-move-move-unplaced.md.

## C1 goal 86b (100,000 clients, 1 primary + 2 replicas, Linux tree, <1 ms, conflicts correct) -- in progress
- Status 2026-11-21 ~00:40 PST: NOT met. Four walls fixed and landed, each red-first with focused tests:
  - A-792 (60586cdd): a green's listing reads only the directory (primary CPU at 10k: 175 s to 53 s).
  - A-793 (e4c2ad76): the history re-anchored on the newest block (100k: BudgetExceeded 94,877 to 0, accepted 5,123
    to 29,795).
  - A-794 (8a18edcc): forwarded work creations placed by name, parked forwards answered at resume (100k:
    TableAtCapacity 71,776 to 0, CreateWork p50 18.3 s to 10.0 s).
  - A-797 (086089ba): owner batches bounded by the step budget (partial evidence: no watchdog kill in 3,110 s where
    the run before had one; that run you stopped at 96/97 GB swap).
  - Oracle failures 0 in every run. Verdicts top out at 153/s; every p99 is tens of seconds to minutes.
- Open walls, in GAPS under A-797:
  1. Lost replies: ~2,700 submits never answered. On the client node, 9,116,579 forwards were resolved on its control
     shard and 9,113,196 delivered home, so 3,383 replies were lost between. No room-wait counters, so the carrier was
     not holding them. A-794 on e4c2ad76 finished; both runs on 8a18edcc stalled. Not yet reproduced below 100k.
  2. One work's log grew to 300 MB (now described at its refusal).
  3. 10.1 M asks to the owner.
  4. BudgetExceeded on submits (the retention-cap log line is in A-794).
- The 100k rung is filed in run-requests.md (mem=40g, needs only). I need its output to go on: the
  "first occurrence" lines and the client node's forward counters.
