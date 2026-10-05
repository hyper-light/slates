# Zero on free scrubbed the claims a refused recovery gave back

Date: 2026-10-05. Scope: A-99 zero on free (`42434b3`, the same day), A-64 recovery claims, §4.10 held replicas.

## Symptom

`slates-cluster` `a_damaged_hold_image_is_refused_and_a_whole_one_recovers` failed: after refused recoveries of
damaged hold images over a restarted arena, recovering the whole image found `BadMagic` where the archive bytes had
been. The test passes with the scrub disabled. My check after `42434b3` ran the mem and server suites but not the
cluster crate's, so it shipped.

## Root cause

A recovery claims the blocks its image names (their bytes survived the restart in anchor RAM) and, when it refuses,
gives the claims back through `ChunkArena::free`. Zero on free made that free scrub the block, so a refused recovery
destroyed the only copy of content a later recovery of the same image (a retry, or a whole image after a damaged one)
would have claimed. Giving back a claim is not freeing data: it returns the arena to how it was before the claim.

## Fix

- `ChunkArena::give_back`: a release that never scrubs; `free` keeps scrubbing. `ChunkStore::give_back_block` and
  `give_back_chunk` for the vfs side.
- Given back, not freed: the vfs `Claims::give_back` and the two refused adoptions in `claim_extent`; the cluster's
  `ClaimedHold::give_back`; and a refused hold rebuild's `abandon`, which forgets everything it adopted (a
  `giving_back` flag on `ContentHold` routes its releases through `give_back`).
- Still scrubbed: the sweep of blocks no recovered volume reaches, a block an image names that decodes as nothing, a
  verification scratch, and a volume whose recovery is refused (its content is discarded by design).

Edits: `crates/mem/src/arena.rs`, `crates/vfs/src/{content,recover}.rs`, `crates/cluster/src/content.rs`.
