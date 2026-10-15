# The hermeticity judge refused the landing's transient barrier threads (AUD-29-42, follow-up)

**Date:** 2026-10-14. **Audit:** AUD-29-42 (closed 2026-10-01; this reopens and re-closes the thread
half). **Design:** §0.2 R1 claims 1–2, Part 6 "Hermeticity", A-130/A-133 (the batched landing barrier).
**CI:** run 38645097263 (`ci-repair-2026-10-13`): all three Linux hermeticity records `VIOLATIONS`,
`2 outside` each (`native-linux-nfs4`, `oci-linux`, `native-linux-fuse`).

## Description

The hermeticity judge (`crates/conformance/src/trace.rs`, `authorized`) accepts a write inside the granted
landing target only from the landing's writers, which the harness names as the daemon's threads: strace
`-f` names every event by its *thread*, so the harness reads `/proc/<daemon>/task` after the landing
(`xtask/src/conformance/hermeticity.rs`, `threads_of`). On all three Linux lanes exactly two `fdatasync`
calls on files inside the target were refused "inside the target by a process that is not the landing's":

```
fdatasync .../land-target/.../traced/d/#9474106 (pid 202548, at 1792016243159488000 ns)
fdatasync .../land-target/.../traced/#9474107    (pid 202549, at 1792016243159889000 ns)
```

Both are inside the target, both fall inside the recorded landing window
(`1792016243151606599`…`163344733` ns), and both act on `O_TMPFILE` staging temporaries the landing had
just created (`(deleted)` in the log; one, `#9474107`, had its times set by the daemon's own long-lived
thread 202514 one line earlier, and thread 202549 wrote an eventfd to signal its shard one line later).
The recorded writers were `[202512, 202514, 202515]` — the daemon's long-lived threads. 202548 and 202549
were absent. They are the landing's own threads, and the writes were legitimate; the judge had a false
positive, not slates a hermeticity breach.

## Root cause

A-130's batched-landing barrier (2026-10-10, `crates/land/src/os.rs`) syncs a batch's files on ⌈√n⌉
streams — the caller's thread and a scoped thread per further stream (`sync_on_streams`,
`std::thread::scope` + `spawn_scoped`), each spawned and joined *within* the barrier. Those threads live
only for the barrier. The harness's `threads_of` snapshots `/proc/<daemon>/task` after the landing, by
which time the barrier threads have exited, so their thread ids never enter the writers set. strace names
each event by thread, so their `fdatasync`s read as a stranger's.

The 2026-10-01 fix for this same audit assumed the landing's threads were the daemon's long-lived shard
threads ("the daemon's shard threads are made at its start and live to its end, so the landing's thread is
among them"); A-130 later added transient threads and silently broke that assumption. The static thread
snapshot was the defect; the barrier's thread model is correct and measured, and is not changed for a test.

## Exact edits

- `crates/conformance/src/trace.rs`: `strace_thread_group(log, roots)` — the daemon's thread ids grown
  from the trace's own thread creation. Seeded with `roots` (the `/proc` snapshot), every `clone`/`clone3`
  whose caller is already in the group contributes the thread id it returned (`clone_child`), to a fixed
  point. A thread is created before it can write, so no real landing write is left outside the group, and a
  write by any other process stays outside it (a stranger's thread is never cloned by the daemon's).
- `xtask/src/conformance/hermeticity.rs`: strace now also traces `clone,clone3` (`STRACE_TRACE`); on Linux
  the policy's writers are `strace_thread_group(trace, threads_of(/proc))`, so the barrier's transient
  threads are the landing's. `threads_of` and `Landed.writers` are documented as the seed, not the whole
  set; the record note reports the grown set. macOS (eslogger names processes, not threads) is unchanged.

## Proof

- `a_thread_the_daemon_spawns_for_the_landing_is_counted_as_the_landings` (golden; every host): a `clone3`
  line spawns a thread the `/proc` seed did not list; its `fdatasync` of a staged `#…`(deleted) file inside
  the target is placed inside the target, while a thread the daemon never spawned, writing inside the
  target, stays a violation. The full trace suite (18) and the structural, literal and unsafe gates pass.
- The three Linux hermeticity lanes on CI: before, `2 outside` (the barrier threads); after — verified by
  the live lanes on the branch run.

## Sibling

The same transient-thread gap would affect any future landing or daemon work that spawns a short-lived
thread to write inside a target; `strace_thread_group` covers all of them, since it attributes by the
trace's thread creation rather than a point-in-time snapshot. No other hermeticity-judge path reads
`/proc` for thread identity.
