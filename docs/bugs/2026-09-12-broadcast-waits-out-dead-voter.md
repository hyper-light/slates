# The consensus broadcast waited out a dead voter's full deadline every period — a slow-retirement vicious cycle that flaked the fleet suite

- **Date:** 2026-09-12
- **Area:** §4.8 "late work" — `slates_server::fleet::broadcast`, the fan-out the configuration council and
  root group use for replication, election and learner fetch; and `slates_cluster::DispatchWait`.
- **Severity:** robustness / liveness — a membership change (a dead node awaiting retirement) slowed
  **every** consensus round behind it, so takeover/promotion/retirement operations intermittently missed
  their deadlines. It was the residual after the extension-budget fix
  (`2026-09-12-fleet-consensus-hard-budget-under-load.md`) — and that fix *amplified* it 11×.

## Description

After wiring the progress-extension budget into fleet consensus, the fleet suite still flaked ~1 run in 7,
**even on an idle machine** (measured at 91% idle), always in the largest (5-daemon) consensus/takeover
tests, though each such test is 17/17 in isolation. Ada's call was decisive: a flake on an idle machine is a
real bug, not a test artifact. A hypothesis that leaked in-process daemon sockets were the cause was
**disproved by a direct experiment** (80 accumulated leaked sockets left fresh formation just as fast).

Instrumenting `broadcast`'s wall-clock time under a reproducing run found it: **39 broadcasts in one run each
took ~1100 ms — the full extended `max_deadline`.** `broadcast` fans a request to every voter concurrently
(one child task each, bounded by the deadline, each returning its endpoint whatever the outcome) and then
waits for **all** children to report (the channel to disconnect). A voter that has died but the council has
not yet retired is still a voter: the leader sends it an append every period, its child never gets a reply,
and it times out only at the deadline. With the extension budget that deadline is ~1.1 s (one period ×
`1 + ELECTION_HEARTBEATS`), so `broadcast` waited ~1.1 s **every period** for the dead voter — where the old
hard budget waited 100 ms. That is a vicious cycle: the council round that would *retire* the dead voter is
itself gated behind the ~1.1 s wait for that same dead voter, so retirement is slow, the voter stays a voter,
and the slow rounds continue — stretching takeover/promotion past the test deadlines.

## Root cause

`broadcast` waited for **every** child to finish, so the slowest child (an unreachable voter timing out at
the deadline) gated the reachable quorum. Correct for a fixed short deadline; pathological once the deadline
was extended for late-work tolerance, because "late work" waiting was being applied to a voter doing **no**
work rather than to one still making progress.

## Fix

`crates/server/src/fleet.rs` — `broadcast` now collects **progress-aware**, via `slates_cluster::DispatchWait`
(the same wait-and-decide policy `collect_acks`/`collect_promises` use): it returns as soon as every child has
reported **or** the round has *stalled* (no new reply within the stall window). The reachable quorum replies
fast; once replies stall, the round returns and the leader folds what it has and re-ships next period
(idempotent). A child that has not reported is **detached**, not joined — joining would wait out the very
deadline the progress-aware stop avoids; each detached child still returns its endpoint at `request_within`'s
deadline and self-cleans (bounded, never orphaned — banned item 9), and its session is re-established by the
per-peer link task (a no-op for a live voter that merely replied late, whose reply and endpoint were already
collected). `slates_cluster::DispatchWait` (and its `new`/`keep_waiting`) are now `pub` for this reuse.

`collect_acks`/`collect_promises` were already progress-aware, so the takeover **quorum** path is unchanged;
only the consensus/record **fan-out** changed.

## Test / validation

Under the same idle machine: `broadcast` timing instrumentation showed **0** rounds ≥200 ms after the fix
(was 39 rounds at ~1100 ms). The full fleet suite went **13/13 green** (was ~1 failure in 7), and each run is
**~30% faster** — ~93 s versus the ~120–140 s the dead-voter waits had been inflating it to — because the
coordinator's periods are no longer stretched. `slates-cluster` unit tests and `cargo clippy` stay clean.

## Sibling sweep

Every consensus fan-out (council replication/election/learner-fetch, root replication/election/learner-fetch)
routes through the one `broadcast`, so all are fixed together. The record/content commit and the takeover
promotion use `collect_acks`/`collect_promises`, which already stop at quorum or stall, so a dead holder never
gated them — no change needed there. `materialize_pending`'s content fetch is a single request to one recorded
holder (not a fan-out), so it has no all-must-report wait.
