# A private purge's intent waited out the idle receive's transaction timeout

**Amendment:** A-190's coherent purge as amended by A-198's receiver (§4.6, Windows). On main since the receiver replaced
WinFsp's dispatcher.

**Found by:** reading, during audit step 7 (2026-11-05, GAPS). The receiver's `purge_intents::Intents::wait` drained
intents while a control call was pending, and BENCHMARKS (run 41977769822) counted 0 of 161,073 receives pending on
arm64.

## Description

A purge publishes the purge it is about to make (its intent) to the receive worker, has the purge client (a child
process) open the file through the drive, and then publishes that no purge is in progress. The worker read the intents
from a channel of two messages, drained whenever its control call returned. When the kernel refused a purge client's
open before any request reached slates, nothing made the idle worker's call return. A Begin and an End filled the
channel, and the next publish blocked until the call's transaction timeout, 1,000 ms, ended it. Every further purge of
the round waited again.

## Root cause

WinFsp's control call never goes pending. The control dispatch calls `FspVolumeTransact` inline (WinFsp v2.1
`src/sys/fsctl.c:95-99`), and `FspVolumeFastTransact` waits for the next request inside the call, with
`FsRtlCancellableWaitForSingleObject` on the call's own IRP (`src/sys/ioq.c`, `FspIoqNextPendingIrp`). So the
receiver's overlapped wait on its completion event and the intent event, which would have drained intents mid-call,
never ran: the call returned only with a request or at the timeout. The publisher's `SetEvent` woke no one, and its
blocking send waited for the call.

## Impact

A purge round whose opens the kernel refuses (a name the drive cannot hold, a path gone since the change) on an otherwise
idle drive took up to a transaction timeout per purge past the second: four such purges took 3.02 s on arm64 and 3.01 s
on x86_64 (diag-winfsp-overlap-43). The command thread that publishes holds every other mount command meanwhile. No data
was wrong; a round took seconds where it should take milliseconds.

## A first fix that did not work

`agents/winfsp-overlap-17` (2026-11-07) had a full publisher cancel the worker's call with `CancelIoEx` and retry. The
volume's handle is overlapped, so the cancel reaches the call, but the round still took 2.54 s on arm64 and 2.65 s on
x86_64 in the same run: a cancel that finds the worker between calls, or serving a callback, finds nothing to cancel,
and the publisher is back to waiting. It was not landed. GAPS' plan had named `CancelSynchronousIo`, which cancels only
I/O issued on a synchronous file object, so it would have found no call at all.

## Exact edits

- **The failing test first:** `purges_the_kernel_refuses_on_an_idle_drive_do_not_wait_out_the_transaction_timeout`
  (`crates/bridge-winfsp/tests/support/host.rs`). An in-process mount, idle, purges four files whose names are one unit
  longer than the volume's `MaxComponentLength`, so the kernel refuses each open before slates sees it. Expect: all four
  outcomes within one transaction timeout. Its request channel's receiver is declared after the mount, so it drops
  first: the mount's thread joins only once that receiver is gone.
- **The fix (A-243):** the channel becomes one slot (`crates/bridge-winfsp/src/purge_slot.rs`, cfg-free), a seqlock
  whose word names the client's process id beside a writing flag and a count of publications. The mount thread's
  publish sets the flag, stores the purge's bytes in the slot's racy span and stores the word with the client
  (Release), and never waits for the reader. On Windows the slot is an unnamed section both threads map through their
  own views (`purge_intents.rs`, `slates_mem::SharedObject`).
- The worker reads the word once when a Create arrives (`dispatcher::PrivateOpens::authenticate`, Acquire). Ada
  authorized that one atomic read on every Create on 2026-11-07, in place of the channel's `try_recv` before every
  callback; a program's Create meets no other atomic. Only the purge's own opens read the purge's bytes and the word
  again, and a read that finds the flag set, or the word changed, is no purge: no purge's open can be in flight while
  the next publication is written.
- The intent event, its signal and the control call's two-event wait go: the call waits only on its own completion
  (`dispatcher::transaction`). No callback drains anything first.
- One `unsafe` block is added (borrowing the section's handle to duplicate it for the reader) and two go (the event's
  `SetEvent` and `WaitForMultipleObjects`).
- The tests besides the live one: in `purge_slot.rs`, 10,000 purges and their ends published with nothing reading all
  return, and a reader racing a writer of 10,000 distinct purges reads none or one whole, never a mix, and a program's
  read is always none; the same race under loom, in every interleaving within the workspace's bounds (CI's loom lane);
  and in `purge_intents.rs` both again through the section's two views, on the Windows runners.
