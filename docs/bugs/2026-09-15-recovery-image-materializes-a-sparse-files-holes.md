# The recovery image materialized a sparse file's holes, so a truncate to a petabyte aborted the daemon at the barrier

Date: 2026-09-15
Area: `crates/vfs/src/recover.rs` (`Volume::file_body`, `BodyImage::File`, `fill_content`); reached
through `crates/server/src/verbs.rs::publish_shard` at the NFS barrier (`crates/server/src/nfs.rs`)
Severity: a client-chosen file size sized a heap allocation in the daemon (banned item 8, the
no-panic law): one `truncate(2)` from an ordinary user took the daemon down, and kept it down.

## Symptom

The conformance harness's pjdfstest run stalled at file 79 of 238 with `ETIMEDOUT` from the mount;
the daemon's log held `memory allocation of 999999999999999 bytes failed` 109 times, each followed by
the anchor's restart. Reproduced with one command through a live `slates mount`:

```
$ pjdfstest create f 0644                       -> 0
$ pjdfstest truncate f 999999999999999          -> ETIMEDOUT   (the daemon is gone)
```

pjdfstest's `truncate/12.t` and `ftruncate/12.t` extend a file to 999,999,999,999,999 bytes and
accept either `EFBIG`/`EINVAL` or a sparse file of that size — never a dead server.

## Root cause

The volume itself handled the extension correctly: `Volume::truncate` records the new length and
allocates nothing for a hole (`apply_truncate` returns before touching content when `len >= size`;
holes read as zeros). The allocation was one layer up. The D-18 barrier after every NFS mutation
publishes the shard's recovery image (`publish_shard` → `Volume::to_image` → `capture_tree` →
`image_of_inode` → `file_body`), and `file_body` imaged a file as **one vector of its logical
length**, filled through the read path:

```rust
let size = usize::try_from(inode.attrs.size)?;
let mut bytes = vec![0u8; size];        // 999,999,999,999,999 bytes for a hole
```

so a hole that cost nothing in the volume cost its whole span in the image. The restart loop
followed from the design working as intended: the op log replays the journaled `Op::Truncate` onto
the last good image at every boot, restoring the size, and the first publish after recovery hit the
same allocation.

Besides the abort, the dense body made every image O(logical size) rather than O(bytes held), and a
file with a hole in the middle carried the hole's zeros.

## Fix

`BodyImage::File` now carries the file's **held runs** — `RunImage { offset, bytes }`, ascending and
non-overlapping — and nothing for a hole; the inode's size (already in the image's attributes)
bounds the last hole. `file_body` walks the body's held spans (`held_spans`: the inline bytes, the
chunk-backed sealed extents and the open extent, coalesced where they touch so an identical file
images identically whatever its chunking; a zero extent is a hole) and reads each through the same
read path as before, so inline, sealed and open bodies are still captured the same and a snapshot's
frozen bytes still come from `read_in`. Restore (`fill_runs`) writes each run at its offset, refusing
a run that is empty, out of order, overlapping or past the size as `RecoveryIncomplete` — a corrupt
image is a typed refusal, as the module's `Wire` discipline requires — and the write path keeps the
restored size, so the trailing hole survives. The dedup crc covers the body's wire form, since a
run's offset is part of the content. `IMAGE_VERSION` is 3; the image lives in anchor-owned RAM for
the daemon's lifetime, so no older image is ever read by the new decoder.

## Verification

- `crates/vfs/tests/recover.rs`: `a_sparse_file_images_as_its_held_runs_not_its_length` (four bytes
  held, truncated to 999,999,999,999,999: one run, the size, an image under 4 KiB — where the
  materialized hole was a petabyte — and a rebuild that serves the bytes, the size and zeros in the
  hole) and `a_file_with_a_middle_hole_images_as_two_runs` (an 8 MiB hole costs nothing and survives
  the rebuild); the existing capture, snapshot, hostile-image and determinism tests updated to the
  run form and passing.
- The CLI's live mount flow (`crates/cli/tests/cli.rs`, `a_sparse_extension_through`) extends a file
  to 999,999,999,999,999 bytes through a real `mount_nfs`, reads the size back, and shows the daemon
  still answering `status` after the barrier that imaged it — the exact operation that killed it.
- The conformance pjdfstest run completes all 238 files with no `ETIMEDOUT` (the record in the same
  change).

## Sibling sweep

- `BodyImage::Base`'s `PinnedImage` already had the run shape (`bytes: None` for a zero extent), so
  base-backed files never materialized their holes; the file body is now the same shape.
- `Volume::read` into a caller's buffer is bounded by that buffer, and the NFS `READ` by the transfer
  cap; no other path sizes an allocation from a file's logical length (grep of
  `vec![0u8;`/`with_capacity` on the image, barrier, seal and recovery paths).
- The NFS export advertises `maxfilesize = u64::MAX` (FSINFO), which is now true in the sense that
  matters: any size is a valid sparse length and costs what it holds.
