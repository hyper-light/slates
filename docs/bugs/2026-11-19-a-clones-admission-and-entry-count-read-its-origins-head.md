# A clone's admission and its entry count read its origin's head (2026-11-19)

## Description

A-541 made a clone count its live inodes from its snapshot. Its sweep found two places that still read the origin's
head as it is at the moment of the clone, where the clone holds the snapshot's tree:

- The server's clone admission (`admit_clone` in `crates/server/src/verbs.rs`) took the clone's inherited inodes from
  the origin's head count. That count sizes a bounded clone's version reservation and the floor of its inode allowance,
  and a dynamic clone's version hold.
- A clone's live-entry count (§4.2's namespace dimension, which its entry allowance is judged against) started from the
  origin's head count (`clone_inner` in `crates/vfs/src/volume.rs`).

Measured on the Mac Studio (2026-11-19), with the tests alone on main `a613562eb` (branch `agents/g234-a549-red`):

- A bounded origin was filled 8 inodes past its fair share of 28 (its allowance raised, as a restart or an admission
  floors one), snapshotted at 36 inodes, then left holding 27. Its clone counted 36 inodes. Its allowance was
  `18446744073709551615` (`u64::MAX`): the admission reserved and floored by 27, so the allowance it set was the share,
  28, below the clone's 36. The setter refused it (`volume.allowance_refused`), and the clone ran with no inode cap.
- A dynamic origin held 13 inodes at its snapshot and 2 at the clone. Its clone held 2 version slots for 13 inodes.
- An origin held 9 entries at its snapshot and 13 at the clone. Its clone counted 13 entries where its tree holds 9.
  After a restart, a clone made from the recovered snapshot started from the recovered origin's head count in the same
  way.

## Root cause

- `admit_clone` read `slot.volume.inode_usage().0`, which is the origin's head now. Nothing let the server read a
  snapshot's count, which A-541 had started recording.
- `clone_inner` set `live_entries: origin.live_entries`. A snapshot recorded no entry count, and a recovered snapshot
  had none to restore.

## Impact

- **Bounded clones.** When the origin removed files after the snapshot, the clone reserved version slots for fewer
  inodes than it holds. Its allowance's floor was too low too. Where the clone's tree was over its fair share, the
  allowance was refused, and the clone could make inodes with no cap, past what its reservation backs. When the origin
  made files after the snapshot, the clone reserved more and was allowed more than its tree warrants.
- **Dynamic clones.** The hold differed from the clone's inodes by the origin's net change. When the hold was short,
  nothing backed the divergence of the clone's inherited inodes until its next create took the difference
  (`hold_growth`). When it was long, the extra slots were held until the clone's teardown.
- **Entries.** Every step after the clone was off by the origin's net change. Through the library, an entry allowance
  set on the clone was judged against the wrong count, and a clone of the clone's snapshot took it in turn. The daemon
  sets no entry allowance on a clone until a restart, which recounts (a sibling below), so there the count bounded
  nothing yet.
- Each error lasted until a restart recounted the clone. A-303's restore clones a cut its origin has usually changed
  since, and so does every clone of an older snapshot.

## Exact edits

- `crates/vfs/src/snapshot.rs`: `Snapshot::live_entries`, the head's entry count when the snapshot is taken.
- `crates/vfs/src/volume.rs`:
  - `Volume::snapshot` records the entry count, and `clone_inner` starts the clone from the snapshot's (`clone_of`'s
    notes say so).
  - `Volume::snapshot_inodes` is new: the snapshot's recorded inode count, which the owner admits a clone by.
  - The lib tests are in `clone_count_tests`. `a_clone_counts_its_snapshots_entries_whatever_its_origin_did_since`
    compares every count with a walk of its namespace after every step: the clone, its own changes, its snapshot and a
    clone of that. It then checks that an entry allowance of one past the tree admits exactly one file.
    `a_clone_of_a_recovered_snapshot_counts_its_snapshots_entries` does the same across a restart, over a snapshot
    that shares an untouched directory with its origin's head. It first checks that the image shares it.
- `crates/vfs/src/recover.rs`: `rebuild_snapshot_tree` returns its tree's live entries. They are the names of its own
  directories' images that name a child (`named_entries`; a whiteout names none, and the live count never holds one),
  plus each live entry of a directory it shares with the head, read from the head's node. `rebuild_snapshot` records
  them. The image's layout does not change.
- `crates/server/src/verbs.rs`: `admit_clone` reads the clone's inherited inodes from `Volume::snapshot_inodes`. A
  stale snapshot is refused there, before any slot is taken, with the refusal the clone itself would give
  (`StaleHandle`). The lib tests are in `clone_admission_tests`: `a_bounded_clone_is_admitted_by_the_inodes_its_snapshot_holds`
  and `a_dynamic_clone_holds_a_version_slot_for_each_inode_its_snapshot_holds`.

Evidence, all on the Mac Studio on 2026-11-19:

- **Red.** The numbers above.
- **Green.** `cargo test --locked -p slates-vfs --lib` passed 81 tests with 1 ignored. `cargo test --locked -p
  slates-server --lib clone` passed 3 of 3, the replayed-write clone test among them.
- **Mutations of the recovery count**, each on a copy:
  - Without the shared directories' entries, the restart test failed at the later clone, counting 7 of the tree's 9.
  - With zero recorded, it failed there too, counting 0 of 9.

## Siblings (the sweep: what else takes a clone's or a volume's counts from the wrong state)

- **Fixed since by A-561 (2026-11-19, `2026-11-19-a-restart-refused-a-clone-holding-its-own-snapshot.md`), found by
  this change's restart test:** a restart refused any clone holding a snapshot while it still
  shared a file with its origin (`Volume::clone_from_image` answered `RecoveryIncomplete`). The clone's snapshot marks
  such a file shared with the head (`capture_snapshot_tree`). Recovery resolves a head-shared entry only through the
  images of the head's own inodes (`shared_source`), and the file is not among them: the clone's image lists it with
  the numbers it shares with its origin. Shown across 32 histories; every one whose clone took a snapshot refused. This
  test's restart half therefore uses a clone with no snapshot of its own.
- **Fixed since by A-572 (2026-11-19), part 2:** a clone is admitted with no entry allowance. `admit_clone` sets the
  inode allowance only, where a create's admission (`admit_dimensions`) and a restart set both. The clone's namespace
  dimension is unbounded until a restart.
- **Open:** a resize moves the inode allowance with the new policy (`resize`) but leaves the entry allowance at the size
  the volume was made with.
- **Fixed since by A-572 (2026-11-19, `2026-11-19-a-restart-counted-whiteouts-as-live-entries.md`), found by
  reading:** recovery counts a whiteout as a live entry. `rebuild_entries` re-inserts an
  image entry that names no child as `Child::Whiteout` through `dir_insert`, which counts it. The live count and
  `live_len` never count one. A restarted overlay with whiteouts therefore counts each whiteout in its head's count and
  in a clone head's own directories, and its entry allowance's floor at the restart is raised by as many.
- **Fixed in place, a doc:** `rebuild_snapshot_tree`'s notes said it returned the numbers shared with the head. It
  returned nothing, and now returns the tree's entries.
- **Correct as they are:** the ones A-541 lists (the inode counter, the bytes, the base plane's frozen views, the carried
  orphans, a restore's references, the clone's own retention, and every count a recovery or a move rebuilds from an
  image).
