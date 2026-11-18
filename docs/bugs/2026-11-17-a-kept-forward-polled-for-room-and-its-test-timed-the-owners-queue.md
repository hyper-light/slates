# A kept forward polled for room, and its test timed the owner's queue (2026-11-17, §4.3 "admission limit", §4.7 "Wake strategy")

Goal 239 of the supervisor's stack: "A forward kept for a full channel is always re-sent once the channel has room, a
lost wake (perf). Main fails `a_forward_kept_for_a_full_channel_is_sent_once_the_channel_has_room` 8 of 30 at load
~35." The gate runs on this Mac (logs `b8`, `b12r`, 2026-11-17) failed the same way: "the kept forward was answered
within 1s of the hold's end (waited 1.008515417s): None", and 1.021138209 s.

## Description

A client's request for a volume another shard owns is forwarded over the owner's bounded control channel. A forward
the full channel refuses is kept and sent again later (§4.3: "backpressure, never a drop"). Since
`2026-11-07-a-kept-forward-waited-for-another-request.md`, the serve loop, while it kept a forward, idled at most one
idle window and then served a round that sent it again. That record left the exact wake owed: "the owner could tell
the shards waiting on it when its channel drains".

`crates/server/tests/forward_retry.rs` holds the owner shard, fills its control channel with no-op tasks, sends one
`Status` whose forward is kept, ends the hold, and required the answer within the liveness budget (1 s) of the hold's
end. Its doc called the owner's drain of the flood "microseconds".

Two defects:
- The test's budget counted the owner's drain of the flood the test itself had queued ahead of the forward. The
  owner runs its channel in the order it took it, so the forward runs after the flood's last task. The flood is a
  whole channel: the shard's admission limit, 41,125 tasks on this Mac. Draining it is the machine's speed, not the
  forward's wait.
- The kept forward's resend waited for a timer. The idle window is the shard's wake estimate × `IDLE_WINDOW_RATIO`
  (100). The estimate is the mean wake the shard measures, with no cap, so the window grows with the machine's load
  (a 10 ms wake makes it 1 s), and the loop armed a timer every window while a forward waited.

## Measured

This Mac (adas-macbook-pro-2, 18 CPUs), 2026-11-17 17:36 to 17:38, 30 runs of the test built with a timeline
(`eprintln` at the hold, the flood, the keep, each resend, the flood's first and last tasks, the forward's run, its
delivery and the answer), each run beside 36 spinning threads (2 × the CPUs) on a machine shared with other agents'
builds. The test's poll was stretched to 20 s so every run finished. Logs: `~/.cache/g239-diag/run-{1..30}.log`, summary
`~/.cache/g239-diag/summary.txt`.

| Quantity | Min | Max |
|---|---|---|
| Tasks the flood queued | 41,125 | 41,125 |
| Time to queue the flood | 5.1 ms | 20.9 ms |
| Idle window while the forward was kept | 0.30 ms | 21.29 ms |
| Wake estimate | 2,987 ns | 212,941 ns |
| Resend, from the test's view of the hold's end | -8.7 ms | +21.0 ms |
| Flood's last task, from the hold's end | 63.5 ms | 4,331.8 ms |
| Forward's run after the flood's last task | 24 µs | 871 µs |
| Answer, from the hold's end | 64.4 ms | 4,332.5 ms |

- The answer took 64 to 565 ms in 29 runs and 4,332.5 ms in run 26. Run 26 was the one past the 1 s budget, and in
  it the forward was re-sent 0.2 ms after the hold's end. The answer waited for the drain alone, at 105 µs a task.
- In every run the forward ran right after the flood's last task, 24 to 871 µs later. The resend never decided the
  answer: in the two runs with a 21 ms window (1 and 27), the resend came 15.2 and 21.0 ms in, and the flood ran
  until 63.5 and 149.6 ms.
- The supervisor's 8 of 30 at load ~35 failed with the same message, an answer not in 1 s after the hold's end. Those
  runs kept no timeline; in these 30 the resend always came within 21 ms and the lateness was the drain's.
- An earlier bisect of the same failure (`fretry-bisect.log`, 2026-11-08, load 74 to 82) was the drain too. Since
  98d7b964 a laptop's control shard was sized for a forward lane it cannot have, a channel of 1,371,635 tasks, and
  9a996608 passed 3 of 3 where 2ae4e832 failed 3 of 3. The laptop's lane has been zero since 2026-11-08
  (`DaemonConfig::control_shard_capacity`).

## Root cause

1. The test measured the owner's queue, not the forward. The forward is queued behind the whole flood, so its answer
   comes at the drain's end plus one task and one reply home. The budget held only while the drain took under 1 s.
2. No wake existed for a full channel's room. The owner drained its channel without a word to the shards waiting on
   it, so the waiter polled once per idle window. The window was 0.3 to 21 ms in these runs, and nothing bounds it
   but the measured wake.

## Fix

The owner's drain now tells the shards that asked, and the test measures the forward alone. This is A-279.

- `slates_rt::room` (new): the room wake. A sender refused `ControlFull` asks the target to tell it when its channel
  has room (`ShardContext::want_room`, `futures::want_room`). It marks its shard in the target's `room_wanted` set,
  fences `SeqCst`, and tries once more. The target, after each drain that took a message, fences `SeqCst` and tells
  every shard its set names (`registry::give_room`). Telling marks the target in the asker's `room_given` set, fences
  `SeqCst`, and publishes the asker's control flag, kicking it if parked, which is a control message's own
  publication. The asker's drain then wakes its tasks waiting on that target. Each side writes, fences, then reads
  what the other wrote, so either the second try finds the room or the drain finds the ask. An exiting shard tells
  every asker, and an ask after the exit is refused `ShardGone`. The cost:
  - the sender pays only on a refusal: a map insert on its own shard, two read-modify-writes and a fence;
  - the target pays a fence and a load per drain that took a message, and a fence per tell.
- The tell's fence was found in review, against the C++ memory model. A drain that stops at its batch bound re-arms
  the control flag with a plain store, which ends a publication's release sequence. A tell published between that
  drain's take and its re-arm is then read by the next take through the re-arm, which acquires nothing from the
  teller, and the relaxed look at `room_given` may miss the mark. The re-arming drain fences after it, and the fence
  in `give_room` answers that one, so the next look finds the mark. ARMv8 and x86 order these stores in hardware; the
  model does not. A message needs no such fence: the channel claims its slot with a `SeqCst` compare-and-swap. Loom
  did not reach the miss: the re-arm model passed without the fence too (81 interleavings), so the fence rests on the
  standard's fence rules (C++20 `[atomics.order]` p4), not on the model.
- Three loom models in `room.rs` cover the asking half against a draining target, the telling half against a parking
  shard, and the telling half against a shard re-arming its flag. `crates/rt/tests/room.rs` holds a two-shard runtime
  with a sender that has no timer at all, so only the drain's word can move it; `crates/rt/tests/room_gone.rs` asks
  a stopped shard and an id no shard holds, both refused `ShardGone` at once.
- `crates/server/src/daemon.rs` (`serve_loop`): the timed idle while a forward is kept is gone. Before idling, the loop
  calls `verbs::ask_room_for_kept_forwards`, which asks the first kept forward's owner for room and tries the kept
  forwards once more. The loop idles with no timer only if that try is refused too. An owner that has left its loop
  has its kept forwards refused `NotFound`, as a send to a gone shard always was (`refuse_unsent_forward`).
- `crates/server/tests/forward_retry.rs`: the flood's tasks count their runs
  (`Daemon::flood_control_channel_counting`). Each step of the owner through the flood restarts the answer's wait,
  so the answer is owed within the liveness budget of the owner's last progress, not of the hold's end. The client's
  shard's `room_waits` and `room_wakes` must both have moved (the non-vacuity check). With no timer left, a lost word
  leaves the forward kept for good and the test fails.

## Tests, after the fix

On Adas-MacBook-Pro-3 (15 CPUs, macOS arm64), 2026-11-18, one run begun 02:24 UTC, at load 59 at its start and 21 / 37 / 35
(1, 5 and 15 minutes) at the end, other agents' builds sharing the machine:
- `forward_retry.rs` 30 of 30, each run its own process, and 10 of 10 more after the tell's fence was added (02:34 UTC,
  load 21 to 33);
- the first two room models under `--cfg loom`: 24 and 37 interleavings (preemption bound 2), every one ending with the
  message sent or the shard told, and both endings of each reached (the third, the re-arm's, 75 interleavings);
- `cargo test -p slates-rt` whole (`room.rs` and `room_gone.rs` among it), `cargo test -p slates-server --lib` (245),
  and `ack_release.rs` (6 of 6), whose three floods go through the same hook;
- red-checked in a copy of the tree on the same runner, 02:29 UTC at load 36 to 38: with the drain's tell removed
  (`take_each(|origin| { let _ = origin; })`), `room.rs` failed "the sender's message landed within 10s", and
  `forward_retry.rs` failed "the kept forward was answered within 1s of the owner's last progress through the 15425
  tasks queued ahead of it (15425 ran; 1.185224583s after the hold's end, 1.006196709s after the last progress):
  None". The runner's channel held 15,425 tasks, and its drain took 0.18 s.

## Impact

- The test failed 8 of 30 runs at load ~35 on main with nothing wrong in the forward path. That is noise in every gate
  run and a false lead: the goal named it a lost wake.
- In the product, a kept forward reached its owner up to one idle window after the room, a window that grows with
  the measured wake. Under fan-in to one owner shard (10,000 clients over one green), every shard waiting on that
  owner armed a timer every window and resent on it. Now each is told by the drain that makes room.

## Siblings, reported, not fixed here

- Under a saturated owner, the tell has a cost the timer did not. Busy client shards retry every serve round and take
  each batch of room first. An idle client shard whose resend keeps losing is told at every drain, so it parks, is
  kicked and wakes about once per its own wake latency, where the timer woke it once per idle window (100 wakes).
  The owner pays one kick per such shard per tell. Nothing here measured it; the 10k fan-in rung should count kicks
  and CPU. Behind it is a fairness question the timer had too: a full channel serves whoever sends first.
- `retry_forwards` stops at the first refused forward, so a forward kept for a full owner A holds back every later
  kept forward, including those to an owner B with room. With the wake, the shard asks only A for room. Whether
  forwards to different owners must keep their order is a §4.3 question for its owner.
- `crates/server/tests/ack_release.rs` fills a control channel with the same hook three times. Its scatter tests hold
  the other shard until the answer is in (`hold_until_answered`) and bound the answer by the client's reply deadline
  (`check_typed_refusal`), and its release test waits through `client.call`. None budgets the owner's drain.
- Two other senders that meet `ControlFull` still poll for room on a timer, every `REPLY_PACE_NS` (a tenth of a
  coordinator period) until a deadline: a scatter's part (`xshard::given_room_within`) and a result carried home
  (`xshard::given_room`, `send_back`'s wait), both since `2026-11-06-a-wait-for-room-counted-tries-not-time.md`.
  Each could ask for room (`futures::want_room`) and wait for the word within its deadline instead.
