# An idle peer's late acknowledgements inflated the RTT, and a lost reply cost 7 s

Date: 2026-09-28. Contracts: RFC 9002 §5 (RTT samples), §6.2 (probe timeout); RFC 9000 §19.11
(`MAX_STREAMS`), §19.14 (`STREAMS_BLOCKED`). Found by the congestion grid on `8907c6f`. At 64 kbit/s with 1 %
loss, every controller's ping p99 was 6.5–7.4 s, with maxima in steps of about 7 s (7.4, 14, 21 s), and Copa
was 6× the best there.

## Evidence

- **Per-ping trace.** 16 of 1,080 pings (1.5 %) took 6.8–7.8 s; one took 14.6 s. Every other ping finished in
  under 1 s.
- **The client wasn't the one waiting.** A temporary print at each probe timeout showed the client's timeouts
  firing after 300–700 ms, with no backoff beyond its first doubling. The server fired none, across 15 runs.
- **The server's timeout was about 6.6 s.** A temporary print of the server's timed wait while a reply was in
  flight showed a timeout of about 6.6 s.
- **Its RTT samples were bimodal**: about 60 % under 500 ms (the path) and about 40 % at 2.5–3.5 s.

## Root cause

When a reply was acknowledged, the server closed the stream and sent the raised stream credit (`MaxStreams`)
in an **ack-eliciting** packet. The ping client, like a pooled fleet session, reads only while an exchange
runs. By then it was asleep until its next ping (about 2.9 s later), so it acknowledged the credit only when
it woke. That round trip spanned the idle gap. Those samples inflated the server's RTT variance, the probe
timeout reached 6.6 s, and a lost reply waited that long to be resent. The client could not help: its
request was already acknowledged, so it had nothing in flight to probe with.

## Fix

- **Credit rides acknowledgements.** The stream credit (`MaxStreams`) rides every acknowledgement, as the
  connection credit does, and elicits none. A finished exchange leaves the idle peer nothing to acknowledge,
  so RTT samples come only from packets the peer is actively waiting on.
- **A blocked sender reports it.** A sender whose next stream waits past the credit, with nothing in flight,
  sends a reliable `StreamsBlocked` (frame kind 10, RFC 9000 §19.14), re-armed while it stays blocked. The
  acknowledgement it forces carries the current credit. All three credits now share one design: credit
  rides acknowledgements, and a blocked report is the reliable path.
- **The minimum packet budget** rises to 37 bytes: an ACK plus two credit frames.

## Found on the way

The first cut left `MaxStreams` ack-eliciting on receipt. Once it rode every acknowledgement, each
acknowledgement demanded one back: an endless exchange of acknowledgements, found by the reuse oracle
(`many_exchanges_reuse_a_connection_with_bounded_state_and_flowing_credit`) as a livelock at a frozen clock.
Credit frames are never ack-eliciting.

## Result

At 64 kbit/s, 100 ms, 1 % loss, ping p99 fell from 5.5–7.1 s to 620–790 ms for every controller (Copa
7.0 s → 0.70–0.79 s). `a_finished_exchange_leaves_the_idle_peer_nothing_to_acknowledge` states the property.
