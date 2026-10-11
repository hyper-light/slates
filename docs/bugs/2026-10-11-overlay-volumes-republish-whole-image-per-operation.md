# Overlay (base-backed) volumes re-publish their whole image on every mutation (2026-10-11)

## Description

A mutation through a mount transport publishes the shard's recovery image before its reply claims stability
(§4.8, D-18, the barrier). For a **plain** volume (no base, no snapshots, no clone) this publish is a **delta**
(A-68): O(changed inodes), and measured flat (~3 µs on the shard, create ~42 µs end-to-end over FUSE, independent
of volume size). For a volume with a **base plane** (an overlay — every OCI/container volume), a **snapshot**, or a
**clone origin**, the publish is instead the **whole image** (`encode_image_into`), O(all live inodes), on **every
barrier** — so every file operation re-encodes the entire volume.

This makes the overlay write path **O(volume) per operation**, and filling one directory **O(N²)**: each create
dirties the directory, and the whole-image publish re-encodes (and re-sorts) every inode and every directory entry.
Overlay volumes are exactly the OCI/container case (condition 2) and this is a condition-12 p99 cost.

It is not a correctness bug (recovery is correct; whole-image publication is correct, just not bounded). It is a
**bounded-work (R4) concern** and the single largest write-path latency lever for the overlay/OCI case. The design
already tracks it as owed: §4.8/line 1251 "Whole-image publication still scales with retained bytes and needs its
separately tracked incremental/cooperative refinement," and `publish_shard`'s doc ("it re-images every volume on
each call; an incremental publish is the owed refinement").

## Evidence (measured, Linux aarch64 release in Docker, kernel 6.12, load ≈ 6–7)

**Plain vs overlay create, as the upper directory grows** (`plain_vs_overlay_scale.sh`, 4000 creates in one dir, p50
by window):

| volume | create p50 [0,500) | [1500,2000) | [3500,4000) |
|---|---|---|---|
| plain (`--dynamic`) | 99 µs | 68 µs | 97 µs |
| overlay (`--dynamic --base`) | 196 µs | 714 µs | 490 µs |

Plain is **flat** (O(1) delta); overlay **grows with directory size** (O(dir)). The overlay cost is independent of
*base* size (100 → 8000 base files: create p50 128–159 µs at a fixed 1000-create depth) — it scales with the
**upper** directory, i.e. with what the whole-image publish re-encodes, not with the base.

**Reconciliation of the earlier "277 µs create" (b8fb3d9b).** That bench used a 500-file-base overlay and 3,000
creates; its p50 (≈ the 1500th create, a ~1500-entry directory) lands at 277–289 µs — the whole-image re-encode of a
large directory, not a copy-up (copy-up itself measured *cheaper*: 134 µs) and not VM load. A plain-volume create is
42 µs. So the "277 µs" is the overlay whole-image penalty, now attributed.

**Daemon perf during overlay creates** (`perf record` of the shard; self + children): `publish_shard` is **70 % of
the shard's CPU** (vs ~0.85 % for a plain volume), dominated by `encode_inode_within` (51 %), `encode_directory` →
`quicksort`/`driftsort` of entries by `NameEquivalence::compare` (33 % + 21 %), and `crc32c` (9 %). The shard re-encodes
and re-sorts the whole directory per publish.

## Root cause

`crates/vfs/src/delta.rs`, `publishes_whole()`:

```rust
fn publishes_whole(&self) -> bool {
  let changed = u64::try_from(self.dirty.inodes.len()).unwrap_or(u64::MAX);
  let shape = self.shape();
  !self.dirty.published            // first publish — correct (the delta needs a base image)
    || shape != Shape::default()   // ANY base/snapshot/clone — THE COST: forces whole-image always
    || self.dirty.published_shape != shape   // shape CHANGED — correct
    || changed >= self.live_inodes // most inodes changed — correct
}
```

`shape()` sets `base: self.base.is_some()`, so a base plane (overlay), any snapshot, or a clone origin makes
`shape != Shape::default()` and forces the whole image **even when the shape is unchanged since the last publish**
(the `published_shape != shape` condition already catches real shape changes). `encode_delta` takes only `store`
(not `host`), so it cannot image base-plane state read through the host; the whole-image path takes `host` and images
the base plane with the overlay.

The design rationale (`delta.rs` header) for the three shapes: a **snapshot** head change can move a shared file
into the snapshot's own image (copy-on-write split a delta of the head does not carry); a **clone** shares the delta
root; a **base** "images through" the overlay. For a **base-only overlay with a stable shape**, created and
copied-up files are **store** inodes (a delta carries them), whiteouts are entry changes (a delta carries them —
`EntryChange` encodes a whiteout's child as `None`), and untouched base files are recoverable from the stable
`BaseRef` in the prior image. The open question for a base-only delta is whether the base **witnesses** recorded on
copy-up are carried (per-inode, hence in the delta) or imaged separately by the base plane (hence missed by a delta).

## Proposed fix (scoped; recovery-correctness-gated)

Narrow `publishes_whole` so a **base-only** volume whose shape is unchanged publishes **deltas** (keep whole-image for
snapshots, clones, shape changes, the first publish, and the `changed >= live_inodes` case). Concretely, replace the
blanket `shape != Shape::default()` with a predicate that forces whole-image only for the shapes a delta cannot carry
(snapshots, clones) and for base-plane state a delta does not yet carry — and extend `encode_delta`/recovery to carry
any base witness delta that recovery needs.

**The arbiter is the recovery oracle, not reasoning.** `crates/server/tests/recovery.rs`
(`an_overlays_copied_up_and_created_files_survive_a_restart_over_their_base`) and the crash-at-every-durable-step
oracle must pass byte-for-byte with the gate relaxed; if they fail, the delta is missing base state (name it, carry
it, or keep whole-image for that case). Only a green oracle justifies the relaxation.

Expected impact: overlay create falls from O(dir)/O(volume) to O(1) delta (plain-volume parity, ~42 µs flat),
removing the O(N²) directory fill on the OCI write path.

## Status

Measured and root-caused 2026-10-11. The fix is a recovery-format-sensitive change (data-loss stakes) that the design
tracks as a separate refinement; it proceeds only behind a green recovery oracle. Not yet implemented.
