# vfs, bridges: a name past NAME_MAX was refused EINVAL, or looked up as ENOENT, never ENAMETOOLONG (found by pjdfstest through the Linux container lane)

**Date:** 2026-10-01. **Audit:** AUD-29-78 (the Linux container lane's pjdfstest cell). **Design:** §4.6 "POSIX and
transparency acceptance"; POSIX.1-2017 §2.3 `ENAMETOOLONG`.
**Found by:** the first root pjdfstest run over slates' own FUSE mount on Linux (`oci-linux`: the suite runs as
container root through a Docker Engine bind of `slates mount --shared`).

## Description

The run matched the reviewed Linux root list (`native-linux-fuse`, an NFSv3 adapter) on 1,798 of its 1,800
cases, with 18 failures it did not list, all in the `*/02.t` files (each checks `ENAMETOOLONG` for a
component of `NAME_MAX + 1` bytes):

```
mkdir/02.t:3   mkdir <256 bytes> 0755, expected ENAMETOOLONG, got EINVAL
chmod/02.t:5   chmod <256 bytes> 0644, expected ENAMETOOLONG, got ENOENT
unlink/02.t:4  unlink <256 bytes>, expected ENAMETOOLONG, got ENOENT
```

and the same for `chown`, `lchown`, `truncate`, `ftruncate`, `link`, `mkfifo`, `mknod`, `open`, `rename`, `rmdir` and
`symlink`.

## Root cause

`VfsError::InvalidName` meant two things: a name longer than `NAME_MAX`, and a name that is not a component
(empty, `.`, `..`, or holding `/` or NUL). The FUSE bridge mapped it to `EINVAL`, which is right only for the
second. A lookup never judged the length at all: `Volume::lookup` searched the directory, found nothing and
answered `NotFound`, so every call that resolves the name first (`chmod`, `unlink`, `rmdir`, `open` of an absent
name) answered `ENOENT`. The NFS lanes never showed it: the NFSv4 decoder refuses a name past `NAME_MAX` itself
(`NFS4ERR_NAMETOOLONG`, `v4/compound.rs`), and macOS's and Linux's NFS clients check `PATHCONF`'s `name_max`
before sending. The Linux FUSE client leaves every name up to its own 1,024-byte `FUSE_NAME_MAX` to the server.

## Impact

On the Linux FUSE mount (the daemon's mount and the container profile), any call with a 256..=1,024-byte
component got the wrong errno: `EINVAL` from a creation, `ENOENT` from anything that looks the name up first.
A program that tells "too long" from "absent" (a build tool shortening names, a test suite) was misled. Nothing
was made or lost. The same mapping put `EINVAL` on FSKit and `STATUS_INVALID_PARAMETER` on WinFsp for the length
case, and NFSv3 answered `NFS3ERR_INVAL` if a client ever sent such a name.

## Exact edits

- `crates/vfs/src/error.rs`: a new refusal, `VfsError::NameTooLong` (`ENAMETOOLONG`). `InvalidName` is now only a
  name that is not a component (`EINVAL`).
- `crates/vfs/src/names.rs`: `check_length` refuses a name past `NAME_MAX` `NameTooLong`; `check` calls it first.
- `crates/vfs/src/volume.rs`: `lookup` and `lookup_in` call `check_length` before searching, so a long name is
  never "not found".
- `crates/vfs/src/dirtree.rs`: a name that does not fit a fresh block (unreachable for a name within
  `NAME_MAX`) is `NameTooLong`, keeping the errno it had.
- Every bridge maps the new refusal to its host's code: FUSE `ENAMETOOLONG` (36, Linux), NFSv3
  `NFS3ERR_NAMETOOLONG`, FSKit the shim's `InvalidName` tag (`ENAMETOOLONG` in `SlatesVolume.swift`; a
  non-component name now goes as `Invalid`, `EINVAL`), WinFsp `STATUS_NAME_TOO_LONG` (Win32
  `ERROR_FILENAME_EXCED_RANGE`). The wire's refusal stays `InvalidName` for both, the SDK taxonomy's one kind for a name the
  volume will not take.

## Proof

- `crates/bridge-fuse/tests/volume_bridge.rs` `a_name_past_name_max_is_refused_enametoolong_on_every_path`:
  lookup, mkdir, create and unlink of a 256-byte name each answer -36, nothing is made, and a 255-byte name is
  made. Before the fix it failed (`lookup of a 256-byte name: left -2, right -36`).
- `crates/bridge-winfsp/src/lib.rs`'s mapping table pins `NameTooLong → STATUS_NAME_TOO_LONG`.
- The vfs, bridge-core, FUSE, NFS, WinFsp and FSKit suites pass (73 test binaries, macOS, 2026-10-01).
- The `oci-linux × pjdfstest` rerun (below in the ledger) no longer fails the `*/02.t` cases.

## Sibling sweep

- `crates/vfs/src/xattr.rs` `check_name` still refuses a long extended-attribute name `InvalidName`. Linux answers
  `ERANGE` for that case and macOS `ENAMETOOLONG`, so it is a separate question of per-host mapping; reported,
  not changed here.
- The NFSv3 decoder relies on clients honouring `name_max`; with this change the volume's own refusal reaches the
  wire as `NFS3ERR_NAMETOOLONG` if one does not.
