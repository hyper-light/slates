# A shard's start ran its heavy pieces in one slice where a yield cost more than they did (2026-11-23)

## Description
`crates/server/tests/recovery.rs`, `a_shards_start_holds_each_heavy_piece_in_a_slice_of_its_own`, failed on Linux.
A plain debug build on adacachy2 at main db3ddff02 (load 13-16) reported "a slice held 8 heavy pieces of the start:
StartReport { slices: 4, units: 15, heavy_pieces_in_one_slice: 8, heaviest_piece: Some(("the council, the fleet and
the root group", 4320186)) }". CI's ThreadSanitizer lane failed it the same way in three runs of three (7 in one slice,
run 45235122249). On this Mac it passed: every unit was a slice of its own (15 slices of 15 units). No plain Linux lane
had reached it since it landed (80412e446, 2026-11-22), since CI's `gates` job stops at fmt or clippy.

## Root cause
84a (`crates/server/src/recovery_slices.rs`) names the start's setup pieces so that each heavy one, a piece of at least
a slice's floor, is a slice of its own. `RecoverySlices::piece_done` noted the piece and then ended the slice as an
ordinary unit does (`unit_done`). An ordinary unit ends its slice only once the slice has run its floor and the
recovery's yields are paid for: the time spent yielding stays within 5% of the time taken (`slice_is_spent`). That rule
keeps cheap units from paying a yield each. After a yield that cost more than a twentieth of the pieces after it, no
slice could end until 20 times that yield had passed, and every piece in that span ran in one slice. On adacachy2 a
yield cost more than that, so all eight pieces ran as one slice. That is the shape 84a set out to break (12 to 21 ms
of the start in one slice on CI's macOS runner).

## Impact
- On a host where a yield costs more than a twentieth of the start's pieces (Linux here, and any loaded host), a
  shard's start ran its pool attach, store, council, content object and state as one slice. The shard's pulse and inbox
  waited for all of it, though the start's report counted the pieces as heavy.
- Bounded: the slice ceiling (a heartbeat, 100 ms) still ends any slice, so the watchdog was never at risk.

## Edits
- `crates/server/src/recovery_slices.rs`: `piece_done` ends the slice after a piece of at least the floor, whatever
  the yields have cost, and leaves any other piece to the unit's rule. `unit_done` and `piece_done` share the slice's
  end and yield (`spent`, `yield_slice`). The pieces are the start's own fixed eight, so this adds at most eight
  yields a start. The recovery's units keep the yield share.
- Test, red first: `a_heavy_piece_ends_its_slice_whatever_the_yields_have_cost` (the module's tests). On an audit shard,
  eight pieces that each spin twice a one-microsecond floor are named done. Before, every piece after the first yield
  ran in one slice. After, each is a slice of its own.

## Evidence
- Red on adacachy2 (plain debug): the new unit test before the fix, "SliceReport { slices: 2, units: 8, ...,
  yielding_ns: 3437, ..., heavy_pieces_in_one_slice: 7, heaviest_piece: Some(("a piece", 2525)) }". The recovery test
  at db3ddff02 reported 8 heavy pieces in one slice (4 slices of 15 units), and in a second run 3 (13 slices).
- Green on adacachy2 with the fix: the unit test, the module's yield-share test
  (`a_recovery_of_units_cheaper_than_a_yield_keeps_its_yields_within_the_limit`), and the recovery test: "StartReport {
  slices: 16, units: 15, heavy_pieces_in_one_slice: 1, heaviest_piece: Some(("the content object, its write log and DAX
  table", 943743)) }".
- Green on this Mac with the fix: the module's four slicing tests, and the recovery test, "StartReport { slices: 16,
  units: 15, heavy_pieces_in_one_slice: 1, heaviest_piece: Some(("the pool's segment and its content regions",
  1816292)) }".
