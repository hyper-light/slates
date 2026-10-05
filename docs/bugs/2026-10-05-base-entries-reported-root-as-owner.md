# Base entries reported root as their owner (2026-10-05)

## Description

Through a macOS NFS mount of an overlay volume over a user's own repository (`volume create --base DIR`, then
`slates mount`), every base file and directory showed uid 0 and gid 0. The host has them as the user's (501:20). The
volume's root is the user's, so new top-level files could be created, but every existing file and directory refused
the user's writes with `EACCES`: the overlay could not be edited where it overlaid. The gap had been recorded in
`docs/wip/base-fuse.md` §5 since September ("`Fingerprint`/`BaseEntry` carry no owner").

## Root cause

The read-only host seam's `Fingerprint` (`crates/vfs/src/inode.rs`) carried device, inode, size, both times and the
mode, not the owner. A base inode's attributes default their owner to 0.

## Impact

Every overlay volume on every transport. An agent working in an overlaid repository could not change any file the
repository already held. Landing was unaffected: it plans from the volume's own records and writes under a grant.

## Edits

- `Fingerprint` gains `uid` and `gid`. They come from `stat` on Unix (`crates/base/src/unix.rs`); Windows has no POSIX
  owner and reports 0, as its bridge maps every file. `SimHost` carries an owner per node with `chown`.
- Base inodes take the owner when they are made from a listing, for files, symlinks and directories, and when they
  re-adopt the disk's fingerprint (`Inode::adopt_observed` takes the mode and the owner together).
- The landing engine's drift checks ignore the owner as they ignore the ctime: a `chown` is a metadata-only change, not
  drift of the bytes, as before.
- `IMAGE_VERSION` 16 (the fingerprint is in the image).
- Test, red first: `a_base_entry_reports_its_owner_as_the_disk_holds_it` (vfs `tests/base.rs`). A base directory and
  file owned 501:20 report 501:20, and a copy-up keeps it.

## Measured after

macOS, an overlay of `crates/` (688 files, 15.4 MB). Ten base files appended, five created, three removed through the
mount. `land` without a grant planned exactly 10 replace, 5 create and 3 delete (161,916 bytes) in 4 ms, CLI process
included, against `diff -rq` finding the same 18 in 29 ms.

## Siblings (not changed here)

- An outsider's metadata-only change to an untouched base file (`chmod`, `chown`) does not change its directory, so
  the overlay's directory check does not see it. It shows when the listing next reloads. Mode and owner alike, in a
  test or not; the content and size path is unaffected.
