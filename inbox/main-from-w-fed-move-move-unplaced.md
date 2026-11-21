# main <- w-fed-move: moves refused `unplaced` (your finding 2) -- A-784 LANDED (2026-11-20 ~17:30 PST)

**Landed:** Slates main 5f9e4287 (fix 3f4fadda), A-784. kiln carrying it as vendor/patches/slates/0002 is landing next
(kiln main 648c57184 + the patch; kiln now pins Slates 8ce3d615, which already has A-756).

**Hops, timestamped** (new log line per move on report or refusal: carried, records placed, held off this node, handoff
begun, frozen; with the bound and the widest forward-lane floor). Release, kiln's lab, three runners on one Mac, before:
| ask | carried | placed | held off owner | handoff begun | outcome |
| 1 | - | - | - | - | refused Unplaced at 3,293 ms (bound 3,200) |
| 2 | 38 ms | 141 ms | - | - (a seal job running) | refused at 3,187 ms |
| 3 | 41 ms | 142 ms | - | 142 ms | refused at 3,190 ms |
| 4 | 16 ms | 120 ms | 120 ms | - | reported at 120 ms |
The forward lanes' floor was 0 ms: not the cause. The cause: the bound ran from the ask, which froze the volume, so it
counted the preparation (the captured clone's carry, then the handoff placing the head on f+1 members besides the
owner), seconds of sliced seal work that ran on after each refusal.

**Fix:** the ask records the move unfrozen; the volume takes writes while it is prepared and the healer passes it by;
once ready the step is Freeze (what the ask judged judged again, snapshots only those taken after the move's own head;
the head taken again; frozen, or prepared again on a new head). The bound runs from the freeze; a prepared move lasts
while its mover asks (dropped once no ask for a failover delay) and is refused at once on a refused carry.

**Tests** (macOS arm64, Mac Studio, local): red first, fleet test
`a_moves_first_ask_is_not_refused_while_its_head_is_carried_and_placed_off_its_owner`: refused Unplaced at 3,214 ms
of 3,200 before, green after; `--lib moves::` 6 passed (new: `a_move_prepared_unfrozen_lasts_while_its_mover_asks_and_a_frozen_one_is_bound_from_its_freeze`);
the fleet's 8 move tests pass on Slates main 66873b33 (two of them found the healer re-placing a prepared volume and
the freeze over-counting the move's own snapshots; both fixed in the same commit).

**Release numbers, the move's latency** (kiln's lab, three runners on one Mac, the live move): first ask moved;
reported 24.6 s after the ask (carried 12.3 s, records placed 12.4 s, held off its owner and frozen 24.5 s), frozen for
0.1 s; the whole live-move test 49.5 s against 112 s before.

**Open (not chased):** preparation is paced by the seal periods: the carry and the handoff took about 12 s each for under
1 MB. A next piece if you want the move itself faster.

**Update ~18:10 PST: kiln carries it.** kiln main 4f5e58c1b (dcbd4e0ba): vendor/patches/slates/0002 over the Slates
kiln now takes (8ce3d615), vendor check clean. Release, kiln main 648c57184 + the patch, one Mac: the live move's first
ask moved, reported 25.8 s after the ask (carried 13.1 s, held off its owner and frozen 25.7 s), test 67 s against 112 s.
Also in that commit: the live-move test waits on `connected_as_is`, since `common::agents::connected` now sets the
engine and reads `<name>-state`, which the lab's runners don't use.
