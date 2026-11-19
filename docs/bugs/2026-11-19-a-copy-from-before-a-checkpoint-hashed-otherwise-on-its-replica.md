# A copy from before a checkpoint hashed otherwise on its replica (2026-11-19, found by A-626 part 3's copy oracle, g307; fixed in A-626 part 3)

## Description

A green's identity is a set hash: one entry for each key of its state, and one for each copy, which stands for the
entries of the tree the copy brought, so a copy costs the identity O(1) (A-586). When a key the copy brought changes,
the identity takes out the entry the copy gave it; a branch or a checkpoint's sum moved below a copy puts those entries
back; and the identity's recount takes out every entry each copy brought. All three read "the entry the copy gave a
key" from the copy's source, read through its history at the source's version.

A green restored from a checkpoint holds no history before the checkpoint. When such a green replayed a copy made after
the checkpoint from a source version before it (the copy's chain entry carries its fix, so the copy's bytes replay
right), the identity's reads of the source at that version found nothing, or the checkpoint's value. A-626 part 3's copy
oracle found it on its first run (Windows x86_64, jupiter, seed
`cc ef9d35575a219623b880d96c51fec0a6ac94e4709ecc554605bd973d73c68dc5`): a copy at version 4 from version 1, replayed on
greens restored at 2 and at 3. Their running identities happened to agree with the owner's, but their recounts did not.
By reading, a key the copy brought that changes in the copy's fold window, or in its own increment, made the replica's
running identity itself differ from its owner's. The same reading moved a replica's checkpoint sum below the copy.

## Root cause

`Green::copied_entries` and `Green::copied_tree_entries` read the copy's source at the source's version
(`entry_at(.., record.source_version)`). On the owner that version is never below the fold floor when the copy is
judged, since the verdict refuses a fixed copy from a folded version. A restored green's floor is the checkpoint's
version, which may be later. The service checkpoints at the reachable floor and folds its histories only as far as the
retention budget needs, so a copy above the owner's fold floor can name a version below its last checkpoint, and a green
recovered from that checkpoint replays it.

## Impact

A recovered or replica green holding such a copy hashed otherwise than its owner: its merge records' identities, its
head check against the checkpoint's, and its own checkpoints' sums were wrong. Its files read right.

## Edits

- `crates/merge/src/engine/copy.rs`: `Green::brought_entry` reads what a copy brought from its graft: the source at the
  version before the copy, through the copy's fix. That version is never below the fold floor or a restored green's
  checkpoint, for any copy a reader asks about. `copied_entries` and `copied_tree_entries` read through it, and the
  latter also counts the keys the fix names. `Appearance` keeps the copy's fix, charged with the change index
  (`record_bytes`) until the fold floor reaches the copy (`release_kept_fixes`).
- `crates/merge/src/engine.rs`: the fold lets the kept fixes go. `changes_reservation` secures their bytes before the
  verdict, as the chain entry's room is secured. The change index's recount counts them.
- `crates/merge/src/engine/copy_checkpoint.rs`: a restored copy keeps no fix. The checkpoint's format is unchanged.

Recording the entries each copy brought was rejected: it turns a copy of a 250 GB tree from O(path length) into O(the
tree), in the format and in the checkpoint. A rule holding checkpoints back was also rejected: the failing copies are
made after the checkpoint, so no rule at the checkpoint holds them (A-626 part 3's entry).

## Tests

`crates/merge/src/engine/copy_tests.rs`:
- `a_replica_restored_after_a_copys_source_version_hashes_as_its_owner`: a copy, then a change to a key it brought,
  replayed before the identity is read; it also compares the replica's checkpoint sum below the copy.
- `a_replica_replaying_a_copy_edited_in_its_own_version_hashes_as_its_owner`: a key the copy's own increment changes.
- The copy oracle replays the seed from its reviewed seed file (`copy_tests.proptest-regressions`).

## Evidence

Pending the run on jupiter (Windows x86_64): red over A-626 part 3 as it stood (7d08dd16a), then green.

## Siblings

- The identity's recount still reads each copy's source at the version before it, so on a green folded past a copy it
  reads below the floor. No test folds a green holding a copy and recounts it, so the recount is no oracle there. Filed
  in GAPS; it is a test oracle's limit, not the running identity's.
