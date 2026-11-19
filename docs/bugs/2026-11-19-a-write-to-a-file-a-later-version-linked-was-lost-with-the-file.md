# A write to a file a later version linked was lost with the file (2026-11-19, found by A-669's sweep, g307; fixed by A-670, g307)

## Description

A-669 hands a removed file's writes to a hard link that still names its node, but only to a link the work's base
had: the work's deriver composes against that base. With the base holding `g` and no link to it:

- version 1, another work, links `a` to `g`;
- a work based at version 0 appends to `g` and removes it.

In the work's own volume the node went with its last name, taking the write with it. The verdict accepted the removal,
since a link made to a file records no change of the file, and the commit handed `g`'s node to `a` with the head's
bytes. POSIX, applying the two works in order, keeps the write at `a`. A rename of another file over `g` lost it the same
way. A stack (A-659) had the same hole one level up: an upper's base is its lower's post-state, which never holds a link
another work made while the stack was in flight, and the restated upper is judged as based on a version after it.

## Root cause

The verdict judged a removal against changes to the removed path alone. A link made after the base changes the node's
names, not the path, so nothing the removal read had changed; and the increment cannot say whether its work wrote, since
writes it could not declare are dropped with the node.

## Impact

A work that wrote to a file and then removed it, or renamed another over it, lost its writes whenever another version
had meanwhile linked that file and the work's base held no link to it: the link kept the head's bytes. No refusal was
raised.

## Edits

- `crates/merge/src/engine.rs`: `decide_moves` (the content dimension's renames, creates and unlinks) refuses removing a
  file, or renaming another over it, when a link naming it was made after the work's base and no link the base had to
  it outlives the increment (`leaves_to_an_unseen_link`, `links_unseen`): delete against modify, or rename against
  rename. An increment that hands the node on itself (A-669) is exempt.
- `crates/merge/src/engine/stack/judge.rs`: a restated upper is refused alike for a link that a version no work of the
  stack landed as made after the stack's base (`upper_unseen_links`, `Against::since`).
- `crates/merge/src/engine/replaced_node_tests.rs`: A-643's whole-state model states the rule, with every version's
  link homes kept.

## Tests

`crates/merge/src/engine/restate_tests.rs`: four verdict cases red first (a write and a removal, a write and a rename
over, a removal without a write, a write whose base link the work removed itself), and a base link outliving the work
keeping the removal accepted; A-663's oracle's heads may link a file late. `crates/merge/src/engine/stack/
handed_tests.rs`: an upper removing a file another work linked under the stack, red first; an upper handing a write on
to a link landing with its node, meeting a change to the home, and removing a file its lower linked.

## Evidence

Windows x86_64 (jupiter, kiln), 2026-11-19, debug builds; the macOS run is owed.

- Red over main's engine at cac45de71 (`cargo test --locked -p slates-merge --lib -- restate_tests handed_tests`): 6
  failing, the four cases, the stack case and the restatement oracle's fixed history `append f; rm f` over the head
  linking `k` to `f` (`k` read without the write), 14 passing.
- Green over main at 6e42e7a2c: at ac6a83b09, 177 of 178, A-643's whole-state model failing at a link made and a
  removal based before it (the engine refused, the old model accepted); with the model's rule, at e8c10097e, 178 of
  178. `cargo fmt -p slates-merge -- --check`: clean.

## Siblings

- A-659's stacks carry an upper's write handed on to a link (the question A-669 left): answered by test, nothing to fix.
- The restatement oracle's census rested on generated works with a random seed; one fixed history a case now meets it
  on every run (A-670).
