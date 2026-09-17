# A control-channel burst larger than one drain batch was forgotten until a later send

Date: 2026-09-17. Contracts: §4.3 (the control channel, admission, bounded work per loop phase),
banned item 9 (a lost message).

## Description

A shard drains its control channel only when its `control_pending` flag says something was sent, and
drains at most `batch` messages per step (the bounded-work rule). `drain_control` cleared the flag
**before** draining the batch, on the reasoning that "a send that lands during the drain sets the flag
again". A burst already queued when the drain began — more messages than one batch — was not a send
landing during the drain: after the first batch the flag was clear, the remaining messages were still
queued, and nothing re-armed the flag until some later send succeeded. Until then the rest of the burst
was invisible to the shard, which went idle and parked.

Two consequences, both met by the observation histories (`crates/server/tests/observe.rs`) that flood a
held shard's channel:

- A spawn queued behind the burst was never admitted (its receipt never answered), so an observation
  submitted after a flood waited its whole budget and was refused at the deadline although the shard
  was idle.
- `Runtime::shutdown`'s message met the still-full channel, was retried (`ControlFull`) against a
  shard parked for good, and the daemon's drop-time shutdown never returned: the test binary hung for
  13 minutes with the test thread inside `Runtime::shutdown → join` and the shard thread inside
  `kevent` (sampled with `sample <pid>`).

## Reproduction (failing test first)

`crates/rt/tests/burst.rs::a_burst_past_one_batch_is_drained_whole_and_the_shutdown_behind_it_lands`:
a shard with `batch = 8` and a control channel bounded at 32 is held inside one poll (a 300 ms spin);
32 fire-and-forget tasks are submitted behind the hold; every one must run within 10 s, then a shutdown
queued after them must complete.

- Before the fix: **FAILED** — `every task of the burst ran (8 of 32; a drain that re-arms only on a
  later send runs one batch of 8)`, after the 10 s wait.
- After the fix: **ok** (recorded in the commit's gate chain).

```sh
cargo test -p slates-rt --test burst
```

## Fix

`drain_control` re-arms `control_pending` when it drained a whole batch — the channel may hold more —
so the next step drains again; a partial batch (the channel ran empty) leaves the flag clear as before.
One extra `try_recv` per burst end is the cost.

## Impact

Any burst of more than `batch` control messages to one shard while it was busy: cross-shard spawns
under load (`verbs::forward`, `xshard` calls, client-id releases), the fleet's fan-outs, and every
test-facing observation. The lost messages were admissions never made and, behind them, a shutdown
never received. The daemon's `batch` is calibrated in the tens, so a burst of that size behind a long
poll — a starved shard under load — is the ordinary case the fleet suites run under; how often it hid
inside "period budget spent" verdicts is not known.

## Siblings

`drain_inbound` (the wake rings) is bounded the same way but is not flag-gated: the rings are polled
every step, so a burst past one batch is drained on the next step. `expire_timers` and the pollers are
per-step as well. The flag-gated drain was the one path with this shape.
