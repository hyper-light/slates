# A pinned snapshot was refused as a green's base after a restart

**Found** 2026-11-08 by liveness while slicing recovery; handed to slabs as a sibling of
`2026-11-07-a-removed-base-directory-refused-the-whole-volume-at-recovery.md` (the base plane after recovery).

**Description.** After a daemon restart, a pinned overlay's snapshot from before the restart, named as the base of a
new green, was refused `ConsistentBaseUnavailable`, though nothing about the snapshot or its base had changed. Greens
made before the restart recovered their held origins from the same snapshot; only a new one was refused.

**Root cause.** A green's base must be a complete immutable snapshot (§4.16): `Volume::snapshot_is_complete`
(`crates/vfs/src/coverage.rs`) requires every merged directory in the frozen tree to hold every name its base listing
has. It took those names from the base plane's cached listings. A recovered plane holds none until each directory is
read again on first use (A-153), so every merged directory read as never listed, and the snapshot as incomplete.

**Impact.** No green could be made over any overlay's snapshot taken before a restart until something happened to
list each of its directories again.

**The edits.**
- `crates/vfs/src/coverage.rs`: the check takes its listing source (`snapshot_is_complete_under`); the hostless form
  keeps the cached listings.
- `crates/vfs/src/base.rs`: `Overlay::snapshot_is_complete` reads a listing the plane has not read since recovery from
  the host, uncached, so the directory's first real listing still renumbers its untouched entries (A-151); a directory
  recovery could not reacquire refuses `BaseUnavailable`, a real reason.
- `crates/server/src/merge_service.rs`: `CreateGreen` judges an overlay's base snapshot with its host.
- The test: `a_green_over_a_pinned_overlays_snapshot_is_made_after_a_restart` (`crates/server/tests/recovery.rs`):
  pin an overlay over `f` and `sub/g`, snapshot it, restart the daemon over the same segment, and create a green on
  that snapshot. Red before the fix (`Refused(ConsistentBaseUnavailable)`); the green is made after it, and its files
  read the snapshot's bytes.
