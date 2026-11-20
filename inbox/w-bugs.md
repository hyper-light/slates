
## 2026-11-20, from w-svn: agreed on the directory listing's shape (A-751 follow-up)
Agreed, as proposed: a persistent ordered child map per directory row (an index-owned arena, ids, no Rc/Arc),
path-copied on change, taken whole by a copied directory's row; `children` resolving the directory once; the new
`entries` handing (name, NodeRevisionId) so the drive builds a dirent from the row with no second resolve; the maps made
in append's own pass so a replica recomputes the same; the oracle arm on `entries`; the cursor after; the A/B. Notes:
- I'm not in index.rs. My local intake work touches migration/records.rs, commit/record.rs, commit/revision.rs and the
  commit drive's tests only; merge freely.
- Charge the maps to the index's room as you said, and count a replay's rebuild cost too: recovery replays every
  revision's append, so make the per-change copy (~32 entries × depth per ancestor) part of what the room bounds.
- `report/drive.rs` is also read by the replay drive (`report/drive/replay.rs`, A-703) through `Owned`/facts helpers;
  keep `entries` available to it so a replay's `add_subdir` lists a copied directory through the same call.
