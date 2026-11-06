# A daemon's own I/O on a volume it serves strands its exit

**Amendment:** A-219, which stays off main because of this. The rule it adds is in §4.6's Windows section.

**Found by:** the live WinFsp oracles of diagnostics runs 42134176889 (`diag-winfsp-overlap-21`, `e070f31e`) and
42136220721 (`diag-winfsp-overlap-22`, `5b5f7ba1`), 2026-11-06, on GitHub's `windows-latest` x86_64 and `windows-11-arm`
runners. Both runs sat in that step 76-93 minutes, against 25 for the whole job of run 42130583401. The supervisor
cancelled them at 01:40 UTC and downloaded their logs.

## Description

A-219's second receive worker measured how many workers to run with a probe. The probe made a hidden WinFsp volume and
served it with probe workers. A client thread in the same daemon opened and closed a file on that volume, timing each
cycle. From `e070f31e` the probe ran on the second worker's own thread, just after the mount, rather than before it.

Every test that killed or stopped a daemon within about a second of its mount then failed to see that daemon end. Each
test had a fixture wait or a suite that never ended. All lines are from the downloaded job logs:

- **Library kill oracles, all four jobs.** A killed role outlived the fixture's 10 s reap, so the test panicked at
  `tests/support/host.rs:118`, "the fixture must reap its child", and the test binary aborted (exit 0xc0000409).
  - diag-21 x86_64: `WINFSP INFLIGHT: first server took the read; killing it before its reply` at 00:08:15.28, and the
    panic at 00:08:25.28.
  - diag-22 arm64: the same pair at 00:24:42.21 and 00:24:52.21.
  - The killed-probe role failed the same way on diag-21 arm64 and diag-22 x86_64. The same taken-read test passed in
    run 42130583401, whose probe ran before the mount, where no kill lands.
- **CLI mount tests, diag-22 x86_64.** At 00:25:41.77 `slates anchor: the daemon's heartbeat lapsed; killing it`, at
  00:25:41.79 `slates: kill: daemon spawn refused (code Some(5))`, and at 00:25:50.82
  `a_file_at_a_two_thousand_unit_path_is_served_across_a_killed_daemon` panicked at `crates\cli\tests\winfsp.rs:549`:
  "the killed daemon must exit before the drive is checked", `left: 258` (`WAIT_TIMEOUT`), `right: 0`.
- **CLI mount tests stopped printing, each in a test that ends its daemon right after the mount:**
  - diag-21 x86_64 at 00:10:23, the last of `a_process_outside_the_anchor_cannot_take_either_holders_mount_handles`'s
    `WINFSP FOREIGN` refusals;
  - diag-21 arm64 at 00:09:50, `WINFSP CACHED READ: followed the SDK's change in 20.3353ms`;
  - diag-22 arm64 at 00:26:38, `WINFSP AFTER THE VIEW: a fresh open read the change in 534.1µs`.
  - Each was silent until the cancel at 01:40.

## Root cause

The daemon sent I/O to a volume it serves itself, and a dying thread cannot leave I/O the dying process alone could
answer.

1. A probe open that has reached a probe worker is in WinFsp's processing queue. That queue is never cancelled or
   expired: `FSP_IOQ_PROCESS_NO_CANCEL` (WinFsp v2.1 `src/sys/driver.h:970`), and `FspIoqRemoveExpired` skips it
   (`src/sys/ioq.c:572-575`). Only stopping the queue ends it, at the cleanup of the volume's control file.
2. When a thread exits, Windows cancels its outstanding IRPs and waits for them to complete. "If a canceled IRP is not
   completed within 5 minutes, the I/O manager considers the IRP timed out. Such IRPs are disassociated from the
   thread" (Microsoft Learn, "Canceling IRPs",
   https://learn.microsoft.com/en-us/windows-hardware/drivers/kernel/canceling-irps).
3. The volume's control handle is closed only at process rundown, after every thread has exited. So nothing completes
   the client's IRP: the probe worker that would answer it is in the dying process, and so is the handle whose cleanup
   would stop the queue.

So a daemon killed or stopped while its own open is delivered cannot exit for minutes. In these logs it had not exited
after 75-90 minutes, so the 5-minute disassociation either did not release it or something else then waited on it. The
anchor's own kill of such a daemon was refused (`code Some(5)`), and its successor waits for the exit (A-198). The
probe ran continuously for its whole measure, about a second after every mount, which is where the CLI tests end their
daemons and the kill oracles strike.

## Impact

- On the A-219 branches only (`e070f31e`, `5b5f7ba1`), never on main. A daemon killed or stopped during its first
  mount's probe could not be replaced for minutes or longer, so its drive stayed unanswered. Neither branch lands.
- **A sibling on main, not fixed here:** A-190's coherent purge (`crates/bridge-winfsp/src/host.rs`, the command thread's
  `CreateFileW` at line 2038, with the private-open marker of A-198's correction) also opens the daemon's own drive. It
  writes and reads each purged range through it. A kill while one of those requests is delivered strands the daemon the
  same way. Its window is one purge's write or read, not a continuous probe. No run has been seen to hit it. It is open
  in GAPS, since the rule below forbids it.

## The rule

**No daemon thread ever opens a path on a volume the daemon serves** (§4.6, Windows). A measurement that needs a client
of the volume runs that client in a child process. The daemon's threads then hold no I/O that only they can answer. A
killed daemon exits, its control handle's cleanup stops the queue, and the child's open fails.

## Exact edits

- A-219's runtime and probe stay off main (`agents/winfsp-overlap-6` at `e070f31e`, `agents/winfsp-overlap-7` at
  `5b5f7ba1`).
- The worker count is measured, from now on, with every count fixed at build time and no probe running
  (`diag-winfsp-overlap-24`'s five arms). A count derived at run time comes from a stated formula or from a probe whose
  client is a child process, with a test that kills the daemon mid-probe and asserts it exits within one transaction
  timeout.
- §4.6's Windows section states the rule; GAPS carries this bug and the A-190 sibling.
