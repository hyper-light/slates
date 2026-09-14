# Releasing a retained inode version freed chunks the head still reached

Status: **fixed** (branch `agent/admission`, 2026-09-13). Found while building the §4.2 retained-byte
charge (GAP-A9-1), whose first by-use test — "destroy the snapshot, the untouched window survives" —
failed on the unmodified tree.

## Description

A file with two chunk windows is snapshotted, then window 0 alone is overwritten. Destroying the
snapshot returns window 1 — never rewritten, still the head's — as zeros:

```
$ cargo test -p slates-vfs --test chunk_ownership destroying_a_snapshot_keeps_the_windows_the_head_still_reaches
window 1 must survive the snapshot's destroy (it was never rewritten): first bytes [0, 0, 0, 0, 0, 0, 0, 0]
```

The same history without any rewrite (a `chmod`, which copies the inode version up and shares every
window) lost both windows on the snapshot's destroy; and a copy-up after the *last* snapshot was
destroyed lost the whole file at once, with no snapshot involved at all. 4 of the 6 histories in
`crates/vfs/tests/chunk_ownership.rs` failed before the fix (2026-09-13, `cargo test -p slates-vfs
--test chunk_ownership`: "2 passed; 4 failed").

## Root cause

`Volume::make_current_inode` copies an inode version up by **cloning its body** — the new version's
sealed extents name the same chunk handles as the retired one — and then retires the old version
(`Dead::Inode`). `release_dead` (crates/vfs/src/volume.rs) freed a released inode version's chunks
unconditionally (`free_extents` over its sealed, open-sealed and base-pinned extents). So whenever a
retained version still shared a chunk with the head — every window the head had not rewritten since
the copy-up — releasing that version (at `destroy_snapshot`, or at once from `retire` when no
snapshot pinned it) freed the head's chunk. The head's extent then pointed at a vacated chunk slot:
`extent_bytes` found no chunk and the read filled zeros; a later allocation could reuse the slot and
the block, so the head would read another file's bytes.

The content module's header stated the rule the code assumed — "each chunk is referenced by exactly
one extent of one inode version" — which the copy-up has never satisfied. The chunk-level release
(`ChunkStore::release_chunk`, driven by `open_window`, `release_body` and `clip_extents`) already
implements the correct ownership: the head releases a chunk exactly once when it stops reaching it,
freeing it or listing it as its own `Dead::Chunk` by the epoch rule. The version-level free was a
second, wrong owner.

The model oracle (`crates/vfs/tests/model.rs`) did not catch it because its histories never destroy a
snapshot, and its whole-volume teardown frees everything regardless of order.

## Impact

Data loss in the head after any `destroy_snapshot` following a copy-up that left windows shared
(partial overwrite, `chmod`, `chown`, `utimens`, `link`, `rename` of a snapshotted file), and after any
copy-up once the last snapshot was gone. Silent: reads returned zeros, no refusal. Recovery images
were unaffected (a recovered snapshot's diverged file is a private copy), and whole-volume destroy was
unaffected (everything goes). No disk effect (R1): the bytes lost were RAM content the snapshot was
supposed to protect.

## Fix (applied)

- `crates/vfs/src/volume.rs` `release_dead`: `Dead::Inode` frees the version's slot and, for the
  head's own open extent, its block — never a sealed or pinned chunk. `free_extents` is gone.
- `Volume::destroy`: the head's walk lists each reachable version's chunks as their own
  `Dead::Chunk`s (`body_chunks`), each with its birth epoch, so a clone's cooperative step skips the
  windows it shares with its origin exactly as it skips shared nodes, and a whole-volume destroy still
  returns every block (asserted: `allocated_bytes() == 0`, `chunks() == 0`).
- `tree_deadlist_excluding` (the §4.8 recovery deadlist): a private version's chunks are listed as
  `Dead::Chunk`s beside it; a head-shared version stays the head's, chunks included. The recovery
  test `dropping_a_recovered_snapshot_frees_its_private_chunks_and_keeps_the_heads` pins the arena
  returning to exactly the head's two windows after the drop.
- `crates/vfs/src/content.rs` header states the real rule: a chunk may be referenced by several
  versions of one inode and is released exactly once, by the head.

After the fix: `cargo test -p slates-vfs --test chunk_ownership` — 6 passed; `cargo test -p slates-vfs`
— every file green (model 7 passed, recover 31, retention 3, clones 1, lifetime 9, base 10, edges 8).

## Sibling sweep

`free_chunk` has no caller outside `crates/vfs/src/volume.rs` and `content.rs`'s own tests;
`release_dead` is private to `volume.rs`; no other crate frees chunks. The deadlist migration in
`destroy_snapshot` and the `claimed` dedup in `recover::rebuild_deadlists` key on the chunk handle, so
listing chunks explicitly cannot double-free (a chunk sits on exactly one deadlist). The retained
version count (`retained_versions`) and its charge are unchanged: they count `Dead::Inode` entries,
which this fix neither adds nor removes.
