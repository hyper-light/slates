# One volume's DAX map storm fills the shard's registry and denies every other volume

- **Date:** 2026-10-18
- **Area:** `crates/vfs/src/dax.rs` (`DaxRegistry::register`, `Volume::dax_map_read_at`,
  `Volume::dax_map_write`)
- **Status:** found by attacking; **FIXED** — on the second attempt. The first was withdrawn the same day as
  unsafe (below), and the guard built in response is what told the two apart. The reproduction is now a plain
  passing test (`one_volumes_map_storm_does_not_deny_another_volume`, no longer `#[ignore]`d).
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

## The fix, and the attempt that had to be withdrawn first

The fix is to make a request for a window that already exists return that window:
`DaxRegistry::live_window(inode, file_offset, writable)`, consulted by both map paths before the pin
and before the writable path's copy-up. The registry then grows with the **windows that exist** rather
than with the requests made — a storm on one window is one entry and can never reach the cap, and a
volume's consumption is bounded by content its own quota already bounds.

**The first attempt did exactly that and was unsafe.** It was green on macOS aarch64, x86_64 Linux and
aarch64 Linux, and it was still wrong:

- the arena's pin is refcounted **per mapping** (`PinnedBlock::holders`): `dax_map_*` pinned once per
  mapping handed out, and `dax_release_after_unmap` unpins once;
- the device keeps **one `DaxWindowEntry` per SETUPMAPPING** (both the fd-sharing and the message arms)
  and calls `dax_release_after_unmap` for each.

So two guest windows over one file range shared a mapping and therefore **one pin** while still
producing two releases: the first unmap took the holder count to zero and released a block the second
window still mapped — reusable by another volume under a live guest page table, the exact escape the
two-phase revoke exists to prevent, and worse than the denial being fixed.

**What makes the version that stands correct is one line: it pins again on every hit.** One pin per
window handed out, one unpin per window released, so the holder count tracks guest windows rather than
registry entries. That is the "move the pin from the mapping to the window" the withdrawal called for,
in the smallest form that achieves it.

## Verification

The decisive check is the guard built after the withdrawal,
`two_windows_over_one_block_each_hold_its_pin` (`crates/bridge-virtiofs/tests/device.rs`), which maps
one file range at two guest offsets, drops one window and asserts the block is still pinned for the
other. Red-checked against this very fix — with the `pin()` on the dedup hit removed:

```
two_windows_over_one_block_each_hold_its_pin ... FAILED
  left: 0   right: 1          (the block released while the second window is live)
```

and with it in place, green. No device-level test looked at pins at all before this episode, which is
precisely how the first attempt passed three platforms.

```
slates-vfs --test dax                                   53 passed, 0 failed, 0 ignored
  (the reproduction is now a passing test, and a_map_storm_is_absorbed_… replaces the
   "refused at the cap" version, whose property was weaker)
slates-bridge-virtiofs (6 suites)                       all green, 33 + 1 ignored on the device suite
slates-server --test recovery (the arbiter)             39 passed, 0 failed
slates-client --test client                             13 passed, 0 failed
cargo clippy --workspace --all-targets -- -D warnings   clean
cargo xtask check                                       clean
```

## Verification of the revert (superseded, kept for the record)





```
slates-vfs --test dax                                   51 passed, 0 failed, 1 ignored  (macOS aarch64)
slates-vfs --test dax -- --ignored                      the reproduction still fails, as it should
cargo clippy --workspace --all-targets -- -D warnings   clean
cargo xtask check                                       clean
```

(The withdrawn fix had also been green on x86_64 Linux and aarch64 Linux — 52/52 on both — which is
exactly why the suite passing is not the same as the change being safe.)
