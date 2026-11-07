# A forward refused at admission left its client waiting (2026-11-07, §4.9 "Exactly-once", §4.3 "admission")

Found by agents/fixtures on its 1,000-client `f = 1` replica-kill rung (adacachy; 9c1d8b27 merged with fixtures'
8be0e745). The forward hop counters of 9c1d8b27 placed it. The same stall signature appeared earlier in the r3k clean
(245 s), the 10k clean (125 s) and the r3k replica kill (125 s), all before those counters, so they are probably the
same bug.

## Description

A shard whose task arena is full refuses a spawn at admission: it answers the spawn's receipt `Refused` and drops the
future unrun (`slates_rt` `handle_control`, `SlabFull`). Its only trace is the shard's own `tasks_refused` count.
`registry::send_control` reports only that the control channel took the message.

The spawns that owe a client its reply checked only the channel: the client's forward to the control shard
(`forward_to_owner`), the volume shard's forward (`forward_for_local_client`), the same-node forward to the owner
shard (`send_forward`) and region promotion. Refused at admission, the forward vanished. Its client waits for a
forwarded verb while its daemon lives (`slates_client::defers_reply`), so it waited for good: no reply, no refusal,
nothing deferred, every daemon idle.

## Measured

- fixtures' rung, the stall at 95 s (worker 0: 499 clients, 4,232 submits; worker 1: 414 / 3,579). The status dump at
  236 s (`rungs7/r1k-kill1.stall`), client-0 summed over its 7 shards:
  - `fleet.forward.client.to_control` 85,750
  - `control_started` 85,663, so 87 never started
  - `resolved` and `delivered_home` 85,663
  - the lane balanced: `fleet.forward.begun` 85,665 against `batch_done` 85,663 plus `batch_given_up` 7
  - client-0's control shard: `tasks_refused` 117
- `crates/server/tests/fleet.rs`, `a_forward_refused_at_admission_answers_its_client`: three nodes; fill b's control
  shard's arena with tasks that wait until cancelled, then read a volume a owns through b's client.
  - Red on both Linux runners, two of two each (CI run 42417663905): no answer within a liveness budget.
  - Green with the fix (run 42417669342): answered `Overloaded`, counted, and served once the arena is freed.

## Root cause

- **Silent loss:** admission is reported only to a receipt, and these spawns read none (an async spawner cannot block
  on the receipt's channel).
- **Why the arena fills there:** `tasks_per_shard` is `clients_per_shard × TASKS_PER_CLIENT (2) + LOOP_TASKS_PER_SHARD`
  plus the fleet's loops. That sizes a shard for its own clients with one request each. But the control shard runs the
  forwards of every shard's clients, and an async client holds up to its ring's slots. The forward lane already admits
  `clients_per_shard × slots × shards`. After a replica kill, forwards stay alive in their resend loops, so the
  control shard's arena runs out.

## Fix

- `xshard::AnswerUnstarted`: a guard moved into each such task and disarmed at its first poll. Dropped unrun on the
  task's shard, it answers the client `Overloaded`, retryable under its own id since the verb never ran, and is never
  recorded as the request's completion. It is delivered at once when that shard is the client's, else sent home on a
  task with its own guard. Counted `xshard.unstarted_answered`, or `xshard.unstarted_unanswered` when even the way home
  is refused.
- `xshard::send_owing`: sends such a task. If the channel refuses it, the sender answers its client on its existing
  path and the guard stands down, so a client on the control shard is never answered twice.
- Applied to `forward_to_owner`, `forward_for_local_client`, `send_forward` and `promote_region_on_root`.

## Not fixed here

- The arena's size. A full control shard now refuses forwards `Overloaded`, and the clients resend them paced; it no
  longer stalls them. Sizing the control shard for every shard's in-flight forwards is a memory decision, raised with
  the supervisor.
- Tasks that carry a reply home (`send_back`'s): refused at the origin's admission, the reply is lost the same way. No
  such loss was measured here (`delivered_home` balanced), but they share the mechanism.

## Also measured

On the same build with the harness's paced resends, the replica-kill rung's primary counted
`lease.refused.unconfirmed` up to 726 and then held flat for 15 minutes. The lease reconfirms. The 18.4 million
`LeaseUnconfirmed` resends of the earlier unpaced primary-kill rung were starving it.
