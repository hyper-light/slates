# A guest's stale cache write-back reverted another path's write

- **Date:** 2026-11-06 (bug-sweep; GAPS 2026-10-21, "the direct virtio-fs route shares `serve_write`")
- **Area:** `crates/bridge-virtiofs/src/device.rs` (every guest request through `slates_bridge_fuse::bridge::dispatch`),
  `crates/bridge-fuse/src/guest_writeback.rs` (new)
- **Status:** fixed, failing test first, under A-192's rule (refused, never masked).
- **Class:** data loss: an acknowledged write by another path reverted, silently.

## Description

A guest kernel caches a file's pages and writes a dirty page back whole, marked `FUSE_WRITE_CACHE`: the program's
stores and the rest of the page as the guest fetched it. A virtio-fs device delivers no invalidation, so the guest is
promised no cache lifetime and negotiates `FUSE_AUTO_INVAL_DATA` (AUD-29-79). When a revalidation then shows another
path's change, Linux drops the file's pages and launders a dirty one first, writing it back. That page carries the bytes
the other path changed, as they were before the change. The device served the write-back as any write, and the other
path's write was reverted.

The host FUSE mount judges such write-backs by per-byte states (A-139/140), and NFS refuses them (A-192). The guest
route shared `serve_write` and judged nothing.

## Reproduction

`a_guests_stale_cache_write_back_never_reverts_another_paths_write` (`crates/bridge-virtiofs/tests/device.rs`), a stand-in
guest over a real volume bridge:
1. the guest creates a file and writes a page of `A`, then reads it back, as its page cache holds it;
2. another attachment replaces the page with `C`;
3. the guest writes its cached page back with one store, `Z`, in byte 0.

Before the fix, 4,095 of the other path's 4,095 bytes read `A`.

## Fix

The device serves every guest request through `GuestWriteback::serve`, which keeps the host mount's byte states
(`PageStates`, `OwnRecords`, `WritableOpens`) for the guest and feeds them itself, since no invalidation round runs:
- a writable open starts its file's states, and the first clears `FOPEN_KEEP_CACHE`;
- an answered read makes the bytes it returned fresh;
- the guest's own records refresh what they wrote;
- every other record, consumed before each request, makes the bytes it touched stale;
- the last writable release drops the file's states.

A write-back holding a stale byte is refused `EIO` whole, A-192's rule; the mask the host mount applies is the inferred
merge banned item 11 forbids for a new transport. One the states cannot judge (a journal that no longer reaches back, a
file lost for room) is refused too. The states are charged to the shard's metadata ledger a page at a time.

## Evidence

- The test above, red as described, then green: the write-back answered `-EIO`, all 4,096 bytes `C`.
- `a_guests_cache_write_back_of_a_fresh_page_is_applied`: a write-back of a page no other path touched, and one made after
  the guest re-read another path's change, are both applied whole.
- slates-bridge-virtiofs and slates-bridge-fuse on macOS arm64. The virtio-fs, FUSE and Shards suites on Linux (container,
  6.12) are recorded in GAPS with the commit.

## Siblings

- A guest across a daemon's takeover: a restarted device starts its states empty, so a page fetched before the restart
  is judged fresh as of the device's first request. This is A-192's open restart case, the same for NFS.
- The guest learns of a refusal only at its next `fsync` or `msync`, as Apple's NFS client does not at all.
