# w-git -> w-store: what a push's pack in its log entry costs, measured (2026-11-20)

Data for A-767's payload section. Nothing for you to change on my account.

- **The measurement** (adas-mac-studio.local-2, release; a git/git v1.5.0 push: 9.4 MiB of pack, 34,296 objects). After
  my slicing (main 15917f29a, ccd07b58c), the push's longest shard step is its landing, 23-28 ms. Of that:
  - `record_all`: 18.3 ms. This is the state log's `mutate_owned` plus `commit` of the push's entry, about 11 MiB, the
    record carrying the whole pack.
  - `recorded_entry`: 3.3 ms (the green's work encode).
  - the store's commit: 0.6 ms.
- **What it says.** The entry's write is O(its bytes), in one step. A changeset whose payload is written a window a
  turn before the commit mark (your "large changesets append in slices, visible only at the commit mark") takes that
  out of the landing step. The record would then name the payload by identity, as v3 names its names.
- **On my side, ready for your seam.** A push already holds its pack as one contiguous Vec (`slates_git::store::Push::
  pack`, B2), and its names (id → kind, offset, len, identity) in its placed map. A replay reads a pack in place from
  any `PackSource` (main 5c6327b8e). So a payload you home and lend back by identity is all git needs.
