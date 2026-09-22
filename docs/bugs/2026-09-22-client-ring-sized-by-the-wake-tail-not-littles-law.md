# The client ring was sized by the boot wake tail, not Little's law, so a pod could seat one client

Date: 2026-09-22. Contracts: §4.7 "Derived constants" (ring depth is Little's law on the per-client
request rate × p99 service time), §4.1 (D-11, R3), AC-2.6.

## Symptoms

- CI run 35791239154 (`628962b`), macOS workspace job:
  `a_session_outlives_a_daemon_restart_and_its_retry_meets_the_completion_record` panicked at
  `crates/client/tests/client.rs:56` with `channel: too many clients (the bound is 2)`. The history
  holds four clients at once.
- A freshly created KIND cluster, with its node containers pinned to two CPUs: `kubectl exec slates-0 --
  /slates bootstrap root` exited 4 with `channel: too many clients (the bound is 1)`, and so did the
  diagnostic `slates status` on that pod. The kubelet's readiness probe runs `slates status`, so it
  held the only seat.

## Evidence

The three pods of that cluster ran one image on identical nodes, booted within the same second, and
logged these derived values:

| pod | `slots_per_ring` | `clients_per_shard` | `tasks_per_shard` | `fleet_sessions_per_plane` |
|---|---|---|---|---|
| slates-0 | 16384 | 1 | 7 | 6 |
| slates-1 | 8 | 1285 | 2575 | 963 |
| slates-2 | 256 | 41 | 87 | 30 |

Other pods of the same image logged 512 slots and 20 clients, and 1024 slots and 10 clients.
`requests_in_flight_per_shard` logged 20 on every pod.

The chain in the code:

- `slots_per_ring = next_power_of_two(ring_entries)` (`crates/server/src/config.rs`), labelled "the
  runtime's ring entries (Little's law)".
- `ring_entries = next_power_of_two(wake.p99 / syscall.median)` (`crates/machine/src/profile.rs`),
  which is not Little's law.
- A client region's bulk area is `slots × 2 × BULK_CHUNK_BYTES`.
- `clients_per_shard = reserve × CLIENT_SHARE_PERMILLE / 1000 / region_bytes`, and the task arena and
  the fleet's session pool are sized from that.

The wake tail is not stable enough to size anything. The pod image's own `slates profile`, run in plain
Docker four times per CPU set (2026-09-22), gave:

| cpuset | wake p50 (ns) | wake p99 (ns) | samples |
|---|---|---|---|
| `0` | 417, 417, 417, 417 | 708, 1334, 709, 1042 | 64 each |
| `0-1` | 10041, 416, 417, 459 | 511042, 1291, 667, 63416 | 256, 64, 64, 576 |
| `0-3` | 459, 542, 8750, 458 | 75958, 21459, 65417, 4583 | 64, 192, 128, 64 |

A separate run measured the syscall median at 119 ns (interval 119–122). Five consecutive profiles in
one pod gave wake p99 of 708, 18375, 21667, 25667 and 750 ns.

## Root cause

§4.7 sizes a client ring by Little's law on the per-client request rate × p99 service time. The daemon
already computes that as `requests_in_flight_per_shard` (the admission value that sizes one client's
in-flight work) and logged 20 on every pod. The client ring instead reused the runtime's inbound-ring
depth, a ratio built on one boot measurement of the wake tail. When that tail reads milliseconds, the
ring and the bulk area that scales with it fill the client share, and the shard seats one client.

## Fix

`slots_per_ring = next_power_of_two(requests_in_flight_per_shard)`, the design's formula: 32 slots
from the current admission value. The ring's credit still refuses `RingFull` rather than dropping, so a
client that outruns its ring waits; nothing is lost.

## Failing test first, and validation

- `crates/server/tests/daemon.rs::a_contended_wake_tail_does_not_shrink_the_client_seats` derives a
  daemon under the pod's 1 GiB memory bound from a profile whose wake p99 is the 511,042 ns measured
  above, then connects four clients. Before the fix it failed with CI's exact message, `too many
  clients (the bound is 2)`; after it, all four are seated and served.
- This laptop, 2026-09-22: every workspace test outside `slates-server` (1,373 passed, 14 ignored); the
  server's unit, daemon, observe, recovery, attach_forms, nfs_mount and virtiofs suites (146 passed);
  the fleet suite (48 of 48, 326.79 s); the real-process CLI flow (`SLATES_TEST_CLI=1`, 10 of 10). All
  passed. `cargo clippy -p slates-server --all-targets -- -D warnings`, `cargo fmt --all --check` and
  `cargo xtask check` are clean.
- KIND, a freshly created cluster per iteration with its node containers pinned to two CPUs: before
  the fix, the `bootstrap` refusal above. After it, 4 of 4 proofs passed, and every pod boot, the
  replacements included, logged `slots_per_ring = 32`, `clients_per_shard = 330` and
  `fleet_sessions_per_plane = 249`.

## Siblings found (owed)

1. **The wake probe measures two different events and does not converge the tail it reports.** It
   counts every park/unpark round trip, whether the waiter was still running (an on-core handoff,
   about 0.4 µs) or asleep on an idle core (a real wake, about 10 µs). Thread placement holds for a whole
   run, so runs settle into one mode or the other. It stops once the median's bootstrap interval is
   within 10% (`CONVERGED_WIDTH_PERMILLE`), in batches of 64 (`WAKE_BATCH`), so the p99 it reports is
   often the maximum of 64 samples. Still consuming it: the runtime's inbound ring depth,
   `task_step_budget_ns` (wake p99), `spin_ns` and the idle window (wake p50), and the guest credits'
   kick round trip.
2. **`spin_ns` is labelled "the measured wake cost p99", but is `spin_before_park_ns = wake.p50`.**
   The design (§4.1, §4.3, §4.7) says p99.
3. **`requests_in_flight_per_shard` comes from two stated assumptions**, `ASSUMED_REQUESTS_PER_SECOND`
   and `ASSUMED_SERVICE_P99_NS`, where the design says measured rates. It is stable (20) but not
   measured.
4. **The memory-pressure hold (`3b38d15`) follows host-wide memory, not pressure on this daemon.** The
   same KIND runs logged `per_shard_hold` up to 23,724,032 bytes on pods whose VM had about 53 GiB
   available. In one earlier fresh cluster, an 8 MiB create was refused `BudgetExceeded
   { available: 0 }` with capacity 268,435,456, committed 0, retained 0 and headroom 2,621,440 (the
   budget's own refuse-all line), which needs a hold of at least 265,814,016 bytes.

## Edits

- `crates/server/src/config.rs`: `slots_per_ring` from `requests_in_flight_per_shard`.
- `crates/server/tests/daemon.rs`: the regression above.
- `docs/wip/SLATES_DESIGN.md` §4.7 status, `docs/wip/GAPS.md` §0, `docs/wip/TBD_FIXES.md`.
