# A cross-shard wait lost a starved shard's answer (2026-11-06, §4.3, §4.14, R3 class A)

## Description

A verb served on one shard that needs work done on another sends it there (`crate::xshard`) and waits for the answer.
About twenty such waits gave up after `LIVENESS_BUDGET_NS`, one second of wall time, whatever the other shard was doing.
On a busy machine, the normal case (Ada's rule), the target shard is often alive but kept off its core, running a
little at a time. Its answer then comes after the second. The caller had already moved on and refused the client
`Overloaded` (or `NotFound`), and the work went on to take effect anyway: a live shard's answer was lost, and the client
was told its request failed when it had not. A-228 already judges a shard's liveness in its own CPU time (A-65's rule),
so the daemon's watchdog called the shard alive while the verb's wait called it gone.

## Measurement

`crates/server/tests/cross_shard_wait.rs` on main `d61c3be2` (macOS arm64, M5 Max, 18 CPUs, 2026-11-06, load 26–33): a
client served on the second shard bootstraps the daemon while the control shard runs one poll of 1.5 s that is on the
CPU a fifth of the time (10 ms bursts, 40 ms off). The bootstrap was refused `Overloaded { shard: 0 }` after
**1.003 s**.

## Root cause

The wait was a wall deadline (`xshard::within`, `futures::within(LIVENESS_BUDGET_NS, call)`), so it measured how the
machine scheduled the target thread, not whether the target was working.

## Edits

- `crates/server/src/xshard.rs`: a call carries its target. `within_shard_time` waits a wall window of the budget, then
  goes on while the target made CPU progress in the window just ended, for the part of the budget of CPU it has not yet
  run. A wedged target is given up after one whole window off the CPU, and a busy one that never answers once it has
  run the budget. `call_in_shard_time` and `task_in_shard_time` are `call_within` and `task_within` waited that way.
  `within`, `call_within` and `task_within` stay for waits bounded by the caller's own clock (a coordinator period, a
  client's remaining deadline).
- `crates/server/src/observe.rs`: `ShardTime` (A-65) is shared, and its next window is also given in nanoseconds for a
  wait on a shard's own timer.
- Each call family moves to the shard-time wait in its own commit, with its own failing-first test:
  - **The consensus verbs** (`verbs.rs` `consensus_on_control`: bootstrap and recovery, the operation on the control
    shard and its publication on every other). Measured green (same host, load 35–49): the starved bootstrap
    acknowledged after 1.52–1.68 s over four runs. A wedged control shard (a 3 s poll off the CPU) is still refused
    `Overloaded` after 1.000–1.004 s.

## Tests

- `a_bootstrap_whose_control_shard_is_starved_is_acknowledged_past_a_wall_window`: red on main, green with the fix.
- `a_bootstrap_whose_control_shard_is_wedged_is_refused_after_a_window`: the guard on the other side, green on both.

Both use a client whose reply deadline is ten windows, so they measure the daemon's wait and not the client's. The
client's own deadline is the liveness window too; deriving that window is A-232 (classes C and D).

## Siblings

Still waiting on the wall second, each its own commit: the forwarded verbs (`verbs.rs` `serve_forward`,
`await_deferred_completion`), the consumer verbs (`revoke_everywhere`, attest), the NFS bridge's calls (`nfs.rs`), the
direct map's delivery (`direct_map.rs`) and a peer's remote reap (`remote_attach.rs` `serve_unforwardable`). The
delegation's recall (`delegation.rs`, `callback::call_on_slot`) also waits a liveness budget, but on the NFS client
across the network, not on a shard: that is class B (the transport's round-trip estimate).
