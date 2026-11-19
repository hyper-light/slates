# A rebase's files lost the link a removed home handed its node to (2026-11-19, found by A-658 part 1's runs, g305; fixed by A-677, g305)

## Description

A hard link names its node, never a path (A-646): when a version removes or replaces a node's home, the least link
that still names the node becomes its home, and it takes the node as the version leaves it (A-669's rule). A rebase
answered other files than the submit for such a work:

- the origin holds an empty file `a`, version 1 links `b` to it, and a work based at the head removes `a`;
- the green that accepts the work holds `b` with the node's bytes (`{"b": []}`): the commit handed the node on;
- the rebase of the same work answered the changes `{"a": None}` and the journal `[Unlink a]`, so the head's files with
  its changes over them held neither `a` nor `b` (`{}`).

The journal was right: replayed on a volume with real inodes, `b` keeps the node. The changes were not: the server keeps
them as the work's files after a rebase (`settle_rebase`, `WorkFiles::from_changed`), so a rebased work's files did not
hold the link the submit keeps.

The rebase oracle (`crates/merge/tests/rebase.rs`, A-202's `a_rebases_changes_are_the_whole_restatements_difference`)
failed on two of the three macOS runs that reached it while A-658 part 1 was tested (kiln r55 and r113, Mac Studio,
2026-11-19), shrunk to the case above and to one with both the file and its link in the origin and the work at version
0. A probe of r55's case answered the same on main's own tree (787f5948b, kiln r89) as on A-658's branch, so the
defect was main's, not A-658's.

## Root cause

The commit adds node moves to a version's effects (`Green::node_moves`, A-646): for each home the version removes or
replaces while links to its node survive, `Green::hand_over` makes the least surviving link the node's home, with its
bytes, mode and attributes. The moves are the node model's consequence, never a work's operation, so a rebase's journal
rightly holds none of them. But `Green::rebased_from` built the rebase's changes from the work's own effects alone, so a
heir's file was in the submit's files and not in the rebase's.

A-669's hand-over with the work's writes was restated (`Green::handed_in`: the link in the changes with the node's
bytes), so only a removal or a replacement of a home without writes lost its heir.

## Impact

- A rebased work's files did not show the link holding the node where the submit keeps it. The next submit composes
  its operations from the journal (`compose_increment`), so no committed version was wrong: the green, its chain and
  the verdicts after the rebase were unaffected; the rebased work's files and their charge were.
- The rebase oracle failed on some seeds, so the merge crate's integration tests were red by chance.

## Edits

- `crates/merge/src/engine.rs`: `Green::heirs_of` (new), the files the version's node moves leave at heirs, each with
  the node's bytes; `Green::rebased_from` takes them after `make_mapped`, as the commit does, and puts them into the
  changes after the work's own (the commit applies its moves after the version's effects), the journal unchanged; the
  docs of `Rebased::Rebased`'s `changed`, `node_moves` and `rebased_from`.
- `crates/merge/src/engine/node_tests.rs`: `a_rebase_restates_the_node_a_removed_home_hands_to_its_link`, a removal of a
  linked file and a rename over one, each rebased beside a twin that submits it.
- `crates/merge/tests/rebase.rs`: the oracle's check a function (`the_restatement_is_the_submits`, `restated_is`) that
  the property and `a_removed_homes_link_holds_its_node_in_the_rebases_files` (the two shrunk cases) both run; the
  property seeded from its new seed file.
- `crates/merge/tests/rebase.proptest-regressions` (new): the two seeds.
- `docs/wip/SLATES_DESIGN.md`: §4.16 "Rebase, the only corrective path" (a status note) and the amendment log (A-677).

## Tests

- The unit test: a removal of `TARGET`, which `HARDLINK` and `SECOND` name, and a rename of `FILE` over it, each
  rebased at the head and submitted on a twin; the rebase's files must be the twin's, `HARDLINK` holding `TARGET`'s old
  bytes, and no journal operation may name `HARDLINK`.
- The oracle's two shrunk cases as a named test, and both seeds replayed by the property before its novel cases, so it
  cannot pass by a seed's luck.

## Evidence

macOS arm64, the Mac Studio (kiln r156, 2026-11-19, debug, fresh clones of the pushed branches, each with its own
target directory, `CARGO_BUILD_JOBS=2`):

- Red at 05593caea (the tests alone, over main 545181b30): `cargo test --locked -p slates-merge --lib node_tests` 24
  passed and the new case failing (the removal's rebased files `{f}` against the twin's `{f, h}`); `cargo test --locked
  -p slates-merge --test rebase` 1 passed and 2 failing, the named cases (`{}` against `{"b": []}`) and the property at
  its first replayed seed (0 successes).
- Green at 79ec79e77: `cargo fmt -p slates-merge -- --check` clean; `cargo test --locked -p slates-merge --lib` 192 of
  192; `--test rebase` 3 of 3; `--test engine -- rebase` 4 of 4; `--test special -- rebases` 1 of 1; the server's
  integration tests that drive a rebase over the wire, by name, 13 of 13.

## Siblings (the sweep, by reading)

- The hand-over also moves the node's mode and attributes to the heir, and an IPC node (`Effect::Special`). A rebase's
  answer holds files alone, so what a rebased work shows of a heir's mode, attributes or IPC node is the server's work
  view's, not checked here.
- Not siblings: the chain entry's copy fixes (`CopyFix::files`, A-626) record a copy's tree, and the stack's
  restatements are operations, from which a commit derives its own node moves.
