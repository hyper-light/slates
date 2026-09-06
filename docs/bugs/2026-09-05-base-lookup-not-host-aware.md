# Base lookup depended on a prior listing: the bridge's LOOKUP was not host-aware (BUG-5)

Date: 2026-09-05. The last of the three bridge findings the A-9 audit left open under GAP-A9-3
(with BUG-7, now complete, and BUG-9, fixed). Fixed.

## Description

A direct `LOOKUP` of an untouched base file — with no prior `READDIR` of its directory — returned
`ENOENT`, even though the file exists on disk. So a tool that opens a file by path without listing
its parent first (the common case: `open("dir/file")`) got a spurious "no such file". Listing the
directory first made the same lookup succeed.

## Root cause

`VolumeBridge::lookup` called the plain `Volume::lookup_no`, which searches only the in-memory
dirtree. For an overlay volume, an untouched base entry is *not* in the dirtree until a listing
hydrates it (D-25: "Create records the path and nothing else. Untouched entries are served from
disk on demand"). So the lookup found a base entry only after a `READDIR` had populated the tree —
exactly what the existing base-overlay tests did (they `READDIR` the root before the `LOOKUP`, with
the comment "a tool lists a directory before opening its files").

The bridge's other base operations were already host-aware: `attr_of` (stat), `readdir` and `read`
all go through `Volume::with_host(host)`, whose wrapper (`base.rs`) consults the base plane —
including a host-aware `lookup_no` (which itself uses the design's change-time listing cache). Only
`lookup` used the non-host path.

## Impact

- A spurious `ENOENT` on the first access of any untouched base file reached by direct path, before
  anything listed its directory. Correctness bug on the overlay read path; no data risk.

## Fix

`VolumeBridge::lookup` now consults the host for an overlay, mirroring `attr_of`/`readdir`/`read`:
`self.volume.with_host(host).lookup_no(...)` when a base host is present, the plain `lookup_no` for
a scratch volume. No new host-trait method and no change to the host seam's deliberate bulk-listing
design (§4.5) — it uses the base plane's existing host-aware lookup, which is backed by the
change-time listing cache.

Gated: `crates/bridge-fuse/tests/base_overlay.rs`
(`a_base_file_is_found_by_direct_lookup_without_a_prior_readdir`) overlays this crate's `src` and
looks up `lib.rs` with no prior `READDIR`, asserting it is found. That test fails on the previous
code (the un-hydrated dirtree has no `lib.rs`), so it discriminates the fix; the existing
`READDIR`-then-`LOOKUP` tests still pass. 152 tests pass; fmt, clippy, xtask, cross clean.

## Ledger

Closes the BUG-5 portion of GAP-A9-3; with BUG-7 (FSYNC/FSYNCDIR/LINK/READDIRPLUS dispatched) and
BUG-9 (truthful statfs), the in-sandbox bridge findings of GAP-A9-3 are now closed. The mounted
conformance the gap also names (AC-3.10/AC-3.12: independent kernel vectors, real setattr/rename
under writeback, open beyond the arena bound) still needs a real mount. Left to the audit-doc owner
to update `docs/wip/GAPS.md` to avoid a concurrent-edit conflict.
