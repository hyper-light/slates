# B2's self-healing net resurrects a deleted file's plaintext into freed memory

**Status:** fixed (two changes, the same day — the first was insufficient and the chaos property said so). Found by `the_four_layers_under_chaos_never_heal_what_nothing_corrupted`
on its second run, 2026-10-19 — the assertion was written for exactly this and fired within minutes of landing.

## What happens

§4.6's B2 net keeps a clean BLAKE3 and a pinned clean copy of a guarded read-only DAX mapping, and the periodic
scrub restores the block when its bytes no longer match. If the mapping's block has been **released and scrubbed
to zero** while the mapping is still in the registry, the net reads the zeroes as a bit flip and writes the old
content back — into a block the arena now considers free.

The resurrected bytes are a deleted file's plaintext, sitting in memory the allocator will hand to the next
caller, in this volume or any other on the shard. That is the cross-tenant disclosure the whole DAX gate exists
to prevent, performed by the layer that is supposed to be the net under it.

## The schedule that shows it

proptest's shrunk input, replayed step by step with the arena's bytes printed:

```
step 0: map f0 writable                → W on block 0, arena=[10,10,10,10] pinned=true
step 1: map f0 read-only (guarded)     → R on the SAME block 0, guard records BLAKE3 of [10,10,...]
step 3: unlink f0                      → the free is deferred: the block is pinned, bytes kept
step 5: revoke+release, then the drain releases the duplicate W
                                       → arena=[00,00,00,00] pinned=false   ← released AND scrubbed
step 6: scrub                          → healed=1, arena=[10,10,10,10]      ← plaintext back in free memory
```

## Root cause

Two things meet.

1. **A pin is per block, not per window** — the defect §4.6 already records under the denied map-storm fix
   ("it shares one refcounted pin between two guest windows, so the first unmap would unpin a block the second
   still maps", `docs/bugs/2026-10-18-one-volumes-dax-map-storm-denies-every-other-volume.md`). Here `W` and `R`
   map the same block; releasing `W` twice drops the holder count to zero even though `R` is still live. The
   deferred free from the unlink then completes and scrubs the block, while `R` remains in the registry.
2. **B2 trusted the registry entry.** `Store::dax_scrub` asked only whether a guard existed and whether the bytes
   matched. It never asked whether the block was still the daemon's to write.

So the new consequence is B2's, even though the shared pin is what creates the state. A net that writes into a
block nobody holds is unsound however the state arose.

## The fix, in two parts — the first was not enough

**First attempt: refuse to act on an unpinned block.** `Store::dax_scrub` reports `Unguarded` when the
mapping's block is not pinned, and the sliced pass drops the guard. A live mapping's block is always pinned —
the gate's own invariant (`dax_map_*` pin, `dax_revoke` unpins) — so an unpinned block under a live entry means
the entry is stale.

**The chaos property then found a second path, with the pin check already in** (seed
`a61af512137a4d9b8afc615c7a3c3164f08ebd6375dd49c181870024854e71a7`, at 4,000 cases): a released block can be
**re-allocated to another file, which pins it again**. The pin check passes, the bytes under the stale guard are
now someone else's, and a heal writes the dead file's content over the live one's. Pinning says the block is
*someone's*; it does not say it is still *this mapping's*.

**What actually closes it:** the scrub now requires all three — the mapping live, its block pinned, **and the
mapping still current for its inode** (`Volume::dax_still_current`). Only a `Volume` can answer the third, so
the sliced pass moved from `Store` to `Volume` (`Volume::dax_scrub_slice`) and the daemon's reap tick iterates
volumes, as the shift loop already does. Each of the three catches a different way the entry outlives what it
describes; the first two alone let a real schedule through.

This is still deliberately narrow. The shared pin stays recorded and owed: moving the pin from the mapping to
the window is the correct repair and it changes the gate's invariant, which is a separate change.

## Impact

Any shard running B2 (`Volume::enable_dax_integrity_guard`) where one inode had both a writable and a read-only
DAX window at once and was then deleted. Before 2026-10-19 B2 had no production caller at all, so no deployed
daemon could reach it; the window was between wiring B2 and this fix, which is the same day.
