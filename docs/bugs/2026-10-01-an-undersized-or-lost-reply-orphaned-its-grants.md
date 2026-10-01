# An undersized or lost reply orphaned what it granted (AUD-29-85)

**Date:** 2026-10-01. **Audit:** AUD-29-85 (P1). **Design:** §4.6 (the FUSE dispatch, virtio-fs), §4.8 (the
barrier before a mutation's reply).

## Description

- **Undersized room.** The virtio-fs device checked only that a chain's writable buffers held the 16-byte reply
  header, then dispatched. A CREATE created the file, opened a handle and took a lookup reference, and only then
  found its 160-byte reply did not fit; the device answered `EIO`. A WRITE with the same room changed the bytes
  and then answered `EIO`. The guest never learned the node id or the handle, so it could never send the
  FORGET or RELEASE: each retry consumed the bounded handle and reference budget until teardown.
- **Directory pages.** READDIR and READDIRPLUS sized their page by the request's `size` alone, not by the room
  posted, and READDIRPLUS referenced each entry before asking whether it fit (undoing it after).
- **Lost replies.** The same orphaning happened wherever a success reply was replaced or dropped after the
  effect: the daemon's owner turn answers `EIO` when the §4.8 barrier is refused (a refused `mkdir` kept its
  lookup reference); the kernel answers `ENOENT` to a reply whose caller was interrupted (and the daemon treated
  that as a failed mount); a scatter into guest memory can fail after dispatch.

## Root cause

The dispatch assumed the reply buffer was always large enough (true of `/dev/fuse`, where the server sizes it),
and no path owned the release of what a reply granted once the reply was gone.

## Exact edits

- `crates/bridge-fuse/src/bridge.rs`: `success_reply_bytes` (the fixed reply sizes); the dispatch refuses below
  it before any effect; `body_room` clamps reads and directory pages; `reclaim_unreported` forgets the entries
  and releases the handle a success reply named (entry replies, CREATE, OPEN/OPENDIR, every non-synthetic
  READDIRPLUS entry).
- `crates/bridge-fuse/src/reply.rs`: `DirBuffer::fits_plus`, asked before the reference is taken.
- `crates/bridge-fuse/src/channel.rs`: `write_reply` returns `Sent` (`Unmatched` on the kernel's `ENOENT`, no
  longer an error); the blocking loop reclaims an unmatched reply; `reclaim_dispatched` for owners;
  `Dispatched` keeps the request's node id.
- `crates/server/src/fuse.rs`: a refused barrier reclaims before answering `EIO`; an unmatched reply is counted
  and reclaimed, the mount kept; counters `fuse.reply_unmatched`, `fuse.reply_reclaimed`.
- `crates/bridge-virtiofs/src/device.rs`: the room check before dispatch (counted `replies_truncated`); a
  failed scatter reclaims (counted `reclaimed`); `scatter` slices without indexing.

## Proof

- `crates/bridge-fuse/tests/dispatch.rs` `a_reply_with_no_room_refuses_before_any_effect`: CREATE, LOOKUP and
  WRITE with header-only room answer `EIO` with no seam call, no reference and unchanged bytes; a READDIRPLUS
  page fits its room with no reference taken. Red before the fix (the CREATE reached the seam).
- `reclaiming_an_unreported_reply_gives_back_exactly_what_it_granted`: every reference a page took is
  forgotten, never "." or ".."; a CREATE's reference and handle; an error reply gives back nothing.
- `crates/bridge-virtiofs/tests/device.rs`: a 16-byte CREATE leaves no file and its retry succeeds; a CREATE
  whose reply memory fails the write gives back two grants and the unlinked inode is reclaimed.
- `crates/bridge-fuse/tests/owner_turn.rs` on a real Linux kernel mount (Docker, as an ordinary user): the
  refused `mkdir`'s reference is given back (`Counts { …, refused: 1, reclaimed: 1 }`).

## Carried

AUD-29-86 (whole-directory work per page outside wire credits) is its own item: the page is now bounded by the
room, but the bridge still materializes the directory per page.
