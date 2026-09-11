# 2026-09-10 — An abandoned request keeps being retransmitted, and its late reply shadows every later exchange (live peers retired under load)

## Description

Under CPU load, a fleet node retires a **live** peer that is answering every probe on time. Seen while
validating the connection-id demultiplexer with the three-process deployment test under eight CPU
spinners: 5 of 12 runs ended with the two survivors of a `SIGKILL`ed owner retiring *each other* (each
reporting `fleet_members` = itself, `fleet_peers_probed` = 0) or failing to retire the dead one in time.
The per-peer-socket build before the demultiplexer passed 12/12 under the same load, but only because its
extra socket hop was absent: the mechanism below was already there, and the same signature ("load-
sensitive" retirement failures) had been seen before.

Instrumenting both ends showed the shape: after **one** genuinely late probe reply (a 100 ms probe
deadline missed under load), every following probe on that session timed out — five in a row, then
retirement — while the *server* side logged no service gap at all: it served one request per period,
promptly.

## Root cause

Two defects in the session plane compounded:

1. **A forgotten stream's frames were still retransmitted.** `Endpoint::request` drops its future at
   the caller's deadline (the fleet probe races it against one), and the next request calls
   `Connection::open` on the same stream id. `forget_stream` removed the send and receive streams but
   left the abandoned request's packet in the in-flight tracker and its frames eligible for the
   lost-frame queue. The next probe's timeout (`Connection::probe`) retransmitted the **stale request**;
   the peer served it and replied with the **old nonce**.
2. **A late reply was left for the next exchange to read.** The client's receive side for the stream id
   kept the abandoned exchange's reply (complete, unread). The next `request` read *that* reply as its
   own; the SWIM nonce check rightly rejected it — and counted the probe as a miss. With (1) feeding a
   stale request every period and (2) handing each probe the previous reply, the client stayed exactly
   one reply behind on every probe: a permanent lockstep the suspicion window then turned into a death.

Two further inefficiencies made the load that triggered the first late reply far worse than it needed
to be, and are fixed in the same change:

3. **Idle sessions spun on the probe timeout.** `receive_or_probe` armed the estimated PTO whenever it
   waited — including a server session waiting for its next request with nothing in flight. On loopback
   the estimate is a few hundred microseconds, so every idle session woke thousands of times a second
   (RFC 9002 §6.2.1 arms the PTO only while ack-eliciting packets are in flight).
4. **No PTO backoff.** Each expiration re-armed the same estimate, so a dead peer was retransmitted to
   every few hundred microseconds until the caller's deadline (RFC 9002 §6.2.1 doubles the period after
   each unsuccessful probe).

## Impact

- A live peer retired under load, with its objects taken over by the survivors while it is still
  serving — a false failure that the fleet's own probes then had to reverse (a rejoin is not built).
- With the demultiplexer's one extra task hop per datagram, the first late reply became likely enough
  to fail ~40 % of loaded runs; before it, the same lockstep was rarer but present.

## Exact edits

- `crates/transport/src/conn.rs` `SentTracker::forget_stream`: drops the stream's data frames from every
  packet in flight (a packet left with nothing else is forgotten) and returns the bytes dropped.
- `crates/transport/src/connection.rs` `forget_stream`: also purges the stream's frames from the
  lost-frame queue and takes the dropped bytes out of the congestion controller's in-flight count. Test:
  `a_forgotten_streams_frames_are_never_retransmitted` (the probe path is shown live before the forget
  and closed after it).
- `crates/transport/src/endpoint.rs` `request`: forgets the stream id before opening, so a late reply
  from an abandoned exchange is never read as this one's. `receive_or_probe`: no probe timer with
  nothing in flight (the conservative initial PTO is the idle re-drive); exponential backoff over
  consecutive expirations (`pto_count`, reset by an acknowledgement), capped at the initial PTO.
- Validation (18-core box shared with other users' work, load average 4–5): the three-process
  deployment test under eight CPU spinners 12/12 after the fix (5/12, 7/12, 6/10, 8/10, 9/12 through
  the intermediate steps); the serialized in-process fleet suite 12/12; transport 85 + 13 tests.

## Siblings swept

- **CORRECTION (2026-09-10, same day):** the claim below — that no caller abandons a content exchange at
  a deadline — was wrong. `cluster::request_within` races `request` against the round deadline and keeps
  the endpoint for reuse, so the content exchanges (offer/put/fetch) *are* abandoned mid-stream, and the
  leaked stream then rode the next exchange's flush and was folded into an unrelated request. Fixed in
  `2026-09-10-abandoned-content-stream-poisons-reused-session.md` (the caller forgets the abandoned
  stream; `serve_once` buffers per stream and serves only the completed one's bytes). The original note
  is kept below for the record.
- ~~`Endpoint::recv_stream` and `serve_once` forget their streams only on completion; neither is
  abandoned at a deadline by any caller today (the content exchanges run to completion inside the
  record plane's own deadline, which drops the whole session, not the stream).~~
- `serve_once` phase two retransmits a reply until it is acknowledged; a client that abandoned the
  request acknowledges the reply's packets with its next exchange, so the server completes then. With
  the backoff this costs a few retransmits, not thousands.
