# A clone's inode count started from its origin's head (2026-11-19)

## Description

A clone of a snapshot counted its live inodes (§4.2's inode dimension, which its inode allowance is judged against)
from its origin's count at the moment of the clone, not from the snapshot it was made from. Every file the origin made
after the snapshot counted against the clone, and every file the origin removed after it did not, until a restart
recounted the clone's tree. Measured on the Mac Studio (2026-11-19, the lib tests on main `fba7cd888` with only the
tests changed). An origin held a directory and three files at its snapshot, then made five more files and removed one.
Its clone reported 9 live inodes where its tree holds 5 (`left: 9, right: 5`). A clone made after a restart, from the
recovered snapshot, reported the same 9.

## Root cause

`Volume::clone_inner` (`crates/vfs/src/volume.rs`) set the clone's `live_inodes` to `origin.live_inodes`: the origin's
head now. The clone's tree is the snapshot's (its `inode_root`), and nothing recorded how many inodes the snapshot held.
The bytes were right only because the snapshot record keeps `referenced_bytes` and the clone starts from that. A
restart's rebuild recounts a clone from its image (its own inodes and the numbers it shares with its origin), so the
error lasted until the daemon's next restart.

## Impact

`next_no` judges a clone's inode allowance against the clone's count. The server sets that allowance when it admits
the clone, at its fair share but never below the origin's count (`admit_clone`). If the origin grew after the snapshot,
the count was too high and the clone was refused `NoSpace` below its allowance. If the origin shrank, the count was too
low and the clone admitted more inodes than its allowance. A clone of the clone's snapshot took the clone's wrong count
in turn. Either error lasted until a restart. This hits A-303's restore, which clones a cut its origin has usually
changed since, and every clone of an older snapshot.

## Exact edits

- `crates/vfs/src/snapshot.rs`: `Snapshot::live_inodes`, the head's count when the snapshot was taken.
- `crates/vfs/src/volume.rs`: `Volume::snapshot` records it, and `clone_inner` starts the clone from it (the notes of
  `clone_of` say so). The lib test module `clone_count_tests` is new:
  `a_clone_counts_its_snapshots_inodes_whatever_its_origin_did_since` checks the count of the origin, the clone and a
  clone of the clone against a restart's recount of each tree after every step, then checks that an allowance of one
  more inode than the tree holds admits exactly one file. `a_clone_of_a_recovered_snapshot_counts_its_snapshots_inodes`
  does the same across a restart. Red before the change at the first clone (`left: 9, right: 5`, both tests).
- `crates/vfs/src/recover.rs`: `rebuild_snapshot` gives a recovered snapshot its tree's count: its image's entries, each
  imaged once in full or shared (`capture_snapshot_tree`). This is how the head is counted, so the image's layout does
  not change.

## Siblings (the sweep: every counter a clone, a restore or an adoption takes from the live origin instead of the frozen state)

- **Open, the same quantity on the server's side:** the server's clone admission (`crates/server/src/verbs.rs`,
  `admit_clone`) reads the clone's inherited inodes from the origin volume's `inode_usage()`, its head's count now,
  before it clones. That count sizes a bounded clone's version reservation and the floor of its allowance, and a
  dynamic clone's version hold. It should read the snapshot's count, which this change records
  (`Snapshot::live_inodes`, through an accessor). Until then:
  - A dynamic clone's hold differs from its count by the origin's net change since the snapshot. When it is short, the
    clone's next create takes the difference at once (`hold_growth`). When it is long, the extra slots are held until
    the clone's teardown.
  - When the origin's own allowance exceeded its fair share (a clone of a clone, its allowance floored at an inherited
    count), the allowance can fall below the clone's count. It is then refused (`volume.allowance_refused`), and the
    clone runs with no inode cap. Before this change the same case admitted the clone under its allowance, its count
    low by the same difference.
  Reported, not changed here.
- **Open, the same class:** `clone_inner`'s `live_entries`, the clone's namespace-entry count (§4.2, judged against its
  `entry_allowance`), is also taken from the origin's head now. A restart recounts it (`rebuild_entries`, the shared
  directories' entries added). Reported, not changed here.
- **Correct as they are:**
  - `next_counter` and `counter_ceiling` continue from the origin's current counter on purpose, so numbers stay unique
    across both volumes. A later counter is only safer.
  - The bytes come from the snapshot (`ByEpoch::inherited` of `referenced_bytes`).
  - The base plane's witnesses, homes, whiteouts and redirects are viewed at the snapshot's epoch (`BasePlane::for_clone`,
    A-48). Its listings and unavailable directories are the live base host's facts, which are not frozen with a
    snapshot.
  - The orphans come from the snapshot (A-532).
  - A restore (`adopt_held`) takes its references from the saved tables, not from the origin.
  - The clone's retention and version charges start at zero, since nothing of its own is retained yet.
  - A recovery or a move's adoption builds the volume from its image and recounts every counter. The server's
    re-admission after a restart floors the allowance at that recounted count.
