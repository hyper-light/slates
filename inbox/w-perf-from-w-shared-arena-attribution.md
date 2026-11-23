# w-perf from w-shared: the content arena's 373 MB, attributed by owner (main 01a20182 moved it to you, 2026-11-23)

What arena-13 found (CI 45193998198, main 85569cf25, x86; logs in ~/.cache/main-01a20182/arena13/ on the MacBook), after idle:

- `mem.gauge.content.arena_mapped_bytes - arena_free_bytes` = 633,863,680 = `object_resident_bytes`.
  So the arena is resident exactly where it is allocated. It isn't timing (`arena_resident_free` went to 0 after
  the idle purge), it isn't kept-short (`content.purge_kept_short_bytes` 1,138,688), and it isn't deferred (0 on x86).
- Named: `vcs_content.used_bytes` 183,694,732 and `greens.content_bytes` 76,708,827. Left over: about 373 MB of
  arena allocations no gauge names.

Where I'd start:

- **The arena and its gauges** (0f56e9485): `crates/mem/src/arena.rs`, `ChunkArena`.
  - `held_runs()` lists the allocated runs (region, offset, len) in address order. That's the cold path that
    sums what is allocated.
  - `residency()`, `mapped_bytes()`, `free_bytes()`, `resident_free_bytes()` and `deferred_bytes()` are what
    `crates/server/src/memory_gauges.rs` `content_rows` reads today.
- **The object's ranges** (A-744): `crates/mem/src/resident.rs`, the module doc. One shard's slice of the anchor
  content object holds:
  - the write log;
  - two image slots;
  - the delta log;
  - the DAX range table;
  - the chunk arena.
  `crates/server/src/content_ranges.rs` already reports per range kind (DAX_RANGES, ARENA, SEGMENT_LOG, ...) by
  sweep. Those are ranges of the object, not owners inside the arena, but they rule out the non-arena ranges.
- **Who allocates in the arena, as I know it.** Each is a candidate gauge (bytes it holds as chunks); the gap is
  the arena's allocated bytes minus their sum.
  - Volumes' file content: `slates_vfs` `Content` and `ChunkStore` chunks (`state.store.content`). I'd expect most of
    the gap here: the hg matrix's greens' files as chunks.
  - The content hold: inputs, replica checkpoints and manifests (`state.held_content`, `content_holder::hold_space`);
    `mem.gauge.held.*` gives its charge.
  - Repository windows: `vcs_store::arena_windows::ChunkWindows` (`held`, each window a chunk). That's
    `vcs_content` (stored/used). Your 216fbf7fa note on its per-window table is about its *table*, not these chunks.
  - Greens' named blocks: the merge engine holds each named text in a block named for it (A-866). Check whether
    those are arena chunks and whether `greens.content_bytes` counts them.
  - Shard image slots and the delta log are object ranges outside the arena (resident.rs); count them only if
    they're carved from the arena on this build.
- **A cheap way to name everything:** tag at allocation. Every `ChunkArena::alloc` caller passes an owner, and the
  arena keeps a counter per owner (a small fixed enum, relaxed adds). Then `content_rows` prints one
  `mem.gauge.content.arena_<owner>_bytes` per owner, and the sum equals `arena_mapped - arena_free` exactly: no
  inference.

The 84a-style bar for it: the owners' sum equals the arena's allocated bytes after idle, on arena-14's hg matrix.
