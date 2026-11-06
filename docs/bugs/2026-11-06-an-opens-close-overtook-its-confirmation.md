# An open's Close overtook its confirmation through the other worker

**Amendment:** A-219. The bug was on A-219's branches only (the separate form `e070f31e`, and every combined-form
response but a batch's last before this fix). It never reached main, which serves with one worker.

**Found by:** diagnostics run 42155161568 (`diag-winfsp-overlap-26`, the five-arm A/B, 2026-11-06). In the arm64 run 1,
the separate form at two fixed workers logged:

```
slates-winfsp: open confirmation refused: open ledger: Stale
slates-winfsp: receive worker 0 ended: confirm handed open answered NTSTATUS 0xc0000001
slates-server: WinFsp receive worker failed: confirm handed open answered NTSTATUS 0xc0000001
```

Its drive died, so mount_bench failed the whole run, the loaded phase of every arm included.

## Root cause

A receive worker confirmed an open after sending the Create's response. The response completes the client's Create,
and the client may close the handle at once. With two workers the Close can be delivered to the other worker, whose
callback reaches the shard first and retires the open. The first worker's confirmation then finds no open: the ledger
refuses it `Stale`, and the worker ends.

With one worker this cannot happen: the Close is that worker's own next request, taken only after the confirmation.
A-215's hand-off already posts a batch's last response's confirmation before its call. So in the combined form only
the other responses of a batch kept the race. That is narrower: it takes a batch of more than one request, then a
client that closes at once.

The two-worker model oracle (`crates/bridge-winfsp/src/inflight/capture/workers.rs`) never closed an open, so it could
not see this.

## Impact

None on main. On the A-219 branches, a worker could end under load with two workers, which ends the mount (the server
counts `winfsp.mount_lost`).

## Exact edits

- **The oracle first.** Every successful Create's response now queues the client's Close of that open. A worker serves
  it and the shard retires the open, and a confirmation that finds its open closed counts as stale. The test requires
  0 stale and 0 wrong contexts. With confirmations after the send (the old order), 16,729 scenarios gave 523 stale
  and 535 wrong contexts. At the run's stride of 13, 9,328 gave 320 and 322.
- **The fix.** `crate::dispatcher::serve_batch` posts every open's confirmation before the response is sent and takes
  its answer after, as the hand-off does (`crate::host::post_confirm`). This is safe because the response is saved
  first: a death before the send still hands the kernel the context, since recovery sends the saved Create again
  (A-215's argument).
  - The model now confirms in the same order: 9,583 scenarios, 100,698 Closes, 0 lost, 0 wrong, 0 twice, 0 wrong
    contexts, 0 stale.
  - `confirm_open`, which confirmed after the send, is gone.
- The successor's crash step in the oracle samples every 13th step (`SECOND_CRASH_STRIDE`), since a life with Closes is
  about three times longer. It runs in 29.8 s on this Mac, and every non-vacuity counter still moves.
