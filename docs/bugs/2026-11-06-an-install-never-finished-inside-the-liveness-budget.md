# An install never finished inside the liveness budget, and a partitioned holder held up the others

**Found** 2026-11-06, raft-chaos, by `candidates_partitioned_in_turn_mid_stream_lose_nothing`
(`crates/cli/tests/live_chaos.rs`; macOS arm64, debug, load 45–50), after the session leak was fixed
(`2026-11-06-a-call-to-its-own-shard-dropped-its-future-and-a-live-owner-lost-its-sessions.md`).

Two defects showed up in the same flow:
- **A livelock.** With the leak fixed, 1 of 7 runs still failed Convergence: a healed candidate stayed at head 201 while
  the owner reached 239. The next run, with install counters added, caught the cause. Both candidates held the owner's
  head (735, same epoch and identity), yet the owner's durable position stayed at 732 and the stream's next write was
  never answered. Meanwhile the owner had sent `fleet.live.install_sent` 1,391 installs, 2,060,874,419 bytes in all
  (1.48 MB each), and counted 1,376 of them `install_unanswered`.
- **A latency defect.** In every passing run, writes made while one candidate was stopped took p99 0.37–1.4 s and p999
  up to 3.9 s. The other candidate answers in milliseconds.

**Root cause.**
1. **An install is the whole volume, sent under the liveness budget (1 s) a small entry gets.** A partition leaves the
   candidate behind the owner's retained tail, so it must be installed. Over a session the partition and the load had
   left at an initial congestion window (18 KB) and a 100–240 ms smoothed RTT, 1.5 MB took far longer than 1 s.
   - The holder received the install and applied it.
   - But the exchange gave up before the reply came. The owner counted the install unanswered, dropped the link, and
     dialed it afresh, at an initial window again.
   - The holder stayed owed an install (`Shipper::pending` answers `Install` until a reply puts it on the chain), so the
     whole volume was sent again.
   - With both candidates owed installs, no write could become durable.
2. **The replies were returned to the dispatch only at its end.** `send_from_control` folded every reply after its last
   send, so a dispatch that outlived its own deadline lost even the replies that did come.
3. **One dispatch task sent to each holder in turn.** A stopped holder's session borrow and exchange, each up to the
   liveness budget, came before the healthy holder's send. So every write waited them out, against §4.10's "each
   holder's offer and transfer progress independently, with no wait on the slowest offer".

**Edits proposed** (`crates/server/src/live_ship.rs`, `crates/db/src/live.rs`; on `agents/raft-chaos-dispatch-wip`, not yet on the work branch):
- `exchange_budget`: a message's budget is the liveness budget plus `bytes × smoothed RTT / congestion window` on the
  session it rides (the probe timeout stands in before any RTT sample). An entry gets about nothing more; an install
  gets its transfer.
- `send_from_control` folds each message's replies on the owner shard as soon as its exchange ends
  (`xshard::run_on_counted`). Nothing a holder answered is lost to the dispatch's deadline.
- `dispatch` runs one task a holder, each sending that holder's messages in order.
- Counters: `fleet.live.install_sent`, `fleet.live.install_bytes`, `fleet.live.install_unanswered`.

**Not closed by these edits.** Runs 1–5 after them passed: 266–369 writes each, all acknowledged, converged, p99
0.37–0.65 s. Runs 6, 7 and 8 failed Convergence at load 42–61: a candidate stayed 21–38 positions behind the owner's
head. Run 8 logged every install exchange (a temporary line, not on this branch), 425 in all:
- about 400 found no session: `borrow_session` waited out its liveness budget and came back empty, because the
  holder's one session was out on loan to another of its tasks, each dispatch spawning one;
- those that had a session were answered in 1.5 ms to 2.6 s, one in 21.9 s.

So the remaining defect is many installs racing for one holder's session, each a fresh copy of the volume. One install
in flight per holder, and a holder's sends queued on one task instead of a task per dispatch, are the next things to
try. Owner: live-durability (the cross-shard dispatch round).

**Tests.** The rolling-partition flow across real processes (`crates/cli/tests/live_chaos.rs`) is the failing test.
`a_laggard_is_sent_its_entries_while_they_cost_no_more_than_an_install` (`crates/db/tests/live.rs`) holds the byte
bound, and the nemesis model passes with it (10 of 10).
