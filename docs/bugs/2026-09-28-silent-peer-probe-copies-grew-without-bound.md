# A silent peer's probe copies grew without bound

Date: 2026-09-28. Contracts: RFC 9002 §6.2.4 (probes), §7.6.2 (persistent congestion); CLAUDE.md
"unbounded growth" ban. Found by the leak census (`Connection::census`) while diagnosing the cluster commit
tests that hung after the concurrent-exchange change.

## Symptom

A cluster holder called `Endpoint::settle` after serving, and the owner stopped driving its end. A
diagnostic print in `settle` (added, run once, removed) showed the holder's tracked in-flight packets
climbing without end:

```
SETTLE-DIAG t=90639627200000 census ConnectionCensus { ..., send_streams: 0, ..., in_flight: 136105 }
SETTLE-DIAG t=90640293200000 census ConnectionCensus { ..., send_streams: 0, ..., in_flight: 136106 }
```

That is 136,106 tracked packets after 90,640 virtual seconds, one more per probe timeout.

## Root cause

Two faults.

1. **`settle` waited on credit frames.** It required every in-flight packet to be acknowledged, and one
   carried the `MaxStreams` credit the holder sends when it closes the owner's stream. That credit matters
   only to a peer still using a live session. The owner had stopped reading, so the holder waited forever.
2. **Probe copies accumulated.** Each probe timeout sent a copy of the oldest packet, and every earlier copy
   stayed tracked beside the original (RFC 9002 keeps the original for loss accounting). A peer that stays
   silent therefore added one tracked packet per probe timeout, for as long as the session was held.

## Fix

1. `Endpoint::settle` waits only for what the peer needs: unacknowledged stream data and unacknowledged
   resets (`Connection::owes_peer`, with `resets_owed` tracked through send, loss and acknowledgement).
2. `Connection::queue_probe_copy` keeps only the two most recent probe copies (`PROBE_COPIES_KEPT`). From any
   older copy it drops the frames the new copy duplicates; that packet's other frames stay tracked, and the
   packet goes only when nothing is left in it. Two, not one: persistent congestion is judged from the span
   between the earliest and the latest *lost* packets. When a path returns, the newest probe is acknowledged
   and the one before it is the latest lost. Keeping one copy collapsed that span to the originals, and
   `persistent_congestion_collapses_the_window` caught it.

## Evidence

- **The unit test, before and after.** `a_silent_peer_costs_bounded_tracking_however_long_it_is_silent`
  runs 1,000 probe timeouts. With the drop disabled it fails, peaking at 1,003 packets for 2 originals. With
  it, the peak is at most originals + 2.
- **End to end.** `a_peer_that_dies_mid_exchange_...` settles toward a peer that died before hearing the
  request, for 30 virtual seconds (hundreds of probe timeouts). Tracking stays at 4 packets or fewer.
- **Suites.** Transport, cluster (all binaries) and the fleet suite (50/50) pass.

## Rejected on the way (measured)

A **user timeout** (RFC 793 / RFC 5482, floored at RFC 9000 §10.1's three PTOs) that declared a peer
unresponsive when data went unacknowledged too long. It bounded the growth, but broke `wan_election`: the
derived timing never elected at the GEO profile. A temporary diagnostic counted 136 sessions killed as
unresponsive, most owing the peer nothing.

The cause is that fleet sessions are pooled and read only while an exchange runs. Between election rounds
(about 13 s) a candidate's acknowledgements sat unread in its socket while its timer ran, so "not listening"
was misread as "peer silent". Bounding the copies addresses the growth directly and kills no live session.
The user timeout was removed entirely rather than kept as a second mechanism.
