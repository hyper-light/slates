# 2026-09-10 — A continuously busy shard never harvests driver I/O, so an I/O-bound task starves behind a CPU-bound one

## Description

With the connection-id demultiplexer in place, the two in-process content-placement fleet tests
(`a_sealed_snapshots_content_replicates_to_the_holder_and_places` and
`a_takeover_successor_serves_the_dead_owners_content_over_nfs`) failed deterministically, each hanging
until its placement deadline (~23.6 s) with the snapshot never placed. The three-process deployment test
(separate OS processes, one shard each) passed; only the in-process shape — two daemons and the test's
poll loop sharing the machine, the fleet tasks and the daemon's client server sharing one shard —
failed.

Instrumenting the shard loop showed the shape: the record plane's content put to the holder timed out at
its 100 ms deadline (`ClusterError::Uncertain`, only the owner acked), and the demultiplexer's receive
loop saw **100 ms+ gaps between datagrams** while a single task poll ran for **230–290 ms**. The long
poll was the daemon's `serve_loop`, and its `serve_round` body grew from 21 ms to over 270 ms across the
run.

## Root cause

The shard's run loop harvests driver I/O completions (socket readiness — the only way a
`recv_from` future is ever woken) **only inside `park`**:

```
loop {
  let outcome = self.step();      // runs ready tasks; never polls the driver for I/O
  if outcome.did_work { continue; }
  if active && spin_until_work() { continue; }
  self.park(deadline);            // driver.wait(...) — the only driver poll
}
```

`serve_loop` re-queues itself every iteration through `futures::yield_now` (a `wake_by_ref` that marks
the task ready again) whenever it did work or is inside its idle window. The content-placement test drives
its `await placed` poll in a tight loop, so the daemon's client is never quiet, so `serve_round` returns
`did = true` every step, so `step()` returns `did_work = true` every iteration — and the loop takes the
`continue` branch **forever**, never reaching `park`. With the shard never parking, `driver.wait` is
never called, so the socket readiness the demultiplexer's single receive task waits on is never
delivered: the peer's datagrams sit unread in the kernel while the shard spins serving one client. The
put to the holder cannot be sent or its reply read within its deadline, and it fails.

The growth (`serve_round` 21 ms → 270 ms) is the same starvation seen from the client side: because the
put never places, the record plane retries every heartbeat forever, appending records the client's
`await placed` reads must then walk — a feedback loop that only exists because the placement is wedged.

This is a runtime-level fairness defect: an I/O-bound task starves indefinitely behind a CPU-bound one on
the same shard. It is invisible when each node is its own process (the shard has only fleet work, goes
idle, and parks — harvesting I/O), which is why the multi-process test passed and only the in-process
one, where a hammered client server shares the shard, failed. The design already names the threshold
this violates: `step_budget_ns` is derived as "a step longer than a peer's wake starves the shard"
(§4.3).

## Impact

- Any shard that stays continuously busy with task work — a client that never lets its server loop idle
  — stalls every I/O-bound task sharing it: the fleet's datagram demultiplexer, and before the
  demultiplexer, a per-peer serve socket's receive. Content placement, record commits and SWIM probes on
  that node stall for as long as the client load lasts, up to a false peer retirement.
- Deterministic under the in-process fleet tests; latent for any co-located client-serving and
  fleet-serving shard under sustained client load ("scale up or down flawlessly" — the load that exposed
  it is exactly the scale-up case).

## Exact edits

- `crates/rt/src/shard.rs` `run`: a run that stays busy without ever waiting now harvests the driver
  **without blocking** (`harvest_io`, a zero-timeout `driver.wait`) once it has gone `step_budget_ns`
  (the config's derived value, no new literal) since its last real wait — so I/O keeps pace with tasks
  under any load. An idle shard reaches `park` every loop and pays nothing. New `harvest_io` helper drains
  the driver's ready completions into the run queue without setting the parked flag (the shard is not
  waiting, so a concurrent kick must not believe it is).

## Validation

18-core box, `--test-threads=1`: both content-placement tests 5/5 after the fix (0/5 before); the full
in-process fleet suite 12/12; the three-process CLI deployment test 12/12 under 8 CPU spinners; the rt
suite (its own run-loop, poller and timer tests) green.

## Siblings swept

- The other run-loop exits are correct: `run_until_idle` (test/bench harness) parks per iteration and is
  never the continuously-busy production loop; `park` and `spin_until_work` are unchanged. The harvest is
  gated on `did_work`, so it never adds a syscall to an idle or lightly-loaded shard.
- `serve_loop`'s `yield_now` re-queue is intentional (a latency optimization to keep serving a client
  burst without a park/wake round trip); the fix leaves it as is and makes the run loop fair instead of
  removing the optimization.
