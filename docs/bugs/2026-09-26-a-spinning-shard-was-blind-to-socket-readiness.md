# A spinning shard was blind to socket readiness

Date: 2026-09-26. Contracts: §4.3 "Loop" (if nothing is ready, spin for the idle window, then park in
the driver), D-9. Found while profiling the NFS bench (§4.6 A-35, A-38), on every OS driver.

## Symptom

The kernel's NFS clients saw about 3 ms per RPC on a release daemon. The bare runtime showed the same
effect: with a client active, the p90 of a loopback TCP request/reply round trip to a shard was
1,311 µs, the length of the shard's idle-spin window in that container.

## Root cause

A shard with nothing to run spins for its idle window before it parks (2-competitive, Karlin 1990).
The daemon keeps every shard spinning once a client is connected (`Control::Active`). While spinning,
the shard asked its rings, its pollers and its timers for work, but not its driver. A socket becoming
readable is known only to the driver, and the driver is asked only by a blocking `park()` or by the
busy path's harvest on the step-budget cadence. So a request that arrived during a spin sat unread
until the window ran out and the shard parked. Each NFS hop paid up to one window.

## Fix

Each turn of the spin also asks the driver without blocking (`Shard::harvest_io`, now returning
whether it found a completion), and a found completion ends the spin as work does.

## Evidence

- `crates/rt/tests/tcp.rs` `a_spinning_shard_answers_a_socket_request_without_waiting_out_its_window`:
  a shard with a 10 s spin window must answer a socket request within 1 s. It timed out before the fix
  and passes after, on macOS (kqueue) and on Linux (epoll and io_uring).
- The same runtime's loopback round-trip p90 went from 1,311 µs to 30 µs (epoll, Linux container,
  2026-09-26).
- Over io_uring the daemon still took about 1 ms per request after this fix. That was a second defect:
  `docs/bugs/2026-09-26-io-uring-zero-timeout-harvest-sleeps.md`.

## Exact edits

`crates/rt/src/shard.rs` (`spin_for_work`, `harvest_io`) and the test named above.
