# A pinned large-class file was refused across its extents

Found 2026-11-04 by the many-client load test (`crates/client/examples/load_bench.rs`), the first time a green was
created over a real tree: Linux v7.4-rc1's `kernel/` directory (754 files, 16,321,721 bytes). Design: §4.16 and the
A-9 integration requirement (a green starts from a complete immutable base), §4.15 (pinned base bytes).

## Description

`pin(overlay, None)` answered 754 entries pinned and `snapshot_is_complete` held, yet `CreateGreen { base }` was
refused `BaseUnavailable { path: "", errno: 0 }`. The origin walk (`walk_origin` in `crates/server/src/merge_service.rs`)
reads each file whole from the snapshot.

## Root cause

A large-class file (above the base's large-class bound) is pinned as several extents. Both base-backed read paths in
`crates/vfs/src/volume.rs` (the head's `read_body` and the snapshot's `read_in`) served a read only when one pinned
extent covered the whole range asked for, so any read across an extent boundary was refused `BaseUnavailable(0)`.
The coverage check that makes a snapshot complete already counted the union, so the two rules disagreed.

## Impact

Every green over a base holding a file larger than the large-class bound was refused (the kernel tree has many:
`kernel/bpf/verifier.c` is 739,748 bytes). At the head the same check sent such reads to the host instead of the
pinned bytes.

## Edits

- `crates/vfs/src/volume.rs`: `covers(extents, start, end)`, whether the extents' union covers the range, used by both
  read paths in place of the single-extent test.
- `crates/vfs/tests/coverage.rs`: `a_pinned_large_class_file_reads_whole_from_its_snapshot_and_head` (failed with
  `BaseUnavailable(0)` before the edit; passes after).
