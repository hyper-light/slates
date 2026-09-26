# Windows committed the whole anchor segment at creation

Date: 2026-09-26. Contracts: §4.8 (the anchor segment and content object: RAM the anchor owns), R1,
R8 (one semantics on every OS), D-12. Found by CI runs 36202635768 and 36260818113 (native Windows,
`the_typed_verbs_drive_the_lifecycle_and_refusals_are_typed`).

## Symptom

```
called `Result::unwrap()` on an `Err` value: Anchor(Memory(OsRefused { call: "CreateFileMappingW", code: Some(1450) }))
```

1450 is `ERROR_NO_SYSTEM_RESOURCES`. The daemon never started on a Windows runner.

## Root cause

The anchor geometry sizes the segment by the design's bounds, not by what it holds:
- a log ring per partition, the recovery budget in microseconds × one page (16.4 GB on this 128 GB
  Mac; 4 GB in a 1 GiB KIND pod);
- two snapshot slots per partition, each twice the table share;
- the audit, profile and landing regions.

`DaemonConfig::derive` on this Mac gives 167–188 GB for one to four shards.

A Linux `memfd` or macOS `shm_open` object is backed page by page as it is touched, so that layout
costs what it holds. A Windows pagefile section created `PAGE_READWRITE` is charged its whole size
against the commit limit (RAM + pagefile) at creation. On a 16 GB runner the creation is refused. The
content object (`reserve × 2 × partitions`) has the same shape.

## Fix

- **`slates_mem::SparseObject`**: the anchor segment's and content object's type.
  - On Windows it is a `SEC_RESERVE` section whose pages are committed (`VirtualAlloc(MEM_COMMIT)`) as
    ranges are reached. A per-process bitmap of allocation granules means a warm range costs no system
    call.
  - On Unix it is the same `memfd`/`shm_open` object as before.
  - A reserved page cannot be read until committed, so the type has no whole-object view. Every access
    is `range`/`range_mut`, or a word through `atomic_u64`/`atomic_u32`, each committing its span
    before the slice exists.
  - It is `!Sync` on every platform, so a cross-thread use cannot compile on one OS only.
- **The anchor segment** (`crates/anchor/src/segment.rs`):
  - `region_bytes`/`region_bytes_mut` (whole-region slices of a 16 GB ring) are replaced by
    `region_read`/`region_write`, which take an offset and length and refuse a span past the region.
  - `region_len` answers the length.
  - The header, the supervision block, published payloads and the issuer secret go through ranges.
  - The unused `AnchorSegment::lock` is removed.
- **The op-log ring** (`crates/db/src/record.rs`) writes and reads each record by range, splitting at
  the wrap.
- **The recovery image** (`crates/vfs/src/recover.rs`) reads and publishes its double-buffered slots
  through `ImageRead`/`ImageWrite`: the header first, then the payload it names. Byte slices implement
  both, and the daemon implements them over its slice of the content object.
- **Unsafe budget, slates-mem 16 → 19.**
  - New sites: `GetSystemInfo`, `VirtualAlloc`, and the two committed-range views.
  - One shared `word_view` replaced the two per-width atomic views.

## Evidence and limits

- New test: `a_sparse_object_is_reached_by_ranges_and_seen_through_a_second_mapping` (a 1 GiB object,
  a range written near its end, read through a second mapping; a span past the end refused).
- The mem, anchor, db, vfs recover (33), server lib (105), server daemon (14), server recovery (6) and
  client suites pass on macOS; the workspace lints clean.
- Clippy for `x86_64-pc-windows-msvc` is clean for mem, rt and ipc. The anchor needs a C toolchain
  this host lacks; native Windows CI lints it.
- The Windows lifecycle is proven only by the next Windows run. Other up-front commits may remain,
  such as memmap2's anonymous regions for the store's arenas.

## For Ada: the log ring is bounded by recovery time, not by memory

`log_bytes_per_partition` is `recovery_budget_us × one page per microsecond` ("until the first replay
measures"): 16.4 GB per partition here, 4 GB in a 1 GiB KIND pod. `trim` moves the ring's head but
frees nothing, so as the ring wraps, a long-running daemon touches all of it. On Linux and macOS it
would hold that much RAM, past the memory budget the rest of the daemon is sized by (RAM-only, D-12).
The snapshot slots are bounded by the table share. The log's bound belongs to the design; this change
does not alter it.

## Edits

- `crates/mem/src/{shared,lib}.rs`, `crates/anchor/src/segment.rs`, `crates/anchor/tests/anchor.rs`,
  `crates/db/src/record.rs`, `crates/db/tests/model.rs`, `crates/vfs/src/recover.rs`,
  `crates/server/src/{state,daemon,verbs,retention}.rs`, `crates/server/tests/recovery.rs`,
  `unsafe-budget.toml`, `docs/wip/TBD_FIXES.md`.
