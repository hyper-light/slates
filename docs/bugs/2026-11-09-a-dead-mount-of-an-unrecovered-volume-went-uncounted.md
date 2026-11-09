# A dead mount of an unrecovered volume went uncounted

**Found** 2026-11-08 by slabs, reading the code while fixing the destroy of a volume recovery could not rebuild
(`2026-11-08-a-volume-recovery-could-not-rebuild-could-not-be-destroyed.md`). That first reading said such a mount was
"let go but never unmounted"; A-102 forbids the unmount, so the defect is narrower. Fixed 2026-11-09 (slabs).

**Description.** A Linux FUSE mount the predecessor kept, of a volume its successor could not rebuild (its base gone),
is dead: nothing can serve it, and it answers `ENOTCONN`. A-102 says what a dead mount gets: left in place for its user
to unmount, counted `fuse.dead_mount_left`, and its attachment's record ended. Such a mount got only the first: it went
uncounted, and its record stayed in the catalog until the volume was destroyed.

**Root cause.** Recovery ends dead mounts in `verbs::reconcile_lost`, which runs only for a volume it rebuilt; a volume
it could not rebuild returns before it (`rebuild_record`).

**Impact.** No data at risk; the mount behaved as A-102 asks. An operator reading the daemon's counters missed one dead
mount per such volume, and the catalog held an attachment record for a mount nothing serves.

**The edits.**
- `crates/server/src/verbs.rs`: `leave_dead_mounts`, run where `rebuild_record` records a volume it could not rebuild:
  each FUSE attachment's mount is queued for `fuse::leave_stale` (left in place and counted) and its record ends.
- The test: `a_kept_fuse_mount_of_a_volume_recovery_could_not_rebuild_is_left_dead_and_counted`
  (`crates/cli/tests/cli.rs`, Linux): the volume refused `NotRecovered`, the mount still in the kernel's table, a `stat`
  through it answering `ENOTCONN`, one dead mount counted.
