# A private purge waited for a callback its refused open could not produce

Observed 2026-11-02 while completing A-198 Step 9.4.

## Description

The first identity-addressed purge implementation passed the taken-read oracle and twenty daemon kills on both
Windows architectures. Its following renamed-view case kept the old bytes, reported zero completed/refused purge
outcomes, then hung during teardown. Run [41472709500](https://github.com/hyper-light/slates/actions/runs/41472709500),
commit `05327144ec0965d3a37a02139f3b2e0e4ca5204e`, was cancelled after the lack of progress. The artifacts retain
both `completed 20/20, drives lost 0, restored reads 4` and the later renamed-view failure.

Detailed callback traces changed the timing: the ordinary-view test passed on both runners in
[41475551676](https://github.com/hyper-light/slates/actions/runs/41475551676), and the isolated renamed case passed
in [41476295731](https://github.com/hyper-light/slates/actions/runs/41476295731). That was not evidence the full
suite had become correct.

## Root cause and correction

The private Begin/End channel had capacity two. Its receiver drained it only while authenticating a kernel callback.
An open can be refused by Windows before a callback reaches the host. Enough such messages can fill the channel;
the command thread then waits for room before it can report its outcome or issue another open, while the receive
thread waits for a callback. Ordinary Windows probes in the detailed trace happened to provide callbacks and hide
this progress defect.

`purge_intents.rs` now owns an auto-reset event alongside the same bounded channel. A publisher signals after
publication, and before waiting on a full channel. The control receiver waits for either its I/O completion or this
event and drains at most the channel bound per wake. It also drains before a callback and after an immediate control
completion. The event is unnamed; the two endpoints own duplicate handles and close them with their threads.

A failed wait/drain still cannot free a pending control call's borrowed buffers: `transaction` settles the
OVERLAPPED with `GetOverlappedResult` before returning that error. There is no cancellation race or alternate receive
path. Microsoft documents the event and wait behavior in
[SetEvent](https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-setevent) and
[WaitForMultipleObjects](https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-waitformultipleobjects).

## Failing oracle and verification

`private_purges_make_progress_without_a_kernel_callback` publishes two Begin/End pairs and supplies no kernel
request. Its owned publisher must finish and all four messages must arrive. The scoped thread is joined on failure
as well; dropping the receiver releases a blocked publisher.

With only the event signal omitted in an offside diagnostics tree, the unchanged oracle fails on both architectures
in [41480713447](https://github.com/hyper-light/slates/actions/runs/41480713447), commit
`7eb86e8a34049aab85038aa3ef4562d9b8803096`: `left: Some(258), right: None` (WAIT_TIMEOUT), after 10.00 seconds.
The real implementation passes on jupiter with native clippy and all 54 active bridge tests. The full live suite
without tracing, [41480565373](https://github.com/hyper-light/slates/actions/runs/41480565373), retains the captured-read
and twenty-kill results but still fails the live renamed-view assertion with zero purge echoes. The event correction
is necessary for the callback-free oracle; it does not by itself establish the renamed-view fix. Stage diagnostics
and green main CI remain required before Step 9.4 closes.
