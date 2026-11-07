# A call to its own shard dropped its future, and a live owner lost its sessions for good

**Found** 2026-11-06, raft-chaos, by `candidates_partitioned_in_turn_mid_stream_lose_nothing`
(`crates/cli/tests/live_chaos.rs`; macOS arm64, debug, load 45–83). Writes stream to a live-shipped volume while each of
its two candidates is stopped (SIGSTOP) for 2 s in turn. Before the fix, 3 of 5 runs failed:
- twice a write was never answered: one stream hung over 20 minutes, until the run's `timeout`;
- once the healed candidate stayed at head 45 while the owner went on to head 130, and writes took 1.01 s at the median
  and 5.5 s at p99.

The run that was caught with its state printed shows the owner at head 61, durable 58, and both candidates healthy at
58. Both of the owner's record sessions were `lent=true` and never came back: `fleet.live.no_session` 129,
`fleet.live.unanswered` 65. The owner could send to no candidate, so nothing past 58 could become durable, and the
write at 61 waited for good.

**Root cause.** `crate::xshard::task_within` has two paths:
- **A call to another shard** spawns the future as a task there. The task runs to its end, and only a late result is
  dropped.
- **A call to its own shard** ran the future in place under `within`, which drops it at the deadline.

`live_ship::send_from_control` runs each dispatch as a `task_within` call to the control shard. When the volume's owner
shard is the control shard (shard 0 here, one volume in two), the dispatch is a same-shard call, so it was dropped at
its deadline of twice the liveness budget. A dispatch sends to each holder in turn, waiting up to a liveness budget to
borrow a holder's session and up to another for its reply, so a partitioned holder pushes a dispatch past that deadline.

Dropped mid-`exchange`, the dispatch dropped the exchange's reply channel. The exchange's detached children still held
the borrowed endpoints. When they finished, their replies had nowhere to go, so the endpoints were dropped. Nothing
returned those sessions to the pool and nothing dropped their links, so the session table kept them lent for good.
`borrow_session` then waited out its budget on every later send and found nothing (`no_session`). This breaks §4.3's
rule that dropping a future leaks nothing.

**Impact.** Opt-in live-shipped volumes on a fleet. Once a dispatch on the control shard outlives its deadline (a holder
partitioned or stalled for about two liveness budgets), the owner may never send to that holder again. No acknowledged
write was lost: every write answered was durable, and NoLoss and Agreement held in every run. But the volume's writers
stall without end, and the log rests on fewer candidates than it should.

**Edits.**
- `crates/server/src/xshard.rs`: a same-shard `task_within` spawns its future as a task of that shard, which carries its
  result home like any other (`spawn_carrying_home`). It runs to its end, and only the result is awaited within the
  deadline: one behaviour whichever shard a volume landed on. The other callers (`reap`, `anchor_hold_windows`, the
  forwarded verb's told-wait in `verbs`, `nfs::serve_remote`) are each bounded inside and already ran to their end
  across shards.
- Failing test first: `xshard::tests::a_call_to_its_own_shard_runs_its_future_to_the_end_past_the_deadline` (red before
  the edit: the future never ended; green after).
- Across real processes after the edit: 6 of 7 runs of the rolling partition pass and none hang (2026-11-06, debug,
  load 30-48; the seventh is the
  separate install defect in GAPS).

**Sibling sweep: every `task_within` caller is bounded inside, so running each to its end is safe.** There are five:
- `live_ship::send_from_control`: each borrow and exchange carries its own budget;
- `reap::remove_on_owners`: `direct_map::write_back_held` is sliced a copy slice a turn;
- `anchor_hold_windows::exchange`: wrapped in `futures::within(budget_ns, …)`;
- the forwarded verb's told-wait in `verbs`: `told::wait`, bounded by `TOLD_WAIT_NS`;
- `nfs::serve_remote`: `stable_once_durable` within the liveness budget, then `told::wait`.

Each already ran to its end when it crossed shards; now it does on its own shard too. `call_within` runs a closure in
one turn and holds nothing across a turn, so it cannot strand anything. The one other live `exchange`, the takeover's
`live_ship::fetch`, is awaited by `fleet::materialize` under no deadline that drops it.

**What the fix does not close.** With it, a partitioned candidate still livelocked on installs in 1 of 7 runs (record
`2026-11-06-an-install-never-finished-inside-the-liveness-budget.md`; owner: live-durability, the dispatch round).
