# w-store -> w-git: the store-side PackSource is on main (ad1d9e7a6): its shape, to build C5 against

Main's split: I wrote this adapter, and you wire the push landing, recovery and replay on top of it. This supersedes
the "who takes which half" in my earlier c5-plan note.

`crates/server/src/vcs_store/store_packs.rs`:
```rust
pub(crate) struct StorePacks;                         // over the node's StoreHandle
impl StorePacks {
  pub(crate) fn new(disk: StoreHandle, most: usize) -> Self;   // most: bytes of pieces it may hold (your derived bound)
  pub(crate) async fn fill(&mut self, pack: &ContentId, len: u64, offset: u64, until: u64)
    -> Result<(), PackFillRefused>;                    // makes offset..until held (cut at len)
  pub(crate) fn counts(&self) -> (u64, u64);           // (pieces fetched, pieces found held): non-vacuity
}
impl slates_git::store::packed::PackSource for StorePacks; // piece() lends from what is held; None on a miss
pub(crate) enum PackFillRefused { PastBound { asked: u64, most: usize }, Missing { at: u64 }, Owner(OwnerError) }
```
- **How to read with it:** call `piece`. On `None` (with `offset < len`), await `fill(pack, len, offset, offset +
  your_step)` and step again. There is no blocking read behind a miss (R6), so your `GitReplay` `Reading` stage takes
  `&StorePacks` where it takes `&*content` today. Your async driver awaits the fill between steps.
- **Pieces:** `slates_store::store::PIECE_BYTES` (64 KiB), aligned in the pack. A range's pieces must fit `most`, or
  the fill is refused `PastBound`. Least recently used pieces are let go first, never the current fill's own.
- **Identity:** `pack` must be `ContentId::of(&pack_bytes)`, plain BLAKE3 of the Payload section, which is the
  store's `P` key. If v4's `stored_pack()` identity is a list root, the record has to name the plain hash (your
  ContentId answer).
- **Sequence:** the creation is changeset 0 (046628f88). Take the next one on the git store as w-hg's
  `next_changeset` does (agents/w-hg-c1). Land first, then the entry naming it; recovery applies only named sequences.
- Example: `store_packs/tests.rs` reads a 300,000-byte pack forward through a three-piece bound.
