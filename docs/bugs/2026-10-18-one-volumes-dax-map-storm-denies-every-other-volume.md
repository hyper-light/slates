# One volume's DAX map storm fills the shard's registry and denies every other volume

- **Date:** 2026-10-18
- **Area:** `crates/vfs/src/dax.rs` (`DaxRegistry::register`, `Volume::dax_map_read_at`,
  `Volume::dax_map_write`)
- **Status:** found by attacking; **fixed in this change**
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

## The fix

**A request for a window that already exists returns that window.** `DaxRegistry::live_window(inode,
file_offset, writable)` finds a live, non-snapshot mapping already covering the asked-for byte, and
both map paths consult it before doing anything else — before the pin, and before the writable path's
copy-up, which is the expensive half there.

The registry then grows with the **windows that exist** rather than with the requests made. Two things
follow:

- A storm on one window is one entry, so it cannot reach the cap at all. To consume entries a volume
  must hold windows onto distinct blocks, and with one entry per distinct block the registry's cap and
  the arena's chunk budget are the same number — which is what the cap's own derivation always claimed
  it was bounding.
- A volume's consumption is bounded by the content it actually has, which its own quota already bounds,
  so one tenant can no longer price another out.

It is also a correctness-neutral reduction in state rather than a new policy: a guest that asks twice
for one window has one window, and that is what it is handed. A write or truncate of the inode revokes
its mappings, so a live window is never stale. Directory windows are untouched — `dax_map_dir` has its
own path, and §4.6 decided they are a point-in-time view a guest re-maps to refresh.

## The existing threat-model test was rewritten, and that is worth saying plainly

`a_map_storm_is_refused_at_the_registry_bound` asserted the storm was **refused** at the cap. That is
no longer what happens, so it is now
`a_map_storm_is_absorbed_and_the_registry_grows_only_with_real_windows`: the storm is absorbed, a
distinct window still adds an entry (non-vacuity, so the dedup is not swallowing everything), the
registry never exceeds its cap, and a released window lets the file map again. Rewriting a hostile-input
test to suit a fix deserves suspicion; here the new assertion is strictly stronger than the old one —
refusing was a bound, absorbing is a bound *and* fairness — and the test that measures the weakness the
old one missed is committed beside it.

## Verification

```
slates-vfs --test dax                                   52 passed, 0 failed   (macOS aarch64)
slates-bridge-virtiofs (6 suites)                       all green
slates-server --test recovery (the arbiter)             39 passed, 0 failed
cargo clippy --workspace --all-targets -- -D warnings   clean
cargo xtask check                                       clean
```
