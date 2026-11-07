# A kept forward waited for another request (2026-11-07, §4.3 "admission limit", §4.7 "Wake strategy")

Found by agents/loadtest-scale diagnosing the 10,000-client rung, which never finished (GAPS, the load-test
blockquote).

## Description

A client's request for a volume another shard owns is forwarded to that owner over its bounded control channel. A
forward the full channel refuses is kept in `pending_forwards` and sent again at the start of the next serve round:
"backpressure, never a drop: kept and retried next round".

The serve loop runs a round only when there is work. It announces idle and idles until a poller says a client ring
holds a request or a deferred reply waits (`state::ring_ready_in`). A kept forward is neither, and when the owner
drains its channel it tells no one. So a shard whose clients were all waiting on kept forwards slept, with the
forwards kept, until some other request came to one of its rings. A lone client's forward waited forever.

At 10,000 clients over one green, every shard forwards to the green's one owner. The owner's channel stays near full,
so this is the steady state: requests waited 10 to 42 s while the daemon idled.

## Measured

- The 10k rung on the quiet Studio (A-238, release, kernel/, 32 clients a connection, 3 rounds, `--think-us 0`,
  2026-11-07):
  - It did not finish in 1,800 s, and the daemon ran 5 min 22 s of CPU in those 30 minutes.
  - The control shard logged its beat task 918,331 ms late with 1 poll in the gap.
- The same rung on this Mac (W2, release without LTO, bounded 900 s, samples every 10 s):
  - Progress stopped for minutes at a time: submits 49,750 at 461 s and 49,720 at 542 s, with 1.6 s of daemon CPU
    in those 81 s.
  - Requests in flight were 1 to 42 s old.
  - The control shard's beat task was 8 to 20 s late, with about 750 polls in each gap.
- `crates/server/tests/forward_retry.rs` (`a_forward_kept_for_a_full_channel_is_sent_once_the_channel_has_room`): a
  two-shard daemon, the owner held while its channel is filled, one forwarded `Status`, then nothing more.
  - Red with the fix reverted in place, on both Linux runners (CI run 42374532065): "waited 1.000000661s: None".
  - Green on both, in the same run: answered.
- The local 10k on the fixed build started faster (10,674 submits by 63 s against 3,848 by 134 s), then stalled
  again. A `sample` of the daemon during that stall put the green's owner shard busy (5 of 1,201 samples idle) in
  `Green::rebase → decide → intervening`, and the other shards idle. That is a second wall, the verdict's walk of
  every delta since a work's base, in its own record.

## Root cause

The doorbell protocol covers a client's request and a deferred reply. A forward kept for a full channel has no wake
of its own: nothing rings when the owner's channel gets room.

## Fix

While a forward is kept, the serve loop idles at most its idle window, the shard's measured wake cost ×
`IDLE_WINDOW_RATIO`, about 4 ms on this Mac. It then serves a round, which sends the forward again. With no forward
kept, it idles as before, until a ring wakes it. That bounds a kept forward's wait at one idle window past the
channel's room, at a cost of one timer a window while forwards wait.

## Impact

Under fan-in to one owner shard (one green, many clients spread over the shards), throughput collapsed to the rate at
which unrelated requests happened to wake the forwarding shards. A client whose shard had no other traffic could wait
indefinitely. Nothing was lost or refused, only stalled, which is why every oracle passed.

## Siblings, reported, not fixed here

- The owner could tell the shards waiting on it when its channel drains. That is an exact wake instead of a bounded
  poll, and is owed if a timer a window shows in a profile.
- Other kept queues that wait on another shard's channel should get the same review: a deferred reply's retry is
  covered by `ring_ready_in`, and the scatter's parts wait by a clock of their own.
