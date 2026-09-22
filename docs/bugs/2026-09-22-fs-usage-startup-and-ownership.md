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

Native mounted verification remains open until the corrected privileged command
finishes. The separate ENOSPC refusal is not explained by this lifecycle fix.

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
