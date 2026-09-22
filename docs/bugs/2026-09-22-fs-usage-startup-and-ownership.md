# fs_usage starts after the workload and outlives its target

Date: 2026-09-22. Baseline: `628962b`; AC-4.5, Part 6 example 8.

## Reproduction and impact

The approved native command
`bash /private/tmp/slates-ci-35604717581-macos-trace.sh` fails at `d/f2` with
`No space left on device`, then prints `ktrace_start: No such process`. Its
`trace.log` is zero bytes. The earlier CI run 35604717581 also recorded zero
events after completing the workload. Log:
`/private/tmp/slates-ci-35615970514-macos-trace.log`.

A tracer that has not attached cannot establish hermeticity. The mount refusal
is a separate failure, still under investigation; no quota or assertion is relaxed.

## Root cause

`FsUsage::start` returned immediately after spawn. Apple's implementation performs
`init_shared_cache_mapping` and `cache_disk_names` before `ktrace_start`.
The harness ran the workload during that interval. The daemon could exit before
the tracer attached. On an early error the harness dropped an unowned `Child`
without cancelling or reaping it. The subsequent fixed sleep cannot repair missing
events. Stopping an already-attached tracer after the daemon is correct: it must
also observe teardown. The defect is failing to establish attachment first.

Source: [Apple fs_usage.c](https://github.com/apple-oss-distributions/system_cmds/blob/main/fs_usage/fs_usage.c),
startup sequence and completion handler. The dropped-event handler also reports
buffer loss on stderr, so a clean exit alone is insufficient evidence.

## Correction

- Own the tracer process group, including sudo's child, on all return paths.
- Before mounted mutations, issue read-only CLI activity and require a complete
  parsed trace event while the tracer remains alive. Use the existing mount wait
  budget for bounded startup and shutdown.
- Keep the attached tracer alive through daemon teardown, then drain it; require a
  successful exit and retain diagnostic output. Reject event-loss diagnostics.
- Remove the arbitrary settle sleep. Preserve the exact landing and outside-write
  checks.
- Exercise delayed readiness, early exit, terminal log output, and cancellation
  through real pipe-connected child processes, without privileged tools.

The corrected privileged command now records 188 rows / 47,000 bytes with empty
stderr and no surviving task tracer or daemon. Startup and early-error cleanup are
verified natively. The separate ENOSPC cause is recorded in
`2026-09-22-hermeticity-fixture-omits-client-metadata.md`; complete macOS hermeticity
still requires attribution of the shared-memory and pre-existing socket descriptors.

## Validation

- Missing-barrier negative control: both real-process readiness cases fail in 0.05 s;
  log `/private/tmp/slates-ci-35615970514-tracer-lifecycle-red.log`.
- Corrected owner: six real-process cases pass on macOS in 0.24 s and in the
  disposable Linux container; no privileged tracer is needed for these checks.
- Conformance parser and records: 50 cases pass, one deliberate writer ignored.
- Strict targeted Clippy passes. Linux `cargo xtask check` passes.
- On the same isolated source, both ordinary-user native FUSE mount histories pass
  (0.39 s and 0.01 s), and the real CLI gate returns 10 passed in 8.17 s. Its macOS-
  only mount/issuer branches still skip on Linux; the provisioned-key and real fleet
  histories execute. This does not replace a complete macOS conformance run.

Linux command script and log:
`/private/tmp/slates-ci-35615970514-native-and-tracer-linux.{sh,log}`.
The follow-up Terminal script uses the isolated `tracer-source`, keeps the original
red log, and writes `slates-ci-35615970514-macos-trace-v2.log`.

## Descriptor coverage follow-up (2026-09-22)

The native trace has 23 ftruncate calls, 23 mmap calls, 92 reads and 50 writes.
It has no shm_open or socket creation rows. Apple's
[fs_usage source](https://github.com/apple-oss-distributions/system_cmds/blob/main/fs_usage/fs_usage.c)
does not register shm_open in its syscall table. Its filesys/network filter also
tracks dup/dup2 internally without emitting their rows. Therefore adding known
descriptor numbers from a later lsof snapshot would not establish the object each
earlier write reached: dup2 can replace a descriptor between the write and snapshot.
The parser also handles close but currently ignores close_nocancel and guarded
close. These are sibling attribution gaps, not evidence of an actual daemon disk
write. No such writes have been reclassified to make the test pass.

Task-scoped DTrace was explicitly authorized on 2026-09-22. The bounded probe is
`bash /private/tmp/slates-ci-35615970514-dtrace-probe.sh`; it owns one disposable
daemon, generates a real CLI session and requires a shm_open return event. It does
not change SIP or host settings. At this checkpoint `sudo -n true` returns
`sudo: a password is required`, and no probe log exists. Terminal authentication
is pending; DTrace coverage is not yet established. Tests and builds remain idle
for that probe.

A complete replacement must preserve descriptor lifetimes, cover descriptors
created before workload tracing, reject event loss, and retain tracing through
teardown. Negative controls must include a descriptor replaced between writes,
close variants, and a disk write outside the granted target. The native mounted
lifecycle must then pass with zero unresolved or outside writes; a source review
or a successful coverage probe alone cannot close that gate.
