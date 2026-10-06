# A deleted file's RAM never went back to the OS

Date: 2026-10-06.
Area: `crates/mem` (the arena's release, the sparse object) and `crates/server/src/write_log.rs`. A-99, A-105.
Condition: memory efficiency on a long-running daemon.

## Description

An adversarial container run on Linux FUSE-mounted a volume and wrote then deleted large files. The anchor's content
memfd (`/memfd:slates-con-*`) never shrank:

- after a 128 MiB file was written and deleted, the memfd still had 168 MiB allocated, measured three seconds later;
- after a 64 MiB file was written and deleted, the memfd had 128 MiB allocated, before and after the delete.

The figures are the memfd's `st_blocks` from `stat -L /proc/<daemon>/fd/N`.

## Root cause

A-99 zeroes every released block (zero on free, as Linux's `init_on_free=1` does), so a deleted file's plaintext
cannot stay in RAM. The zeroing was `fill(0)`: it writes zeros over every page, which keeps every page resident and
spends CPU writing the zeros. The daemon's footprint therefore only ever grew to its high-water mark.

The FUSE write log (A-63) lives in the same object and holds a second copy of every logged write. `clear` scrubbed it
by writing a zero buffer as long as everything logged, with the same effect: 64 MiB written left 128 MiB resident.

## Edits

- **`Region::discard` and `SparseObject::discard`** give a range's whole pages back to the OS: `MADV_REMOVE` on the
  shared content object (a hole punch every mapping sees), `MADV_DONTNEED` on a private region. Not
  `fallocate(PUNCH_HOLE)`, because xtask allows file writes only in `slates-land` (R1).
- **On free, a block is still zeroed in place (A-99)**, and its bytes are counted as resident free memory.
- **`ChunkArena::purge(max_blocks)`** gives free blocks of at least `discard_from_bytes` back, resuming from a cursor.
  It is run by `daemon::purge_if_idle` at a reap tick when the content arena allocated nothing since the last tick,
  in slices with a yield between them. The count is `content.purged_bytes`.
- **`WriteLog::clear`** zeroes in place a page at a time, with no buffer as long as the log. `WriteLog::purge` gives
  the emptied record area back on the idle tick, once per emptying.
- **`discard_from_bytes`** is a new machine-profile constant: the smallest measured `memcpy` whose time exceeds one
  syscall. It is printed by `slates profile` and carried by `DaemonConfig`.
- **Why decay rather than give-back at every free (the first cut):** allocating, touching and freeing a 64 KiB block
  cost 0.32 µs zeroed in place against 6.8 µs given back and faulted in again (`mem_bench` on Linux, three runs). The
  first cut's FUSE churn p99 medians came out at 578 and 592 µs against HEAD's 325 and 318, though that run was too
  loaded to attribute them. This follows jemalloc's decay-based purging.

## Tests

- **`a_deleted_files_memory_goes_back_to_the_os`** (`crates/server/tests/recovery.rs`, Linux): write a 256-chunk file,
  publish, delete it, and leave the shard idle. Within the bound, most of its bytes must have left the object's
  allocation (`SEEK_DATA`/`SEEK_HOLE`).
  - Red before the change: 16,986,112 bytes allocated before the release and 16,986,112 after.
  - Green after.
- **`a_deleted_files_plaintext_leaves_the_content_object`** and **`a_cleared_log_keeps_none_of_the_bytes_it_logged`**
  stay green, so the scrub guarantee holds.
- **`an_idle_purge_resumes_across_calls_and_leaves_freed_bytes_zero`** (mem): the purge resumes across calls, freed
  bytes read zero, and live blocks keep theirs.
- **Mutation check:** with `purge_if_idle` disabled, the memory test fails again with 16,986,112 bytes before and after.
- **A/B against HEAD**: the numbers are in BENCHMARKS.md.
