# A stranger took a dead client's seat, and any client could plant replies under another's request ids

- **Date:** 2026-11-07. **Status:** fixed (the session key; the foreign-request refusal).
- **Found by:** the bug sweep, reading `Listener::accept_pending` for GAP-A9-9's threat model (a same-uid process that
  is not the client); the second while writing the first's test.

## Description

Two holes in how a request's client is established, both on every platform.

1. **The seat.** A client resumes its session under its id after a daemon restart (`Client::resume`, §4.9), and the
   rendezvous carries only the id it wants. `accept_pending` granted a wanted id to whoever named it while no live
   client held it (`wanted != 0 && !in_use(wanted)`). Ids are small integers, and on macOS and Windows the claim slot
   that carries them lies in an object any same-uid process maps. So a stranger resuming under a dead client's id was
   served its seat: its retained completion records, its attachments' verbs, everything after.
2. **The request id.** A request carries its id, client and sequence, in its slot word, and the daemon keys a completion
   record by that id. `serve` took the client from the word and never compared it with the slot's own client. The
   daemon already dropped a reply addressed to another client (`send_reply`), so a stranger read nothing back; but it
   ran the request and recorded its reply under the other client's id, where that client's own request of the same id
   later met it as completed.

## Evidence

- `a_stranger_with_a_dead_clients_id_gets_neither_its_seat_nor_its_records` (`crates/client/tests/client.rs`): the
  victim sends a create and has not seen its reply; the daemon restarts (the test plays the anchor); a stranger resumes
  under the victim's id and sequence and retries the victim's request. Before: "the stranger took client 1's seat; its
  retry of the victim's create answered Ok(Created { id: … })".
- `a_client_cannot_send_a_request_under_another_clients_id` (same file): a second client of one daemon sends a create
  of `planted` under the victim's next request id, then the victim creates `victims-next`. Before: the victim's create
  answered the planted volume's id and never ran (listed `["forged-victims", "planted"]`).

## Root cause

The rendezvous authenticates the process (uid on every platform; the audit token's pid on macOS, A-233) but nothing
authenticated the claim to an id, and nothing tied a request's id to the slot it came on.

## Impact

Confidentiality and integrity across clients of one account: a seat's retained replies (a read's bytes among them) and
its attachments' verbs went to a stranger; any client could make another's next request answer with a reply it chose,
and keep that request from running. Within the account's own rights, so no privilege was gained, but a client bound
to a consumer (§4.13) or a workload's session is exactly what the threat model keeps apart.

## Edits

- **The session key** (`crates/server/src/session_key.rs`, new). Each id has a 256-bit key, keyed BLAKE3 over the id
  under the node's session secret, which the first daemon under an anchor makes and publishes in the supervision block
  (`layout::SUP_SESSION_PRESENT`, `SUP_SESSION_SECRET`; appended) and every later one adopts. The key travels only in
  the client's region (header bytes 64 to 96, `ClientRegion::session_key`; region layout version 4): the daemon writes
  it for an id it issues fresh; a client resuming an id writes its own key there before its first request
  (`Client::over`, both reconnect paths). `Session` carries the key, and its `Debug` leaves it out.
- **The check** (`crates/server/src/verbs.rs`, `session_proven`). Before a slot's first request the shard compares the
  region's key with the id's, in constant time. A slot that does not prove its id is answered `SessionUnproven`
  (appended), with nothing recorded, and ended by `reap::end_unproven`, which retires nothing of the id's (its
  attachments and records are its owner's) and gives the id back. The reaper ends a dead unproven slot the same way.
- **The foreign request** (`refuse_foreign_request`). A request whose id names another client than its slot's is
  refused `BadRequest` before any effect, recorded nowhere, and counted (`client.foreign_request`).

## Residual

- A stranger that claims a dead id and sends nothing holds the slot until the reaper finds its process gone, delaying
  the owner's resume. It reads nothing and changes nothing; a same-uid process can end the owner anyway.
- On macOS the anchor segment, and with it the session secret, is opened by name by any same-uid process: the
  exposure recorded in GAPS and with Ada. Closing it closes this too.
