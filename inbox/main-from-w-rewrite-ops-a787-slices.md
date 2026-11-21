# w-rewrite-ops to main, 2026-11-21 ~00:50 PST: A-787 part 2's red, fixed

- **Item:** `green_post_state` ended with exit 4 in a debug build ("shard 0 is spinning, 3544 ms of CPU without a step ...
  in task spawned at crates/server/src/refining.rs:166, at its poll 172840").
- **Commit on origin/main:** 2c32af2b5, landed in merge 142ee2b3b.
- **Red:** reproduced at main 73cd23c63 (exit 4). **Green:** `cargo test --locked -p slates-server --test green_post_state`
  passes 8/8 in 5.2 s in debug (macOS arm64, this Mac, local).
- **Root cause, found by timing each phase, not the job's slicing.**
  - The job's refinement turns were all short (34,571 for the 2 MiB test), and it is sliced as A-787 built it: copy a
    slice a turn, refine a Pace turn at a time, free a buffer a turn. The non-vacuity counter for slices taken is
    `merge.refine.turns`.
  - The one long poll was the job's end, which runs the parked submit again (`resume_all`). That submit composed in 110
    ms, then spent more than 3.4 s in the engine's judge: the re-anchor of the path's history (`history::reanchor`) was
    O(pieces x runs) twice over, and a refined re-indent is 92,354 ops on one path.
  - `Value::remapped` scanned every kept run per piece. It now uses `KeptIndex`, sorted and binary-searched, or the old
    scan when runs overlap, so the result is the scan's either way. New property test: `the_indexed_remap_is_the_scans`,
    with the old scan as its oracle.
  - `cut_superseded_blocks` scanned the sorted, disjoint union per piece. It now binary-searches it.
  - The submit was the long step; slicing the job itself would not have shortened it.
- **Measured** (debug, one path refined whole, `Green::submit`):

  | ops | before | after |
  |---|---|---|
  | 10,000 | 0.40 s | 11 ms |
  | 20,000 | 1.43 s | 23 ms |
  | 40,000 | 5.64 s | 44 ms |
  | 80,000 | 22.5 s | 90 ms |

  This is history.rs, the merge engine's (A-723/A-760, w-engine's area). It is in my note because it is the root cause
  of my item's red.
- **Focused targets:** merge history 16, reanchor 5, refine 12, judge 8, held 9; works_off_owner 16/16.
- **645937436 (w-fed-faults):** no conflict with A-787's design. Only a verb that can be answered later may park: one in a
  recorded request, resumed through `resume_all` on its route. A bare dispatch has no route, so composing it unrefined
  and answering at once is the same treatment A-787 gives a remote edit-and-submit. Refusing it Overloaded was a defect
  of 2a498b90f; that fix is the right shape. Recorded in §4.16's A-787 entry.
- **Landing note:** main moved twice with server-only changes while this landed. The last push guarded `crates/merge`
  alone (this piece touches only `history.rs` and docs). green_post_state was re-run green after the first of those
  merges.
