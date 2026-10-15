# A client reported a live daemon `Stalled` while it was still forwarding the request

**Found:** 2026-10-07, reading the client's reply deadline against the daemon's forward bounds; then reproduced.
**Status: fixed.**

## Description

A client waits one liveness budget (1 s) for a reply (`Deadlines::derive`: `reply_ns = liveness_budget_ns`). Past it,
the synchronous call fails `Stalled` unless the verb defers its reply (`defers_reply`). The async driver, which both
SDKs use, fails the call unless it was submitted patient.

The daemon bounds a forwarded verb by more than that (`verbs::resolve_and_forward`):
- a location round, one liveness budget, run when there is no usable route;
- then the forward, one liveness budget plus the path's measured round-trip tail.

`defers_reply` covered only a page read (`ReadRange`), a granted landing and FUSE attaches. A forwarded write or
status on a live daemon, with each step inside its own bound, was reported `Stalled` at 1 s.

## Reproduction

`a_forward_answered_within_its_bounds_is_never_reported_stalled` (`crates/server/tests/fleet.rs`):
- the copyset takeover shape, with the owner dead and the successor serving;
- the successor answers each location query and each forward 0.6 s late (`Daemon::inject_serve_delay`, test support);
- a fresh `slates_client` on the foreign node writes a snapshot.

Before the fix it got `Err(Stalled { after_ns: 1000000000 })` at 1.002 s, while its daemon had run the round (2
claims) and was mid-forward.

## Root cause

The client's reply deadline doubled as its daemon-liveness check, so it could not cover a forward's longer bound.
Liveness is answered separately: `daemon_gone`, through the anchor's heartbeat. The set of verbs the daemon may forward
lived only in the server (`is_forwardable_read`, `volume_of`, `mutates_shard_image`), so the client could not ask.

## Fix

- The forwardability rules moved onto `RequestBody` in `slates-ipc`: `volume`, `mutates_shard_image`,
  `forwards_as_read`, `forwards_as_write`, `may_be_forwarded`. The server's predicates delegate to them, so there is
  one source of truth.
- `defers_reply` includes `may_be_forwarded`, as `ReadRange` already did and for the reason it gave: the daemon bounds
  the forward and always answers, so the client waits while its daemon lives. The daemon's bounds still end every
  forward, and a dead daemon is still found through `daemon_gone`.
- The async driver derives patience from the request (`Client::defers`, the same `defers_reply`), so an SDK call and a
  synchronous call follow one rule. Before, the driver waited only for calls submitted patient.

## Impact

Any forwarded verb, same-region or cross-region, could fail `Stalled` to a client whose daemon would have answered.
That is more likely on WAN paths and under load: a write to a volume owned elsewhere, a status, a staging verb. The
async SDK paths had the same failure for every deferring verb a binding did not submit patient.

## Tests

- The reproduction above fails before the fix and passes after it.
- `an_async_patient_landing_outlasting_the_reply_deadline_is_waited_for` (`landing_fairness.rs`) is restated: its
  control, a plain submission of a granted landing that ended `Stalled`, is now waited for like the patient one, and
  both must land every file.

## Owed

Done the same day: while the successor materializes a taken-over volume, it holds its own client's verb until the
materialization is no longer pending, polled at the heartbeat, for at most one liveness budget
(`verbs::await_materialization`). Only then does it refuse retryably, and the retry meets the materialized volume. A
client that retries without a pause is held to about one request per budget, where it had been refused 2.8 million
times in 400 s. The successor tests (copyset, held session, grant takeover, pressure takeover, this reproduction) pass.
