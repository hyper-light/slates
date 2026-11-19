# A range a dead daemon's DAX fill left part-way showed its bytes to the successor's guest

Date: 2026-11-18. Area: `crates/vfs/src/shards_backing.rs` (`ShardsBacking`, `ShardsBackings::adopt_vmm`),
`crates/bridge-core/src/dax_backing.rs` (a range's fill). Design: §4.6 the backed route and its Shards form; A-365 (a
stream's DAX objects outlive their daemon), A-419 (a fill that fails part-way); amendment A-526. Goal 85, one of
A-419's siblings (found by reading at A-419, proven here by its unit test).

## Description

A Shards stream whose guest maps files with DAX outlives its daemon: the anchor holds each file's DAX object, and the
successor takes the objects back with the ranges the dead daemon's range table names (A-365). When that daemon died
while filling a range, or had refused a fill part-way, the object held bytes of that fill in a range the table does not
name. The successor's guest later asked the range, the successor filled it from the volume, and where the file had
since become a hole or zeros the guest's map showed the dead daemon's bytes instead.

`a_range_a_dead_daemons_fill_left_part_way_reads_the_volume_once_filled_again` (`crates/bridge-core/src/dax_backing.rs`)
reproduces it with no daemon: an object holding `A` over a range's first slice, taken back into the store with no range
named, the file zeroed there through the bridge, the range asked and filled whole through the session that took the
backing back. Without the fix the object still read `A` over that slice.

## Root cause

The range table records a range only once its ask is answered (`crate::dax_ranges`, A-365), by design: a range named
filled must be whole. A fill writes the object before that, slice by slice, so a daemon that dies mid-fill leaves bytes
in a range nothing names. A-419 made a fill refused part-way leave its range uncleared, but that mark lives in the
daemon's memory and dies with it. The successor's fill then treats the range as never filled: it copies only the
volume's data runs and pages that are not zeros, since a range never filled reads zero, and this one did not.

## Impact

A guest of a Shards stream that outlived its daemon could read its file's older bytes through its DAX map. Only that
file's own older bytes, never another file's, and only after its daemon died mid-fill (or after a refused fill) and the
file then changed to holes or zeros there.

## Exact edits (A-526)

- `crates/vfs/src/shards_backing.rs`: `ShardsBacking` keeps whether its object was taken back from a dead daemon
  (`inherited`, set by `adopt_vmm`), and `owes_clear(range)` names a range to clear before its fill: one A-419 left
  uncleared, or, for an inherited backing, any range not filled.
- `crates/bridge-core/src/dax_backing.rs`: a range's fill begins by clearing the range when `owes_clear` names it (it
  cleared A-419's uncleared ranges alone before). The failing test is in its unit tests.
- Docs: the amendment log (A-526), GAPS.

## Tests

`a_range_a_dead_daemons_fill_left_part_way_reads_the_volume_once_filled_again`, red without the fix and green with it
on the Mac Studio (d1323898, debug). Command: `cargo test -p slates-bridge-core --lib dax_backing::tests`.

## Left

Bytes a dead daemon's fill left in an unnamed range count against no budget until the range is filled or the backing
goes: at most the ranges that daemon's fills had begun and not finished. The other A-419 sibling, a Windows range
committed before its charge is asked, is open in GAPS.
