# A removed base directory refused the whole volume at recovery

**Found** 2026-10-22 while fixing A-153, and left open there (GAPS, A-153's "Open, found here"). Fixed 2026-11-07
(slabs).

**Description.** An overlay's base directory that the volume had listed (so the image recorded it) and that was then
removed from the disk after the last publication made the restart refuse the whole volume, `RecoveryIncomplete`: every
file the volume held, its own writes included, was lost to the successor because one directory beneath the base had
gone. A directory another replaced at the same path did the same.

**Root cause.** `BasePlane::recover` (`crates/vfs/src/base_recovery.rs`) reopens every directory the image recorded,
component by component (`open_source`), and checks each is the recorded one (`Fingerprint::same_object`, A-153). A
directory that would not open, or was another, failed the reconstruction, and so the volume. §4.8's failure matrix
already says what such a directory is at runtime: "Base directory unreadable or gone: Refused (`BaseUnavailable{path,
errno}`) for untouched entries under it; witnessed and pinned entries keep serving."

**Impact.** A user who removed one base subdirectory between the last publication and a daemon's death (or a kill, or
a crash) lost the overlay at the restart, its own files in every directory included.

**The edits.**
- `crates/vfs/src/base.rs`: `BasePlane::unavailable`, the directories a recovery could not reacquire as the recorded
  ones, each with its recorded source and fingerprint and the errno its untouched entries refuse with
  (`GONE_DIRECTORY_ERRNO`, `ENOENT`, when the host says only that it is not there). `load_listing` and `listing_dir`
  refuse `BaseUnavailable` for them, so an untouched entry's lookup, read and listing refuse typed, while a lookup of
  the volume's own entry there is answered from the volume first and serves.
- `crates/vfs/src/base_recovery.rs`: `recover` keeps a directory that will not open, or is not the recorded one, as
  unavailable instead of failing; a witness homed in it is kept (it refuses with its directory); `image` writes the
  unavailable directories' records again, in inode order with the rest, so a later recovery tries the recorded
  directory again. A directory another put at the path is never substituted (A-153's guard holds).
- Tests: `an_overlay_whose_loaded_base_directory_was_removed_recovers_and_refuses_only_that_directory` and
  `an_overlay_whose_base_directory_another_replaced_never_serves_the_other` (`crates/vfs/tests/base.rs`, SimHost);
  `an_overlay_whose_base_directory_was_removed_serves_its_own_files_after_a_restart` (`crates/server/tests/recovery.rs`,
  NFS and the client); `an_overlay_whose_base_directory_was_removed_serves_its_own_files_through_a_kept_mount`
  (`crates/cli/tests/cli.rs`, a kernel mount kept across the restart). The vfs test failed before the fix
  (`an overlay whose loaded base directory is gone still recovers: NotFound`).

**Changed with it.** A-153's guard test expected a replaced directory to refuse the whole volume; it now expects the
volume to recover and the replaced directory's file to refuse `BaseUnavailable`, never reading the other directory's.

**Not covered.** A directory that comes back while the daemon runs stays unavailable until the next restart. A clone
taken from such a volume opens its directories afresh and meets the host's own refusal there.
