# A sparse file was exported and restored dense

**Date:** 2026-10-01. **Area:** `slates-vfs` (export), `slates-archive` (restore), `slates-server` (takeover
rebuild, merge inputs). **Audit:** AUD-29-57 (P2). **Design:** §4.5 content, §4.11 archive (holes are zero
extents, D-17), R8.

## Description

A sparse file — a large length with little data — cost its logical length at every step after a seal:

- the export walked and hashed every byte up to its end, holes included;
- restore expanded every extent, holes too, into one dense buffer of the file's length;
- the takeover wrote that buffer through `Volume::write`, so the successor stored zeros as data.

A volume whose sparse state fit its bound on the origin could fail its takeover (`BudgetExceeded`) or force a
logical-size allocation, and `SEEK_DATA`/`SEEK_HOLE` on the successor found data where the origin had holes.

## Root cause

- The exporter's cutter read `[0, size)` in chunk-sized pieces. Nothing consulted the body's data map,
  although the volume had one (`data_ranges`, used by `seek`).
- Restore's output type was `Vec<u8>` per file, so a hole could only be zeros.

## Fix

- **Export (`crates/vfs/src/export.rs`).**
  - The body cutter takes the body's data ranges at the snapshot (`Volume::data_ranges_in`) and visits only
    the chunk windows holding data.
  - Each such window is still cut as one chunk at its aligned offset, so dedup is unchanged.
  - The extent map is exact: one data extent per data range in the window (naming its slice of the chunk
    through `chunk_offset`), and one hole extent per gap, the trailing one included.
  - A body with no data is its hole extents alone. Attribute values go through the same cutter.
- **Restore (`crates/archive/src/restore.rs`).**
  - A file restores as `RestoredFile { len, pieces }`: each data extent's bytes at its offset, contiguous
    pieces merged (fallibly).
  - The budget admits a file's data bytes, not its length.
  - An attribute value restores dense, admitted at its length.
- **Takeover (`write_sparse` in `crates/server/src/verbs.rs`).** It writes only the pieces, then truncates to
  the length, so holes stay unwritten and uncharged. A symlink reads its target dense; the merge service
  reads its small inputs entry dense.

## Tests

- `a_sparse_file_keeps_its_holes_through_export_restore_and_rebuild` (server, in-process, the takeover's own
  rebuild). A 32-window file holds three islands: one window-aligned, a small write inside a window, one
  straddling a boundary.
  - The rebuilt file's bytes, length, `SEEK_DATA`/`SEEK_HOLE` map and physical charge equal the origin's.
  - The export hashed exactly the 4 windows holding data. That is the non-vacuity counter: under the old
    cutter it would equal all 32.
  - The restore carries exactly the origin's data map.
- `a_huge_hole_restores_as_its_length_without_allocating_it` (archive). A one-terabyte hole restores as its
  length with no piece and no chunk decoded. It replaces the test that asserted the old dense behaviour,
  `OverBudget`.
- `a_taken_over_archive_lands_at_its_offsets_and_an_oversized_one_is_refused` (server):
  - a gibibyte hole under a 1 MiB bound now takes over at its length with nothing charged;
  - the oversize refusal is kept with real data: two mebibyte extents under the same bound, refused
    `BudgetExceeded`.
- The export, archive, cluster, recovery and fleet takeover suites pass unchanged in meaning. Dense files cut
  exactly as before: one data range covers them.

## Siblings reported

- **Hole punching.** The volume has no hole punch, and NFSv4.2 `DEALLOCATE` is not served
  (`crates/bridge-nfs/src/v4/v42.rs`). The audit's "punch holes" leg cannot be exercised until it exists.
- **Inline granularity.** A small file is held inline, and an inline body is data from offset zero. The
  origin's own data map says so too, and the successor matches it exactly.
