# A refused sleep completed at once, read as elapsed time (AUD-29-39)

## Description

- **The audit's reproduction.** A one-shard simulation has one timer slot, and two tasks each
  `sleep(10_000)`. The first slept 10,000 ns; the second completed after 0 ns.
- **The code.** `Sleep::poll` mapped every `arm_timer` error to `Poll::Ready(())`, including timer
  exhaustion and a foreign waker. So under pressure a periodic loop turned into a busy loop, and a race
  against a deadline read the refusal as the deadline passing.

## Root cause

- **The output could not carry a refusal.** `Sleep::Output` was `()`, so every outcome had to be
  "elapsed".
- **Every race repeated the mistake.** The hand-rolled deadline races (swim, the cluster's timed request,
  the endpoint's receive, DNS, cross-shard calls, fleet discovery and the probe period) all tested the
  timer with `Poll::is_ready()`.

## Fix

- **`Sleep::Output = Result<(), RtError>`.**
  - It completes `Ok` no earlier than its deadline.
  - Off a shard, or with a waker that is not a task's, it is refused `NotOnShardThread`.
- **A full wheel.** The task is queued by slot (`ShardContext::wait_for_timer`; at most once per slot, so
  the queue is bounded by the arena), and each freed timer wakes one queued task (a fire or a cancel,
  `wake_timer_waiters`). The next poll arms the sleep, or completes it if the deadline passed meanwhile.
  This is counted as `Counters::timer_waits`.
- **A deadline past `u64`.** It saturates and never fires; its timer is released when the future drops.
- **One combinator for races.** `futures::within(ns, work) -> Result<Option<Output>, RtError>` keeps the
  work, the deadline and the refusal apart. It replaced the hand-rolled races.
- **Callers.** Every call site handles the refusal:
  - `DispatchWait::keep_waiting` stops waiting;
  - the timed request fails the exchange like a transport error;
  - the swim probe fails with `EndpointError::Io`;
  - discovery ends with `DiscoveryFault::Unbounded` (its own counter);
  - the probe period waits for traffic alone (`fleet.probe.period_unbounded`);
  - bounded polls return their "gave up" result;
  - perpetual server loops use `daemon::pace`, which counts `runtime.sleep_refused`, logs once and stops
    the loop without spinning;
  - tests unwrap, and test races map `Result::unwrap` before `is_ready`.

## Evidence

- **`crates/rt/tests/timers.rs`** runs on the simulated clock and is in CI's Miri lane:
  - `a_sleep_facing_a_full_wheel_waits_rather_than_completing_early`: red on the old behaviour ("slept 0 ns
    of 10000"), green now, with `timer_waits ≥ 1`;
  - `a_cancelled_sleep_hands_its_timer_to_the_waiter`;
  - `a_sleep_polled_off_a_shard_is_refused`;
  - `a_deadline_past_the_clock_never_fires_and_its_timer_returns_on_drop`;
  - `shutdown_ends_a_sleep_waiting_for_a_timer`;
  - `within_tells_the_work_the_deadline_and_a_refusal_apart`.
- **Found while writing them.** With one timer, a `within` whose work holds the only timer cannot arm its
  own deadline until a timer frees. Under exhaustion a deadline is therefore enforced late, never early.
  This is stated at `within`, and the server's one-timer-per-task derivation makes it an overload
  tripwire.
