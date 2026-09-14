# The SDK by-use tests list right after `destroy`, before the daemon's teardown slices finish

Date: 2026-09-14. Branch `agent/consumer-capability`. Found while running the SDK by-use tests the
consumer-capability charter requires (they exercise `Client::connect`, which now takes a delivered
capability).

## Description

`crates/sdk-python/tests/test_sdk.py::test_lifecycle_round_trip_over_a_live_daemon` fails on this
machine, twice in a row (0.9 s each, no timing budget involved):

```
AssertionError: False is not true : the destroyed volume is gone from list
```

The test calls `client.destroy(volume)` and, in the very next call, asserts `volume` is absent from
`client.list()`. The async Python test (`test_sdk_async.py`) and both Node tests (`sdk.test.mjs`,
`sdk_async.test.mjs`) hold the same assumption; they happened to pass in the same runs.

## Root cause

By design (§4.4 `destroy`; `Client::destroy`'s doc: "the daemon tears the volume down in cooperative
slices after replying"), the `Destroyed` reply precedes the catalog's removal of the volume. A `list`
issued right after the reply can still see it. Measured here (2026-09-14, this box at the memory
wall: swap 15 991 MB of 17 408 MB used; `~/.cache/slates-consumer-capability-scratch/destroy_then_list.py`
against `target/debug/slates`, three rounds per shard count):

```
shards=1 round=0: gone after 84 us, 1 polls that still listed it
shards=1 round=1: gone after 54 us, 1 polls that still listed it
shards=1 round=2: gone after 67 us, 1 polls that still listed it
shards=2 round=0: gone after 271 us, 1 polls that still listed it
shards=2 round=1: gone after 5056983 us, 545 polls that still listed it
shards=2 round=2: gone after 302 us, 1 polls that still listed it
```

The first `list` after the reply still showed the volume in 6 of 6 rounds; it was gone 54–302 µs
later in 5 of them. The Rust client test never assumed otherwise: `crates/client/tests/client.rs`
polls with `wait_until_listed` after its destroy. The SDK tests asserted on the first reply.

The 5.06 s round on the two-shard daemon is a separate observation, recorded here for the
integrator: a destroy's teardown slices once took five seconds to reach the catalog, a
liveness-cadence-scale delay that suggests the teardown task waited for a parked shard's next wake
rather than running on the reply's shard at once. Not investigated further on this branch (outside
its scope; one occurrence in six rounds).

## Impact

A flaky by-use test on any machine where the SDK's next call is issued within tens of microseconds of
the reply — deterministic on this one for the Python sync client. No product behaviour is wrong.

## Exact edits

- `crates/sdk-python/tests/test_sdk.py`: a `_listed_until_gone(client, volume)` helper that polls
  `list` within the existing startup budget (`STARTUP_SECS`, `POLL_SECS`), used by the round-trip's
  destroy step.
- `crates/sdk-python/tests/test_sdk_async.py`, `crates/sdk-node/tests/sdk.test.mjs`,
  `crates/sdk-node/tests/sdk_async.test.mjs`: the same bounded poll at their destroy step (the
  sibling sweep; they pass today by the width of a call's overhead).

Failing first: the Python sync test, twice (above); after the edit, the whole Python suite and both
Node suites pass (numbers in the branch's final report).
