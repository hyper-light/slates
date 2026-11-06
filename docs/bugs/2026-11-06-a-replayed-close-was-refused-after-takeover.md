# A replayed Close whose open the dead daemon had ended was refused after takeover

**Amendment:** A-198 (the receive journal and its recovery), shared by main's one-worker form and A-219's combined form.
The bug is on main.

**Found by:** diagnostics run 42165934408 (`diag-winfsp-overlap-31`, the paired A/B, 2026-11-06), x86_64 pair
`sep1-comb1`, run 4. The arm `sep1` is main's build `28359a75`. Under the bar's load the anchor killed that daemon when
its heartbeat lapsed, and its successor logged:

```
slates-winfsp: context refused: open ledger: Stale
slates-winfsp: close refused with NTSTATUS 0xc0000008
slates-winfsp: settled 2 captured requests before receiving
```

## Description

A client closed a handle while its daemon was being killed. The successor replayed the captured Close and answered it
`STATUS_INVALID_HANDLE` (0xc0000008), counting it as a refusal. A Close never fails because the daemon restarted, so
recovery must answer that Close as done.

Windows ignores the status of an `IRP_MJ_CLOSE` and of an `IRP_MJ_CLEANUP`, so the client's `CloseHandle` still returned
success. The wrong part is the daemon's own answer and record: it refused a request it had in fact completed, logged a
context refusal, and would count it in any refusal tally a test or an operator reads.

## Root cause

Recovery replays every Cleanup and Close a dead worker's journal held unacknowledged (`settle_recovered` in
`crates/bridge-winfsp/src/dispatcher.rs`, `settle` on main). The replay runs the request through the shard again
(`crate::host::Request::serve`, which binds the request's context to an open in the open ledger).

The dead daemon may have served that Close already. It ended the open on the shard, which retires it from the ledger,
and was killed before its worker sent the response and acknowledged the request. The journal still holds the Close, so
the successor replays it. The ledger has no such open now, `bind` answers `Refusal::Stale`, and `serve`'s refusal arm
turns it into `STATUS_INVALID_HANDLE`.

Both forms take this path: main's one-worker form (`settle` → `operation` → `Request::serve`) and the combined form.
There is no variant one form has and the other does not. The combined form's fix below fixes both, because the shared
code is `Request::serve`.

## Impact

On main since A-198: any kill landing between a Cleanup's or Close's effect on the shard and the worker's
acknowledgement. The client sees nothing. The daemon logs a refusal and answers a wrong status for a request it did.
No open is leaked: the dead daemon ended it.

## Exact edits

- **The failing tests first**, in `crates/bridge-winfsp/tests/support/host.rs`:
  - `a_close_served_before_the_kill_is_answered_done_after_takeover`, the replay. A client opens the fixed file
    uncached, reads it and closes it. CloseHandle returns once the Cleanup is answered, and Windows then sends the Close.
    The serving process (`closer-first`) ends the open on the shard when the Close arrives, holds the Close's reply, and
    tells the parent. The parent kills it, and a successor (`closer-successor`) takes over and reports its recovery: how
    many captured requests it settled, and how many of those were a replayed Cleanup or Close the file system refused.
    Expect: at least one settled and none refused.
  - `a_handle_open_across_the_kill_closes_through_the_successor`, the handle open across the kill. A client opens and
    reads the file under the first server and keeps the handle open; the server is killed, and the successor
    (`holder-successor`) takes over. Only then does the client close. Expect: the successor serves the open's Cleanup and
    Close without a refusal, and CloseHandle returns nonzero.
- **The counter it reads.** `Outcome::Recovered { settled, refused }` (`crate::host`): the dispatcher counts a replayed
  Cleanup's or Close's response whose status is not success. The server counts it as
  `winfsp.captured_requests_refused`, and the recovery log line carries it.
- **The fix.** The dispatcher marks a replay (`crate::dispatcher::replaying`), each `Request` carries the mark
  (`replayed`), and `Request::serve` answers a replayed Close (`Op::Release`) whose open is `Stale` as done, logging
  `slates-winfsp: a replayed close found its open already ended: answered done`. Only a Close retires an open, so only
  a Close can find its own effect done; a replayed Cleanup, a live request and every other replayed request keep the
  refusal, since a stale context there is still a refused context. The ledger's close record already passes a `Stale`
  open over (`finish_context_effects`), so the answer changes and nothing else does.
- **The test's own first bug.** In diagnostics run 42171690165 (`diag-winfsp-overlap-32`), every arm, the fixed ones too, failed
  `the successor's recovery was never told`. The successor's role waited for its recovery's outcome before serving
  any request, but recovery replays the captured Close through that same shard and waits for its answer. So neither
  went on. The role now serves while it watches for the outcome (`next_request`, `report_close_recovery`). That run
  also showed the client's CloseHandle return before the kill, which is why the first test's account above says the
  Close follows CloseHandle.
- **The red check.** Diagnostics run `diag-winfsp-overlap-33` runs both tests on both runners: on main's form without
  the fix, on the combined form without the fix (the `replayed && ends_open` arm kept out), and with the fix on main's
  form and on the combined form at one and at two fixed workers. Red must show `refused` above zero; the fixed arms
  must show zero.
