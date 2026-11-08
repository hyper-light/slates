# The control shard could not run what its forward lane admitted (2026-11-07, §4.3 "Task arena full", §4.8 "Lookup")

The supervisor's order of 2026-11-07, after
`2026-11-07-a-forward-refused-at-admission-left-its-client-waiting.md`: a lane that admits work its shard's arena cannot
run is the defect. Size the control shard from what its lane admits, and fix the reply carriers the same way.

## Description

Every shard's task arena, its control channel (bounded by the same `tasks_per_shard`) and its timers were sized
alike: `clients_per_shard × TASKS_PER_CLIENT (2) + LOOP_TASKS_PER_SHARD`, plus the fleet's own tasks. That is a shard's
own clients with one request each.
- The control shard runs every forward of every shard's clients to another node, each a task there for its whole
  round.
- Its forward lane admits `clients_per_shard × slots × shards` of them: 164,864 on CI's runners, against an arena and
  channel of 10,314.

So under a burst or a kill, the channel or the arena refused forwards the lane would have admitted. The two paths
differed:
- `forward_to_owner` and `forward_for_local_client` answered a full channel `HomedElsewhere`, which tells the client
  its volume lives in another region; the verb had not run, and it was only a full channel.
- A spawn refused at admission dropped its future, which 6fe4b30d's guard answers `Overloaded`.

The tasks carrying a reply home share the mechanism. A reply task refused at the client's shard's admission was
dropped unrun and its reply lost. That covers `send_back`'s callers (the forward replies; the listing, grants, status
and acknowledgement scatters' parts) and seven synchronous deliverers (FUSE, WinFsp, live ship, landing, the merge
service's deferred acceptance, image builds, the lease wait).

## Measured

`crates/server/tests/fleet.rs`, `a_silent_owner_leaves_no_forward_the_lane_admitted_unrun`: three nodes; a's owner
shard of one volume held silent for two liveness budgets; through b, a full ring of reads from each of enough clients
to put twice the old arena's forwards in flight on b's control shard (645 to 647 clients × 32, about 20,700, within
the lane's bound).
- Before (CI run 42423846590, both Linux runners):
  - b's control channel refused 10,198 to 10,298 forwards (`fleet.forward.client.control_refused`);
  - each was answered `HomedElsewhere`.
- After (runs 42425123591, 42427500000):
  - b's control shard refused no spawn and its channel none;
  - every hop balanced, `fleet.forward.client.to_control` through `delivered_home` (20,641 and 20,705);
  - `deferred` was 0 on both shards.
- Replies late, not lost. At the failover SLO plus one liveness budget, 76 to 120 replies had not come, and every one
  came in the budget after (run 42426714842). At the SLO plus three, 38 to 98 had not (run 42427500000). They were
  always the first clients' last requests. Those clients are read first, so they alone are measured from the hold's
  end. A forward's resend loop is bounded by the SLO and its last forward by a stall budget whose path tail this burst
  stretches on a 4-core runner. The test holds that nothing is lost, with twice the SLO as its backstop.

## Fix

- `slates_rt`: `Runtime::start_sized(config, &[ShardCapacity])` and `RuntimeConfig::for_shard`. A shard named in the
  list is built with its own task capacity, which sizes its arena, run queue and control channel, and its own timers.
  The rest use `config`. `RuntimeConfig` itself is unchanged.
- `DaemonConfig::control_shard_capacity()`: `tasks_per_shard + forward_lane_bound` and
  `timers_per_shard + forward_lane_bound`, logged at boot as `control_shard_capacity` and noted again after
  `with_fleet` raises the base. `forward_lane_bound` is the one formula the lane and the capacity read.
- A full control channel is answered `Overloaded { shard }` (retryable under the same id; the verb never ran), and
  `HomedElsewhere` only when the control shard is gone (`control_refusal`).
- `xshard::send_reply_home` (asynchronous) and `xshard::hand_reply_home` (synchronous, the channel's refusal returned
  for the caller to count as before): a reply's synchronous end runs on the client's shard whether its arena admits
  the task or refuses it at admission, counted `xshard.reply_run_unstarted`. Every reply carrier above uses one.

## Not fixed here

- A same-node owner shard receives forwards from every shard's clients too (`send_forward`) and is sized for its
  own. Refused at admission, those are answered `Overloaded` by 6fe4b30d's guard; they no longer stall.
- The fleet's own tasks are added to every shard's capacity though they run on the control shard alone.
