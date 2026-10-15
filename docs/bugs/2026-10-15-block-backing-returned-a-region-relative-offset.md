# `block_backing` returned a region-relative offset against the whole-object descriptor

- **Date:** 2026-10-15
- **Area:** `crates/mem` (the content arena's shared-object backing), §4.6 DAX / A-135
- **Status:** fixed

## Description

A consumer that maps an arena block **through the block's descriptor** — the virtio-fs DAX window
(§4.6), or the same-kernel direct map (A-135) — read the wrong bytes. The first real exercise of this
path (the A-135 client end-to-end test `a_same_kernel_client_maps_a_file_with_no_copy`) mapped a 20 KiB
file and read back `[2, 0, 0, …]` — the content object's own start — instead of the file's `0,1,2,…`.

## Root cause

`ChunkArena::block_backing` returned `(region.backing_fd(), extent.offset(), extent.len())`. But the
content arena's shared region is an `ExclusiveObject::open_range(handoff, base, len)` view — a shard's
**sub-range** of the anchor's content object (A-64 content-in-place; `crates/server/src/content_pool.rs`).
So `region.bytes()` begins at the object's byte `base`, while `backing_fd()` / `as_fd()` is the **whole**
object (byte 0 is its start). A block's offset *within the region* (`extent.offset()`, 0-based, what the
buddy allocator hands out) is therefore `base` bytes short of the block's offset *in the descriptor*. A
consumer mapping `extent.offset()` of the descriptor lands `base` bytes early — on the object's header,
not the block.

The path had been latent because the only prior consumer was the simulated guest, which reads through
`Store::dax_read_raw` → `arena().bytes(block)` — the in-view slice, where `extent.offset()` is correct.
No real descriptor map (a VMM's DAX window, or a same-kernel `mmap`) had ever run against it until the
A-135 client, so the offset base mismatch was never observed. It would equally have broken the guest
virtio-fs DAX path (`serve_dax` maps `block_backing`'s offset into the guest cache window).

## Impact

Any real descriptor-mapping consumer of `block_backing` read the wrong bytes whenever the region's
`open_range` base was non-zero — i.e. the production content arena (`content_pool.rs` opens each shard's
region at its slice offset). The in-process / simulated paths (`dax_read_raw`, `dax_write_raw`) were
unaffected (they use `bytes()`), so every existing test passed.

## Fix

Track the view's base and add it to the block's in-view offset:

- `crates/mem/src/shared.rs`: `ExclusiveObject` carries its base offset (0 for `new`, the `open_range`
  offset otherwise); `ExclusiveObject::base()` exposes it.
- `crates/mem/src/region.rs`: `Region::backing_base()` returns the shared object's base (0 for anonymous).
- `crates/mem/src/arena.rs`: `block_backing` returns `base + extent.offset()` as the descriptor offset.

No change to callers: `serve_dax` and the A-135 client map at the offset `block_backing` reports, which is
now descriptor-absolute.

## Evidence

- Repro: `a_same_kernel_client_maps_a_file_with_no_copy` failed `left: [2, 0, 0, …]` vs `right: [0, 1, 2, …]`
  before the fix, passes after.
- `content_pool.rs` opens the region at a non-zero base; `shared.rs` `open_view` maps the fd at that offset,
  so `bytes()[0] == fd[base]`.
- Full `mem` + `vfs` + `bridge-virtiofs` + `ipc` + `client` regression green after the fix (x86_64 Linux).
