# §4.8 Content recovery — daemon-restart survival of bytes and roots (design draft, to ratify)

> Status: **design draft, awaiting ratification** (2026-09-08). This is the design section for
> closing GAP-A9-6 / BUG-11 (`docs/wip/GAPS.md`): a daemon restart today rebuilds scratch volumes
> empty and drops snapshots — acknowledged content loss, not content recovery (SLATES_DESIGN.md §4.2
> status: "rebuilding scratch volumes empty and dropping snapshots is acknowledged content loss
> (BUG-11) … Anchor-owned bytes/roots … are required before the design's restart-survival promise
> can be offered"). It realizes the durability the design states — **D-18** ("daemon-restart survival
> only when bytes, roots, witnesses, accounting and completion records are recoverable from
> anchor-owned RAM (§4.8)") and the **A-2 amendment** (live working state owner-local, made durable
> by auto-sealing into snapshots). **Most of the machinery already exists** (see §2); what is owed is
> the daemon integration and one design decision (per-shard partitioning, §5) — so this is Ada's to
> ratify, then a bounded implementation, not a new subsystem.

## 1. What is lost, and what is by design

Three state classes, three mechanisms (A-2). Only the middle one is the gap.

| State | Durability mechanism | Status |
|---|---|---|
| **Metadata** — roots, registers, witnesses, accounting, completion records | The db partition log in the anchor segment, replayed on restart. | **Works** — `crates/server` `rebuild_recovered`/`recover_images`/`rebuild_volume`; the daemon-restart test passes. |
| **Sealed content** — the chunk bytes a snapshot's extents reference | Must be **recoverable from anchor-owned RAM** (D-18). | **The gap (BUG-11)** — but the mechanism exists; only the daemon wiring does not (§2, §3). |
| **Unsealed live scratch** — open extents not yet sealed | Owner-local RAM, lost within the volume's **stated loss window** (A-2: auto-seal bounds it). | By design — the boundary of this work, not a bug. |

So "content recovery" is precisely: **the sealed chunk bytes a recovered snapshot references must still be in anchor-owned RAM after a daemon restart, and their identities must validate.**

## 2. What already exists (the surprise on inspection)

The persistence spine is built and, at the unit level, proven:

- **`slates_mem::SharedObject`** (`crates/mem/src/shared.rs`) — a real shared-memory object (`memfd_create` on Linux, `shm_open` on macOS, a pagefile-backed section on Windows; **unprivileged**, so R10 holds; RAM, so R1 holds) with `create`/`open(handoff)`/`handoff`/byte and atomic accessors/`lock`.
- **`slates_mem::region::Region::shared(object, page)`** — a chunk-arena region backed by a `SharedObject`. The arena **addresses bytes by extent `(region, offset, len)`, never by a raw pointer** (`crates/vfs/src/content.rs`), so the same bytes are found regardless of the address the object maps at in a given process — the property that makes content relocatable across daemon incarnations. The mapping uses `memmap2`'s safe wrappers, so this adds **no unsafe on Unix**.
- **Proven at the seam** (`crates/mem/tests/shared_region.rs`): `an_arena_over_a_shared_region_allocates_and_serves_bytes` and `content_in_a_shared_region_survives_the_writing_mapping_being_dropped` — the latter drops the writing mapping, re-`open`s the object from its handoff, rebuilds a `Region::shared` over it, and reads the bytes back. **That is a daemon restart at the memory level, already green.**
- **`slates_anchor::AnchorSegment`** already creates a content object (`with_content`), hands it off (`content_handoff`), re-adopts it on attach (`adopt_content`), and exposes it (`content()`). The daemon's handoff path (`crates/server/src/daemon.rs` ~114) already re-opens the content object into the segment on restart.

## 3. The one gap: the daemon builds the arena over private memory

`init_shard` (`crates/server/src/daemon.rs` ~282) builds each shard's chunk arena over `Region::map(...)` — an **anonymous private mapping**, daemon-local, gone at exit — instead of over the segment's anchor-owned content object:

```rust
let mut arena = ChunkArena::new(config.page);
arena.add_region(Region::map(region_len, config.page, config.huge_pages)?)?;  // Anon → lost on restart
```

So every chunk byte is written into memory that dies with the daemon, and recovery has nothing to point its (correctly replayed) extent maps at. The fix is to build the arena over `Region::shared(<the shard's slice of the segment's content object>)`, and to add the recovery-time validation. That is the whole of the owed work.

## 4. Integration and recovery

1. **Plumb the content object to each shard.** `init_shard` runs per shard and attaches the segment; the content object (or its handoff) must reach each shard so it can build `Region::shared` over its slice. (Today the content object is adopted on the main path, not delivered into `init_shard` — the plumbing to add.)
2. **Build the arena over the content object** per the partitioning decision (§5).
3. **Restart** (the anchor cross-process harness already exercises handoff, `crates/anchor/tests/anchor.rs`; `crates/server/tests/daemon.rs` restarts the daemon): the anchor keeps the content object; the new daemon re-opens it (already happens) and rebuilds each shard's arena over it. **Critically, the arena's buddy allocator must be re-seeded from the recovered extent maps before it serves a single allocation** — a fresh `ChunkArena` believes the whole region is free, so without marking every live extent allocated it would hand out ranges that still hold a recovered snapshot's bytes and corrupt them. So recovery replays the metadata partitions (roots, extent maps, witnesses — as today), reserves each live extent in the arena, then **validates each recovered chunk's BLAKE3 against the bytes in the object** (§4.11 identity). A chunk whose identity fails is a **typed content-loss refusal** for that snapshot — never a silent wrong read — reported as the volume's loss (D-18). (This re-seed step is why the wiring cannot be a partial: content surviving as bytes is useless until the allocator knows those bytes are live. It needs a `ChunkArena` API to reserve a known extent — the one new arena method the implementation adds.)
4. **The gate:** extend `crates/server/tests/daemon.rs` so a snapshot's content reads back byte-identical after the restart, and a fault-injected byte flip is a typed loss (AC-2.12 / T-2.14).

## 5. What needs ratification

- **Per-shard partitioning of the content object** — the one real design decision. The arena is per shard; the content object is one segment. Either (a) **one content object per shard** (the anchor creates N, named per shard — cleanest isolation, each `Region::shared` owns its object) or (b) **one object, each shard's arena over a disjoint offset slice** (needs a `Region::shared_slice(object, offset, len)` and a sub-range accessor on `SharedObject`). Recommendation: **(a)** — it reuses `Region::shared` unchanged and keeps a shard's bytes in its own object, at the cost of N handoffs instead of one. Ada's call.
- **Sizing** — each shard's content object from the residency budget (`reserve_per_shard`, already the Anon region's size), derived, not a literal.
- **Recovery validation policy** — a failed-identity chunk is a typed per-snapshot loss (this draft); confirm that versus refusing the whole volume.
- Unsafe budget: **none expected** on Unix (the mapping is `memmap2`-safe); the Windows section path already carries its documented site.

## 6. What this does not change

R1 (a shared segment is RAM; no host path is written), R2 (the object is anchor-owned and extent-addressed — no `Arc`, no cross-process reference counting), R8 (one code path; the laptop is one anchor + one daemon, the fleet adds holders with no mode switch), R10 (shm and a Windows section are unprivileged). The metadata recovery path (`rebuild_*`) is reused unchanged; it gains the arena-over-content wiring and the identity validation before a version is served. Unsealed-scratch loss beyond the last auto-seal stays the accepted D-18 loss window.
