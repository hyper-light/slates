# A dirty page in a mount's shared map is written back over a later write through another path

**Status:** open. Root cause confirmed by measurement on Linux and macOS; the fix is a design decision (a recall
before another path's write), so it is not built.
**Severity:** loss of an acknowledged write. Bytes another path wrote, including bytes the map never touched,
revert to their earlier value.
**Found:** 2026-10-21, adversarial testing of the mount route for DAX (`docs/wip/virtiofs.md` §8 item 7).

## What happens

A process holds a writable `MAP_SHARED` map of a file through a slates mount and stores into it. So does a VM's
sharer, for DAX. The store makes the page dirty in that mount's kernel cache, and nothing reaches slates until the
page is written back. If another path writes the same page meanwhile (the SDK, another attachment, another mount),
that write is applied and acknowledged. Then the mount's kernel writes its dirty page back. That is the whole page:
the bytes from before the other write, plus the store. Slates applies it too, and the other write's bytes on that
page are gone.

On one local filesystem this cannot happen. Both writers share one page cache, so the later write overwrites the
store and no byte reverts.

## The evidence

A probe over a real CLI, anchor, daemon and mount, 2026-10-21. A two-page `map.bin` of `A` is mapped `MAP_SHARED`.
One byte `M` is stored through the map at offset 10, with no `msync`. The whole file is then replaced with `S`
through the SDK. After about 200 ms of waiting (on macOS with an `fstat` every 20 ms), slates' own read, then an
`msync`, then slates' own read again:

| | page 0 | page 1 |
|---|---|---|
| expected (one page cache) | all `S` | all `S` |
| macOS, NFS v3, 16 KiB pages, 2 runs | 16,383 `A` + 1 `M` | all `S` |
| Linux, FUSE, arm64 container, 4 KiB pages, 3 runs | 4,095 `A` + 1 `M` | all `S` |

On both platforms the stale page was already in the volume **before** the `msync`:

- **macOS:** the NFS client wrote its dirty page back when the `stat` made it revalidate.
- **Linux:** the stale page arrived within the 200 ms after the SDK's write, with no sync. That fits the kernel
  writing the dirty page back (laundering it) when slates' invalidation of that page arrives, which is the change
  notification `7deddf54` delivers promptly. The kernel's periodic writeback (30 s dirty expiry) is too slow to
  explain three runs of three; the kernel itself was not traced. Before `7deddf54` the notification waited for the
  mount's next request, so the stale page came back later, at the next writeback or sync: the same loss, later.

## Root cause

Slates applies a write from one path without first collecting the dirty state other paths' caches hold for the same
bytes. The design has that protocol for one cache only, an NFSv4 client's write delegation. The recall gate (§4.6
A-79, `crates/vfs/src/recall_gate.rs`) refuses a change to a delegated file until the delegation is returned, and
RFC 8881 §10.2.1 obliges the client to flush its modified data before returning it. So the client's dirty pages
arrive before the other write, not after it.

FUSE and NFS v3 mounts hold write-back state with no recall:

- A **FUSE** mount's only dirty pages are writable shared maps. Slates refuses the writeback cache at `FUSE_INIT`,
  so `write()` is write-through.
- An **NFS v3** client buffers writes and dirty map pages until it commits, and the protocol has no way to call it
  back.

## Impact

Affected:

- **DAX through a mount.** A guest's DAX stores are dirty pages of the host's mapping until written back, so a
  host-side writer of the same file (an agent through the SDK, a tool through another mount) can have its
  acknowledged bytes reverted.
- **Two host processes sharing a file across paths,** when one writes through a shared map.

Not affected:

- Writers that all go through one mount (one page cache).
- `write()` through a FUSE mount (write-through).
- An NFSv4 client holding a delegation (the recall gate).

## What a fix needs (a design decision, for Ada)

- **Linux FUSE: a recall before the write.** Take a write from another path to a file a FUSE mount may hold dirty
  pages of; an open for writing on that mount is the conservative test.
  1. Park the write.
  2. Send the mount an invalidation of the range. The kernel writes dirty pages back during it; those are the
     earlier writes, and they are applied first.
  3. When that round completes, apply the parked write and invalidate again.

  A store between the round and the apply would dirty the page again (its fault reads the bytes from before the
  write). So the mount's reads of a range under a parked write must wait for the apply too. The machinery, writers
  parked on a FUSE mount's delivery, is the same that the open AC-3.3 question needs (`docs/wip/GAPS.md`,
  2026-10-21).
- **macOS NFS v3: no server-side fix exists.** A v3 client cannot be called back. Either the macOS mount moves to
  NFSv4 with delegations, where the recall gate already does this (A-32 keeps it on v3 today), or a writable shared
  map of a file other paths also write is recorded as unsupported on macOS.

## Reproduction

Over the `MappedThroughAMount` fixture in `crates/cli/tests/cli.rs` (`0106c51f`), with `SLATES_TEST_CLI=1`. The
probe was run and removed; it lands as the fix's failing test.

```rust
let mut fixture = MappedThroughAMount::with_pages("mapdirty", b'A', 2);
fixture.view()[10] = b'M';               // a store, left dirty: no msync
fixture.replace_through_slates(b'S');    // a later write through another path, acknowledged
for _ in 0..10 {
  if !cfg!(target_os = "linux") {
    let _ = rustix::fs::fstat(&fixture.file);
  }
  pause();
}
let before_sync = fixture.volume_bytes(); // page 0 already stale: 'A' except the 'M'
// msync(map, length, MS_SYNC), then fixture.volume_bytes() again: unchanged.
// Correct: every byte 'S'.
```

## Siblings (reported, not checked here)

- The direct virtio-fs device (`crates/bridge-virtiofs`) and WinFsp: whether a guest's or a Windows client's
  write-back state has the same hazard.
- AC-3.3 (the mount told after the writer is answered): the same "park a writer on a mount" machinery.
