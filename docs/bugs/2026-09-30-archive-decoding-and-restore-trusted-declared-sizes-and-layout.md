# Archive decoding and restore trusted declared sizes and layout

**Date:** 2026-09-30. **Area:** `slates-archive`, and its callers in `slates-server` and `slates-vfs`
(§4.11 D-17). **Audit:** AUD-29-13, AUD-29-14, AUD-29-15.

## Description

The archive decoder and restore acted on sizes and layout the archive declared:

- **Chunk sizes.** A chunk's `raw_len` was the decompressor's output capacity with no bound. A raw chunk's
  `raw_len` was never compared with its payload. The header's `chunk_max` was never checked.
- **Manifest decoding.** The decoder recursed without a depth bound and accepted trailing bytes. It accepted
  any UTF-8 name (`..`, `a/b`, NUL, repeats, unsorted), extents in any layout, and a recorded size unrelated
  to the extents. A directory's declared count reserved that many entries up front: a count as large as the
  remaining bytes, at about 100 bytes per entry, in every open directory.
- **Restore.** Restore appended every extent and never read `extent.offset`, so a one-byte extent at offset
  eight restored as one byte. It expanded holes into buffers and decoded the chunk again for every extent.
  It reconstructed the whole volume before the takeover successor reserved anything.

## Root cause

A valid archive hash or Merkle identity proves the bytes are what the sender produced, not that the sender
is honest: a sender computes its own checksums. The format had no admission layer between verifying and
acting.

## Impact

An authenticated peer could make a receiver:

- allocate up to a declared `raw_len` or a hole's length (an abort, since allocation failure aborts);
- overflow the stack with a deep tree;
- spend CPU decoding one chunk once per reference;
- restore bytes that differ from the manifest's own description, or overwrite one path with a duplicate.

## Fix

- **`format.rs`.** New constants:
  - `CHUNK_PAGES` (16): the content store's chunk rule, which `vfs::content::chunk_bytes` now reads.
  - `MAX_BASE_PAGE_BYTES` (64 KiB): the largest supported base page.
  - `MAX_CHUNK_BYTES` (1 MiB, derived from the two).

  New typed refusals: `ChunkMaxTooLarge`, `ChunkTooLarge`, `NonCanonicalChunk`, `TotalsMismatch`,
  `BadExtents`, `BadName`, `DuplicatePath`, `OverBudget`.
- **`archive.rs`.**
  - The header's `chunk_max` must be within the cap.
  - Each chunk's `raw_len` must be within `chunk_max` and in canonical form (`check_canonical_chunk`), both
    checked before its payload is read. `decode_payload` enforces the cap and form for any chunk, including
    one built in memory.
  - The header's raw and stored totals must equal the sums of the chunks.
- **`manifest.rs`.**
  - Decoding is exact and iterative: `TrailingBytes`, `BadName`, `Unordered`, `TooDeep`, `BadExtents`,
    `SizeMismatch`.
  - `valid_component` and `tiled_length` are shared with restore.
  - Counts are bounded by the remaining bytes at the item's smallest encoding: `EXTENT_BYTES` for extents,
    `MIN_ENTRY_BYTES` for entries. A directory reserves nothing ahead.
  - The depth bound counts path components (`MAX_DEPTH = PATH_MAX / 2`).
- **`restore.rs`.** `restore(archive, budget)`:
  1. An iterative plan: tiled lengths, valid names, unique paths, sizes matching the tiling.
  2. `OverBudget` before any allocation when every file's length plus the largest referenced chunk
     exceeds the budget.
  3. Fallible allocation.
  4. Each chunk decoded once and copied into every extent at its file offset. `Restored::chunks_decoded`
     is the non-vacuity counter.
- **`server/verbs.rs`.** `materialize_taken_over` restores within the volume's bound (or dynamic maximum)
  capped by the shard's admittable bytes. `OverBudget` becomes `BudgetExceeded` before any volume exists.
- **`server/merge_service.rs`.**
  - `inputs_archive` cuts the inputs into chunks of at most the page it declares as `chunk_max`. They had
    been one chunk past that declaration.
  - `held_inputs` restores within the chunk bytes its hold admitted.

## Tests

- `crates/archive/tests/archive.rs`: `hostile_chunk_sizes_are_refused_before_decoding` covers a maximal
  `raw_len`, a `raw_len` past the header, a lying raw length, an empty chunk, and a `chunk_max` past the
  cap.
- `crates/archive/tests/manifest.rs`:
  - `non_canonical_encodings_are_refused_typed`;
  - `a_tree_past_the_depth_bound_is_refused`, which found the decoder off by one: it counted the root
    against the bound;
  - `extents_that_do_not_tile_are_refused`.
- `crates/archive/tests/restore.rs`:
  - `a_huge_hole_is_refused_before_allocation` (a terabyte);
  - `a_chunk_named_many_times_is_decoded_once_and_its_expansion_is_admitted` (256 references, one decode,
    refused one byte short);
  - `a_non_canonical_tree_is_refused_typed`;
  - `a_tree_at_the_depth_bound_restores`, which encodes, decodes and restores at 2,048 levels.
- `crates/server/src/verbs.rs`: `a_taken_over_archive_lands_at_its_offsets_and_an_oversized_one_is_refused`
  runs through the successor's real materialization path.
- Fixtures now record canonical sizes. The manifest golden vector was re-pinned over the canonical sample;
  the encoding itself is unchanged.

## Sibling sweep

- **A volume nested deeper than 2,048 levels.** The VFS holds it and the exporter walks it, but every
  reader refuses the archive `TooDeep`. `Node`'s encode, identity and compiler-generated drop are
  recursive; a 100,000-deep in-memory tree overflowed the test's stack in the fixture helper. Raising the
  bound needs iterative forms and a memory-derived bound. Recorded in GAPS.
- **Restore output is dense** (AUD-29-57, open).
- **`Archive::chunk_by_identity`** now also admits against the header's `chunk_max`.
