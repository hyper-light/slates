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
