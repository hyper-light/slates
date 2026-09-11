# 2026-09-10 — The idle spin consumes a poller's readiness without waking it (a client's rendezvous claim is lost for up to a second)

## Description

A `slates` client verb sometimes exits 3 ("no daemon at instance …") against a daemon that is alive,
beating, and answering the verb before and after. Observed while building the multi-process fleet
deployment test (`crates/cli/tests/cli.rs`): three `slates daemon --fleet` processes formed their
fleet, then one `slates status` against the first node failed with exit 3 after **1.005 s** — exactly
the client's claim wait (`CLAIM_WAIT_NS`, `crates/ipc/src/rendezvous.rs`) — and the next call, 20 ms
later, succeeded with a heartbeat age of 88 ms and no refusal counted. Reproduced 4/4 runs, always on
the node whose control shard was busiest (the fleet mesh forming).

The same signature was previously seen in the anchor CLI flow and attributed to "a verb landing in the
daemon's one startup-restart window" (`RESTART_RETRIES` in `crates/cli/tests/cli.rs`); at least part of
those were this bug.

## Root cause

`crates/rt/src/shard.rs` `Shard::spin_until_work` (the idle spin a shard runs when a client is active
and it has nothing to do) checked every registered poller with

```rust
inner.driver.has_pending() || inner.pollers.iter().any(|p| (p.ready)())
```

and returned "work arrived" — but **never woke the poller** whose `ready()` said yes. `run()` then went
back to `step()`, whose `wake_ready_pollers` asked `ready()` again. The control loop's poller is
registered with `|| DOORBELL_RANG.swap(false, AcqRel)` (`crates/server/src/daemon.rs`): the question
**consumes** the doorbell flag. So a ring that landed during the spin was consumed by the spin, the
second question in the step saw `false`, the control task was never polled, and the client's claim
slot stayed `CLAIMED` until another client rang or the claimant gave up at its 1 s claim wait — exit 3.

Timeline (macOS; Linux writes the kick eventfd but the same poller path applies):

1. client claims a slot, bumps the doorbell word, wakes it;
2. the doorbell thread wakes, sets `DOORBELL_RANG`, kicks every shard;
3. the control shard is in `spin_until_work` (a client was active, nothing to do): its poller check
   swaps `DOORBELL_RANG` to false, the spin returns true (`spin_hits += 1`);
4. `step()` → `wake_ready_pollers` → `DOORBELL_RANG.swap(false)` → false → no wake;
5. nothing else rings; the claimant waits `CLAIM_WAIT_NS` and reports the daemon unavailable.

The `Poller` contract said "the loop asks `ready` each step and during the idle spin, and wakes the
task when it says so"; the spin violated the second half. The server's other poller
(`state::any_ring_ready`) is a pure question, so it survived the spin; only a consuming question loses.

Confirmed by `crates/rt/tests/pollers.rs` (`a_ring_during_the_idle_spin_wakes_a_consuming_poller`):
a one-shard runtime with the spin enabled, a task registered as a poller with a consuming question,
rung 50 ms into a 2 s spin window (flag set, shard kicked — what the doorbell thread does). Before the
fix: "served 1 times, poller wakes 0" after the 2 s wake deadline. After the fix: served twice,
`poller_wakes` ≥ 1, `spin_hits` ≥ 1 (the ring landed in the spin — the timing premise, asserted).

## Impact

- Any client connect that lands while its daemon's control shard is idle-spinning (a client active,
  the shard momentarily out of work — the common state of a lightly loaded daemon) can wait up to the
  claim wait and fail with exit 3. Rate depends on how much of the shard's time is spent in the spin;
  a fleet node's control shard spins more (its membership tasks keep it active), which is why the
  multi-process fleet test hit it 4/4 while the anchor flow hit it rarely.
- No data loss, no wrong answer: the claim is refused typed, the client's retry connects. The cost is
  the second and the false "no daemon" report.

## Exact edits

- `crates/rt/src/shard.rs` `spin_until_work`: `inner.pollers.iter().any(|p| (p.ready)())` →
  `self.wake_ready_pollers(inner)` (the same function the step uses: it asks, and pushes each ready
  poller's task to the local ready queue, counting `poller_wakes`). The `Poller` doc now states that
  `ready` may consume its signal and every asker wakes on a yes.
- `crates/rt/tests/pollers.rs` (new): the failing test above, kept as the regression gate.
- `crates/cli/src/main.rs`, `verbs.rs`: exit 3's message carries the client's cause (a rendezvous
  absent, a claim unanswered within the claim wait, a daemon gone past the reconnect budget) — this
  investigation needed the distinction and an operator will too. `crates/ipc/src/error.rs`
  `IpcError::DaemonUnavailable` gained `why`, set at each of its four sites.

## Siblings swept

- `wake_ready_pollers` (the step) already wakes; `run_until_idle` goes through `step`. No other caller
  asks a poller's `ready`.
- The doorbell thread's own wait (`crates/server/src/doorbell.rs`) compares against the value it last
  acted on, so a ring during its kick loop is not lost there.
- `RESTART_RETRIES` in `crates/cli/tests/cli.rs` (the `--json` flow's tolerance for exit 3) stays: the
  anchor really does restart a daemon whose first heartbeat lapses, and that window is real; but the
  "one restart" attribution was doing double duty for this bug.
