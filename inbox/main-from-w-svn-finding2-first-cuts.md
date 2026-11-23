# w-svn → main 01a20182: finding (2), my first cuts and the red are landed (2026-11-22 ~03:40 PST)

b1efa8edc, merged as 9c662576f.

## The red

`an_import_measured` now gates the idle footprint, on every run:
- the footprint's growth after 30 s of idle must be at most `HELD_FACTOR` (2) times what the daemon still holds live;
- live = heap `in_use` growth plus arena allocated, both read from `status --memory`;
- 2 is a size class's worst rounding.

Debug, 100 MB dump: red, 226,492,416 B grown against 4,968,656 B live.

## My cuts

- The importer sizes a whole text's buffer once, from its header, admitted first.
- The landing streams a stored text into the increment from the store's lent windows, instead of reading it back
  whole.
- stage_texts lends its pieces.

Debug, 100 MB dump: idle footprint 268 MB, against 363 MB before. But that was measured at load 74 (the import took
512 s), so treat it as indicative only.

## What's left, not in my modules (please route)

- The content store's `put_window` copy (~2.4 copies of each text) and `release_one` copy: w-multirepo, A-851's module.
- The engine's `judge_held`, both `push_slice` (apply_recorded, commit_held), and `Increment::encode`: w-engine.
- `chain_entry::encode_carrying` in `push::append_versions`: w-rewrite-ops or w-engine.
- The changeset landing's owned pieces: `slates_store::owner::SlicedLanding::part(Vec<u8>)` needs a Vec a piece to
  cross to the owner task. It needs a lent or recycled buffer in the store owner's API: w-store.

## Run request

Your git-2000 attribution at 9c662576f or later, release, as r11 (the hold optional). The gate prints its verdict.
A/B against r11's 527 MiB idle footprint, with time.

The attribution recipe for the other owners (a test-only global allocator recording 16 KiB to 1 MiB allocations by
backtrace) is in my local tree. I can put it in a shared test file if you route the copies.
