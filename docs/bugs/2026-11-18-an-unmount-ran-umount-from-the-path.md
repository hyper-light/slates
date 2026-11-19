# An unmount ran `umount` from the PATH, so a host whose PATH lacked /sbin could not take a mount down (2026-11-18, §4.6, A-507)

## Description

`slates_api::mount::unmount` and `mount::unmount_busy` removed a macOS mount by running `umount` (and `umount -f`)
found on the `PATH`. g264 found that a kiln runner's `PATH` held only `/usr/bin` and `/bin`, so `/sbin/umount` was
never found: no mount could be taken down there. Kiln then destroyed the volume anyway, until g264 made it keep the
volume while the mount table still lists the mount (kiln 6e1d95f32). The same was true of the BSDs' `umount`.

## Root cause

The unmount was delegated to a program looked up by name, so it depended on the caller's environment. The kernel
offers the call itself: `unmount(2)`, which a mount's own user may make without privilege ("[EPERM] The caller is not
the super-user, and the mount() was not done by the user", macOS `man 2 unmount`), with `MNT_FORCE` for the forced form.
The answer was also believed from the program's exit alone, where kiln had learned to believe an unmount only when the
kernel's mount table agrees.

## Impact

On a host whose `PATH` lacked `/sbin`, no slates mount on macOS (or a BSD) could be unmounted through the library or
`slates unmount`; the refusal named only the error of starting `umount`. Nothing was lost: the mount stayed.

## Fix (exact edits)

- `crates/api/src/mount.rs`: `unmount` and `unmount_busy` share `unmount_as`. It builds the request
  (`unmount_request`, the point as a C string and whether it lets go; a NUL byte is `MountPoint`), names the mount
  point as the kernel's table records it without touching the mount (`recorded_point`: the parent's real path with its
  name), and finds the mount there in the table (`mount_at`, the last listed: a later mount shadows an earlier one).
  None there is `NotMounted`, with no call made. It then asks the OS (`ask_unmount`, two halves of one signature:
  `unmount(2)` with `unmount_flags`' `MNT_FORCE` on macOS and the BSDs; `fusermount3 -u [-z]` on Linux, whose mount is
  that setuid helper's and whose `umount2(2)` needs `CAP_SYS_ADMIN`), reads the table again, and judges (`judged`): gone
  is done, still listed is the call's refusal or `StillMounted`. `refusal_of_errno` types `unmount(2)`'s errors:
  `EBUSY` is `Busy`, `EINVAL` is `NotMounted`, `ENOENT` and `ENOTDIR` are `MountPoint`, others `Unmount`.
- `crates/api/src/error.rs`: `MountFault::Busy`, `StillMounted` and `MountTable` (the table could not be read);
  `Unmount` and `NotMounted` restated for the call and the table.
- `crates/api/src/lib.rs`: `CONTRACT` = 42. `crates/api/Cargo.toml`, `Cargo.lock`: `slates-bridge-oci` on Unix (the
  mount table).
- `crates/api/tests/api.rs`: the held mount's plain unmount is `Busy` on macOS. `crates/cli/src/verbs.rs`: a doc line.
- Docs: design §4.6's contract log and A-507, GAPS, `docs/library.md`.

## Tests

- `an_unmount_is_asked_as_the_os_takes_it`, `the_kernels_unmount_refusals_are_typed`,
  `an_unmount_is_believed_only_when_the_table_agrees` (pure, every Unix host) and
  `an_unmount_of_a_path_where_nothing_is_mounted_is_refused_not_mounted` (the host's own table, no call made), all in
  `crates/api/src/mount.rs`.
- Red, once, on a copy where `EBUSY` maps to the old untyped `Unmount` and no request asks to let go: the mapping and
  request tests fail.

## Siblings

- The daemon's own Linux FUSE mount and unmount (`crates/bridge-fuse/src/mount.rs`) and Linux's capability check run
  `fusermount3` by name too. It lives in `/usr/bin` or `/bin`, which every `PATH` holds, unlike macOS's `/sbin`. Left
  as is: it is the mount route, and changing how it is found is its own piece.
- `crates/cli/examples/slates_mount.rs` and `vfs_tails.rs` run `umount` by name (examples, not shipped).
