# A takeover refused an overlay whose base directory an outsider had changed

**Status:** fixed 2026-10-22 (A-153, approved by Ada in plan mode). Reproduced on the box (Linux 7.2.9) and in the volume
core, with the refusing comparison logged. Found while fixing A-151.

**Severity:** the loss of a volume across a daemon takeover. Recovery refused a whole overlay volume when any loaded
base directory's entries had changed since the last publication: an outsider adding, removing or renaming a file
beneath it. The image's content stays in anchor RAM, but the volume is not rebuilt, and every later recovery refuses it
again for the same reason. A FUSE mount the anchor held across the death then answers nothing. Reads publish nothing,
so the window is not the instant of the takeover but everything since the last publication: a `git pull` in the base
repository, hours before a crash, is enough.

## Reproduction

- **On the box:** `crates/cli/tests/cli.rs` `a_takeover_recovers_an_overlay_whose_base_directory_an_outsider_changed`.
  A volume over a base directory holding `notes.txt` is mounted; the file is read; a file is created through the mount
  (its barrier publishes); an outsider adds `added.txt` to the base directory; the daemon is killed. The successor
  logged:

  ```
  slates-server: partition 0: volume basechanged not rebuilt: EIO
  slates-server: shard 0: recovered 0 volumes from their images (1 refused), ...
  ```

  and the read of `notes.txt` through the mount failed.
- **In the volume core:** `crates/vfs/tests/base.rs`
  `an_overlay_recovers_after_an_outsider_changed_its_base_directories_since_its_image` is refused `RecoveryIncomplete`.
  A temporary line at the comparison in `BasePlane::recover` showed the cause:

  ```
  RECOVDBG directory [] refused: image Fingerprint { dev: 1, ino: 1, size: 0, mtime_ns: 0, ctime_ns: 0, ..., birth_ns: 0 }
                                 now   Fingerprint { dev: 1, ino: 1, size: 0, mtime_ns: 10, ctime_ns: 10, ..., birth_ns: 0 }
  ```

  It is the same directory (device 1, inode 1), with only its modification and change times moved by the added file.

## Root cause

`BasePlane::recover` (`crates/vfs/src/base_recovery.rs`) reopens each directory the image recorded and requires its
whole fingerprint to equal the image's. The recovery design asks for "validated source identity" ("reopening a path
alone cannot substitute another base"), and a whole fingerprint is stricter than identity: it includes the times and
size that any change to the directory's entries moves. `an_unvisited_overlay_recovers_its_verified_source`
(`crates/vfs/tests/base.rs`) asserted the refusal, so the strictness was tested as if it were the rule.

Nothing in the recovered state needs the directory's entries unchanged:
- every recovered listing starts unloaded and is read again on first use;
- an untouched entry beneath is checked against the disk at that first listing (A-151);
- a witnessed one is checked by its drift rule.

## The fix (A-153)

Recovery compares a directory's identity: its device, inode number and birth time (`Fingerprint::same_object`, the three
fields A-151's `FileTag` digests), instead of its whole fingerprint.
- A directory whose entries changed recovers.
- One another directory replaced is still refused, as §4.8 says ("source replacement refuses").
- On a filesystem that records no birth time, the device and inode number decide. A directory replaced by one that took
  its number is then taken for the recorded one, but serves no wrong bytes: its untouched entries are renumbered at the
  first listing (A-151), its witnessed files show drift, and only a whiteout could hide a name of the new directory.
  Refusing would cost the volume whole.

## Exact edits

- `crates/vfs/src/inode.rs`: `Fingerprint::same_object`; `FileTag`'s doc names it.
- `crates/vfs/src/base_recovery.rs`: `BasePlane::recover` compares `same_object`.
- Tests:
  - `crates/vfs/tests/base.rs`:
    - `an_overlay_recovers_after_an_outsider_changed_its_base_directories_since_its_image` (new, red before);
    - `an_overlay_whose_base_directory_another_replaced_is_refused` (the guard, green before and after);
    - `without_birth_times_an_overlay_recovers_after_an_outsider_changed_its_base_directory` (new);
    - `an_unvisited_overlay_recovers_its_verified_source`, whose second half had asserted the refusal and now recovers;
  - `crates/cli/tests/cli.rs` `a_takeover_recovers_an_overlay_whose_base_directory_an_outsider_changed` (new, red before).
- Docs: `SLATES_DESIGN.md` §4.8 and A-153, `docs/wip/recovery.md`, GAPS.

## Verification

| | before | after |
|---|---|---|
| the CLI takeover test (the box) | "volume basechanged not rebuilt"; "recovered 0 volumes (1 refused)"; the mount answered nothing | "recovered 1 volumes (0 refused)"; the base file, the outsider's file and the mount's own file read |
| the vfs recovery after a changed directory | `RecoveryIncomplete` | recovers; every name reads the disk as it is now |
| the guard: a directory another replaced | refused | refused |

- **macOS:**
  - the vfs, bridge, land and base suites: 943 of 943;
  - the server's `recovery` (39) and `nfs_mount` (40);
  - workspace clippy `-D warnings`, the doc gate, `cargo xtask check` and fmt.
- **The box (Linux 7.2.9, load average 39 to 56):**
  - workspace clippy `-D warnings`;
  - the vfs suite (369);
  - the server's `fuse_mount` (3), `recovery` (41) and `virtiofs` (18);
  - the guest tests (3);
  - the CLI suite: 55 of 56. The one failure was the busy unmount already recorded open in GAPS
    (`a_fuse_mount_survives_its_daemons_kill_with_its_open_files_usable`, "fusermount3 … Device or resource busy").
    It touches nothing this change does, and it passed 3 of 3 alone at load average 41. Its diagnosis is queued.
- **Open, found here:** a loaded base directory removed after the last publication still refuses the whole volume
  (GAPS).
