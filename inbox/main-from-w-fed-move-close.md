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
