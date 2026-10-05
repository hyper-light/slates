# A seal after an image turned its open extent into ciphertext

Date: 2026-10-05. Scope: A-99 (idle content sealed in RAM) with A-64 (content in place across a restart), §4.8
recovery.

## Symptom

Found by reasoning about the idle sweep A-99 owes, then reproduced: on a store that seals, write 1,000 bytes into a
file's open extent, publish an image (it names the open block as plaintext), write past the first chunk window (which
seals the first window), and restart from the surviving arena before the next publication. The recovered file's 1,000
bytes were the window's ciphertext, served as content with no error.

## Root cause

`ChunkStore::seal` encrypted the open block in place. Under A-64 a block's bytes survive a daemon restart in the
anchor's RAM and the last committed image says what they mean: here, a plaintext open extent. Rewriting a block an
image names changes its meaning before a new image is committed, the rule shadow paging exists to keep (ZFS and WAFL
never overwrite a block a committed tree references; the arena already refuses a plain free of such a block and defers
it, `Buddy::free_or_defer`). Every seal outside a publication was exposed: a write crossing a chunk window, a
copy-up, and the idle sweep about to be built.

## Impact

A daemon killed between a sealing write and the next publication recovered a file with garbage in place of
acknowledged bytes. Only daemons with a sealing root (A-99, since `7a75b32`, earlier today) were affected.

## Fix

- Failing test first: `a_seal_after_the_image_leaves_the_imaged_open_extent_readable_after_a_restart`
  (`crates/vfs/tests/content_in_place.rs`, with a deterministic stream cipher in the test). It read ciphertext; with
  in-place sealing forced it fails on the guard that the out-of-place path ran.
- `Buddy::imaged` and `ChunkArena::imaged`: whether the committed image, or one being published, may name a block.
- `ChunkStore::seal_where_safe`: a block no image names is sealed in place, as before (no extra memory on the common
  path); one an image names is copied to a new block, sealed there, and the old block freed through the deferral, so it
  keeps its plaintext until the next commit. With no room for the copy the chunk stays in the clear, counted as a
  refused seal. `ChunkStore::moved_out_of_image` counts the moves.

Edits: `crates/mem/src/{buddy,arena}.rs`, `crates/vfs/src/content.rs`, `crates/vfs/tests/content_in_place.rs`.

## Siblings

- A freed block keeps its bytes until reused: a deleted or truncated file's plaintext, and the old block of a move
  once its deferred free commits, stay readable in free arena memory. Owed next (scrub plaintext blocks when freed).
- Small files never cross a window, so their open extents stay plaintext until a snapshot (seen live: 109 chunks
  sealed after npm, pip and tar wrote thousands of files). Owed next (the idle sweep).
