# The position-mapping oracle judged only the head (2026-09-30, T-6.4)

Contracts: §4.16 "Position mapping", T-6.4. Found by CI run 36655388624 (`c59f3aa`, the Linux
test job): `crates/merge/tests/map.rs` `the_map_agrees_with_provenance` failed on a generated
history. The later runs passed only because proptest drew other histories.

## Symptom

The shrunk case: a base of 4 bytes and range `1..3`, with two intervening deltas.

1. An insert of 12 bytes at position 2, strictly inside the range.
2. A delete of exactly those 12 bytes.

The mapper answered `Overlaps`. The oracle expected `Shifted(1..3)`.

## Root cause

The mapper was right, and the oracle stated a rule the design does not. §4.16 hands a range to the
verdict when "an intervening effect range" overlaps it, delta by delta, and the oracle's own doc said
the same: "one that any change touched is reported as overlapping".

The oracle's code, however, looked only at the head's provenance. There the range's base bytes were
intact and contiguous again, because the delete had removed what the insert added. So it certified a
range the first delta had touched as untouched.

The CLAUDE.md gotcha for oracles applies: "the model must state the design's rule, not the
implementation's, or it certifies drift". Here the model's code had drifted from the model's own
words.

## Impact

None in shipped code: the mapper never followed the oracle. The risk was the reverse. An
implementation "fixed" to agree with the oracle would have mapped touched ranges cleanly. The same
risk applies to the owed checkpoint fold: a fold that kept only a delta pair's net effect would
compose an insert and its delete into nothing, and stop being exact.

## Fix

- **`crates/merge/tests/map.rs`.** `simulate` keeps the provenance at every version. The oracle
  (`expected`) requires the range intact and contiguous at each version (`intact_at`), and maps it to
  where it sits at head. The CI case is pinned as a named test,
  `the_shrunk_history_of_ci_run_36655388624_agrees`.
- **`crates/merge/src/map.rs`.** The module doc records the gotcha for the checkpoint fold: keep every
  touched span, not only the net size change.

## Evidence

- **Red.** The pinned case failed on the old oracle (left `Overlaps`, right `Shifted(1..3)`).
- **Green.** It passes now, and the property holds over 200,000 generated histories (release build,
  0.51 s). The merge suite is whole.

## Sibling, reported

The failing run wrote `crates/merge/tests/map.proptest-regressions` into the runner's checkout.
Proptest persists failures to files by default, and 20 of the 28 files with property tests leave
that default on. AUD-29-63 records the class, noting that "a passing test run does not exercise
that write branch"; this failing run did. The fix belongs to AUD-29-63's sweep of every site.
