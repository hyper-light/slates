# The memory report's heap row counted the large blocks' spans as resident (2026-11-22)

## Description
main's run r41 read a separate daemon's `slates status --memory` heap row as "reserved_bytes=1331986432
resident_bytes=1305079984" with 12 mappings, and took it as about 1.3 GB of freed heap the system allocator kept. That
sent the work toward an idle heap give-back (`malloc_zone_pressure_relief`), already measured and rejected in
`crates/mem/src/large.rs` (it released 0 bytes). GAPS had recorded the symptom on 2026-11-21 ("the heap row counts
allocated, not resident, bytes: ~920-960 MiB on an idle daemon whose RSS is 35-87 MiB", not traced).

## Root cause
`daemon_memory` (`crates/server/src/verbs.rs`) built one `heap` row from `slates_mem::heap::stats()`:
- reserved: malloc's `size_allocated` plus the large path's live mapped bytes;
- resident: malloc's `size_in_use` plus the same live mapped bytes.

The large path's mapped bytes are its live blocks' whole page-rounded spans: address space, which is in RAM only once
written. r41's numbers split cleanly. reserved − resident = 26.9 MB, all the system allocator held past its live blocks.
`size_in_use` ≈ the gauge's `system_live_bytes` (31 to 60 MB). The remaining ≈1.27 GB was the 12 live large blocks'
spans, of which `mem.gauge.process.mapped_resident_bytes` read 13.7 MB in RAM.

## Impact
The report called address space resident, so an operator (and main) read a daemon holding about 1.3 GB that it did not
hold, and chased a give-back that would release nothing. RAM itself was not lost. The spans remain a finding of their
own: allocations sized from the machine rather than the work (main's (b2); Windows charges commit for them).

## Edits
- `crates/mem/src/heap.rs`: `HeapStats` carries its parts: `system_reserved`, `system_in_use` and `mapped_bytes`
  (the large spans). `reserved` and `in_use` keep their meaning for their other callers.
- `crates/server/src/verbs.rs`: `daemon_memory` reports two rows. `heap` is the system allocator: reserved its allocated
  size, resident its size in use, no mappings. `large` is the blocks mapped on their own: mappings the live blocks,
  reserved their spans, resident their pages in RAM (`slates_mem::large::heap().mapped_resident`), read only when the
  status asked for the memory report. Test: `verbs::tests::the_memory_report_counts_a_large_blocks_written_pages_never_its_span`.
- `crates/ipc/src/protocol.rs`: `MemoryKindReport`'s docs name the two rows.
- `crates/cli/tests/git_interop.rs`: `MEMORY_KINDS` holds `large`.
- `crates/cli/tests/svn_interop.rs`: `an_import_measured`'s live heap sums the `heap` and `large` rows' resident, so its
  held bound counts the large blocks by their written pages.
- `docs/wip/SLATES_DESIGN.md`: the memory-by-kind sentence names both rows.

## Evidence
- Before, the MacBook, main 41eabe8f8: the new test against the single-row logic fails at its first assertion ("the
  report has a large row"); by the code it replaced, that row's resident added the untouched block's whole span.
- After: the test passes; `slates-mem --lib heap` 1/1; `slates-server --lib memory` 9/9; `slates-ipc --test status` 3/3;
  `slates-cli --test git_interop a_status_during_a_push_reports_the_daemons_memory_by_kind_while_the_anchor_keeps_it`
  1/1 (a real push, every status's six kinds once, resident within reserved).
