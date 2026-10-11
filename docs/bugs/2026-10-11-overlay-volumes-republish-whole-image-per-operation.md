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
the overlay was `NFS3ERR_NOENT`** — the create did not survive.

**What the debug pass showed, and what it did not (honest record).** With the gate hacked to let base-only volumes
delta (`SLATES_DELTADBG`), the overlay volume's logged delta publications carried **empty change sets** (`changed=0`,
`dirty.inodes=[]`, no entries) and recovery applied no entry changes — the delta never carried `g`. **But this does not
by itself establish the mechanism:** the overlay create path delegates to `self.vol.create_file` (and `mknod_no`,
`mkdir`, `symlink`) — the *same* volume methods a plain create uses, which *do* record `dirty.inode`/`dirty.entry`. So
a plain overlay create should dirty the volume. The empty-delta the hack produced is therefore most likely a
publish-timing or shard-ownership artifact of the hack (e.g. an earlier forced delta cleared the dirty set before the
logged publishes, or the logged publishes were a non-owner shard's clean view with `TEST_SHARDS=2`), **not** a proven
"overlay does not dirty" bug. The precise recovery-loss mechanism is **not yet pinned**.

The change was reverted (correctness first — never ship broken recovery).

## The mechanism, pinned (and the earlier debug's capture error)

The first gate-relaxation attempt left the delta empty because of a **second, coupled gate I had missed**:
`Dirty::recording()` returned `self.published && self.published_shape == Shape::default()`, so for any base/snapshot/
clone volume it **skipped recording changes into the dirty set** (a 2026-10-05 memory optimization — a volume that
always publishes whole need not hold "a name and a number per change"). Relaxing only `publishes_whole` therefore asked
`encode_delta` to encode an **empty** dirty set → the create was never carried → lost on restart.

My intervening debug passes read "zero publishes" and "the landing test doesn't call `publish_shard`" — both were a
**`--nocapture` capture error**: libtest hides a passing test's output, and the daemon runs on in-process shard threads
whose `eprintln` the harness swallowed. With `--nocapture`, `publish_shard` is called on every NFS barrier (proc 7
WRITE / proc 8 CREATE, `outcome_some=true`), the overlay's deltas were empty (`changed=0 dirty_inodes=[]`), and that
emptiness is exactly `recording()` skipping base volumes — not a shard/ownership or directory-representation issue.

## The fix (landed 2026-10-11)

Two coupled changes, gated by one predicate (an overlay with a stable base plane and no snapshots/clone may delta):

- **`crates/vfs/src/delta.rs`** — `recording()` now records for a base-only volume (`published && no snapshots &&
  no clone`); it still skips snapshot/clone volumes (they always publish whole), and still skips before the first
  publish. `publishes_whole()` drops the blanket `shape != Shape::default()` and instead forces the whole image only
  for snapshots, clones, a shape change, `changed >= live_inodes`, the first publish, or **`base_plane_changed()`** —
  so an overlay publishes a delta while its base plane holds still, and the whole image on any copy-up/whiteout/rename/
  drift. `is_clean` and `mark_published` track the base token; `Dirty` carries `published_base_token`.
- **`crates/vfs/src/base.rs` + `base_versions.rs`** — `BasePlane::durable_change_token()` = the sum of the four
  versioned tables' own write counts (`Versioned::writes`, bumped in `set`, so an in-place head overwrite counts too —
  `version_count` would miss it) plus a drift-write counter. Complete and cheap; it moves on every durable base-plane
  write, so `base_plane_changed()` forces the whole image whenever the base plane a delta cannot carry has changed.

**Why it is safe:** recovery applies a delta over the last checkpoint, and `VolumeImage::apply` leaves `self.base`
untouched — so the checkpoint's base plane is preserved and the delta carries only the store changes (created/copied-up
inodes, entries, whiteouts). A base-plane change forces a whole image (which carries the plane), so no base-plane delta
is ever dropped.

**Proven:** `crates/server/tests/recovery.rs` 39/0 — the overlay oracle
(`an_overlays_copied_up_and_created_files_survive_a_restart_over_their_base`) and the crash-at-every-durable-step oracle
pass; the full `crates/vfs` suite is green; and a new load-independent regression,
`crates/vfs/tests/delta_cost.rs::an_overlay_create_publishes_a_delta_costing_the_same_in_a_large_upper_as_in_a_small_one`,
asserts an overlay create now publishes a **delta** whose publication+replay allocate the **same** in a 4,096-entry
upper as in a 64-entry one (was O(volume), O(N²) to fill a directory). `cargo clippy --workspace --all-targets
-- -D warnings` and `cargo xtask check` clean.

## Status

**FIXED 2026-10-11.** Overlay (base-backed) volumes publish deltas per barrier while their base plane is unchanged,
recovery is byte-correct (oracle-proven), and the write path is no longer O(volume) per operation. Owed, separately:
snapshot/clone deltas (still whole-image — the head-into-snapshot and shared-root problems a delta cannot carry).
