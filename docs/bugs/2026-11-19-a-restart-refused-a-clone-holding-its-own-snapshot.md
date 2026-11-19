# A restart refused a clone holding its own snapshot (2026-11-19)

## Description

A restart refused to rebuild any clone that held a snapshot of its own while it still shared a file with its origin:
`Volume::clone_from_image` answered `RecoveryIncomplete`. A clone shares every file it has not rewritten, so in
practice this was any clone that had taken a snapshot. A-549's first restart test found it.

On the Mac Studio (2026-11-19), with main's vfs sources (`a613562eb`), a 32-case matrix was run. The origin changes a
directory after its snapshot or not, and the clone removes an inherited file, makes a directory, takes a snapshot of its
own, and has that snapshot cloned, each or not. Every case whose clone took a snapshot was refused, and every other case
was rebuilt. A log line placed in the refusing arm printed "a head-shared entry, number 1970324836974595, has no head
image". That number is the origin's file `d/inner-0` (prefix 7, number 3), which the clone never touched.

## Root cause

Capture and recovery disagreed about which inodes the head holds.

- `capture_snapshot_tree` (`crates/vfs/src/recover.rs`) marks a snapshot's file `SharedSource::Head` whenever the
  head's table holds its very handle. For a clone, that includes a file it still shares with its origin. The capture
  is right: such a record is "the version the head still holds, shared by CoW handle identity", as `SharedSource::Head`
  is defined.
- A clone's image carries those records by number alone (`origin_shared`, A-64). Recovery rebuilds the clone's head
  table over its origin snapshot's, so the rebuilt table holds them.
- `shared_source` resolved a head-shared entry only among `head_images`, which are the images of the head's own
  inodes. For a clone that is a strict subset of what its head holds, so a head-shared origin record found no image
  and the rebuild refused.

## Impact

- At every daemon restart, each such clone was listed as not recovered (`RecoveryCause::ImageRefused`), and its
  content could not be reached through slates, though its image and records stayed in the anchor's RAM.
- A clone of its snapshot was not recovered either (`OriginNotRecovered`).
- No test had restarted a clone with a snapshot of its own, in `crates/vfs`, `crates/server` or `crates/client`.
- The image is unchanged by the fix, so images already published, by a daemon before the fix, rebuild with it.

## Exact edits

- `crates/vfs/src/recover.rs`, `shared_source`: a head-shared entry resolves through the head's rebuilt table, as an
  origin-shared one resolves through the origin snapshot's table, and its kind is the inode's. The table holds every
  number the head holds, its own and those it shares with its origin. The fix is on the recovery side, not the capture
  side (an origin rule checked first for a clone), for three reasons:
  - The capture follows the image's rules. Only recovery's lookup was narrower than the set they name.
  - Images published before the fix still name these entries `Head`, and a capture-side change would leave them
    refused. Neither the layout nor `IMAGE_VERSION` changes.
  - Both encoders of a publication (the struct builder and the streamed one) stay as they are.

  The deadlist pass already treats `Head` and `Origin` alike ("not this snapshot's to free"), so the rebuilt
  deadlists are those the live volume kept.
- `crates/vfs/src/volume.rs`, the lib test
  `a_clone_holding_its_own_snapshot_is_rebuilt_at_a_restart_whatever_it_shares_with_its_origin` in
  `clone_count_tests`, a 6-bit matrix with 48 distinct cases. To the 32-case matrix it adds the clone rewriting an
  inherited file (after its snapshot where it took one), so its snapshot keeps the origin's record while the head
  holds its own (`SharedSource::Origin`).

  Each case restarts the origin, the clone and the clone of the clone, then compares each against itself before the
  restart:
  - The clone and the clone of the clone must read the same: every path's kind, number, size and bytes.
  - Their inode and entry counts must match.
  - A clone of the clone's snapshot made after the restart must read and count as one made before it.

  The rebuilt clone's image is not compared with its first image, because recovery deliberately re-stamps the birth
  of a directory record a clone's snapshot keeps from its origin (A-64).

Evidence, all on the Mac Studio on 2026-11-19:

- **Red.** With main's `recover.rs`, 32 of the 48 cases failed at "rebuild the clone: RecoveryIncomplete": every case
  with a snapshot of the clone's own, and none other.
- **Green.** With the fix, all 48 cases passed, and `cargo test --locked -p slates-vfs --lib` passed 83 with 1 ignored.

## Siblings

- **Checked:** `held_directory`, which serves A-395's directories shared with the head, is the only other user of
  `head_images`. A clone's snapshot never shares a directory with the head (its directories are captured whole), so
  its lookup among the head's own images is complete.
- **Not run here (main's):** the recovery oracle and the server's recovery suite over a clone with a snapshot of its
  own. Before this change, no test restarted one.
