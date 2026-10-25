# A seal the chunk slab refused left the file on a freed block

- **Date:** 2026-10-25
- **Status:** fixed 2026-10-25, in the same change as this record.
- **Subsystem:** the content store's seal of an open extent, `ChunkStore::seal` and `seal_where_safe` in
  `crates/vfs/src/content.rs`, under at-rest sealing (A-99) and content in place (A-64). The cold reseal
  (`ChunkStore::reseal`) shared the helper and the hazard.
- **Severity:** silent data corruption. A file's live record named a block the arena had freed, and the next file
  written took it. The refused seal also leaked the block it had sealed into.
- **Exposure:**
  - A daemon that seals (a cipher and a volume key) whose chunk slab is full when it seals an open extent named by the
    committed recovery image. The idle sweep (`Volume::seal_pending`, which the daemon runs) and a write's seal of its
    window both reach it.
  - The cold reseal's twin, a seal that failed after copying out of the image, is reached only through
    `Volume::seal_idle`, which the daemon does not call (it calls `seal_pending`), so it was latent.
- **Found by:** reading `seal_where_safe` while sweeping for siblings of A-170, then a failing test.

## Symptom

`crates/vfs/tests/content_in_place.rs`, `a_seal_the_chunk_slab_refuses_leaves_an_imaged_open_extent_whole`, before the
fix:

```
assertion `left == right` failed: a refused seal allocates and frees nothing
  left: 196608
 right: 163840
```

With that check reporting rather than asserting, the test's last check failed too: "the first file reads its own
bytes after the refused seal and the next publication".

## Root cause

When the block an open extent is about to seal into is named by the committed recovery image (A-64), the seal cannot
write ciphertext over it in place. So `seal_where_safe` copies the bytes into a new block, seals the copy, and frees
the old block, deferred until the next publication commits. It freed the old block before `seal` had recorded the
chunk that replaces it. When the chunk slab then refused the record, `seal` gave back only the seal's tags:
- the copy, holding the sealed bytes, was never freed;
- the caller put the file's open extent back, as the refusal promised ("a refused insert leaves the caller's open
  extent and its block exactly as they were", `trim_sealed`'s comment), but on a block already let go;
- the next publication's commit freed that block, and the next allocation took it while the file still read from it.

## Fix

- `seal_where_safe` no longer frees anything: it returns the copy and its seal.
- `seal` frees the old block only once the chunk that replaces it is recorded, and on a refused record gives back the
  seal's tags and the copy.
- `reseal` frees a chunk's old block once its record names the copy, and gives a copy back when the seal fails. Before,
  a failed seal left the record on the freed old block.

## Verification

- The failing test above fails before the fix (both checks) and passes after; the 17 tests of `content_in_place.rs`
  pass, among them the copy-out seal across a restart (`moved_out_of_image` still 1).
- The whole vfs suite (43 test binaries) and the server's recovery suite (41 tests) pass.
- Not yet tested: the reseal's failure after a copy (it needs the tag store full mid-reseal); the daemon does not run
  the cold reseal today.
