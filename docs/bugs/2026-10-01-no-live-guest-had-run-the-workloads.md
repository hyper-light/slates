# No live guest had run the workloads (AC-9.7; AUD-29-68)

**Date:** 2026-10-01. **Audit:** AUD-29-68 ("then exercise a real guest"), AUD-29-78 (live-platform acceptance).
**Design:** AC-9.7: `LiveGuest` may only appear after a Linux guest has mounted the tag and run the §6 workloads.

## Description

The guest transports' conformance said `SimulatedGuestDriver` because no live guest had run the workloads. The live
QEMU guest (`a_linux_guest_mounts_the_volume_through_qemu_over_vhost_user`) mounted the tag and exchanged a few
files, but the roster's workloads (git, cargo, npm, python, rg, rsync, sqlite, an editor, a watcher) had never run in
a guest.

## Exact edits

- `crates/server/tests/virtiofs.rs`
  `a_live_guest_runs_the_roster_workloads_identically_on_slates_and_on_its_ram`.
  - The test places the conformance roster's scripts in a volume.
  - It boots the live guest in workload mode, with the host container's root shared read-only over 9p
    (`cache=mmap`; see below), the slates tag at the guest's `/mnt` and RAM at `/tmp`.
  - For each roster tool, the guest runs the workload on its RAM and on slates under one fixed environment, and
    prints each run's output and tree manifest.
  - Each pair is judged by the harness's own rule, `slates_conformance::workload::compare`: exit code, output with
    the directory normalized, manifest under the roster's reviewed exclusions.
- `crates/conformance/src/workload.rs`: the roster's bounds (`SQLITE_BUSY_MS`, `WATCH_SECONDS`) moved here from
  xtask, so every leg (host, container, guest) runs the roster under one value.
- `Conformance::LiveGuestWorkloads` is appended in both the device's report and the wire. The inherited-descriptor
  (vhost-user) form reports it on Linux. The in-process seam keeps `SimulatedGuestDriver` until a VMM drives it.

## Proof

Linux arm64 (Docker Desktop's VM on Apple Silicon), QEMU 10.0.13 under software emulation, a 1 GiB Debian 6.12.111
guest, 2026-10-01. All nine Linux roster workloads came out `Identical`: git, cargo, npm, python, rg, rsync,
sqlite, editor and watcher. Two runs took 364 s and 317 s. Earlier runs on images lacking some tools named those
tools as skipped and judged the rest `Identical`.

## Found on the way (not slates)

With 9p's default uncached mode the guest kernel overflowed its stack in `netfs_retry_reads` (recursing through
`netfs_rreq_terminated`), while `rustc` read from the 9p root. This is a Linux 6.12 9p/netfs client problem; slates
was not involved. Mounting the 9p root with `cache=mmap,msize=524288` avoids it. The recipe
(`docs/bugs/2026-10-01-vhost-user-descriptors-closed-with-an-earlier-message.md`) gains: the image also installs
`xz-utils`, `rsync`, `ripgrep`, `vim`, `nodejs`, `npm`, `sqlite3` and `inotify-tools`. The initramfs carries `netfs`,
`9pnet`, `9pnet_virtio` and `9p`, decompressed. The init's workload mode (`slates.workloads` on the command line)
mounts the 9p root, the tag at its `/mnt`, a tmpfs at its `/tmp`, and runs `/mnt/run.sh` chrooted.

## In CI

Ada authorized QEMU in CI on 2026-10-01. The recipe now lives in the repository (`ci/guest/Dockerfile`,
`ci/guest/init`), built for the host's architecture. The `live-guest` job runs the virtio-fs suite in it, and a
skipped live guest fails the job.
