# B2's self-healing net resurrects a deleted file's plaintext into freed memory

**Status:** **corrected 2026-10-19, later the same day — see "What this document got wrong" at the end.** The
unsoundness is real and the fixes stand as preconditions, but the state that triggers it is **not reachable in
production**: it needed a double-unpin that only the test harness performed. The impact section below was
written before that was checked, and overstated it. Found by `the_four_layers_under_chaos_never_heal_what_nothing_corrupted`
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

## What this document got wrong

The sections above describe the unsoundness correctly and the fixes are right to keep. The **impact claim was
wrong**, and finding that out took one question I should have asked before writing it: *which production
caller creates this state?*

The state needs a block whose pin reaches zero while a mapping of it is still live. Pins are per *window*
handed out, not per registry entry — that is already the fix recorded in
`docs/bugs/2026-10-18-one-volumes-dax-map-storm-denies-every-other-volume.md`, where `dax_map_*` pins again on
every dedup hit. So reaching zero early requires **two unpins for one window**, and the production paths do
not do it:

- the virtio-fs device pairs `dax_revoke_pending` (unregisters, keeps the pin) with `dax_release_after_unmap`
  (unpins) — one unpin per window, and it never calls the `Bridge::dax_revoke` verb at all;
- a client's own maps (A-135) are torn down by `reap::drop_client` with `dax_revoke` alone — one unpin;
- `merge_service` calls `dax_revoke` only on abandon paths, for a mapping it just minted and never released;
- there is no client-facing "release this mapping" verb, so nothing drains `client.dax_maps` twice.

**The double-unpin was the chaos harness's.** Its teardown step called `dax_revoke` *and*
`dax_release_after_unmap` on one mapping. `dax_revoke` is the synchronous one-shot that unregisters **and**
unpins; `dax_release_after_unmap` is phase two of the *two-phase* revoke whose phase one keeps the pin. Doing
both is a second unpin, and the arena's holder count cannot tell whose pin it takes — with two windows over one
block it steals the other's. The bug predates this session: the older `chaos_step` had the same step. Both are
fixed.

Proven rather than argued: with the harness corrected to one unpin per window, the chaos property is green at
4 × 4,000 cases **with the currency check removed** — where, before, removing it failed on run 1.

## Impact

**None in a deployed daemon.** No production caller double-unpins, and B2 had no production caller at all
before the same day. The fixes stay as what they should have been described as from the start: **preconditions
on an operation that writes memory**. A scrub that heals is a write, and checking that the entry it writes
through is live, pinned and still its inode's content is that write's own precondition, not a "just in case"
second path — cheap, and sound whatever future caller creates the state.
