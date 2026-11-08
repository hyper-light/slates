# A FUSE cache write-back was applied around its stale bytes: a merge, and a store lost while its sync said 0

- **Date:** 2026-11-07. **Status:** fixed (A-248). **Found by:** the bug sweep, sending the supervisor the mask's exact
  lines for Ada (A-140's masked apply against A-192's refusal and banned item 11). **Decided by:** Ada, 2026-11-07:
  "Refuse, like NFS."

## Description

A page a process dirtied through a shared map on a Linux FUSE mount comes back from the kernel whole, as a `WRITE`
carrying `FUSE_WRITE_CACHE`: the process's stores, and the rest of the page as the kernel fetched it. Where another path
changed the file after that fetch, those bytes are stale. Since A-139 the mount applied the write-back except over its
stale bytes (`unmasked_segments`, `serve_cache_write`) and answered the whole size.

1. **It was a merge.** Applying the page outside its stale bytes chose bytes between the process's store and the other
   path's change: an inferred merge, which banned item 11 forbids. NFS refused the same write-back since A-192, and the
   guest device since 2026-11-06; only the host mount merged.
2. **It hid its losses.** A store into stale bytes was dropped and the write-back answered in full, so the process's
   `msync` and `fsync` answered 0 for a store that never landed (the case GAPS records under A-147).

## Evidence

- `a_dirty_shared_map_page_never_undoes_a_later_write_through_another_path` (`crates/cli/tests/cli.rs`, Linux),
  rewritten for the rule first: store a byte through the map; replace the file through the SDK; sync. The write-back
  must be refused and the sync fail. Then, after the refetch, store and sync: applied whole. Then store again, write
  bytes 100–200 of the same page through a second mount, sync: refused, the second mount's bytes kept, the store not
  applied, the sync failing.
- Before the change, on both Linux arches (`diag-bug-sweep-18`, run 42438497719; GitHub's runners, Linux
  7.0.0-1017-azure, x86_64 and aarch64): red 3 of 3 on each, every run at the same assertion, "the write-back of a page
  the replacement made stale is refused, and the sync says so": the masked apply answered the stale page's sync `Ok`.

## Root cause

A-139 took "what one shared page cache would hold" as the rule. One page cache would hold both writers' bytes only by
deciding, byte by byte, whose write wins, which is the inference the design bans; and the mount had no way to tell the
process that part of its page did not land.

## Fix (A-248)

- **One rule** for every transport that writes back a client's cache (`slates_bridge_fuse::coherence::judge_writeback`):
  a write-back with no stale byte is applied; one whose stale bytes all equal the file's now is applied (it changes
  nothing there); one with a stale byte that differs is refused `EIO` whole, a conflict; one whose stale bytes cannot be
  told is refused, unknown. The host mount (`channel.rs` `serve_writeback`), the guest device (`guest_writeback.rs`)
  and NFS (`nfs_writeback::judge`) all judge by it. The stale bytes are read back through the bridge past its read cap
  (`slates_bridge_core::dax_backing::read_whole`), or through the volume for NFS.
- **Removed:** `unmasked_segments`, `serve_cache_write`, `Dispatched`'s kept-back ranges, and
  `fuse.writeback_bytes_kept_back`. `fuse.writeback_conflicts` and `fuse.writeback_identical` count as NFS's
  `nfs.writeback_conflicts` and `nfs.writeback_identical` do. A refused write-back answers an error, so the write log
  records nothing of it; an applied one is logged whole.
- **The guest device** refused any stale byte before, equal or not, against its own module doc; it now applies an
  identical one, as NFS does.

## Tests changed with it

- `crates/bridge-fuse/tests/writeback.rs`: the two tests of the masked apply are replaced by
  `a_write_back_is_judged_whole_against_the_files_bytes_now` (the four verdicts over a real volume bridge; an identical
  range twice the bridge's 256 KiB read cap; a range past the file's end).
- `crates/bridge-fuse/tests/coherence_mount.rs`: A-144's test expects a conflict and a failed sync without the direct
  read, where it expected a page kept back and a sync of 0; the generated histories count conflicts.
- `crates/cli/tests/guest_dax.rs`: the oracle allows a guest's unsynced stores on a page the other path wrote into to
  be lost with the refused write-back, and holds every other byte to its latest write and the map to the volume. It is
  gated on crosvm and KVM and runs on no CI lane; it was not run for this change.

## Impact

A process sharing a file's map with another writer of the same page now loses its unsynced stores to that page when the
other writer changed it first, and is told at its sync, where before it kept the stores outside the other writer's bytes
and was told nothing when it lost the ones inside. No byte another path wrote is ever undone, as before.
