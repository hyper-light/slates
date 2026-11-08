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
- Replies late, not lost. Read client by client, the first clients' last replies came after every deadline tried
  (the SLO plus one budget, plus three, twice the SLO, 60 s), and each time all of them came on the next read (runs
  42426714842 to 42437055903). A probe at the first shortfall (run 42437675578) found:
  - none of the client's requests untaken on its request ring and none of its replies untaken on its reply ring;
  - nothing unwritten on b's other shard;
  - b's control shard too busy to run the probe's task within its budget.
  So the replies were still being produced on a saturated control shard. With about 20,700 forwards to an owner
  silent for two budgets, it drains slowly on a 4-core runner (the last reply 64 to 105 s after the hold): thousands of
  forwarders find the session lent and poll for it (`session_never_returned` about 5,000 to 6,000), and each resend
  begins a new forwarder. The test reads every client once, measuring the rate the control shard served the burst at,
  then gives each client still short one unbroken wait, timed, to a backstop derived from that rate: twice the time
  it needs to serve the whole burst, plus the failover SLO. It asserts none refused and none lost, and prints the
  arrivals.

## Fix

- `slates_rt`: `Runtime::start_sized(config, &[ShardCapacity])` and `RuntimeConfig::for_shard`. A shard named in the
  list is built with its own task capacity, which sizes its arena, run queue and control channel, and its own timers.
  The rest use `config`. `RuntimeConfig` itself is unchanged.
- `DaemonConfig::control_shard_capacity()`: `tasks_per_shard + forward_lane_bound` and
  `timers_per_shard + forward_lane_bound`, logged at boot as `control_shard_capacity` and noted again after
  `with_fleet` raises the base. `forward_lane_bound` is the one formula the lane and the capacity read.
- With no peers the lane's bound is zero, so a laptop's control shard is sized like its other shards (corrected
  2026-11-08). The first form added the bound on every daemon: a laptop's control shard took a channel of 1,371,635
  tasks on a Mac, for forwards that cannot exist, and `a_forward_kept_for_a_full_channel_is_sent_once_the_channel_has_room`,
  whose flood fills that channel, failed there 3 of 3 (it passed on CI's Linux and on GitHub's macOS runners, where the
  derived sizes are small). With the bound zero: 20 of 20 on the Studio, a flood of 30,845, answered in 44 to 88 ms.
- A full control channel is answered `Overloaded { shard }` (retryable under the same id; the verb never ran), and
  `HomedElsewhere` only when the control shard is gone (`control_refusal`).
- `xshard::send_reply_home` (asynchronous) and `xshard::hand_reply_home` (synchronous, the channel's refusal returned
  for the caller to count as before): a reply's synchronous end runs on the client's shard whether its arena admits
  the task or refuses it at admission, counted `xshard.reply_run_unstarted`. Every reply carrier above uses one.

## Not fixed here

- The control shard's own cost under a large in-flight set: a forwarder that finds the session lent polls for it at a
  tenth of a heartbeat, so thousands of them make the shard drain slowly (above). Measured, not fixed; it is the
  shared session GAPS owes ("Open — one session per peer is lent exclusively", its 2026-11-07 status).

- A same-node owner shard receives forwards from every shard's clients too (`send_forward`) and is sized for its
  own. Refused at admission, those are answered `Overloaded` by 6fe4b30d's guard; they no longer stall.
- The fleet's own tasks are added to every shard's capacity though they run on the control shard alone.
