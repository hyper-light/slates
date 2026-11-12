# Windows base file modes were raw file attributes (2026-10-26)

## Description

Building landing on Windows (A-185), the worked example's landing (`crates/land/tests/os.rs`,
`the_worked_example_lands_on_a_real_directory`) ended in `Conflict(TypeChanged)` for its removal of the directory
`/tree`, on jupiter (Windows 10.0.26200), while it lands on macOS and Linux.

## Root cause

`crates/base/src/windows.rs`'s `fingerprint_of_handle` set `Fingerprint.mode` to the handle's `dwFileAttributes`.
Every consumer reads a fingerprint's mode as a POSIX `st_mode`, as the Unix host fills it. The landing's manifest
decides a witnessed entry's kind by `mode & S_IFMT == S_IFDIR` (`crates/land/src/manifest.rs`, `kind_is_dir`), so a
witnessed directory (attributes `0x10`) read as a file, its delete was planned as a file's, and the verdict met a
directory on the disk (`TypeChanged`). The base plane copies the mode into the inode as well
(`crates/vfs/src/base.rs`, `adopt_fingerprint` and the listing's inodes), so a Windows base entry's attributes carried
no POSIX type or permissions.

## Impact

Windows only, since the base host was written:
- a landing could not remove a witnessed directory (measured above);
- a moved file's mode changed with attribute bits a rename may set, which the engine compares (`displaced_matches`);
- from reading the code, not measured: every Windows base entry's mode, as a transport shows it, had no type bits.

## Exact edits

`crates/base/src/windows.rs`: `posix_mode`, from the attributes and the reparse tag: a link or junction as
`S_IFLNK | 0o777`, a directory as `S_IFDIR | 0o755`, a read-only file as `S_IFREG | 0o444`, any other file as
`S_IFREG | 0o644`. Windows keeps no POSIX mode; the read-only bit is the one permission on the file itself, and the one
the landing writes back (`os_windows.rs`, `set_mode`). The worked example lands after it, with the land suite whole,
on jupiter.

## Siblings checked

`fingerprint_of_handle` is the only place a Windows fingerprint is made (`entry_fingerprint`, `fstat`, `list` and
`fingerprint_dir` all call it).

## Superseded (2026-11-09)

Main fixed the same fault as A-262 (`windows_mode_and_bits` in `crates/base/src/lib.rs`, DrvFs's mapping with the
attribute bits kept apart). Merging main `1c49c33f` into the Windows writer's branch kept that mapping alone and removed
this record's `posix_mode`, so the base has one. They differ on a reparse point that does not redirect (a cloud
placeholder, a deduplicated file): this record's read the tag and showed a regular file, A-262's shows a link (GAPS).
