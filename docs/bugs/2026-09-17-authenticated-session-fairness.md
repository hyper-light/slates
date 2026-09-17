# Reserve handshake capacity and bound sessions by authenticated identity

Date: 2026-09-17. Contracts: §4.3 bounded work and §4.10a session ownership.

## Reproduction

`cargo test --offline --locked -p slates-transport --test admission -- --nocapture`
on `169c355` fails in 0.01 s: three connections with one certificate all establish
while earlier server endpoint owners remain held. The existing global pool limits total
tasks but gives no other authenticated identity a reservation. Log:
`$TMPDIR/slates-followups-o5314nq6/fairness-red.log`.

## Policy and implementation plan

For peer capacity C, reserve C pending handshakes and two authenticated slots per peer:
the live session plus its replacement. Total accepted endpoint capacity is 3C per plane.
Pending handshakes cannot take the authenticated reservation. Authentication, before a
connection id is published or the current session replaced, checks both the distinct-peer
bound and the certificate's two-slot bound. A replaced endpoint keeps its authenticated
charge until its owner drops it. Refused authentication cannot close the existing session.
Use typed reasons and separate counters for pending capacity, peer capacity and per-peer
exhaustion. The fleet's task/timer reserve derives from the same total capacity.

The key is the authenticated certificate, never the source IP or port: two identities
behind one NAT are different peers, and a replacement on a new IP is the same enrolled
identity. Before authentication a source has no trustworthy peer identity. The pending
pool bounds that anonymous work; this is not a promise of admission under an unlimited
unauthenticated flood. Existing endpoints retain their reserved capacity and continue
serving while pending work is full. Existing bounded handshake lifetimes reclaim that work.

Tests must hold stale owners, refuse a third same-identity authentication, serve another
identity, release only the stale owner's charge, and reconnect again. Separate histories
fill pending capacity while exchanging on live sessions, and release pending work before
a retry. Update the old shared-pool exhaustion fixtures to these two admission classes.

## Implemented evidence

Three deterministic histories pass in 0.01 s (`crates/transport/tests/admission.rs`):
per-certificate exhaustion, pending-handshake exhaustion while serving a live peer, and
distinct-identity exhaustion. The first also retries the very same refused endpoint to
prove that a cached connection id cannot bypass admission, and exchanges again after a
stale owner drops to prove the current routes survive. Every history releases all owners.

The old two global-pool exhaustion histories in `session.rs` assumed there was no reserved
handshake capacity. Their saturation and stale-owner/reuse proofs now live in `admission.rs`
under the two separate admission classes. The other 15 session histories pass in 0.53 s,
including replacement, large rosters, loss recovery, RTT measurement and setup refusals.
The real fleet burst passes in 23.04 s; no client request is refused, and serve tasks return
to their prior count. Commands on local macOS arm64, Rust 1.98.0, serial with two build jobs:

```sh
cargo test --offline --locked -p slates-transport --test admission -- --nocapture
cargo test --offline --locked -p slates-transport --test session -- --test-threads=1
cargo test --offline --locked -p slates-server --test fleet \
  a_peers_re_dial_burst_replaces_its_sessions_and_never_refuses_a_client -- --exact --nocapture
cargo clippy --offline --locked -p slates-transport -p slates-server --all-targets -- -D warnings
```

Clippy passes. The fleet exports `fleet.accept.peer_sessions` and
`fleet.accept.peer_capacity` separately from handshake failures. The transport retains
the corresponding typed `SessionRefusal` and distinct counters. Session closure removes
only that slot's source/id routes by key; late stale drops cannot remove current routes.
Closed inboxes refuse before yielding queued bytes. Neither quota accounting nor cleanup
walks the other peers' route entries.

Six configuration histories pass in 2.96 s, including the fleet's task/timer reserve;
the whole-RAM fresh-voter history passes in 13.54 s. `cargo xtask check` passes all
structural/literal/unsafe/version gates. The live burst no longer assumes zero temporary
capacity refusals: a machine deriving one pending-handshake slot may legitimately refuse
part of its concurrent burst; every dial must still establish in turn and clients remain served.
Its final retry-aligned run passes in 22.90 s. The fixture now uses the production dialer's
two-budget limit before a fresh socket, closing a sibling mismatch in that test's claimed behavior.
