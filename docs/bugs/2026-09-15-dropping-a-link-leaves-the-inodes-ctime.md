# Dropping a link left the inode's ctime where it was

Date: 2026-09-15
Area: `crates/vfs/src/volume.rs` (`Volume::drop_link`, reached by `unlink`, `rename` over an
existing name, `rmdir`)
Severity: a POSIX timestamp rule broken on every transport (the volume is the source of every
`stat`): a file that lost one of its names reported the status-change time of before the loss.

## Symptom

pjdfstest `unlink/00.t` ("successful unlink(2) updates ctime", cases 29–110 for a file with two
names) and `rename/23.t` ("destination inode should have reduced nlink and updated ctime",
cases 7–39) failed as bare `not ok` lines (`test_check $ctime1 -lt $ctime2`). Reproduced through
a live `slates mount` with the suite's own binary, 2026-09-15:

```
$ pjdfstest create f 0644; pjdfstest link f g
$ pjdfstest stat f ctime; sleep 1; pjdfstest unlink g; pjdfstest stat f ctime
1789518865
1789518865                     # same, and still the same 4 s later (past the attribute cache)
$ pjdfstest create a 0644; pjdfstest link a b; pjdfstest stat b ctime; sleep 1
$ pjdfstest rename f a; pjdfstest stat b ctime; pjdfstest stat b nlink
1789518940
1789518940                     # nlink 1: the count moved, the time did not
```

## Root cause

`adjust_nlink` (the `+1` of `link`) stamps `ctime`; `drop_link` (the `-1` of `unlink`, of a
`rename` that replaces a name, and of `rmdir`) decremented `nlink` and stamped nothing. POSIX.1-2024
`unlink()`: "if the file's link count is not 0, the last file status change timestamp of the file
shall be marked for update"; the replaced name in `rename()` is the same inode change, and every
Unix (and pjdfstest) treats it so. The directory's times were right (`touch_dir`); only the
inode's were not.

## Fix

`drop_link` stamps `ctime` with the operation's wall time, whatever the count becomes: at zero the
inode is either reclaimed (nothing observes the stamp) or kept as an orphan for its open
descriptors, where `fstat` after an unlink-while-open must show the change too.

## Verification

- `crates/vfs/tests/edges.rs::dropping_one_name_of_a_linked_file_advances_the_survivors_ctime`:
  a two-name file, one name unlinked — `nlink` 2 → 1 and `ctime` strictly later under the step
  clock; then a two-name file with one name renamed over — the same; the renamed file took the
  name. Fails without the fix (`ctime` equal), passes with it.
- pjdfstest through a live mount after the fix: the record in the same change (`unlink/00.t`
  and `rename/23.t` no longer fail their `test_check` lines for regular files).

## Sibling sweep

- `adjust_nlink` (link, directory `..` counts) already stamped; `touch_dir` stamps the
  directory's `mtime`/`ctime` on every entry change; `chmod`, `chown`, `set_times`, `truncate`
  and `write` stamp theirs. No other count or attribute changes without a stamp (grep of
  `attrs.nlink` and `attrs.ctime` in `volume.rs`).
- Replay (`recover.rs`) re-applies journaled ops through the same methods, so a recovered
  volume's `ctime` after a dropped link is the replay's time, as every replayed stamp is; the
  image the daemon publishes after each NFS mutation carries the stamp itself.
