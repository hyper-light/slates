# A forward its owner never answered stayed open on a busy session (2026-11-07, §4.8 "Lookup")

Found by agents/fixtures on its 1,000-client `f = 1` primary-kill rung, on 6fe4b30d merged with fixtures' 8be0e745
(adacachy, `~/.cache/slates-agents-scratch/fixtures/rungs8/r1k-kill0.stall`, dump at 86 s). The forward hop counters
placed it.

## Description

A forward lane forwarder gives up its batch once none of its forwards has made progress for its stall budget (a
liveness budget plus the path's tail). Progress was any byte the session consumed while one of the forwards was open
(`mark_progress`). So a forward whose owner never answered stayed alive as long as the session carried other
exchanges' replies, and nothing ended it:
- its batch was never given up;
- so `resolve_and_forward` never resent it, and never answered `OwnerUnreachable` past the failover SLO;
- and its client, which waits for a forwarded verb while its daemon lives, waited for good.

## Measured

- fixtures' rung. After the primary's kill and restart, the fleet split into two membership views that never merged
  (reported to the supervisor for membership's owner, not fixed here). The restarted primary's owner shard deferred
  205 submit acceptances and resolved none. On client-0:
  - `fleet.forward.client.control_started` 39,493 against `resolved` 39,195;
  - `fleet.forward.begun` 39,903 against `batch_done` 39,605 and `batch_given_up` 0.
  So 298 forwards stayed open in the lane, on a session that kept carrying the harness's 2.18 million
  `LeaseUnconfirmed` replies.
- `crates/server/tests/fleet.rs`, `a_silent_forward_gives_up_while_its_session_carries_others`: a leaves one
  volume's forwarded verbs unanswered (`Daemon::inject_forward_silence`) while b reads another volume on a over the
  same session for two and a half liveness budgets.
  - Red without the fix on both Linux runners, four of four (CI run 42425119057): `batch_given_up` 0 while the session
    carried 2,152 to 4,604 other replies.
  - Green with it (runs 42425123591, 42427500000).
  - A first form held a's owner shard instead. a answered the held verb with a refusal at its own cross-shard budget,
    so b's forward was never silent, and the test failed with the fix and without (run 42423104678). It was replaced.

## Fix

Landed in 2bdaa7c0, the commit whose summary names A-245's docs: `git add -A` took the fix in with them (reported to
the supervisor, 2026-11-07). The test that holds it came in 98d7b964.

- A forward's progress is bytes of its own reply read: `Endpoint::reply_bytes_read(exchange)` (`slates-transport`),
  compared with the bytes read when its progress was last marked (`Ticket::reply_bytes`, `mark_ticket_progress`).
- Bytes the session consumed for other exchanges are not its progress.

## Impact

A forward to an owner that is alive but never answers it is now given up at its stall budget and sent again, and past
the failover SLO its client is answered `OwnerUnreachable`. It no longer waits for good. The owner-side cause in the
rung, the membership split that left the owner unable to commit, is not fixed here.
