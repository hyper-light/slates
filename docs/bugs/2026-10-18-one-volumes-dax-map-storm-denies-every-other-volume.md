# One volume's DAX map storm fills the shard's registry and denies every other volume

- **Date:** 2026-10-18
- **Area:** `crates/vfs/src/dax.rs` (`DaxRegistry::register`, `Volume::dax_map_read_at`,
  `Volume::dax_map_write`)
- **Status:** found by attacking; **OPEN.** A fix was built and **withdrawn the same day** — it was unsafe, for the
  reason below. The reproduction is committed `#[ignore]`d
  (`one_volumes_map_storm_does_not_deny_another_volume`; run with `--ignored`).
- **Class:** cross-tenant resource exhaustion through a shared bound (CWE-410)

## Description

The DAX mapping registry is bounded, and deliberately so: its cap is the shard's chunk budget, on the
stated reasoning that "a mapping pins a chunk, and the shard holds no more chunks than `max_chunks`".
`a_map_storm_is_refused_at_the_registry_bound` existed to prove a guest could not grow it without
limit, and it passed.

The bound was the shard's, and the accounting was per **request**. `dax_map_read_at` and
`dax_map_write` minted and registered a fresh entry every time they were called, so a guest asking for
*the same window* a thousand times cost a thousand entries for one window onto one block. One volume
could therefore take the whole registry — and the next volume's first window was refused.

So the old test proved the weaker half of the threat (nothing grows without bound) and missed the half
that matters on the multi-tenant host the §4.6 gate exists for: the tenant who fills a shared bound is
not the tenant who pays for it.

## The measurement

Two volumes on one store with a 32-chunk budget; the attacker re-maps one file, the victim then asks
for a single window of its own (`one_volumes_map_storm_does_not_deny_another_volume`, macOS arm64,
2026-10-18):

```
the victim volume was denied its first window by another volume's storm:
  Err(NoSpace) (attacker holds 32 of 32)
```

The victim's map is shown to succeed on a quiet registry immediately before, so the refusal is the
storm's and not a volume that could never map.

## The fix that was built, and why it was withdrawn

The obvious fix is to make a request for a window that already exists return that window:
`DaxRegistry::live_window(inode, file_offset, writable)`, consulted by both map paths before the pin and
before the writable path's copy-up. The registry would then grow with the **windows that exist** rather
than with the requests made — a storm on one window would be one entry and could never reach the cap,
and a volume's consumption would be bounded by content its own quota already bounds.

It was built, the suite was green on three platforms, and then it was withdrawn, because it is unsafe:

- The arena's pin is refcounted **per mapping** (`PinnedBlock::holders`): `dax_map_*` pins once per
  mapping handed out, and `dax_release_after_unmap` unpins once.
- The device keeps **one `DaxWindowEntry` per SETUPMAPPING** (`dax_windows.push(entry)`, both the
  fd-sharing and the message arms) and calls `dax_release_after_unmap(&entry.mapping)` for each.

So two guest windows over the same file range — two SETUPMAPPINGs at different guest offsets — would
share one mapping and therefore **one pin**, while still producing two window entries and two releases.
The first unmap would take the holder count to zero and unpin a block the second window still maps:
reusable by another volume under a live guest page table. That is precisely the escape the two-phase
revoke exists to prevent, and it is worse than the denial it was fixing.

Nothing in the test suite caught it. It was found by reading the device's window bookkeeping after the
change was already committed (`e4535b01`), which is itself the lesson: a change to the registry's
accounting has to be checked against the *device's* accounting, because the two halves of the pin live
in different crates.

**And it is a measured hazard, not a reading of the code.** A new device-level test
(`every_window_a_device_maps_gives_its_pin_back`) maps the same file range at two guest offsets, drops
one window and asserts the block is still pinned for the other. It passes on the reverted tree, and
with the dedup re-applied it fails exactly there:

```
assertion `left == right` failed: the block is STILL pinned: the second window over it is live,
and a block released here would be reusable under a live guest page table
  left: 0
 right: 1
```

That test is committed, and it is the guard this class was missing: **no device-level test looked at
pins at all** before, which is why three platforms' suites went green over an unsafe change.

**What a correct fix has to do: move the pin from the mapping to the window.** One pin per window
handed out, and a `dax_revoke` that no longer consumes one — so the holder count tracks guest windows
rather than registry entries. That changes the gate's core invariant, so it is a considered change
rather than a passing one, and it is not made here.

## Verification of the revert



```
slates-vfs --test dax                                   51 passed, 0 failed, 1 ignored  (macOS aarch64)
slates-vfs --test dax -- --ignored                      the reproduction still fails, as it should
cargo clippy --workspace --all-targets -- -D warnings   clean
cargo xtask check                                       clean
```

(The withdrawn fix had also been green on x86_64 Linux and aarch64 Linux — 52/52 on both — which is
exactly why the suite passing is not the same as the change being safe.)
