# A private purge's intent waited out the idle receive's transaction timeout

**Amendment:** A-190's coherent purge as amended by A-198's receiver (§4.6, Windows). On main since the receiver replaced
WinFsp's dispatcher.

**Found by:** reading, during audit step 7 (2026-11-05, GAPS). The receiver's `purge_intents::Intents::wait` drains
intents while a control call is pending, and BENCHMARKS (run 41977769822) counted 0 of 161,073 receives pending on arm64.

## Description

A purge publishes a Begin intent to the receiving worker, has the purge client (a child process) open the file through
the drive, and publishes an End. The worker drains the intent channel, bounded at two messages, whenever its control call
returns. When a purge client's open is refused by the kernel before any request reaches slates, nothing makes the idle
worker's call return: a Begin and an End fill the channel, and the next publish blocks until the call's transaction
timeout, 1,000 ms, ends it. Every further purge of the round waits again.

## Root cause

WinFsp's control call never goes pending. The control dispatch calls `FspVolumeTransact` inline (WinFsp v2.1
`src/sys/fsctl.c:95-99`), and `FspVolumeFastTransact` waits for the next request inside the call, with
`FsRtlCancellableWaitForSingleObject` on the call's own IRP (`src/sys/ioq.c`, `FspIoqNextPendingIrp`). So the receiver's
overlapped wait on its completion event and the intent event, which would have drained intents mid-call, never runs: the
call returns only with a request or at the timeout. The publisher's `SetEvent` wakes no one.

## Impact

A purge round whose opens the kernel refuses (a name the drive cannot hold, a path gone since the change) on an otherwise
idle drive took up to a transaction timeout per purge past the second. The command thread that publishes holds every
other mount command meanwhile. No data was wrong; a round took seconds where it should take milliseconds.

## Exact edits

- **The failing test first:** `purges_the_kernel_refuses_on_an_idle_drive_do_not_wait_out_the_transaction_timeout`
  (`crates/bridge-winfsp/tests/support/host.rs`). An in-process mount, idle, purges four files whose names are one unit
  longer than the volume's `MaxComponentLength`, so the kernel refuses each open before slates sees it. Expect: all four
  outcomes within one transaction timeout.
- **The fix:**
  - `purge_intents::Sender` learns the volume's control handle once the mount holds it (`cancel_on`).
  - A publish that finds the channel full signals the intent event and calls `CancelIoEx` on that handle, then retries.
    It retries while the cancel finds no call (the worker serving a callback, or between calls), for at most two
    transaction timeouts (`ROOM_WITHIN`), and then refuses.
  - WinFsp ends a cancelled wait with `STATUS_CANCELLED` and nothing delivered, after completing every response the
    call carried; a call with no output returns before any wait (`src/sys/volume.c`). So in
    `dispatcher::transaction` the receiver reads `ERROR_OPERATION_ABORTED` as an empty receive, drains and receives
    again, but only when it finds the intent event set (`Intents::cancelled_by_publisher`). A cancellation it did not
    ask for, such as a stopped queue's, stays its refusal, so a torn-down volume cannot spin the worker.
- The plan in GAPS had named `CancelSynchronousIo`, which cancels only I/O issued on a synchronous file object. The
  volume's handle is overlapped (WinFsp's `FspFsctlCreateVolume` opens it `FILE_FLAG_OVERLAPPED`), so it would have
  found no call.
- Two `unsafe` blocks are added (the `CancelIoEx` call and the event test), each with its SAFETY line; the crate's
  budget rises from 132 to 133 (`unsafe-budget.toml`, main's 131 sites and these two).
