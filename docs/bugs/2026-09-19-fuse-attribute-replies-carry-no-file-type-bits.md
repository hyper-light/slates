# FUSE attribute replies carried no file-type bits, so a real kernel marked every inode bad

**Date:** 2026-09-19. **Found by:** AUD-02's mounted regression
(`crates/bridge-fuse/tests/coherence_mount.rs`), the first run of the FUSE serve loop against a real
kernel (Linux 6.12.76-linuxkit in a container on this box, `fusermount3` 3.17.2): the first write
through the mount failed `Input/output error` with no request reaching the loop after the root's
`GETATTR`.

## Description

The volume keeps an inode's `mode` as **permission bits alone** (`crates/vfs/src/inode.rs`,
`Attrs::mode: "Permission bits"`), with the file type apart (`Kind`); the seam reports both
(`NodeAttr { kind, mode }`), and the NFS edge composes its wire fields from the pair (`ftype3` and
`mode`, as RFC 1813 has them). The FUSE edge did not: `fuse_attr(node)` copied `node.mode` into
`fuse_attr.mode`, so every attribute reply — `GETATTR`, `LOOKUP`, `CREATE`, `MKDIR`, `SETATTR` —
carried `0755`-shaped modes with no `S_IFMT` bits. The Linux kernel validates every attribute reply's
mode for a type it knows (`fs/fuse/dir.c`: `fuse_invalid_attr` → `fuse_valid_type`, and
`inode_wrong_type` against the inode it holds) and on failure calls `fuse_make_bad(inode)`: the inode
answers `EIO` to everything after. The root's first `GETATTR` did that to the root, so the mount was
unusable from its first operation.

In the other direction the kernel's `fuse_create_in.mode` carries `S_IFREG` and a `FATTR_MODE` the
inode's type bits, and the edge passed them to the volume unmasked, storing type bits in the
permission bits of every file created through FUSE.

## Root cause

The FUSE codec's own tests use a mock seam whose modes carry the type bits (`FILE_MODE = 0o100_644`,
`DIR_MODE = 0o040_755` in `crates/bridge-fuse/tests/dispatch.rs`), so the codec's expectation was
never checked against the volume's contract — the CLAUDE.md gotcha: a model that states the
implementation's rule certifies drift. And the serve loop had never run against a real kernel: the
one live FUSE test (`tests/oci_container.rs`) asks for `allow_other`, which `fusermount3` refuses
without `user_allow_other` in `/etc/fuse.conf`, so on the CI runner it skips; the design's status text
described it as proving T-4.13's Linux leg over a real FUSE mount, which it could not have.

## Impact

Every FUSE mount on Linux was dead on arrival: `EIO` on the first operation. The daemon does not yet
serve `/dev/fuse` (Linux mounts reach it through the NFS export), so no shipped path was affected;
the OCI container leg and any future daemon-side FUSE transport were.

## Fix

`crates/bridge-fuse/src/bridge.rs`: `wire_mode(kind, permissions)` composes the wire `st_mode` from
the seam's `kind` (`S_IFREG`/`S_IFDIR`/`S_IFLNK`) and the permission bits (`07777`); every attribute
reply goes through it. `permission_bits(mode)` strips the kernel's type bits from `CREATE`, `MKDIR`
and `FATTR_MODE` before the volume sees them.

## Failing test first

- `crates/bridge-fuse/tests/dispatch.rs`: a `GETATTR` of a file the seam reports with permission
  bits alone answers `S_IFREG | 0644` on the wire; a `CREATE` the kernel sends with `S_IFREG | 0644`
  reaches the seam as `0644`. Both failed before the fix (the reply carried `0644`; the seam received
  `0100644`).
- `crates/bridge-fuse/tests/coherence_mount.rs` (Linux, real kernel): the first write through the
  mount succeeds and the scenario runs; before the fix it failed `EIO` at the first `open`.

## Validation (2026-09-19)

`cargo test -p slates-bridge-fuse --test dispatch`: **18 passed** (the two regressions among them,
macOS and Linux); the mounted scenario on Linux 6.12.76-linuxkit (`fusermount3` 3.17.2): the write
through the mount succeeds and the whole coherence scenario passes, three runs (see the AUD-02
record).

## Siblings

- `tests/dispatch.rs`'s mock now states the volume's rule (permission bits alone) for the objects the
  new tests read; its other constants keep the type bits they had, which the composition leaves
  unchanged (`S_IFDIR | (0o040_755 & 07777)` is `0o040_755`).
- The design's T-4.13 status is corrected in the same change: the container leg's Linux variant is
  gated on `user_allow_other` and skips on the CI runner; the real-kernel proof of the FUSE serve loop
  is the mounted coherence test.
