# FUSE invalidations of the volume root named an inode the kernel does not know

**Found:** 2026-10-06, by caching FUSE lookup misses as negative entries (§4.6 "Cache posture"), which made a change
to a name in the root through another attachment visibly fail to reach a mount.

## Description

A FUSE mount caches a volume's own names and attributes for good, and slates keeps that cache true by delivering
an invalidation for every change made through another attachment (AUD-02, `crates/bridge-fuse/src/coherence.rs`).
For the volume's root directory those invalidations were never applied. Two symptoms were reproduced on Linux 6.12,
both through a real kernel mount:
- **A name created in the root through another attachment.** A mount that had cached its absence kept answering
  "absent" (`a_name_cached_absent_in_the_root_appears_when_another_attachment_makes_it`, failing before the fix).
- **A `mkdir` in the root whose barrier was refused.** The caller is told `EIO`, but the directory is in the volume
  (unpublished). It stayed invisible, because the kernel kept the negative entry its lookup cached
  (`a_mutations_reply_waits_for_its_barrier_and_a_refused_barrier_answers_eio`).

## Root cause

The kernel names the volume's root by node id 1 (`FUSE_ROOT_ID`). The bridge translated node id 1 to the volume's
root inode on the way in (`resolve`), but an invalidation was written with the volume's root inode number. The
kernel found no inode by that number (`fuse_reverse_inval_entry` and `fuse_reverse_inval_inode` look up by node id)
and ignored it silently. Every invalidation naming the root, its entries and its attributes alike, was lost.

Before negative entries the loss was mostly hidden: an absent name was looked up again on each probe. But a
positive root entry deleted, renamed or replaced through another attachment stayed in a mount's cache for good.

A second defect sat beside it. A refused reply (a mutation applied, its barrier refused, `EIO` in its place)
advanced the coherence cursor past its own change, as a successful reply does. So the kernel was never told the
change happened.

## Fix

- **Addressing.** Delivery addresses the volume's root as node id 1 (`addressed_to_the_kernel` in
  `crates/bridge-fuse/src/channel.rs`).
- **Refused replies.** After the error reply is written, the cursor goes back to where it stood before the request
  and one round delivers the request's own change (`redeliver_after_refusal`). It runs after the reply, never
  before, because an entry invalidation takes the directory's lock, which the waiting caller holds.
- **The test harness.** `owner_turn`'s serve thread no longer panics on a closed channel when its test has already
  failed.

## Sibling sweep

- virtio-fs guests have no invalidation channel and are served zero lifetimes (`CacheCoherence::Revalidated`), so
  nothing is cached for them to miss.
- NFS mounts revalidate through change attributes and `wcc`, not by node id, so they are unaffected.
- No other reverse mapping from volume inode to kernel node id exists: the root is the only node whose numbers
  differ.
