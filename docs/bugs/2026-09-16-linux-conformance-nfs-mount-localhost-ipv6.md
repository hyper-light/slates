# The Linux conformance NFS mount targeted `localhost`, which resolves to IPv6 first, but the daemon binds IPv4 only

Date: 2026-09-16
Area: `xtask/src/conformance/slates.rs` (`mount_volume`, the Linux adapter)
Severity: the whole Linux conformance lane (pjdfstest, fsx, fsstress, workloads, hermeticity on
`ubuntu-latest`) could not mount, so every suite failed the mount step and the job went red. No
product code was at fault; the daemon's NFS server was serving correctly.

## Symptom

The `conformance (…) (ubuntu-latest)` CI job failed, and every suite that needs a mount reported the
same thing:

```
conformance: fsx: sudo mount -t nfs failed: mount.nfs: mount system call failed for /tmp/slates-mount.JZW8L8
conformance: fsstress: sudo mount -t nfs failed: mount.nfs: mount system call failed for /tmp/slates-mount.91qfwQ
conformance: pjdfstest: sudo mount -t nfs failed: mount.nfs: mount system call failed for /tmp/slates-mount.JnfjUs
```

macOS conformance mounts fine (its own `slates mount` path). In a Docker container the same
`mount -t nfs` command with the same options succeeded, so the harness options were not the fault.

## Root cause

The Linux adapter mounted the daemon's loopback NFS export as `localhost:/<name>`. The daemon's NFS
listener binds IPv4 `127.0.0.1` **only** — `slates_rt::tcp` is an `SocketAddrV4` API with no IPv6 bind
(confirmed on the running daemon: `ss -tlnp` shows `LISTEN 127.0.0.1:<port>` and nothing on `::1`).
On a dual-stack host, `localhost` resolves to IPv6 `::1` first (RFC 3484 / `getent hosts localhost`
returns `::1` ahead of `127.0.0.1`), so `mount.nfs localhost:/…` targets `[::1]:<port>`, where nothing
is listening. The kernel mount to `::1` is refused and `mount.nfs` reports the generic "mount system
call failed" without falling back to the IPv4 address.

It hid in local containers because Docker Desktop's Linux VM has IPv6 disabled at the stack level
(`::1` mounts return "Address family for hostname not supported"), so `mount.nfs` fell back to
`127.0.0.1` and the mount worked. GitHub's `ubuntu-latest` runners have a working IPv6 loopback, so
`localhost` genuinely reached `::1` and the mount failed.

## Fix

Target the IPv4-only server by its IPv4 literal: mount `127.0.0.1:/<name>` (`LINUX_NFS_SERVER`) rather
than `localhost:/<name>`. This removes the dependency on the NFS client's address-family selection and
fallback. The mount options are unchanged — they were already correct. The failure message now also
records the exact command and both output streams, because `mount.nfs`'s own message carries no cause.

## Verification

Reproduced in privileged Docker with a live daemon (`rust:1.98.0`, `nfs-common`):
- `ss -tlnp` confirms the daemon's NFS port listens on `127.0.0.1` only.
- `getent hosts localhost` returns `::1` first.
- `mount -t nfs -o vers=3,tcp,…,port=P,mountport=P 127.0.0.1:/<name> <point>` mounts, and a file
  written through the mount reads back byte-for-byte.
- `[::1]:/<name>` fails; `localhost:/<name>` succeeds only because this container's IPv6 is disabled
  (it falls back to IPv4) — exactly the behaviour that hid the bug locally while the runner failed.

## Sibling sweep

- macOS `slates mount` (`crates/cli/src/mount.rs`) also passes `localhost:/<name>` to `mount_nfs`, and
  the daemon is IPv4-only there too. It works on the macOS runner (macOS `mount_nfs` reaches the IPv4
  loopback), so it is not failing today, but it is the same latent shape; if a macOS host ever resolves
  `localhost` to `::1` without IPv4 fallback it would fail identically. Left unchanged (the macOS lane
  is green and this is a user-facing command), recorded here as the sibling to watch. The robust
  long-term answer is a dual-stack (or IPv6) loopback bind in `slates_rt::tcp`, a larger change.
- The Windows conformance has no mount step (`bridge-winfsp` mounts a drive letter, not over NFS), so
  it is unaffected.
