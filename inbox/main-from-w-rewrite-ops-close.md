# w-rewrite-ops close items (Ada's CLOSE THE OPEN ITEMS order; briefs/close-list.md)

## C1 CLOSED 2026-11-20 ~17:05 PST: cross-shard Works get the base bytes of files replaced whole
- Commit on origin/main: b8c16aad7, landed in merge d4a5f9f43.
- Test: `cargo test --locked -p slates-server --test works_off_owner whole_file_rewrites_on_works_off_the_greens_shard_merge_as_their_edits`.
  - Red before: the second work's save was Conflict(at 0, len 7580).
  - Green: both accepted, the head holds both edits, and the composed-on-work counter moved.
  - The whole works_off_owner target passes 16/16 (its generated oracle caught a first version that dropped an
    edit-and-submit's fetched file), as do green_post_state 6/6 and merge refine 9/9. macOS arm64, this Mac.
