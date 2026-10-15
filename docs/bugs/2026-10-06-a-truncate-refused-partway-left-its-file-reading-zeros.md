# A truncate refused partway left its file reading zeros

**Found:** 2026-10-06, by review after the 2026-10-03 fix
(`2026-10-03-a-write-or-truncate-refused-partway-dropped-the-files-body.md`). A sibling sweep of every place that
takes an inode's body out found `apply_truncate`'s releases still returning early.

## Description

`apply_truncate` takes the body out of the inode, cuts it, and puts the result back. The 2026-10-03 fix made a
refused smaller block keep the larger one. But three `?` returns inside the cut remained: the releases in
`clip_extents`, `release_open` and `shrink_open`. Each returned the refusal with the body still taken out.

The inode was left with `Body::None` and its old size, so every read returned zeros. `clip_extents` also drained the
extents it had not reached, so their chunks were leaked as well as unreadable.

A refusal there means a chunk record can no longer be resolved (a stale handle), or the arena or tag store refuses a
free. The test reproduces the first by freeing the middle window's chunk record of a three-window file, then cutting
into the first window. Before the fix, the truncate refused and the first window then read as zeros.

The same test exposed a second defect in the read path. `read_body` and `read_in` resolved the chunk of every extent
in the file before testing whether it overlapped the read. So one unresolvable chunk anywhere refused every read of
the file, and each read cost O(extents). A 4 KiB read of a 512 MiB file took 22.2 µs.

## Root cause

- The truncate mutated before it validated: the releases came first, and the body went back only on success.
- The reads tested overlap after the chunk lookup, and never used the extents' order.

## Fix

- **Guard, then apply** (the shape of `slates-db`'s partition, `crates/db/src/partition.rs`).
  - `apply_truncate` first checks that every chunk the cut releases or clips resolves (`truncated_chunks`,
    `ChunkStore::check_chunk`). A stale chunk refuses the truncate before anything changes, as a failed
    `ftruncate(2)` leaves a file.
  - The cut (`cut_body`) always returns a whole body. `clip_extents` now keeps the extent that refused and every one
    it had not reached. The open extent's block is freed through its handle, so the extent survives a refusal.
  - The body goes back, the dead list is kept, and the epoch histogram is reconciled on every path before a refusal
    is returned.
- **Reads touch only the extents they overlap.** `overlapping` binary-searches a body's extents, which every body
  keeps ascending and non-overlapping (`Body::Sealed`, `Body::Open`, `BaseBody::pinned`).

## Impact

- **Truncate.** In a consistent store no refusal reaches the cut any more. In an inconsistent one, the file keeps its
  content instead of reading as zeros.
- **Reads.** Random 4 KiB reads of one file, release build, best of 5, 2026-10-06, this Mac (M5 Max):

  | File | Before | After |
  |---|---|---|
  | 16 MiB | 953 ns | 223 ns |
  | 128 MiB | 5,624 ns | 383 ns |
  | 512 MiB | 22,225 ns | 514 ns |

## Tests

- `crates/vfs/tests/refused_partway.rs` `a_truncate_refused_by_a_stale_chunk_keeps_the_file`. It failed before the
  fix: the first window read as zeros, and then the read itself refused.
- The model test's generated reads hold `overlapping` against the map model.

## Siblings

`release_body` takes the body out of an inode being removed and returns a release's refusal without putting it back.
That is intended, since the inode is leaving, but its unreleased chunks then leak. It is left as it is and recorded
here.
