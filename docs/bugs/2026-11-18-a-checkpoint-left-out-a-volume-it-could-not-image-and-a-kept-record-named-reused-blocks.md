# A checkpoint left out a volume it could not image, and a kept record named reused blocks (2026-11-18, §4.8, A-64, A-68, A-430)

## Description

A publication that cannot image a volume leaves it out: an overlay whose base host is away (a landing holds it), or
whose image refuses. The two kinds of publication treated it differently.

- **A delta** kept the volume's committed record (`publish_delta_into`: "a skipped volume keeps its committed record,
  so it stays present"), so a recovery rebuilt the volume as of its last capture.
- **A checkpoint** dropped it (`stream_checkpoint` left it out of the image entirely). A recovery from that checkpoint
  found no image for the volume and refused it (`NoImage`) until a later publication captured it. Yet the volume's
  content had been acknowledged durable at earlier publications.

Reading the delta path for the fix found a worse fault in it. A-64 defers the free of a block the committed image names
until a newer image commits, and each commit releases the deferred blocks its own capture does not name. A left-out
volume's blocks freed since its record was taken (a file rewritten, say) are exactly such blocks. So the first commit
that left the volume out handed them out again while the record it kept still named them. A recovery would then:
- rebuild the volume's file over whatever took the block next, when that block was named the same way (Claims takes a
  block two images name alike for a shared one); or
- refuse the whole shard (`RecoveryIncomplete`), when it was named two ways.

Found by reading on 2026-11-18 (g268, A-422's sibling sweep); then each half was proven by a test below.

## Proof

- `a_streamed_checkpoint_restarts_without_a_volume_it_cannot_capture` (`crates/server/src/verbs.rs`, now asserting the
  carried record): a 2,000-file volume and an overlay whose host is taken, checkpointed. The committed image must hold
  the overlay's record as a recovery would have rebuilt it before the checkpoint (`Journal::recover`). Before the fix
  it held the plain volume alone.
- `a_left_out_volumes_kept_record_keeps_the_blocks_it_names` (same file): an overlay's file of two chunks,
  published; the file cut to nothing (its blocks freed, deferred); the overlay left out of the next publication; then
  captured. The freed blocks must stay deferred across the publication that left the overlay out, and be released by
  the one that captures it. Before the fix they were released at once. (A rewrite in place would not do for this test:
  an overwrite within a published length lands in the file's open extent and frees nothing.)
- Red-checked once on a Mac Studio (2026-11-18T20:51Z, debug), each half with its part of the fix removed:
  - with no record carried, the committed image lacked the overlay's;
  - with no deferred block kept, the publication that left the overlay out released the file's two chunks
    (`left: 0, right: 524288` deferred bytes).

## Fix (exact edits)

- `crates/mem`: `Buddy::capture_deferred` and `ChunkArena::capture_deferred` add every deferred block to the open
  capture, so the commit keeps them.
- `crates/vfs`: `checkpoint_log::committed_volume` reads one volume's record from the committed checkpoint
  (`ShardImage::committed_volume`, one volume at a time, stopped past its key). It folds that record with the
  volume's records in the committed deltas, as `Journal::recover` folds every volume's.
- `crates/server/src/verbs.rs`:
  - When an attempt cannot image a volume the committed image holds, `publish_checkpoint` reads that volume's record
    (`carry_record`), and the restarted attempt writes it in the volume's place (`stream_checkpoint`).
    `published_keys` counts the carried volume.
  - `publish_shard` keeps every deferred block when the publication kept a left-out record
    (`keep_left_out_records_blocks`).
  - `relieve_deferred` publishes nothing while such blocks are kept (`ShardState::deferred_kept`): it could release
    none of them.
- Tests: `a_commit_that_keeps_the_deferred_blocks_hands_none_of_them_out` and the deferral model's keeping commits
  (mem); `a_volumes_committed_record_is_what_a_recovery_rebuilds_for_it` (vfs, against `Journal::recover`); the two
  server tests above.

## What it costs, and what remains

Keeping every deferred block is a superset of the blocks the kept records name: correct, with no per-chunk bookkeeping.
But while a volume stays left out, no freed block on its shard is reclaimed, and operations the arena cannot fit meet
`PublishNeeded` (EAGAIN on a FUSE mount) rather than a relief publication. The precise set, the kept records' own
blocks with A-118's trimmed pieces, is owed in GAPS if a lasting left-out volume ever shows in
`arena.deferred_kept_for_left_out`.
