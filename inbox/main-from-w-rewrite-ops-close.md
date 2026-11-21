# w-rewrite-ops close items (Ada's CLOSE THE OPEN ITEMS order; briefs/close-list.md)

## C1 CLOSED 2026-11-20 ~17:05 PST: cross-shard Works get the base bytes of files replaced whole
- Commit on origin/main: b8c16aad7, landed in merge d4a5f9f43.
- Test: `cargo test --locked -p slates-server --test works_off_owner whole_file_rewrites_on_works_off_the_greens_shard_merge_as_their_edits`.
  - Red before: the second work's save was Conflict(at 0, len 7580).
  - Green: both accepted, the head holds both edits, and the composed-on-work counter moved.
  - The whole works_off_owner target passes 16/16 (its generated oracle caught a first version that dropped an
    edit-and-submit's fetched file), as do green_post_state 6/6 and merge refine 9/9. macOS arm64, this Mac.

## C2 CLOSED 2026-11-20 ~17:40 PST: a rename over an existing file is refined
- Commit on origin/main: 95e88882f, landed in merge 7998a7356.
- Tests, each red first:
  - `cargo test --locked -p slates-server --test green_post_state a_rewrite_renamed_over_another_file_merges_with_an_edit_of_its_source`.
    Red: Conflict(at 0, len 7580). Green: both accepted, `b` holds both edits, `a` gone.
  - `cargo test --locked -p slates-merge --lib refine::tests::a_rewrite_renamed_over_another_file_is_refined_against_its_source`.
  - `refine::tests::a_refined_rewrite_keeps_its_paths_other_operations`. The A-776 seam dropped every non-content op of
    a refined path (a rename: TypeChanged; a mode change: lost). It blocked C2, so it was fixed here.
  - Totals: refine 11/11, green_post_state 7/7, works_off_owner 16/16. macOS arm64, this Mac.
- Recorded in GAPS D-O17, not worked: the backup-then-recreate save (rename `f` to `f~`, create `f`) is refused by the
  deriver, `Unsupported("f")`.

## R3 CLOSED 2026-11-20 ~19:10 PST: the refine runs in slices under the shard's quantum, its worst case bounded
- Commit on origin/main: 2a498b90f (the piece), with merges 22da94920 (main's lint round 10 had split refine.rs into
  phases; the piece was grafted onto it) and d86bd3cdb (A-790: the job reads the work's file over its base, since a
  changed file may now be pieces).
- What it does: a submit or rebase whose composition replaces a file whole never refines inside the verb. It parks: a
  local verb on the job's own list, a work off its green's shard at its gate. A job of the shard then:
  - copies the two files a slice a turn;
  - refines a Pace turn at a time, each held to half the step quantum;
  - frees what it held a buffer a turn.
  The verb then runs again and composes with the kept result.
- Test: `cargo test --locked -p slates-server --test green_post_state a_large_rewrite_is_refined_over_many_turns_off_the_submit`.
  Red with no job: a whole-file conflict over all 2 MiB. Green: accepted, the head holds the edit, and the refinement
  ran over tens of thousands of turns.
- Measurement (release, adas-mac-studio-4 M4 Max, load 3-4, three runs a size): 2 MiB longest turn 54-69 µs, 8 MiB
  109-311 µs. Before, the whole refinement was one step of the submit: about 19 ms for a 300 KiB file, about 185 ms at
  12 MiB.
- Other targets: green_post_state 8/8, works_off_owner 16/16, work_edit_cow 1/1, merge refine 11/11.
- Found and recorded in GAPS D-O17, not worked (close order):
  - The tail follows the largest single free; deferring it to idle waits on A-783.
  - `pace::sort_by` (A-752) grows by doubling, so one push copies every item before it; `canonicalize_paced` uses it.
  - A remote edit-and-submit composes unrefined.
  - An edit-and-submit retried while parked re-applies its edit.
  - The rest of a submit is still one step.
  - **For main:** lint round 10's reshaping of refine.rs costs 1.1-1.3x per save (12 MiB 234-236 ms against 181-184
    ms, M4 Max, interleaved). The likely cause is the byte-class table replaced by comparisons.

w-rewrite-ops' close list (C1, C2, R3) is done.
