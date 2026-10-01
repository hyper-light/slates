# A directory page did whole-directory work (AUD-29-86)

**Date:** 2026-10-01. **Audit:** AUD-29-86 (P2). **Design:** §4.5 (directory structures), §4.6 (FUSE, NFS).

## Description

`VolumeBridge::readdir` materialized every row of the directory (`Volume::readdir_no`), then skipped `offset` of
them and allocated the tail, for every page. FUSE and NFS asked for pages from a positional cookie, so a listing of
`n` entries in pages of `k` did Θ(n²/k) row visits and allocations, none charged to the reply's credit. The §7.6
witness: 64 children, 66 rows built, one entry returned. The positional cookie was also wrong under concurrent
unlinks: removing an entry already returned moved every later entry down one, so the next page skipped a survivor
(POSIX requires every entry not removed to be returned exactly once).

## Root cause

The volume core offered only whole listings, and cookies were positions over them.

## Exact edits

- `crates/vfs/src/dirtree.rs`: `Tree::iter_from_hash` (one descent to the first hash at or above a value).
- `crates/vfs/src/dir.rs`: `DirNode::iter_from_hash`; the cookie (`dir_cookie`: top 31 bits of the hash, never
  below 3; `resume_hash`; `FIRST_CHILD_COOKIE`, `COOKIE_BITS`).
- `crates/vfs/src/volume.rs`: `DirRow::hash`; `readdir_page`, `readdir_page_no` (a limit, then the last cookie's
  group); `crates/vfs/src/base.rs`: the overlay's `readdir_page_no` after the base merge (`merge_base`).
- `crates/bridge-core`: `DirEntry::cookie`; `Bridge::readdir(object, cx, fh, cookie, limit)`;
  `whole_cookie_groups`.
- `crates/bridge-fuse/src/bridge.rs`: READDIR and READDIRPLUS ask for what the room can hold plus one, carry each
  entry's cookie, cut between cookies, answer `EOVERFLOW` for a group larger than the page;
  `crates/bridge-fuse/src/reply.rs`: `DirBuffer::dirent_len`, `plus_len`.
- `crates/bridge-nfs/src/procedures.rs`: the same for READDIR/READDIRPLUS (`NFS3ERR_TOOSMALL` for an oversized
  group; `eof` when the bridge had fewer entries than asked and all were sent).
- `crates/bridge-fskit/src/lib.rs`, `crates/bridge-winfsp/src/host.rs`: whole listings, as before (FSKit's wire
  resumes by position).

## Why 31 bits

A 32-bit process's `getdents`/`telldir` carry a signed 32-bit offset; Linux refuses a larger one `EOVERFLOW`
(ext4 hands such a process 32-bit hashes for the same reason, `fs/ext4/dir.c` `is_32bit_api`). i686 is a tested
target. Names sharing the 31 bits share a cookie (about n²/2³² pairs in n names), which the page rule covers.

## Proof

The tree's oracle over splits and merges; the bridge's paged listing (every entry once), unlinks between pages
(every survivor once, no repeats), a cookie-sharing pair never split; FUSE pages resumed from the kernel's offsets
and `EOVERFLOW`; the macOS kernel NFS mount (CLI 13/13) and the Linux kernel FUSE mount and CLI (15/15, Docker, as
an ordinary user). The unlink test states the rule; the positional implementation it replaced is not rerun.

## Carried

An overlay page repeats the base merge check (Θ(base entries) per page); the order is FNV-1a, unkeyed, so a writer
can craft a large cookie group; no per-page work counter.
