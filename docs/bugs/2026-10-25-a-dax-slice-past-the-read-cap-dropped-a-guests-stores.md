# A DAX copy slice past the read cap dropped a guest's stores

- **Date:** 2026-10-25
- **Status:** fixed 2026-10-25, in the same change as this record.
- **Subsystem:** the DAX backing session's ingest and refill (`crates/bridge-core/src/dax_backing.rs`), over the volume
  bridge's read cap (`MAX_READ` in `crates/bridge-core/src/volume_bridge.rs`, 256 KiB); §4.6, A-163. The session serves
  a Shards guest over the daemon's stream (A-161, A-163) and a crosvm guest over slates' own virtio-fs device (A-168,
  A-169), so both had the bug.
- **Severity:** acknowledged writes lost, and wrong bytes shown. A guest's stores through its DAX map that lay past the
  first 256 KiB of a copy slice never reached the volume at `FSYNC`, which still answered 0; and a refill wrote zeros
  into the guest's map past the first 256 KiB of a slice, where the file holds data.
- **Exposure:** every machine whose derived copy slice is longer than 256 KiB. The slice is the machine's measured
  memcpy rate over half the shard's live step quantum (`DaemonConfig::copy_slice_page_bytes`), so it varies by machine
  and load: CI's ubuntu runner derived one longer than the cap on 2026-10-25; fedora, adacachy and this Mac derived
  shorter ones and never saw it. Present since A-163 (2026-10-24).
- **Found by:** CI's ubuntu lane on `d71ce235` (run 40211222944), two Shards tests; the refill by the sibling sweep and
  a strengthened test.

## Symptom

`crates/server/tests/shards.rs` on CI, `a_guests_stores_through_its_map_reach_the_volume_at_fsync_and_survive_a_restart`:
the store at offset 100 reached the volume at `FSYNC`, the store at 1 MiB + 7 did not.

```
assertion `left == right` failed: after the FSYNC it holds what the guest stored through its map
  left: [118, 119, 116, 114, 115, 112, 113, 118, 119, 116, 114, 115, 112, 113, 118, 119, 116]
 right: [115, 116, 111, 114, 101, 100, 32, 112, 97, 115, 116, 32, 97, 32, 77, 105, 66]
```

and `the_guests_map_and_the_volume_hold_the_latest_write_to_every_byte` shrank to one store past 256 KiB and a sync:

```
Test failed: after step 1 (Sync) the file differs
minimal failing input: acts = [Store { at: 261281, len: 3541, seed: 196 }, Sync]
```

Both passed 5 of 5 on fedora (idle) and on adacachy (load 242–249), in the whole binary and alone: load was not the
condition.

## Root cause

A bridge answers at most its read cap a call: `VolumeBridge::read` takes `size.min(self.max_read)`, 256 KiB. The
ingest's compare (`write_back_runs`) read a slice in one call and compared the backing with as much as that call
returned, by design "a volume shorter than the object's span here (the file cut since) is compared as far as it
reaches", so a capped call passed for the file's end and the rest of the slice was never compared or written back.
The refill (`refill_slice`) read a slice in one call and padded what came back with zeros to the slice's length, then
stored the difference into the map: past the cap, zeros. The initial fill was right: it advances by what a read
returned and reads again.

Reproduced on fedora by forcing the slice to 4 MiB in a scratch copy (the override fired 293 times): both CI failures
came back exactly, the proptest shrinking to `[Store { at: 304215, len: 1071, seed: 58 }, Sync]`. Forcing it to one page
left both passing.

The oracle had run at a degenerate parameter: on every development machine the slice was within the cap, so no test
ever read a slice the bridge cuts short.

## Fix

`read_whole` reads a slice in as many calls as the bridge's cap takes, stopping short only when a call reads nothing
(the file's end), bounded by the slice's length; the ingest's compare and the refill both read through it. The tests
now run each targeted case and every generated history twice, with the slice as the machine derives it and with a
whole 2 MiB range a slice (`Slices::WholeRange`, a memcpy rate past any machine's), and the refill test compares the
whole range rather than its first page.

## Verification

- This Mac: the restart test and the refill test fail in the whole-range regime before the fix and pass after; the
  Shards suite 25 of 25.
- fedora: the three tests pass with the fix; with `read_whole` cut to one call (the old behaviour), all three fail,
  the proptest shrinking to `[Store { at: 296852, len: 2927, seed: 24 }, Sync]` in the whole-range regime.

## Siblings found by the sweep

- **WinFsp** (`crates/bridge-winfsp/src/host.rs`, `Op::Read`) answers a Windows read with one bridge read, so a read
  longer than 256 KiB returns short in the middle of a file; Windows expects a short read only at the end, and a paging
  read cut short may be zero-filled. To measure on Windows; not changed here.
- **FSKit** (`crates/bridge-fskit/src/lib.rs`, `ShimRequest::Read`) does the same with whatever size the kernel asks.
  Not changed here.
- Correct already: NFS sets `eof` from the file's size, not the read's length, so a client reads on after a capped
  read; FUSE and virtio-fs negotiate `max_read` at the cap, so no kernel asks more; the initial fill advances by what a
  read returned.
