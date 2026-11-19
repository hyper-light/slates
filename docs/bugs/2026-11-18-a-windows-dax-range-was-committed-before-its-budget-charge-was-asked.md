# A Windows DAX range was committed before its budget charge was asked

Date: 2026-11-18. Area: `crates/bridge-core/src/dax_backing.rs` (the start of a range's fill). Design: §4.6 the backed
route and its fill budget; A-180 (a Windows section committed a range at a time); amendment A-530. Goal 85, one of
A-419's siblings (found by reading at A-419, proven here by its unit test).

## Description

A guest maps a file's range over DAX, through the Shards route on Windows. The daemon begins the range's fill by readying
the range for the VMM's map, which commits it whole on Windows, and only then counts it against the shard's fill budget.
At the budget's bound the ask is refused `Budget`, but the range stays committed: Windows gives no committed page of a
section back while the section lives, and nothing counts it. Every further ask refused at the bound commits one more
range of 2 MiB, up to the object's size, the machine's memory, all of it counted by nothing.

`a_range_is_charged_before_anything_is_made_for_it` (`crates/bridge-core/src/dax_backing.rs`) holds the order on every
host, over the start's steps recorded in the order they run. With the old order, a refused charge ran `prepare`,
`clear` and `charge`, where `charge` alone was owed.

## Root cause

The start of a range's fill (`stepped`) ran its steps as: ready (`prepare_range`), clear where owed, mark and charge.
The charge's refusal undid the mark alone. On Unix readying does nothing, so nothing was held; on Windows the commit
stayed.

## Impact

Windows Shards guests whose maps reached the fill budget: each refused ask held 2 MiB of committed memory outside the
budget for the section's life, which Windows charges against the system's commit limit.

## Exact edits (A-530)

- `crates/bridge-core/src/dax_backing.rs`: the start is one function over its steps, `begin_range` over `RangeStart`
  (`StartingRange` does them on the store): charge first, and a refusal stops it `Budget` with nothing readied or
  cleared; then ready, then clear where owed; a readying or clear the OS refuses gives the charge back and refuses
  `Object`. The ordering test is in the unit tests.
- Docs: the amendment log (A-530), GAPS.

## Tests

`a_range_is_charged_before_anything_is_made_for_it`: red with the old order on the Mac Studio (d1323898, debug), and
the module's tests 7 of 7 with the new one. Command: `cargo test -p slates-bridge-core --lib dax_backing::tests`.

While the old order was drawn out for the red run, a second test, `a_part_way_fill_stays_counted_until_its_range_is_
filled_again`, also failed there: that drawn-out version cleared an uncleared range's mark before charging it, so the
range was counted twice. The code before this change marked the range before clearing it, and the new order charges
first, so neither counts it twice. The test passes with the new order.

## Owed

The lib tests on both Windows runners, and a Windows Shards DAX map asked past the budget (main's Windows suites).
