# A WinFsp request taken before daemon death never finished

Observed 2026-11-02, A-198 Step 9.4.

## Description and evidence

The live transport oracle starts a mount server, retains a real Request::Read after the WinFsp DLL delivers it,
kills that process, and starts a successor on the three anchor-held handles. The original reader must return its
bytes or ERROR_OPERATION_ABORTED within ten liveness budgets. Each process is owned and reaped; teardown kills the
servers before releasing the held controls, so a failed assertion cannot leave the reader behind.

On diagnostics branch `diag-s9-request-red-1`, `3163830c0c11acc151a76fb66df787aa88d839b8`,
[41436041154](https://github.com/hyper-light/slates/actions/runs/41436041154), the oracle fails on both architectures:
`a taken read stayed unanswered after the successor started`, `left: None`, `right: Some(3)`. It takes 10.12 s on
x86_64 and 10.14 s on arm64. On x86_64 the reader reports ERROR_OPERATION_ABORTED only after the parent fails and
deletes the held volume during teardown. The successor's idle deadline exceeds the parent's read deadline, so its
own shutdown cannot masquerade as successful settlement.

Command: `cargo test -p slates-bridge-winfsp --lib host::tests::a_read_taken_before_the_kill_is_settled_after_takeover -- --exact --nocapture --test-threads=1`, with `WINFSP_TEST_MOUNT=1`.

The refined oracle also requires a successful fresh read of a second file through the successor before judging the
original read. This distinguishes a stranded processing request from a successor that never resumed serving.
`diag-s9-request-red-2`, `3b92911546fdfc5dcbb1d99101b32bb39ba3e99e`,
[41437374883](https://github.com/hyper-light/slates/actions/runs/41437374883), confirms that distinction on both
architectures: the fresh read returns the file's bytes, then the original read fails the ten-second deadline
(10.14 s per run). Both independent second-daemon tests also pass there, reaching the existing-instance check and
being refused with Windows error 183, CLI exit 4; the original daemon and drive still serve afterward.

## Root cause and impact

WinFsp removes a request from pending and puts it in processing before returning it to the DLL. A successor's
dispatcher fetches pending requests, not processing ones. The predecessor's in-memory callback and reply sender
die with it, and the open ledger retains no request hint or response. Merely keeping the volume alive therefore
keeps the unanswered request alive too. The twenty-kill history with no deliberately retained request does not
establish this boundary.

The initial A-198 design also read the queue's conditional timeout code without its active configuration.
`driver.h` defines FSP_IOQ_PROCESS_NO_CANCEL. The compiled branch uses an infinite timestamp, installs no
cancellation routine for processing requests, and excludes them from expiration. There is no timeout to cover the
DLL's transact-to-guard interval. Kiln research f9 verified both quotations (A, 90/100).
[Driver configuration](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/src/sys/driver.h),
[queue implementation](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/src/sys/ioq.c).

The driver matches a response's Hint to an IRP address in processing. After a response completes, that address can
be reused. With another receiver active, replaying a saved hint can complete a different operation. A missing hint
being ignored is insufficient evidence of replay safety.
[Response matching](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/src/sys/volume.c).

## Fix and verification

Replace the DLL-owned receive loop with one owned receive worker per mount. Keep its fixed batch receive buffer
and one checksum-protected response in the already-held unnamed ledger section. Use the public direct-output batch
FSCTL: its output describes retained pages directly, whereas the ordinary transact can use an I/O-manager buffer.
The driver copies a request before placing it in processing. The successor reads retained pages only after the
predecessor's process object signals exit, which also settles that process's own pending I/O. This receive-boundary
choice is an inference, kiln f10 B, 79/100; the process-exit contract is f11 A, 85/100. The live result below establishes the captured-read boundary.
[Transaction formats](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/inc/winfsp/fsctl.h),
[direct I/O buffers](https://learn.microsoft.com/en-us/windows-hardware/drivers/kernel/buffer-descriptions-for-i-o-control-codes),
[process exit](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-terminateprocess).

Send responses separately from receiving the next batch. Settle the entire captured batch before any fresh receive,
including after recovery. Then a completed saved hint can name only a pending new IRP, never a newly processed one;
an unanswered saved hint still names its original request. Preserve a complete response before sending it; otherwise
refuse buffer-bearing operations with STATUS_CANCELLED without dereferencing a predecessor's virtual addresses.
Cleanup and close need their own idempotent terminal steps and reference publication, rather than generic replay.

The same change must retain the twenty-kill handle/view history, foreign and second-daemon refusals, every A-158
takeover inode and link name, and acknowledged hold release before intentional unmount. A worker dropped without
that acknowledgement waits for its finite control transaction, joins, and closes only its own copies, leaving the
held volume for its successor. It never stops the held volume or cancels its processing requests. The control wait
uses the API minimum TransactTimeout of 1000 ms, distinct from the infinite processing-request lifetime.

Diagnostics [41457226598](https://github.com/hyper-light/slates/actions/runs/41457226598), commit
`18fbf536a5ddff33e10ae0dcb654128162b41e41`, settle one counted captured read on each architecture; both the original
and fresh reads return their file bytes. The exact command above completes in 0.15 s on x86_64 and 0.16 s on arm64.
The seven then-current live CLI tests also pass, including zero lost drives in twenty kills per architecture.
Diagnostics [41480565373](https://github.com/hyper-light/slates/actions/runs/41480565373) retain the captured-read
and twenty-kill results after adding durable kernel names and the private purge protocol. Their separate live
renamed-view failure is recorded in the renamed-view bug; it keeps the whole Step 9.4 change open.

Pure tests exercise malformed and torn captured batches, saved responses, name updates interrupted at every byte,
unconfirmed open retirement, and restored-reference reconciliation. The native bridge suite has 54 passing tests
and one explicitly ignored child role in run 41480565373. The live request oracle is run separately with the driver
enabled; the ungated native suite alone cannot establish it.

Final diagnostics [41484542209](https://github.com/hyper-light/slates/actions/runs/41484542209) retain one counted captured-read settlement in 0.15 s on
x86_64 and 0.19 s on arm64, with all nine live CLI tests passing, zero lost drives of twenty, both renamed-view
histories, all 54 native bridge tests, 32 active anchor tests, native clippy and private Windows documentation.
