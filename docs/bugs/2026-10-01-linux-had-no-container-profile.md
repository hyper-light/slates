# Linux had no container profile (AUD-29-67/74/78)

**Date:** 2026-10-01. **Audit:** AUD-29-67 (a runtime profile per engine), AUD-29-74 (identity semantics per
profile), AUD-29-78 (live-platform evidence). **Authorization:** Ada authorized `user_allow_other` on the CI runner
(2026-10-01).

## Description

On Linux a container bind was refused `ContainerWorkloadUnproven`. The daemon's FUSE mount served its own user
alone, so a container's processes, running as other ids, could not reach it. No Linux engine profile had evidence,
and the identity a container reaches the export with had never been measured there.

## Exact edits

- `crates/bridge-fuse/src/mount.rs`: the helper's standard error is captured. A refusal naming
  `user_allow_other` is the typed `MountError::AllowOtherNotGranted`, read from the helper and never from
  `/etc/fuse.conf` (R1).
- `AttachRequest::SharedFuseMount { mount_point, subtree }` and `AttachForm::SharedFuseMount { path, scope }`, both
  appended. This is a FUSE mount made with `allow_other`; the kernel still checks every caller's bits through
  `default_permissions`. A refusal is `AttachmentUnsupported { Fuse, AllowOtherNotGranted }`. The CLI form is
  `slates mount ID DIR --shared [--subtree DIR]`; on macOS `--shared` is refused, because Desktop's share already
  reaches the mount as the mounting user.
- The OCI bind on Linux binds only a shared mount; any other is refused `MountNotShared` (appended). The transport
  report offers the Linux bind where FUSE is.
- `crates/bridge-oci/src/runtime.rs` adds the Linux Docker Engine profile, tested:
  - `IdentityRule::ContainerIdsAsHostIds`;
  - `HardLinkRule::EveryNameServedAtOnce`;
  - `slates oci-runtime docker` prints both.

## Measurement

Docker Engine 26.1.5 (Debian trixie, runc), rootful, no user namespace, inside a privileged container on Docker
Desktop's arm64 VM, 2026-10-01. The mount was a `--shared` mount by the unprivileged user `runner` (1000:1000).

| Identity | Result |
|---|---|
| 0:0 | wrote files owned 0:0 on the host; read the mounting user's 0700 directory |
| 2000:2000 | refused both creating in the 0755 root and reading the 0700 directory |
| 1000:1000 | wrote its own files, owned 1000:1000 |
| hard links | link, remove the first name, read the second: 0 of 100 failed |

## Proof

`crates/cli/tests/cli.rs` `a_linux_container_reaches_the_shared_mount_as_its_own_ids` exercises the whole flow by use
through the real binary and a real engine:
- `--shared` mount, the handshake's profile, `attach --oci`;
- containers as 0:0, 2000:2000 and the mounting user;
- the hard links;
- an unshared mount refused `MountNotShared`.

It passed in 2.3 s. With the shared-parent check mutated out, the unshared mount was admitted and the test failed.
CI's Linux lane grants `user_allow_other` before the CLI flow, so the test runs there. Elsewhere it skips loudly
without the grant.

## What this does not claim

`allow_other` exposes the mount to every local id as far as its permission bits allow, and fully to root. That is
the operator's grant, stated in the profile, never assumed.
