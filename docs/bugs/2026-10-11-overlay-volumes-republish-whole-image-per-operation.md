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

## Attempted fix and what the oracle rejected (2026-10-11)

The first attempt narrowed `publishes_whole` so a base-backed volume whose **base plane is unchanged** since the last
publication takes a delta (keeping whole-image for snapshots, clones, shape changes, the first publish, and
`changed >= live_inodes`). The signal was a `BasePlane::durable_change_token` — the sum of the four versioned tables'
own write counts (a new counter inside `Versioned::set`, so an in-place head overwrite is counted too, which
`version_count` misses) plus a drift-write counter. `VolumeImage::apply` (delta.rs) was confirmed to preserve
`self.base` across a delta, so recovery = checkpoint (carries the base plane) + deltas (patched over it). All 256 vfs
tests passed.

**The server recovery oracle rejected it.** `an_overlays_copied_up_and_created_files_survive_a_restart_over_their_base`
failed: after the restart the copied-up base file (`outside`) read back correctly, but a **newly created file (`g`) in
the overlay's base-backed root directory was `NFS3ERR_NOENT`** — the create did not survive. So the gap is not the base
*witnesses* (which the token caught); it is the base-backed **directory** itself. Creating an entry in a base-backed
directory is not recovery-correct through a plain `EntryChange` delta: the directory's merged (base + upper) entry
state is carried only by the whole image, so a delta that adds `g` to a base-backed directory is lost on recovery. The
change was reverted (correctness first — never ship broken recovery).

## The real fix (owed; larger than the gate)

A base-backed volume can publish deltas only once the **base-backed directory** representation is delta-recoverable:
the upper entries of a base-backed directory must be carried and applied as a delta over the checkpoint's merged
directory (not assumed already materialized as a plain `BodyImage::Directory`). Options to scope: (a) carry a
base-backed directory's upper-entry delta explicitly and apply it against the recovered merged directory; or (b) allow
deltas only for creates in **upper-only** (non-base-backed) directories — which helps real OCI workloads that create
under their own new subdirectories, while base-directory creates keep whole-image — with the oracle extended to prove
upper-dir creates survive a restart. Either needs the recovery oracle to cover: new entries in a base-backed directory,
new entries in an upper-only directory, copy-ups, whiteouts, and renames, each with a crash and restart.

The `durable_change_token` idea (a complete, cheap base-plane change signal via per-table write counts) is sound and
reusable for whichever option is chosen; it is necessary but not sufficient.

## Status

Measured and root-caused 2026-10-11; one fix attempt made and **reverted** after the recovery oracle caught a
base-backed-directory create not surviving a restart. The real fix is the design's separately-tracked incremental
refinement and needs the broadened oracle above; not landed. The measurement, root cause and the oracle-proven gap
stand as the record for the next attempt.
