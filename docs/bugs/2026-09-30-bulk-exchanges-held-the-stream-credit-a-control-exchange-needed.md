# Bulk exchanges held the stream credit a control exchange needed

**Date:** 2026-09-30. **Area:** `slates-transport` (§4.10a; the constrained-link design §5.3; RFC 9000
§4.6). **Found by:** the sibling sweep of
`docs/bugs/2026-09-30-bulk-spent-the-connection-credit-a-control-exchange-needed.md`.

## Description

Stream credit is by sequence (`MaxStreams`), so a stream past the peer's credit is sent only after every
earlier one. `begin` took the next sequence whatever the exchange's class, and up to a limit's worth could
wait past the credit.

Bulk exchanges begun first therefore held every sequence the credit covered, plus a limit's worth beyond
it. A control exchange begun after them was either refused `Backlogged` or waited for them to finish.

Red test: `a_control_exchange_is_not_stranded_behind_bulk_exchanges_holding_the_stream_credit`, with
twice a limit's worth of 8 KiB bulk exchanges on a 1 Mbit/s, 40 ms path, then one ping. The ping took
1,561 ms, with 16 bulk exchanges finished before it (seed 1).

## Fix

- **Admission by class.** `StreamSpace::admits(class)`: a control stream may wait past the credit up to the
  limit, as before. A stream of any other class opens only while its sequence leaves one slot of the
  credit per more urgent class, so a later, more urgent exchange's sequence is still covered.
- **Deferred binding.** `Endpoint::begin` returns an exchange id, not a stream id. An exchange the credit
  does not yet admit waits unsequenced in a queue bounded by the stream limit, less a slot per more
  urgent class. `bind_queued` binds waiting exchanges, the most urgent class first, whenever the credit
  admits them (at `begin` and at every `flush`). `take_reply`, `abandon`, `last_exchange` and the reply
  routing (`by_stream`) follow the exchange id. Callers used ids only opaquely.
- **The blocked report.** An exchange waiting unbound is not a stream past the credit, so the connection
  never reported itself blocked. A quiet peer, whose `MaxStreams` rides on its acknowledgements, then never
  sent the credit it had raised: the first run stalled with credit 17 for 15 finished streams.
  `set_streams_wanted` makes waiting exchanges count as blocked (`STREAMS_BLOCKED`, RFC 9000 §19.14).

## Tests

- The red test above now answers the ping within RTT, plus one queue drain, plus two packets (99 ms),
  while bulk exchanges are still running, on seeds 1–3.
- The test pings once the bulk flow has settled into the path (300 ms, as the head-of-line test does). A
  trace showed that a burst at session start loses its first packets (0, 1 and 4) to a queue still
  holding the handshake flight. That is the initial window meeting a four-packet drop-tail queue, not
  credit.
- The exchanges, key-update, amplification and session suites pass.
