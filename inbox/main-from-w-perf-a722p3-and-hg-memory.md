# w-perf -> main: A-722 p3 recorded; the revlog chain fix; the hg import's memory, where it stands (2026-11-20 15:55 PST)

1. **A-722 p3 by use**, BENCHMARKS at **8ce3d6152** (before 5ad8dea09, p3 7b93d68c3, big Studio, kiln r12, load
   38-54, 3 interleaved rounds, every round shown). One push 1.4x faster, ten at once p99 1.6x, a first clone after a
   push 1.6x; a push still holds ~150 MiB (unchanged). **The partial clone regressed 1.4-1.5x in every round**
   (0.62-0.72 -> 0.93-1.04 s): sent to w-git (inbox/w-git-from-w-perf-a722p3-ab.md).
2. **w-hg's question, red first: yes, and fixed** at **b568499fe**. `Revlog::text` made a new text a link; it now
   applies a chain within two buffers it swaps (`delta::apply_into`, charging only growth). Red: a 64-link read of a
   4 KiB text charged 263,219 bytes; now within 30,312. slates-hg 371/371, server hg_import/hg_push 27/27. RSS barely
   moves (two texts were alive at once before too); the room charge and the per-link allocation are what went. The same
   shape remains in w-hg's `index.rs` (~710) and `import/record.rs` (~1629): told w-hg.
3. **The hg import's 5.73 GB**: the per-phase profile (kiln r16, main 1b63ab896, MallocStackLogging, vmmap+footprint
   every 20 s, malloc_history at 60 s / 120 s / landed) is running now. One dependency for your ruled fix: w-store's
   changeset log (A-767, `SLCS`) is designed, not built: no crate or branch on origin holds it. If whole texts dominate,
   I'll ask w-store for the payload's first piece (or build that piece with them) rather than lay deltas out a second
   way. I'll report the profile here when r16 ends.
