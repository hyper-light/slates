# 2026-09-14 — Base metadata mutations through the bridge bypassed the overlay rules

**Ledger:** GAP-A9-3 ("coherence delivery and metadata copy-up need sweep"); AC-1.17 / T-1.21.
**Design:** §4.5 "Copy-up" ("Metadata-only changes copy up the witness and pin nothing"), §4.5
"Mutation" ("Unlinking a base-backed entry writes a `Whiteout`"), §4.5 "Rename" ("renaming a
base-backed file copies up its witness ... and leaves a whiteout"), §4.6 "POSIX and transparency
acceptance" ("Base lookups and metadata mutations go through the same overlay rules as reads and
writes").
**Baseline:** `4f5deae` (main's head on 2026-09-14).

## Description

The shared operation layer (`crates/bridge-core/src/volume_bridge.rs`, the one `Bridge` every
transport dispatches onto) routed reads, writes, lookups, stats and listings of an overlay volume
through the base plane's `Overlay` (`Volume::with_host`), but routed every **mutating** verb —
`create`, `mkdir`, `symlink`, `link`, `unlink`, `rmdir`, `rename`, and the four halves of
`setattr` (truncate, chmod, chown, utimes) — through the plain `Volume::*_no` wrappers, which know
nothing of the base beneath a merged directory.

Observable through any mount (FUSE, NFS, FSKit, WinFsp, virtio-fs all share the seam), driven here
by the base plane's simulated host (`crates/bridge-core/tests/base_overlay.rs`, 8 of 10 cases
failing at the baseline):

1. `chmod`, `chown`, `utimes` or `truncate` of an untouched base file recorded **no witness**, so the
   entry was not in the diverged set the landing plans from (`Volume::diverged`), and — worse — the
   next stat of the still-"untouched" entry followed the live disk (`Overlay::follow_live_disk`) and
   **undid the change**: a truncate to 3 bytes read back as 15, a requested mtime of 222 read back as
   the disk's 0.
2. `rename` of an untouched base file left **no whiteout** at the old name and no witness on the
   moved entry, so the next listing showed the base name beside the new one (`{lib.rs, lib2.rs,
   main.rs}` where `{lib2.rs, main.rs}` was right).
3. `RENAME_NOREPLACE` onto a base name the mount had not looked up yet **replaced it silently**: the
   existence check used the plain lookup, which knows only materialized entries.
4. `link` to an untouched base file recorded no witness, and a link **at a name the base holds**
   succeeded, shadowing the disk's file.
5. `unlink` of a base name after a watcher hint had invalidated the directory's cached listing
   **lost its whiteout** (`Volume::dir_remove` consults only a loaded listing through
   `base_listing_has`), so the base name came back at the next listing.
6. `create`, `mkdir` and `symlink` **over a name the base holds** succeeded (a second file shadowing
   the disk's; an NFS `CREATE GUARDED` would report success where `NFS3ERR_EXIST` is right), and a
   directory made through the mount was not opaque, so it was **absent from the diverged set** while
   its children were present — a landing would plan the children without their parent.

## Root cause

`VolumeBridge` held the host as the concrete `OsHost` and consulted it only where the read path
had needed it (`attr_of`, `readdir`, `read`, `write`, `lookup`). The overlay's mutating verbs
(`Overlay::{create_file, mkdir, symlink, link, unlink, rmdir, rename, truncate, chmod}`) existed
but had no by-inode-number form, and `Overlay` had no `chown`/`set_times` at all, so the bridge's
mutating verbs fell to the plain forms. Nothing at the seam tested a mutation against the host
oracle: the only overlay tests through a bridge (`crates/bridge-fuse/tests/base_overlay.rs`) read
a real read-only directory and copied up through `write`, the one mutating verb that was routed.

## Impact

Any tool that renames, chmods, touches, truncates, links or deletes files of an overlay volume
through a mount left the volume's record of the base inconsistent with what it showed — a landing
verdict (§4.15) had no witnessed base for the changed entries and would skip or misplan them, and
some changes were silently undone on the next stat. The disk was never written (R1 held); the
overlay's own view was wrong.

## Exact edits (all in one change, 2026-09-14)

- `crates/vfs/src/base.rs`: `Overlay::{chown, set_times, open_base}` (metadata-only copy-ups; the
  explicit descriptor open of §4.6 "Base files") and the by-inode-number wrappers
  `Overlay::{create_file_no, mkdir_no, symlink_no, link_no, unlink_no, rmdir_no, rename_no}`.
- `crates/vfs/src/volume.rs`: `Volume::set_times` takes each time as an `Option` (an unset field is
  left alone, `UTIME_OMIT`) and an optional explicit change time (a writeback-caching kernel's
  `FATTR_CTIME`).
- `crates/bridge-core/src/lib.rs`: `SetAttr` gains `ctime`.
- `crates/bridge-core/src/volume_bridge.rs`: the host behind the seam is `dyn HostFs`
  (`with_base(Box<dyn HostFs>)`, `attached(Option<&mut dyn HostFs>)`), so the simulated host drives
  the bridge in tests as the real one does in the daemon; every mutating verb routes through
  `Overlay` when the bridge has a host; `rename`'s `NOREPLACE` existence check goes through the
  overlay lookup; `open` takes the base descriptor. `slates-base` is no longer a dependency.
- Call sites: `crates/server/src/{nfs,virtiofs}.rs`, `crates/bridge-fskit/src/mount.rs`,
  `crates/bridge-fuse/tests/base_overlay.rs` (the host passed as `dyn HostFs`);
  `crates/bridge-nfs/src/procedures.rs`, `crates/bridge-fskit/src/lib.rs` (`ctime: None` — those
  wires carry no change time); `crates/server/src/verbs.rs` (the restore path's `set_times`).
- Tests: `crates/bridge-core/tests/base_overlay.rs` (T-1.21 over the host oracle; the failing-first
  record is in the file's header).

## Siblings found, not changed here

- `Fingerprint`/`BaseEntry` carry no owner: an untouched base entry reports uid/gid 0 through
  every mount (`Inode::new`'s born default), so `ls -l` in an overlay shows base files as root's.
  Needs an owner field on the read-only seam (`crates/vfs/src/host`), which the digest work is
  editing concurrently; reported for the integrator.
- `Overlay::rename` onto an existing base *target* records the source's whiteout but no witness of
  the replaced target's fingerprint; the landing plane's verdict for a replaced base target is
  §4.15's concern.
