# A ring slot's bulk body was overwritten before it was read (2026-11-06, §4.7 `Ring`, the bulk area)

Found by agents/loadtest-scale's multiplexing load harness. One `CreateWork` and one or two `Edit`s in every 300-client
run at 32 clients a connection were refused `BadRequest { reason: "body does not decode" }`; one client a connection
never showed it.

## Description

A message whose framed body is longer than a slot's 40-byte inline payload is written to a bulk chunk, and the slot
carries a reference to it (`protocol::pack`). The chunk is chosen by the ring position, `index mod slots`
(`protocol::chunk`), so one chunk serves every message at that position, `slots` apart. Two orderings let one message's
body be written over another's before it was read.

1. **The consumer released the slot before reading the body.** `Ring::pop` read the 64-byte slot and released it to
   the producer at once, and the body was decoded later from the bulk area (`protocol::unpack`). A producer sending on
   wrote the next message at that position into the same chunk in between. On the daemon this is
   `DaemonEnd::try_take`, then `verbs::serve`. On the client it is `ClientEnd::try_take`, then `unpack`.
2. **The producer wrote the body before knowing the ring had room.** `Client::try_send`/`send` and the server's
   `send_reply` packed a body into its position's chunk and only then pushed, and the push could be refused `RingFull`.
   With the ring full, that chunk belongs to the oldest message still unread, whose body was overwritten. The refused
   message was then sent again later.
   - A client below its in-flight bound (`awaited_cap`, the ring's slots) still meets a full command ring, because
     unawaited acknowledgements share it.
   - The daemon meets a full completion ring whenever the client is slow to take its replies.

When the lengths differ, the reader fails to decode ("body does not decode"). When they agree, it decodes the other
message, silently: a request runs another request's body under its own id, or a client takes another request's reply.
The red tests show both: request 0 decodes as request 8, and reply 0 as reply 8.

## Root cause

The ring's credit protected the 64-byte slot but not the bulk chunk the slot names, which is shared by every message at
that position. With one request in flight per connection (every client before the multiplexing harness), no message
ever reached a chunk still in use.

## Exact edits

- `crates/ipc/src/slot.rs`: `Ring::peek` reads a slot without releasing it, and `Ring::release` releases it. `pop` is
  the two together.
- `crates/ipc/src/protocol.rs`: `owned` copies a bulk slot's body out of the bulk area, bounds-checked as `unpack`
  checks it, and carries it inline, so `unpack` reads it as before.
- `crates/ipc/src/endpoint.rs`:
  - Both `try_take`s peek, copy the body out, then release. A hostile slot is still released and refused, typed.
  - `ClientEnd::send_message` and `DaemonEnd::reply_message` check that the ring has a slot at the next position
    before anything is packed.
- `crates/client/src/client.rs` (`try_send`, `send`) and `crates/server/src/verbs.rs` (`send_reply`) send and reply
  only through those two calls. No other code in the client or the server packs a message.

## Tests (each red first)

All three are in `crates/ipc/tests/rings.rs`:
- `a_request_taken_from_the_ring_keeps_its_bulk_body_when_the_client_sends_on` (request 0 decoded as request 8);
- `a_reply_taken_from_the_ring_keeps_its_bulk_body_when_the_daemon_replies_on` (reply 0 as reply 8);
- `a_request_refused_ring_full_never_writes_over_an_unread_body` (request 0 as request 8, after a refused ninth send).

## Field check

Mac, single-node anchored daemon, 300 clients at 32 a connection, one round:
- before: 1 to 3 `BadRequest` in every run (5 of 5 runs);
- after: none in 3 of 3 runs, every submit accepted in the two runs the daemon survived.

The third run's daemon was killed by its anchor under load 60 to 89 (an open GAPS item), and that run is recorded in
GAPS.

## Siblings

No other code pops a ring: the two `try_take`s are its only consumers. Tests that pack by hand keep one message in
flight, so they never reach a chunk in use.
