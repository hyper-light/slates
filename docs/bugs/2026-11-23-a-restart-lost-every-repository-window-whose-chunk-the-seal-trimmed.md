# A restart lost every repository window whose chunk the seal trimmed (2026-11-23, A-877, A-118, A-64, A-727 a3)

Found on Windows CI by w-win2's diagnostics run 45269168513 (windows-latest and windows-11-arm, main at c1da780ec's
successors). Two server tests failed there on both runners: `repository::store_tests::
a_batch_killed_between_its_commits_slices_recovers_and_resumes_to_the_same_history` and
`a_region_held_window_is_restored_beside_the_named_texts_brought_back`, each "recovered and folded: left None, right
Some((13, 13))". It is a restart data-loss defect on every platform, which only Windows' smaller granule showed.

## Description

A-877 (c1da780ec) gave the repository windows' image piece a version 2, which carries a window's slot in its chunk.
Its reader, `read_piece` in `crates/server/src/vcs_store/arena_windows.rs`, refused as `Malformed` any piece whose
chunk's bytes used were more than its block's length. That held for a slab, whose chunk is sealed over its whole
granule. It does not hold for a window of a granule or more, which takes a chunk of its own: the seal trims that chunk's
block to its length (A-118). The chunk's record then names the trimmed head, which can be shorter than the bytes used.
A 9 KiB window at a 4 KiB granule keeps an 8 KiB head and a 4 KiB piece after it, and the image names the head. A
recovery claims the head and the pieces that follow from the length (`ChunkStore::claim_chunk_blocks`), as the volumes'
restore always has. So every such window's piece was refused at a restart, counted in `vcs_store.windows_unrestored`,
and the window was left to the record replay.

## How a 16 KiB granule hid it

On the Mac the arena's granule is a 16 KiB page, and the slabs take every window shorter than that. In the two failing
tests only two windows were a granule or more, and the record replay put both again from the records that still hold
their bytes, so the green recovered and both tests passed. On Windows (and on Linux) the granule is a 4 KiB page, so
every window of 4 KiB or more was lost. The import's texts that the replay could not put back left the Subversion green
unrecovered ("left None"). Measured with a traced build of main (`DIAG restore piece 11 refused: Some(Malformed)` and
piece 12, at every restart of both tests on the Studio).

## Impact

At every restart since c1da780ec, each repository window in a trimmed chunk of its own was not restored from the shard
image. Where a record still held its bytes, the replay put it again, costing a second copy's work and nothing else.
Where none did, it was lost: a green whose recovery needed it did not recover. No volume content was affected, since the
volumes' restore reads its own pieces through `vfs::recover`, which handles trimmed chunks.

## Edits

- `read_piece` holds only the window's slot to the chunk's bytes (`at + len <= used`) and no longer refuses a used
  length past the head.
- A chunk of its own is no longer asked again after its seal or claim (`place`, `restore_slab`): `ChunkArena::reask`
  caps a block's asked bytes at its head, which a trimmed chunk's bytes pass, so the windows' row under-reported what it
  asked. Only a shared slab asks again, its windows' bytes.
- Red first: `a_restart_claims_each_slab_once_and_reads_its_windows_in_their_slots` gained a window of two and a half
  pages, a trimmed chunk of its own. It failed with two pieces refused, the old-version piece and the trimmed one. Now
  the old piece alone is refused, and every piece the restored windows encode equals its original.

## The sweep (main 01a20182's order)

Every reader of a window piece or a chunk record, for the same assumption:
- `arena_windows::read_piece`: the defect, fixed above.
- `arena_windows::restore_slab` and `place`: the re-ask, fixed above. `piece_block` (A-870's oracle) names a window's
  bytes from its block's offset and slot, which for a trimmed chunk runs through its pieces in order, as the chunk does.
- `vfs::recover` (the volumes' pieces): holds an extent to the chunk's used bytes and claims through
  `claim_chunk_blocks`. Correct.
- `ChunkStore::{claim_chunk_blocks, give_back_chunk_blocks, adopt_chunk, stored_bytes}`: each branches on a trimmed
  chunk. Correct.
- `slates_cluster::content::ContentHold`'s restore refuses `used > block_len`, which is right there: the hold allocates
  from the arena directly and never trims (no `trim` or seal in `crates/cluster/src/content.rs`).
