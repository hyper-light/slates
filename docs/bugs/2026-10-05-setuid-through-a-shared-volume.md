# A setuid-root binary planted on a shared volume ran as root for another user (2026-10-05)

## Description

Condition 4's adversarial battery mounted a slates volume through Linux's own NFSv4.2 client, as root, with the
options the documented PersistentVolume used (`nfsvers=4.2, xprtsec=mtls, port=…`, with no `nosuid`). As root it
copied `/usr/bin/id` onto the volume, `chown root:root` and `chmod 4755`, and ran it as `nobody`:

    == setuid as nobody: 0

A volume is shared by every pod that claims it, so one pod running as root could plant a binary that gives an
unprivileged user in another pod uid 0. Device nodes were already refused by the daemon (`mknod` of char 1,1 and
block 7,0 answered NFS4ERR_BADTYPE, which the Linux client reports as errno 527), so only the setuid path was open.

## Root cause

The kernel honours setuid and setgid bits on a mount unless the mount is `nosuid`. Each path that mounts a volume
decides that for itself:

- `slates mount` on Linux (FUSE): already `rw,nosuid,nodev` when run as root (measured in this change).
- `slates mount` on macOS: `MNT_NOSUID` only. xnu adds `MNT_NODEV` for an unprivileged caller but not for root. The
  flag word went through `u32::try_from(...).unwrap_or(0)`, which would have dropped `MNT_NOSUID` silently on a
  failed conversion.
- The OCI binding entry: `["bind", "rw"|"ro", "private"]`. A bind copies its source mount's flags, so the slates
  mount's `nosuid` held, but the entry did not say so itself.
- The PersistentVolume in `docs/deploy.md` and the KIND lane: no `nosuid`, no `nodev`. This is the path the probe
  above reproduced.

## Impact

Privilege escalation between consumers of one exported volume on Kubernetes (the documented PersistentVolume) or
on any operator-written NFS mount without `nosuid`. Nothing escaped to the host's disk; the daemon itself made no
write (traced the same day, `docs/wip/bench/realworld_trace.sh`).

## Exact edits

- `docs/deploy.md`, `xtask/src/kind_export.rs`: the PersistentVolume's `mountOptions` add `nosuid, nodev`. The KIND
  lane's reader pod now reports its mount's flags from `/proc/self/mountinfo`, and the lane fails unless both are
  there.
- `crates/cli/src/mount.rs`: macOS `MNT_NOSUID | MNT_NODEV`; a flag word out of range is a typed failure. The
  `mount_nfs` arguments and `crates/cli/examples/slates_mount.rs` add `nodev`.
- `crates/bridge-oci/src/binding.rs`: the entry's options add `nosuid` and `nodev`.
- Tests:
  - `crates/cli/tests/cli.rs`: the Linux FUSE flow reads the kernel's flags for its mount. As root, it also plants
    a setuid-root `id` on a `--shared` mount and requires `nobody`'s uid back; it ran as 65534.
  - `crates/cli/tests/cli.rs`: the macOS flow requires `nosuid` and `nodev` in the mount table's line.
  - `crates/bridge-oci/tests/verify.rs`: the binding options.

Validation of the PV path: the same probe on a kernel NFSv4.2 mount with `nosuid,nodev` printed
`== setuid as nobody: 65534`.

## Siblings checked

- Device nodes: refused by the daemon on every path (NFS4ERR_BADTYPE).
- A setgid directory (`chmod 2775`) is kept, as POSIX requires. It grants group inheritance, not privilege.
- The Linux FUSE mount was already safe.
- Docker `type=nfs` volumes made from `slates export` (A-73) are operator-written like the PersistentVolume, so
  `docs/deploy.md`'s note covers them.
