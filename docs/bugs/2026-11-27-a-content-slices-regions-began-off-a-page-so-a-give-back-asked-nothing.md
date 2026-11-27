# A content slice's regions began off a page, so a give-back asked nothing

- **Date:** found 2026-11-27 (main's diag CI 2, run 45986066740 at 89f01a607: windows-latest x86_64, job 140461887964,
  and windows-11-arm, job 140461888146); diagnosed and fixed the same day (w-perf, main's ruling of 05:00 on the
  layout).
- **Status:** fixed in `crates/server/src/config.rs` (the layout) and `crates/server/src/daemon.rs` (the give-back's
  top end); the anchor's layout version 21.
- **Subsystem:** a shard's slice of the anchor's content object (A-64, A-68) and the recovery memory's give-back
  (A-117, A-926).
- **Severity:** a red test on both Windows lanes, and recovery memory a give-back could not ask back there: the page
  at each region's start, and, over a whole dead memory, every one-page step of the give-back until its learned slice
  grew past a page.

## Symptom

```
daemon: dead Some(DeadRanges { stale_slot: (0, 46), log_past_tail: (0, 73) }), 0 bytes of whole pages asked back a
page a step (0 taken by the OS), 0 left
panicked at crates\server\src\daemon.rs:11945:5: whole pages asked back, not only the extents fallen
```

`daemon::tests::a_one_page_give_back_takes_the_pages_of_extents_that_end_off_a_page` asked no whole page of the OS on
Windows (`recovery.asked_bytes` 0), where it asked 16,384 bytes on the MacBook.

## Root cause

`DaemonConfig::shard_content_layout` lays a shard's slice out from its reserve: the write log is a quarter of the
reserve (`write_log_bytes`), then two slots of the reserve's length, then the delta log of one more. Nothing rounded
them to pages. A Windows runner's reserve was 5,724,786,688 bytes (its effective capacity over one shard and its memory
classes), no multiple of four 4 KiB pages, so the write log was 1,431,196,672 bytes, 349,413 pages and 1,024 bytes,
and the slots and the delta log began 1,024 bytes past a page. A dead range of a few dozen bytes at a slot's start,
rounded up to its page, held no whole page: the page holding its first byte is shared with the region before it, so
asking nothing was right, and the test's premise (the layout's regions on pages) did not hold there. The MacBook's
delta log was off its 16 KiB page too: the same test asked one page of two (16,384 bytes) where both ranges now give
one (32,768).

A second effect on any such layout: over a whole dead memory (a restart's), the give-back's dead range ended at the
memory's own end, off a page, so every one-page piece of its top-down walk straddled two pages and none was asked
until the learned slice grew past a page.

## Impact

On a machine whose reserve is no multiple of four pages (most, since the reserve is derived from memory), the slots'
and the delta log's first pages could never go back to the OS, and a restart's give-back asked nothing at its one-page
slice. Nothing was lost or misread: the regions were consistent within a build, and a daemon adopts only a content
object its own build laid out (A-145; the segment's layout version is checked at adoption).

## Fix

1. `DaemonConfig::content_reserve`: the reserve in whole pages, one at least; `write_log_bytes`: a quarter of it in
   whole pages, one at least. `shard_content_layout` lays the slots and the delta log from them, so every region
   begins and ends on a page.
2. `daemon::dead_to_pages`: a dead range's end goes up to the page at or past its last byte where its memory reaches
   that far, else down to the last page inside the memory, so a one-page piece from the top is a whole page on any
   layout.
3. The anchor's `LAYOUT_VERSION` 21, with its history line: the slots and the delta log lie at other offsets of the
   content object wherever the reserve was no multiple of four pages.

Reds: `config::tests::every_region_of_a_shards_content_slice_begins_and_ends_on_a_page` (Windows' own reserve at a
4 KiB page: the write log ended 1,024 bytes past a page before); `daemon::tests::a_dead_memorys_top_end_lies_on_an_object_page_inside_it_wherever_the_memory_begins`
(a memory at Windows' slot start, wholly dead: its top end 2,024 bytes past a page before). The one-page give-back test
now asserts the whole pages its dead ranges hold on the platform's own layout, counted apart (`whole_pages_dead`),
asked back: 32,768 of 32,768 on the MacBook.
