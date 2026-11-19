# A write to a file a hard link names was lost with the file (2026-11-19, found by A-663's oracle, g307; fixed by A-669, g307)

## Description

A hard link names its node, never a path (A-646): when a version removes or replaces a node's home, the least link
still naming the node becomes its home. With the base holding `g` and a hard link `h` to it, a work that appended to
`g` and then removed it was accepted without the append:

- the work's own volume, which has real inodes, held `<g>\n<+w>\n` at `h`, the node's last name;
- the green that accepted the work held `<g>\n` at `h`, the head's bytes;
- so did the work's restated journal replayed after a rebase, and that journal composed again.

A rename of another file over `g` after the write (`echo w >> g; mv f g`) lost it the same way, and so did a write
handed on through a rename (`mv g f; echo w >> f; rm f`). The submit reported success: a lost update, not a refusal.
A-663's oracle found it on its first run (Windows x86_64, jupiter): `[(31, 13, 0), (22, 88, 0)]`, an append to `g` and
its removal.

## Root cause

Two halves, each right alone and wrong together:

- The deriver dropped a removed file's content operations (`apply_unlink` cleared them; a file replaced by a rename
  kept them on an entity nothing emitted), as if a removed file's node always went with it.
- The commit's hand-over (`Green::hand_over`) gives the heir link the home's bytes as the head holds them. With the
  writes dropped, nothing told the verdict the node had changed before it passed to the link.

A-646's decision said the link "takes its content", without saying whose: the head's, or the node's as the version
leaves it.

## Impact

Any work that wrote to a file a hard link also names, then removed the file or renamed another over it, lost those
writes on submit while the link kept the old bytes; every later version, rebase and landing carried the loss. No
refusal or conflict was raised, so neither the agent nor a reviewer could see it.

## Edits

- The rule, in A-646's decision (§4.16) and its part 2a entry: the link takes the node as the version leaves it,
  never as the head held it.
- `crates/merge/src/increment.rs`: `apply_unlink` keeps the writes. At the seal, `heirs_of_writes` finds the links that
  still name a removed or replaced file's node (`linked_nodes`: each link's node fixed when the journal makes it, as
  A-646 rules) and declares the writes at the least base link to the file the journal left alone, where the node
  stands at the seal and the seal reads them (`classify`). A write whose node only links the work made still name, and
  a write to a file the work then made again at its name, are refused, typed (`Unsupported`, owed).
- `crates/merge/src/engine.rs`: writes declared at the head's link whose home the increment removes or replaces
  (`handed_home`) are judged as the node handed on with them (`merge_handed`): the link becomes the node's home with
  the home's bytes and the writes, its mode and its attributes, recorded as a copy into the link, and the other links
  follow it through the commit's node moves. A link or a home changed since the work's base is delete against modify.
  A rebase restates the work's own form (`handed_in` in `rebased_from`).

## Tests

`crates/merge/src/engine/restate_tests.rs`, A-663's four readings (the work's own order with real inodes, the green
that accepts it, its restated journal replayed, that journal composed again): five cases, red first, the write kept
when the file is removed, when another is renamed over it, and at every name left; refused when only a link the work
made is left, and when the file is made again at its name. A-663's oracle no longer leaves the case out of its works,
and its census counts it.

## Evidence

Windows x86_64 (jupiter, kiln runner f766f092), 2026-11-19, debug builds; the Mac Studio offline, so the macOS run is
owed. Main's slates-merge was the same from 4b12a582e to 0e755c9b1.

- Red, the tests over main's engine (`cargo test --locked -p slates-merge --lib restate_tests` at 59ee404f5): 6 of 11
  failing, the five cases and the oracle, A-663's five passing. The oracle shrank to
  `create l; mv g f; append f; rm f`, `h` read without the write in all three readings.
- The first green run (ae27b01d3) passed the five cases and failed the oracle at
  `append f; append g; rm g; ln h l; rm h`: the deriver had resolved `l` through the links standing at the seal, so
  `rm h` lost `l`'s node and the write with it. Fixed in the same change (`linked_nodes`); the work is now refused,
  typed, as only a link it made is left.
- Green over main at 0e755c9b1 (`cargo test --locked -p slates-merge --lib -- --show-output` at 72177dcd7): 152 of
  152, main's 141, A-663's 6 and A-669's 5. Oracle census over 256 works: 157 agreed, 99 refused by the deriver,
  typed, no conflicts; the write handed on twice at a removal and once at a rename. `cargo fmt -p slates-merge --
  --check`: clean.

## Siblings (the sweep, by reading)

- A work's writes to a file that a later version linked are lost when the work removes the file: its deriver saw no
  link, and the verdict accepts the removal, since a link made to a file records no change of the file. Filed open in
  GAPS; owed: such a removal judged delete against modify, so the rebase brings the link and the next submit hands the
  writes on itself.
- A removed file's own mode or attribute change is refused (`SetModeMissing`, `XattrMissing`), not lost.
- A file renamed away keeps its writes, which travel with it; the commit's other hand-overs carry no writes of the
  version.
