# From w-engine: closes (CLOSE THE OPEN ITEMS)

## C2 A-752's two literals, derived (R3) -- CLOSED
- Commit on origin/main: 4b6d5c24b (merge of the R3 piece; base 89a1d6c06).
- What: the build's copy and hash units are the turn's `Slices` (`copy_slice_bytes_within` /
  `archive_slice_bytes_within`, from the shard's measured memcpy and BLAKE3 rates), never `UNIT_BYTES`; the sort's
  units are its own maximal non-decreasing runs, never `RUN = 16`. Checked on origin/main today (`git grep` finds
  neither constant in crates/merge or git_push.rs).
- Test: `cargo test -p slates-server --lib git_push` (t_a752: the update's build past_budget 0), 23 of 23 passed at
  base cd1261e43 (2026-11-20 17:45).

## C1 A-760, whole -- CLOSED
- Commit on origin/main: b1460a5e1 (landed in merge 4a9259ff5, 2026-11-20).
- What: its last owed piece, `Green::make_step`: a held verdict's files made a copy slice a step before the commit,
  so a splice into a large file never copies it in the commit's one step; and part 2's bug fixed (a whole run's buffer
  moved before the other files were made shifted every later run's bytes: a file spliced from a later run was made of
  the wrong bytes, silently). Not A-760's, named in GAPS: w-svn's landing calls make_step between judge and commit.
- Tests: `cargo test -p slates-merge --lib named` 25 of 25, including
  a_run_moved_into_its_file_leaves_later_runs_where_they_were (red first: "bytes" where "secon" belonged) and
  a_splice_into_a_large_file_is_made_a_slice_a_step_before_its_commit (1 MiB, 64 KiB slices: no step past its slice,
  the commit making no file); merge lib 248; server named_objects 1 (base 33465d641).

## Also landed: A-783 (pacing, the lever for 89b) -- ca496af8a in 4a9259ff5. BENCHMARKS "Paced work yields only when
someone waits". Finding for you: content_ranges' sweep test failed on origin/main 33465d641 itself, run with its
sibling (0 sweeps in 10 s, 240,139-249,996 idle slices, 2 of 2); it passes after your later content_ranges change.
