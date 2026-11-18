# A DAX fill refused part-way left its bytes for a later fill to show

Date: 2026-11-18. Area: `crates/bridge-core/src/dax_backing.rs` (the backed session's fill), `crates/vfs/src/
shards_backing.rs` (`ShardsBacking`). Design: §4.6, the backed route (A-163, A-193); amendment A-419. Closes the GAPS
line "a fill refused part-way unmarks its range but leaves the pages it stored in the object" (the 2026-10-25 G-vhost
entry, observed while reading) and its twin in the 2026-10-31 entry on the backed route's L1. Found by reading; goal 83
(every open slates bug), item B7.

## Description

A guest maps a range of a file over DAX, through the Shards route or slates' own virtio-fs device. The daemon fills the
range of the file's backing from the volume a slice at a time, then answers. If the volume refuses a slice part-way, or
the transport gives the fill up, the guest's map is refused. Later the file changes so that some of the bytes the
first slices copied are now a hole or zeros: a cut and an extension, or zeros written by another path. When the guest
maps the range again, the fill is answered and the guest's map shows the old bytes there, not the file's.

The unit tests reproduce it with no guest (`crates/bridge-core/src/dax_backing.rs`):
- `a_fill_refused_part_way_leaves_nothing_a_later_fill_shows`: a range of `A`, one slice filled, the next refused, the
  bytes zeroed through the bridge, the range asked and filled again. Without the fix the object still read `A` over the
  first slice.
- `a_fill_abandoned_part_way_leaves_nothing_a_later_fill_shows`: the same, the fill given up after its first slice as
  the Shards route gives one up when the attachment's bridge is gone. Without the fix, `A` again.
- `a_part_way_fill_stays_counted_until_its_range_is_filled_again`: one range filled, the next refused after its first
  slice. Without the fix the backings counted 2,097,152 bytes (one range) while two ranges held bytes.

The refusal is injected by a context of another volume, which the bridge refuses as it refuses any read it cannot serve.
In the field the refusal is a disk's read error beneath an overlay, a file's base drifting under the fill, or the OS
refusing a store or a placement.

## Root cause

A range is marked filled when its fill begins, so a change written through meanwhile reaches it. On a refusal part-way,
and when the fill was given up (`abandon_fill`), the range was unmarked and its charge given back, and the pages its
slices had stored stayed in the object. From then on:
- no write reached the range, since write-through writes only over filled ranges;
- its next fill copied only the volume's data runs (`SEEK_DATA`) and pages that are not zeros (`store_pages`), since a
  range never filled reads zero, and this one did not.

So any page the file had since made a hole or zeros kept its old bytes. And the backings held those pages uncounted
against the fill budget.

## Impact

Every backed DAX route: the Shards route on Linux, macOS and Windows, and slates' own virtio-fs device on Linux. A
guest read stale bytes of its own file through its map after a fill had failed part-way and the file had changed
since. Only the file's own older bytes could show, never another file's: a backing holds one file of one attachment.
A refusal part-way needs the volume or the OS to refuse mid-fill, which is rare. The budget's undercount was at most the
pages one failed fill stored, per failure.

## Exact edits (A-419)

- `crates/vfs/src/shards_backing.rs`: `ShardsBacking` keeps the ranges a fill began and did not finish
  (`mark_uncleared`, `uncleared`, `cleared`). The module's bound counts them.
- `crates/bridge-core/src/dax_backing.rs`:
  - a fill refused part-way, and one given up, leaves its range uncleared rather than unmarked, its charge kept;
  - a fill of an uncleared range clears it first (`DaxObject::zero`), and a clear the OS refuses refuses the fill;
  - `charge_of` counts filled and uncleared ranges together;
  - the fill's `range_charge` is gone, since no failure gives a charge back now;
  - the three tests above.
- §4.6 (the Shards DAX line), the amendment log (A-419) and GAPS.

## Tests

The three unit tests above. On the Mac Studio (d1323898, macOS arm64, debug build, 2026-11-18): red without the fix,
3 of 3 failing (kiln r97); green with it, the module's 5 of 5. Command:
`cargo test -p slates-bridge-core --lib dax_backing::tests`.

## Siblings found, reported, not changed here

- A successor's adopted DAX objects (A-365). The range table records a range only once its ask is answered, so a range
  whose fill the dead daemon had under way, or that failed there, is not named, and its object may hold that fill's
  bytes. The successor's next fill of it skips holes and zeros as before A-419. At most one range a stream was filling
  at the death, plus ranges that failed earlier. Owed: a test that adopts an object holding bytes in an unnamed range.
- Windows: a range is committed (`prepare_range`) before its charge is asked. A charge refused `Budget` leaves the
  range committed and uncounted, and Windows gives no committed page of a section back, so every such refusal commits
  another range, up to the object's size (the machine's memory). Owed: a Windows test, and the charge asked first.
