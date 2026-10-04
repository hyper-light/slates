# vfs: a write, truncate or edit refused partway dropped the file's body

**Date:** 2026-10-03. **Design:** §4.5 (content), T-1.1 (a refused operation leaves nothing changed). **Found by:**
A-64's `PublishNeeded` test. A write refused for arena room left the arena holding 4 MiB more than its live and
deferred blocks, and the retried write read back wrong.

## Description

`Volume::apply_write` and `Volume::apply_truncate` take the inode's body out with `mem::replace(.., Body::None)`,
work on it, and put the result back. Every allocation in between returned through `?`. A refusal partway
therefore returned with the inode's body still `Body::None`:
- the file's content vanished (its size kept, its bytes reading as zeros);
- the blocks the attempt had allocated leaked.

The siblings, found in the sweep:
- `ChunkStore::shrink_open` allocated the truncated window's smaller block, and refused if it could not.
- `rebuilt_if_smaller` (a clipped extent rebuilt at its smaller block) refused if it could not allocate,
  partway through `clip_extents`' drain, which emptied the extent list.
- `Volume::open_window` removed a sealed extent before reopening it, and lost it if the reopen was refused.
- `ChunkStore::reopen` leaked its copy's block if the copy's write was refused.
- The edit verb truncated the file before writing its new bytes, so a refusal after the truncate left the file
  cut short.

## Root cause

The functions treated an allocation failure as impossible. The quota admitted the write's charge, and the
operation headroom kept room for in-flight copy-ups, so the arena was never supposed to run out mid-write. The
chunk slab was never accounted that way. A-64's deferred frees, which hold blocks until the next publication,
made arena exhaustion mid-write reachable too.

## Impact

Data loss of the file being written, truncated or edited, for any operation the arena or the chunk slab refused
after it began. Before A-64 this needed a full chunk slab or arena fragmentation, and no report reached it.
With A-64 it is reachable whenever deferred frees fill the arena before a publication.

## Exact edits

- `Volume::write_into` returns `Landed`: the body it leaves, always whole, the bytes written and the refusal
  that stopped it:
  - a seal or a growth refused keeps the open extent;
  - a window refused before opening keeps the sealed list.
- `Volume::apply_write` and `write_body` restore the original body on a refusal before any byte landed. A
  refusal after some windows landed is a short write, as POSIX `write(2)` reports one: `write_secured` sets the
  size, the times and the journal record from the bytes written.
- `Volume::write_room` checks, before anything changes, that the chunk slab and the arena can land a write
  whole: a block per window, one more for a growth, and a chunk record per window
  (`ChunkArena::can_allocate`, `Buddy::allocatable`). The whole-value writes use it: the overlay's copy-up
  (before the base body is replaced), the edit (before its truncate), and extended-attribute values. A base
  body has no open extent, so `write_base` checks the slab first.
- `shrink_open` and `rebuilt_if_smaller` treat the smaller block as an economy. When it cannot be had, the
  extent keeps its block, and its charge follows that block (`content_by_epoch`).
- `open_window` puts the sealed extent back when its reopen is refused, and `reopen` frees its copy's block when
  the copy is refused.

## Proof

`crates/vfs/tests/refused_partway.rs`:
- `a_write_refused_partway_keeps_the_windows_it_wrote`: a three-record chunk slab, a six-window write; a short
  write of whole windows, the rest the old content. It failed before the fix with `SlabFull` after the body was
  dropped.
- `a_truncate_in_a_full_arena_keeps_the_file`: it failed before with `ArenaExhausted`.
- `an_edit_without_room_is_refused_with_the_file_unchanged`: red with the room check removed (the file left at
  size 0).

`crates/vfs/tests/content_in_place.rs`
`an_arena_full_of_deferred_frees_refuses_publish_needed_until_a_publication_commits`: the short write, then
`PublishNeeded`, then the rest after a commit.

## Sibling sweep

Every `mem::replace(.., Body::None)` site in `crates/vfs/src`: `release_body` (frees the body, nothing to
restore), `apply_write` and `apply_truncate` (fixed). Every caller of `apply_write`: `write_secured` (short
write), the edit and the copy-up (whole, checked first). Every caller of `write_unrecorded`: the attribute
values (whole, checked first) and the AppleDouble working copy (a file write, short allowed).

Recorded, not fixed here: `build_recovered_volume` in `crates/server/src/verbs.rs` returns its error from
`volume.resize` without `discard_partial`, so a recovered volume refused for its size policy leaks its slots. Its
blocks are swept (A-64); its inode and directory records are not.
