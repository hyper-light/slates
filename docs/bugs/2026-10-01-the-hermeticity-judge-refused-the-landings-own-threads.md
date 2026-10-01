# The hermeticity judge refused the landing's own threads, and a kernel control file (AUD-29-42, follow-up)

**Date:** 2026-10-01. **Audit:** AUD-29-42 (closed earlier the same day), AUD-29-41. **Design:** §0.2 R1
claims 1–2, Part 6 "Hermeticity". **CI:** run 36860862041 (`724bf8d`): both Linux hermeticity records
`VIOLATIONS`.

## Description

1. **Threads.** The judge (`crates/conformance/src/trace.rs`, `authorized`) accepts a write inside the granted
   target only from the landing's writers, and the harness named one: the daemon's process id. strace `-f`
   names every event by its *thread* id, and the landing runs on a shard thread, so every one of the
   landing's own writes was refused "inside the target by a process that is not the landing's" (pid 133111
   against daemon 133108).
2. **Kernel control files.** The dump exclusion (`crates/cli/src/dumps.rs`, AUD-29-41) writes
   `/proc/self/coredump_filter`; the judge had no class for a kernel control file and refused it "outside
   every allowed class". A path-prefix exemption for `/proc` is exactly what AUD-29-42 removed (the audit:
   "a path spelling such as /proc … does not establish that distinction").

## Root cause

1. The harness's landing record carried one id where the tracer names threads.
2. The judge classified paths only by strace's synthetic object names, so a real file on a kernel virtual
   filesystem had no class.

## Exact edits

- `xtask/src/conformance/hermeticity.rs`: the landing's writers are the daemon's threads on Linux, read from
  `/proc/<pid>/task` right after the landing while the daemon lives (`threads_of`); the process alone where
  the tracer names processes (eslogger). The record's note names them.
- `crates/conformance/src/trace.rs`: `Policy::mounts` — the tracer's mount table — and `parse_mountinfo`;
  a write whose longest-prefix mount is a kernel virtual filesystem (`proc`, `sysfs`, `cgroup`, `cgroup2`;
  `pstore` excluded, it persists) is a kernel control file. The class comes from what is mounted at the
  path, never its spelling: a disk mounted beneath `/proc` and a look-alike `/procfoo` stay violations, and
  with no mount table nothing is assumed.
- `xtask/src/conformance/{hermeticity,slates}.rs`: the harness and the Linux startup-trace regression test
  pass `/proc/self/mountinfo`.

## Proof

- `mountinfo_yields_each_mount_point_and_its_type` and
  `a_kernel_control_file_is_known_by_its_mount_never_its_spelling` (golden; every host).
- The Linux hermeticity suite in a privileged container as a non-root user with passwordless `sudo` (the
  GitHub runner's shape): before, `VIOLATIONS` (thread ids, then the core filter); after, `native-linux-nfs4`
  RAN with 0 outside (22 inside the target, 6 matched, 129 RAM-only), and the FUSE adapter likewise. The
  trace regression tests pass (19).

## Sibling found the same day

The dump exclusion's first form made the processes not dumpable, which broke every non-root Linux start and
closed `/proc/<daemon>` to the harness's grant path; see
`docs/bugs/2026-10-01-the-anchor-and-daemon-could-be-core-dumped.md` ("Correction").
