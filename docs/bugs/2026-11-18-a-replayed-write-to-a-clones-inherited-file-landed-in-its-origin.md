# A replayed write to a clone's inherited file landed in its origin (2026-11-18, §4.6 "Linux", A-63)

## Description

Every FUSE write is logged in the shard's write log before its reply (A-63), so a daemon that dies before its next
publication leaves the write for its successor, which replays the log over the volumes its image rebuilt. A record held
the inode number, the offset and the bytes, and the replay found the record's volume by the inode number's prefix, its
top 16 bits (`verbs::slot_of_inode`), assuming an inode number names its volume within a shard. It does not: a clone
keeps its origin's inode numbers for every file it inherits (`VolumeSeed::root_no`: "a clone inherits its origin's root,
so it carries the origin's prefix"). So a write a dead daemon had acknowledged on a clone's inherited file was replayed
into the first volume of that prefix, the origin or another clone of it, and the clone lost the write: a write
acknowledged to one volume's writer changed another volume's file. A logged `FORGET` was routed the same way, giving its
lookups back in the wrong volume.

Reported from reading by A-166's sibling sweep (GAPS: "each from reading; each to be proven by a test before a change").

## Proof

`a_replayed_write_to_a_clones_inherited_file_lands_in_the_clone` (`crates/server/src/verbs.rs`, Linux): a volume whose
file "f" holds "origin", snapshotted and cloned; a logged write of "clone!" through the clone to its "f" replayed. With
the prefix routing the origin read "clone!" and the clone "origin"; routed by the record's volume, the clone reads
"clone!" and the origin "origin". The test asserts the inherited inode carries the origin's prefix, so it cannot pass
vacuously.

## Fix (exact edits)

- `crates/server/src/write_log.rs`: each record carries the 16-byte id of the volume it was written through, after its
  head (`Record::volume`, `Forgotten::volume`); the header gains a layout word ("WLOG", version 2), whose low half is no
  record kind, so a log of the layout before it, whose first record began there, is never read as this one's; one that
  holds records is reported as lost writes (every file a taken-over mount had open reports them, as an overflow does),
  an empty one as nothing lost.
- `crates/server/src/fuse.rs`: `log_write` and `log_forgets` log the mount's volume.
- `crates/server/src/verbs.rs`: `replay_one` and `replay_forget` find the record's volume by its id
  (`slot_of_volume`); `slot_of_inode` is gone; `relog_remaining` (a replay that met a full volume logs what it has
  not applied again) keeps each record's volume.
- Compiled on Linux alone (the write log is Linux's), so the change was proven there (diag-g251-replay on GitHub's
  ubuntu runner): the first run found `relog_remaining`'s call, which no macOS build compiles.

## Siblings (A-166's sweep, still open, each from reading)

- The in-process DAX gate's recall (`has_writable_of_prefix`, `writable_of_prefix`) counts a clone's writable windows on
  inherited files under the origin's prefix.
- `DaxRegistry::has_writable_of` keys by inode number alone, so its `EBUSY` over-refuses across volumes.
- `volume_config` wraps `next_prefix` at 65,535 with no check against live volumes' prefixes.
