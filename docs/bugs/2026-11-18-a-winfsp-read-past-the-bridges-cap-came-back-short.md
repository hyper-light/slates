# A WinFsp read past the bridge's cap came back short

Date: 2026-11-18. Area: `crates/bridge-winfsp/src/host.rs` (`run`, the `Read` arm). Design: §4.6 "Windows". Found by
reading, by the sweep that followed `docs/bugs/2026-10-25-a-dax-slice-past-the-read-cap-dropped-a-guests-stores.md`
(GAPS: "Open, siblings found by the sweep"). Goal 83, every open slates bug.

## Description

A Windows program reads more than 256 KiB of a file through a WinFsp drive in one call, for example a 1 MiB
`ReadFile` from an unbuffered handle. The read returns 256 KiB, though the file goes on.

- **A program reading in a loop.** It takes a short read for the file's end, and a copy stops there: the copy
  is truncated, with no error anywhere.
- **A paging read from Windows' cache manager or a mapped view.** The pages past the short reply are filled
  with zeros, so a cached reader sees zeros where the file holds data.

## Root cause

The host answered a Windows read with one call to the bridge: `bridge.read(object, cx, offset, length, &mut out)`. A
bridge answers at most its read cap a call: the volume bridge's `MAX_READ`, 256 KiB. The cap is a property of each
call, and a caller that needs more must call again. The DAX backing path learned this on 2026-10-25 and reads through
`slates_bridge_core::dax_backing::read_whole`. WinFsp's host still took one call's bytes for the whole reply.

FUSE and NFS are not affected.
- FUSE: `init::max_pages` holds a request to `MAX_WRITE`, 256 KiB in host pages on any page size, which is the bridge's
  cap.
- NFS: a client asks again from where a short `READ` without `eof` ended.

## Impact

Every WinFsp drive, for any read longer than 256 KiB:
- Unbuffered reads, which Windows passes through at the size the program asks.
- Paging reads, which the cache manager and mapped views make in large runs.

Data read through the drive could come back truncated or zeroed. The volume itself held the right bytes throughout.

## Exact edits

- `crates/bridge-winfsp/src/host.rs`: the `Read` arm calls `read_whole`, which reads in as many bridge calls as the cap
  takes and stops short only at the file's end (a call that reads nothing).
- `crates/cli/tests/winfsp.rs`: `a_read_longer_than_the_bridges_cap_returns_every_byte_asked_for`.
- GAPS: the sweep's line, now fixed.

## Tests

`a_read_longer_than_the_bridges_cap_returns_every_byte_asked_for` (`crates/cli/tests/winfsp.rs`, gated
`SLATES_TEST_CLI=1` and `WINFSP_TEST_MOUNT=1`).
- **Do:** write a file of 1 MiB and a page through a writable drive, each byte a function of its offset. Open it
  unbuffered and read 1 MiB from its start in one call.
- **Expect:** 1,048,576 bytes, each the file's. Without the fix the read returns 262,144 bytes.
- **Where it runs:** CI's Windows lanes only, since it is a live-drive test. The red and green runs are filed for
  main (`run-requests.md`, 2026-11-18). The change was compile-checked for Windows with `cargo xwin check`
  (`x86_64-pc-windows-msvc`: `slates-bridge-winfsp` with its tests, and `slates-cli`'s `winfsp` test).

## Siblings

- FSKit's shim makes the same single call, but it is not shipped: slates never mounts through FSKit.
- No other transport answers a read from a single capped bridge call:
  - the Shards stream and the vhost-user device answer FUSE reads within the negotiated read size;
  - NFS's `READ` caps its count at the transfer size, and its client continues past a short reply.
