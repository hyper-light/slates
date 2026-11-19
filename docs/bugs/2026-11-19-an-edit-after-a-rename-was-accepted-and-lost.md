# An edit after a rename was accepted and lost (2026-11-19, A-547)

## Description

Found by perf on 2026-11-09 and recorded open in GAPS (§4.16 "Composition at seal"). A work renames `a` over `b`,
writes at the end of `b`, and submits. The submit is accepted, and `b` holds `a`'s head bytes alone: the edit is lost.
No rebase is involved.

## Root cause

The verdict merged a rename by copying the source's head bytes to the destination (`Green::merge_rename`) and skipped
the destination's content ops in `decide_content` ("handled with that effect"), which never applied them. The chain
entry's `Applied::Copied { dst, from }` had no ops either, and replay maps all of an entry's effects before committing
any, so a second `Spliced` effect could not have carried them.

## Impact

Every accepted submit that edited a file after renaming another over it lost those edits in the green, silently: the
work's client was told its change merged. Replay of the chain reproduced the loss.

## Edits

- `crates/merge/src/engine.rs`: `merge_rename` takes the destination's ops and the increment's post-state;
  `edits_after_rename` maps the ops through the source's intervening deltas (an overlap is a conflict on the
  destination); `ContentSource::Copied` carries the mapped ops; the effect's chain form, its replay
  (`effect_of`) and the rebase's restatement (`rebased_from`) take them.
- `crates/merge/src/chain_entry.rs`: `Applied::Copied` gains `ops`; tag `COPIED_SPLICED` (17) encodes a copy with ops; a
  rename alone keeps `COPIED`.
- `crates/server/tests/green_post_state.rs`: the parked server test is no longer ignored.
- Test: `an_edit_after_a_rename_over_a_file_is_merged_with_the_renamed_bytes` (`engine.rs`), red without the change.
