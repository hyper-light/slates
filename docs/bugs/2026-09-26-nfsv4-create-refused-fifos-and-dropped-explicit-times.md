# NFSv4 CREATE refused FIFOs and sockets, and the client dropped explicit times

**Date:** 2026-09-26. **Found:** pjdfstest over the new NFSv4.2 conformance transport
(`native-linux-nfs4`) in a privileged Linux container: 2946 pass / 5818 fail against NFSv3's 6970 /
1800 on the same host.

## Description

- `mkfifo` and a UNIX socket's `bind` failed with 527 (the kernel's `EBADTYPE`, from
  `NFS4ERR_BADTYPE`): CREATE served only directories and symlinks. The failures cascaded through every
  pjdfstest file that makes a FIFO or socket before testing something else.
- `utimensat` with explicit times left the times unchanged: `supported_attrs` did not name
  `time_access_set` or `time_modify_set`, and the Linux client sends only attributes the server
  supports.

## Root cause

The front end's CREATE mapped only the two `createtype4` arms with a v3 MKDIR/SYMLINK counterpart; the
v3 MKNOD was never wired. The supported set was the set this server *encodes*, which excludes the
write-only times SETATTR accepts.

## Impact

Linux v4 clients could not create FIFOs or socket names in a volume, and could not set explicit times
(`touch -d`, `cp -p`, `tar` extraction, rsync's `-t`).

## Exact edits

- `v4/compound.rs`: CREATE decodes `createtype4` into `Creation` (directory, link, or a node through
  MKNOD with its `specdata4`); `attrset` reports the attributes applied.
- `v4/v3call.rs`: `mknod_args`.
- `v4/attr.rs`: `time_access_set` and `time_modify_set` in `supported()`; `check_readable` refuses a
  GETATTR or READDIR of either with `NFS4ERR_INVAL` (§5.6); the duplicate `ftype4` table in
  `compound.rs` removed.
- Tests first: `nfs_v4_kernel.rs` `special_names` (failed with 527) and `explicit_times`;
  `tests/v4.rs` `the_write_only_times_are_supported_settable_and_never_read`.

Sibling check: block and character devices go through the same MKNOD and are refused there (A-26), as
over NFSv3.
