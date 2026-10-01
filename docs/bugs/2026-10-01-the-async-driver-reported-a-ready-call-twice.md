# The async driver reported a ready call twice (AUD-29-19–20 regression)

**Date:** 2026-10-01. **Design:** §4.7 (the async client), AUD-29-19, AUD-29-20 (the driver every binding runs).

## Description

From `153ca63` on, the Linux CI lane failed `every_call_ends_and_the_loop_is_never_held_across_restart_and_death`
(`crates/client/tests/driver.rs`) on every run: an `Event::Ready` whose reply the loop then could not find
(`a list answered None`). It passed in Docker under epoll and failed under io_uring
(`docker run --security-opt seccomp=unconfined`), and passed 3/3 with `153ca63`'s new stop-path sweep (an
observation of every shard to unmount FUSE mounts) disabled. The sweep was only the trigger.

## Root cause

`Client::take_ready` returns the words of every reply the client holds, not only new ones: a reply stays held
until the binding takes it. `Driver::pump` turned each held word into an `Event::Ready`, so a call whose reply
had not been taken yet was reported on every pump. A loop step that pumps and then ticks (the tick pumps first)
reported it twice; the loop took the reply on the first event and found nothing on the second. The stop-path
sweep changed when the first daemon's last replies landed, so one landed between the step's pump and its tick.

## Impact

Any binding loop (the Node and Python SDKs run this driver's pump) could see a second `Ready` for a call it had
already resolved: a spurious `None` or a resolution attempt on a settled promise.

## Exact edits

- `crates/client/src/driver.rs`: `Call::reported`; `pump` reports a call ready once and marks it; `finish` ends
  it; a reconnect's resend clears the mark.
- `crates/client/tests/driver.rs`: `a_ready_call_is_reported_once_until_it_is_finished` (one call, two pumps with
  its reply on the ring: red before on macOS and Linux); the list assertion prints the body.

## Proof

The new test passes on macOS; the driver suite passes 3/3 under io_uring in Docker as an ordinary user with the
stop-path sweep in place.

## Siblings

`take_ready`'s other callers are tests (`crates/client/tests/async_core.rs`) that read the set, not events; the Node
binding's `take_ready` is exposed but its async layer (`crates/sdk-node/async.mjs`) drives `pump`.
