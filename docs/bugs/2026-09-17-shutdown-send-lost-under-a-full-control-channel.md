# A runtime shutdown against a full control channel hung for good

Date: 2026-09-17. Contracts: §4.3 (the control channel, admission, shutdown), banned item 9 (a lost
error).

## Description

`Runtime::shutdown` sent `Control::Shutdown` to every shard with `let _ = registry::send_control(..)`
and then joined the shard threads. A shard's control channel is bounded at its admission limit and
refuses `ControlFull` when it is full — routinely, under a burst of submissions behind a long poll. A
refused shutdown message was dropped, and the join that followed waited for a loop exit the shard
would never reach: the process hung at shutdown.

## Root cause

The refusal was discarded. The channel drains as the shard runs, so the send would have landed on a
retry within one step of the shard; nothing retried it.

## Reproduction (failing test first)

`crates/rt/tests/admission.rs::a_shutdown_lands_against_a_full_control_channel`: a task holds the shard
inside one poll (a 300 ms spin), fire-and-forget submissions fill the channel to `ControlFull`, then
`shutdown()` runs on a helper thread and the test waits 10 s for it to return.

- On the pre-fix line (`let _ = registry::send_control(id.0, Control::Shutdown)`, restored for the
  run): **FAILED** after 10.01 s — the shutdown never returned
  (`thread 'a_shutdown_lands_against_a_full_control_channel' panicked at crates/rt/tests/admission.rs:244`).
- On the fix: **ok**, with the other four admission histories, in 0.30 s (5 passed).

Command, in either case:

```sh
cargo test -p slates-rt --test admission a_shutdown_lands_against_a_full_control_channel
```

## Fix

`Runtime::shutdown` retries the send while it is refused `ControlFull`, yielding between attempts,
and stops on `Ok` or any other refusal (`ShardGone`: nothing to shut down). The wait is bounded by the
shard's next drain; a shard that never drains again would hang the join just the same, as before.

## Impact

Any process shutting a runtime down while one of its shards' control channels was full: the daemon's
own stop, a test's teardown, an operator's restart. Found by analysis while adding admission receipts
(the §4.3 status of 2026-09-17), before it was observed in the field.

## Siblings

None found: `shutdown` was the one control send whose refusal was discarded rather than returned or
counted (`release_client_id` counts and reports its first refusal; the observation path returns it
typed).
