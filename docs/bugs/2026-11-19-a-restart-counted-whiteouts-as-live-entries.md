# A volume's entry count and allowance missed the namespace rule in three places (2026-11-19, A-572)

A-549's sweep found three siblings, each against §4.2's namespace dimension: the live-entry count, and the allowance it
is judged against. They are taken in order of harm, one commit each.

## 1. A restart counted whiteouts as live entries

### Description

A restart counted every whiteout in a rebuilt directory as a live entry. A whiteout is the mark an overlay leaves where
a base name was removed, and the live count never holds one: `dir_remove` takes one off as it turns a name into a
whiteout, and `DirNode::live_len` skips them. So after every restart, an overlay's count was its live count plus the
whiteouts in its own directories, and the restart floored its entry allowance at that higher count.

Measured on the Mac Studio (2026-11-19), with the test alone on main. The overlay sat over a base holding `/top` and
three files in `/src`. With `/top` and two of the files removed (three whiteouts), and a file and a directory of its
own, it counted 3 live entries, as the recount of its image did. Restarted, it counted 6.

### Root cause

`rebuild_entries` (`crates/vfs/src/recover.rs`) rebuilds each directory from its image. An entry that names no child is
a whiteout, which `child_for` returns as `Child::Whiteout`, and it is placed through `dir_insert` like any other entry.
`dir_place` (`crates/vfs/src/volume.rs`) counted every placement. The live path never places a whiteout (it turns a
name into one in place, with `set_child`), so only a recovery reached that count.

### Impact

- An overlay with whiteouts counted one entry too many per whiteout after each restart, in its head and in a clone's
  own directories. The count never came back down: a whiteout replaced by a new name adds one, as a name should.
- The restart's re-admission floors the entry allowance at the recovered count (`entry_allowance(size).max(count)`), so
  the allowance's floor rose by as many.
- A recovered snapshot's own count was already right (A-549 derives it from the image's named entries).

### Exact edits

- `crates/vfs/src/volume.rs`:
  - `dir_place` counts a placement only when it names a child.
  - `dir_insert` admits a placement against the allowance only when it names a child.
  - `dir_unplace`'s notes name its one caller, a rename's undo of a named placement.
- `crates/vfs/src/recover.rs`: A-549's note in `rebuild_snapshot_tree` no longer says `dir_insert` counts a whiteout.
- `crates/vfs/src/base.rs`, the lib test `a_restart_counts_no_whiteout_as_a_live_entry`. It sets up an overlay over a
  base holding `/top` and three files in `/src`. It removes `/top` and two of the files (three whiteouts), and makes a
  file and a directory of its own. Its count before the restart, after it, and the recount of its image (the names its
  directories carry that name a child) must all be equal.

Evidence: red first with the test alone on main's `volume.rs` and `recover.rs` (6 counted against 3), green
with the change, and `cargo test --locked -p slates-vfs --lib` 85 passed, 1 ignored, on the Mac Studio, 2026-11-19.

## 2. A clone was admitted with no entry allowance

### Description

The server's clone admission set a clone's inode allowance but no entry allowance. A create's admission
(`admit_dimensions`) and a restart's re-admission both set it. So a clone's namespace dimension had no bound until the
daemon's next restart, and a clone could make names, hard links and new names alike, past its quota's share.

Measured on the Mac Studio (2026-11-19), with the test alone on main. The origin was bounded at 64 KiB, whose share is
4,096 entries. Its snapshot held 4,104 names (the share and 8 past it), and its clone carried an entry allowance of
`u64::MAX`.

### Root cause

`admit_clone` (`crates/server/src/verbs.rs`) called `set_inode_allowance` alone. Since A-549 the clone's entry count is
its snapshot's, so there was a count to floor an allowance at, but nothing set one.

### Impact

- Until a restart, every clone could hold any number of names. Only the shard's directory-block slab bounded them, which
  is the pressure §4.2's namespace allowance exists to keep one volume from putting on the rest: hard-link fan-out and
  name churn.
- A restart did set the allowance, so a clone's limit changed across a restart.

### Exact edits

- `crates/server/src/verbs.rs`, `admit_clone`: after the inode allowance, the clone's entry allowance. It is a
  create's share (`entry_allowance(size)`), never below the entries the clone holds, which are its snapshot's (A-549),
  as a restart's re-admission floors it. A refusal is counted as `volume.allowance_refused` (there is none, since the
  floor is the count). It reserves nothing, so it is set from the clone's own count once the clone exists.
- The lib test `a_clone_is_admitted_with_an_entry_allowance_never_below_its_snapshots_entries` (`clone_admission_tests`):
  - A bounded origin has its allowance raised past its share, is linked up to the share plus 8 names, is snapshotted,
    and is left one under the share, then cloned. The clone's allowance must be its snapshot's 4,104 entries, and its
    count must equal a walk of its namespace.
  - A small origin's clone must get the share.
  - No allowance may be refused.

Evidence: red first with the fix's lines taken out (`u64::MAX` against 4,104), green with them; `cargo test --locked -p
slates-server --lib clone` 11 passed, 1 ignored (main's open-bug test of a live clone), on the Mac Studio, 2026-11-19.
