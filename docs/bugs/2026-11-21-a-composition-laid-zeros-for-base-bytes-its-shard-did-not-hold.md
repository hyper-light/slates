# A work's composition laid zeros for base bytes its shard did not hold (2026-11-21, w-fed-move, A-840)

## Description

On a daemon with more than one shard, a work whose green lives on another shard composes its own increment (A-247).
After a rebase, or a resolution, the owner answers each changed file as parts over the head (A-806): runs of the
work's file equal to the head's are named as the head's, the rest written. A run of the work's own write can equal
the head's (in the load harness every slot ends in the same dots). At the next submit the work's shard lays each
operation's bytes into the post-state through `WorkFiles::copy_range`, which answered `BaseGone` for that run, since
the shard holds no head bytes; `lay_op` filled the range with zeros and went on. The green accepted the increment and
published zeros where the work had written.

## Root cause

`lay_op` treated a failed copy as "contributes nothing" (`files.copy_range(..).ok().flatten() != Some(len)` then
`into.fill(0)`): the error was swallowed. The work's own read was right (a read fetches the head's pages), which hid it.

## Impact

Any work off its green's shard whose rebased file held a run of its own write equal to the head's. The read-and-rebase
loop rarely met it, since its writes after a rebase are fresh edits held whole; one-ask resolutions keep rebased writes,
and met it in every run of the 1,000-client stand-in (213-342 oracle failures a run). Found by the generated oracle
`one_ask_resolutions_land_exactly_what_the_works_wrote` in `crates/server/tests/merge_check.rs` (24 of 24 histories on
eight shards, 0 on one), cut down to `a_kept_slot_beside_a_given_up_one_lands_its_bytes`.

## Edits

- `crates/server/src/verbs.rs`: `lay_op`, `lay_ops` and `assemble_post_state` return `BaseGone`; `compose_increment`
  answers it `ContentUnavailable`, so the work's shard (`work_home::composed`) hands the owner its journal and files,
  which the owner composes over its head.
- `crates/server/src/refining.rs`: a sliced build that meets it gives the job up (the composition runs unrefined).
- `crates/server/src/work_home.rs`: `SealedWork::composes_here`, so the owner's answer to a work whose shard composes
  its own increments carries the base entries even when the owner composed this one (without it, every later submit
  asked the owner for the files: `a_resolution_the_owner_composed_leaves_the_work_its_entries`, red first).
