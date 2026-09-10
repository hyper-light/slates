# A SWIM probe mishandles a redelivered acknowledgement, and a single missed probe retires a live peer

Date: 2026-09-10
Area: `crates/cluster/src/swim.rs` (the SWIM probe/ack) and `crates/server/src/fleet.rs` (the daemon's
probe loop), with the enabling transport behaviour in `crates/transport/src/{endpoint,connection}.rs`.
Severity: fleet correctness — a survivor keeps a dead peer marked alive and never retires it; and, once the
first defect is closed, two live nodes retire *each other* during formation. Both miss the test's deadlines.
At two nodes there is no gossip redundancy to mask either, so the flake is fully exposed.

## The two defects (found in sequence)

The two-node retirement/formation flake had **two** independent root causes; fixing the first exposed the
second more strongly, which is why the raw failure rate barely moved after the first fix alone.

### Defect 1 — a stale acknowledgement passes for a fresh one (retirement never happens)

When one node was stopped, the survivor's SWIM probe of it kept returning **acked** — 109 acknowledgements
over 11 seconds in the captured failure, a fresh "Alive" reading every ~100 ms period — so the detector
never suspected the dead peer and never retired it.

**Root cause.** Two facts combined:

1. *The probe did not correlate its acknowledgement to the ping it sent.* `probe_once` accepted any
   decodable `SwimMessage::Ack` as proof of life — no sequence number tied the ack to *this* probe.
2. *The reliable transport redelivers a stale acknowledgement.* The probe reuses one session across periods
   (`PROBE_STREAM` = 1 every time). `Endpoint::request` completes a reply on stream 1 and then
   `Connection::forget_stream(1)` drops that stream's assembler. A packet the peer sent before it died — a
   retransmission of its ack, or one still buffered in the survivor's UDP socket — arrives later carrying a
   `STREAM` frame for stream 1 (offset 0, fin); with no packet-number dedup in `handle_incoming` and the
   assembler forgotten, it creates a *fresh* assembler and re-completes stream 1, so `request` returns the
   **old** ack again. A backlog of the dead peer's buffered acknowledgements is drained one per period, each
   looking like a brand-new one.

**Fix.** A per-probe **nonce** (the SWIM/memberlist probe sequence number) the acknowledgement must echo:
`SwimMessage::{Ping,Ack}` gain a `nonce: u64`; `serve_probe` echoes the ping's nonce; `probe_once` counts a
reply only when `Some(ack.nonce) == probe.nonce()`; `probe_peer` stamps each probe with a per-session
monotonic nonce. A stale ack carries an old nonce, so it never satisfies the probe and the dead peer times
out.

### Defect 2 — a single missed probe drops an unrecoverable session, retiring a live peer (formation fails)

With the nonce in place, the retirement failures vanished but **formation** failures rose: two live nodes,
both timing out on one probe period (~iter 11 under the test thread's busy-spin, which starves the shards on
a small machine), each dropped its probe session and aged the other to death *before* the test's kill — the
"the fleet is up: A holds B in its membership" assertion then failed. The nonce made this *more* likely: it
correctly rejects stale/duplicate acks the old code had wrongly accepted, so more probes legitimately time
out, and every timeout was fatal.

**Root cause.** `probe_once` moved the endpoint into a spawned request task and, on the deadline, cancelled
it — **dropping the endpoint**. `probe_peer` treated a timeout as "peer unreachable, drop the session, age
to death," and the session could not be re-established (`Endpoint::accept` pins one source, so a re-dial
from a fresh port strands it). So a *single* transient miss — a lost packet, a moment's scheduling jitter,
or now a nonce-rejected stale reply — permanently killed the session and forced retirement. That defeats
SWIM's design, in which the suspicion *window* (several missed probes) is what tolerates a transient miss.

**Fix.** Keep the session across a timeout and let SWIM's refutation recover it:

- `probe_once` drives the request/reply **inline**, racing it against the deadline with a `poll_fn`; on the
  deadline the request future is dropped (releasing the borrow) and the **endpoint is returned for reuse**
  whatever the outcome — acknowledged or timed out. It no longer spawns tasks or a channel.
- `probe_peer` reuses the returned session on a timeout and re-probes next period. The ping carries this
  node's suspicion; a still-live peer *refutes* it (SWIM incarnation refutation — the detector already does
  this: an incoming self-suspicion produces `Change::Refuted` and gossips `Alive` at a higher incarnation),
  and the acknowledgement's gossip clears the suspicion (`age_suspicions` drops the counter for any member
  no longer suspected). A genuinely dead peer never refutes, so it still ages to death across the window and
  is retired. One missed probe retires no one; sustained silence does.

## Why two nodes, not three

At `N = 3` a survivor also learns a peer's death (and hears a peer's refutation) from the *other* survivor's
gossip, which masks a single stalled or mis-timed probe — the three-node retirement test measured 27/27.
At `N = 2` there is no third party, so the survivor's own probe is the only evidence and the only carrier of
a refutation; both defects are fatal to it. The fixes make the probe itself sound, so retirement and
formation no longer depend on redundancy — the fleet scales down to two nodes as reliably as it scales up.

## Diagnosis method (reusable)

Instrumenting the probe loop (each probe's nonce and outcome) and the serve loop (each answered probe)
localized both defects. Defect 1: both nodes' serve tasks fell silent by ~1.96 s yet the survivor's probe
kept logging `ACKED` with the peer `Alive` to 12 s — and since `Endpoint::ingest` accepts only
session-key-valid packets (only the peer holds the keys) while `Runtime::shutdown` joins the peer's threads
before `stop()` returns, a valid ack after the peer is gone can only be a redelivered stale one. Defect 2:
after the nonce fix, every captured failure was the formation assertion, with both nodes' probes ending
`TIMEDOUT` on the same period and then no further probes (the sessions had been dropped).

## Impact

Any single reused-session request/reply that accepts a reply without binding it to the request is exposed to
a redelivered stale reply; the SWIM probe was the only exchange with no binding at all (see sibling scan).
And any liveness protocol that treats a single missed probe as a verdict, on a session it cannot
re-establish, is brittle to transient loss — the second defect.

## Regression tests

- `crates/cluster/tests/swim.rs` `a_stale_nonce_acknowledgement_is_rejected_and_the_peer_is_suspected`: over
  a real session, the target answers with a valid `Ack` whose nonce does not match the probe's (carrying a
  distinctive rumour); the probe must time out, the target must be suspected, and none of the stale
  acknowledgement's gossip may be folded — proof it was received and rejected. Fails before the nonce fix.
- The two-node test `a_daemon_detects_its_dead_peer_over_the_transport_and_retires_it` exercises both
  formation and retirement and is the end-to-end proof; it was ~3% flaky (defect 1), ~8% flaky with the
  nonce alone (defect 2 exposed), and is the ratcheted gate after both fixes.

## Sibling scan

- **Record commit** (`commit_record`/`serve_record`, `Ack::binds`): binds to the record's
  `(object, sequence, generation, identity)`, so a stale ack for a *different* record cannot count. A stale
  ack for the *same* idempotent head from a since-dead holder would still bind (deduped per holder; the ship
  loop redials a timed-out session); a per-commit nonce or holder-liveness cross-check would close that
  narrower window — flagged to Ada, not changed here.
- **Promotion** (`promote_record`/`serve_promotion`, `Promise::binds`): binds to the prepare's
  `(object, epoch, generation)`; same narrower same-operation residual, flagged with it.
- Only the probe counted a reply with **no** binding, and only the probe treated one miss as a verdict; both
  are fixed here.

## Defect 3 — the record dispatch drops a holder's session on a timeout (the takeover flakes under load)

The same "one timeout drops an unrecoverable session" fragility lived in the **register dispatch**
(`commit_record`/`promote_record`, `crates/cluster/src/lib.rs`), and surfaced once the takeover ran: under
contention a cross-node commit or promotion misses its deadline, and the collection loop **cancelled the
straggler task, dropping its endpoint** on the assumption "that holder reconnects." In the per-peer-socket
mesh it cannot (`Endpoint::accept` pins one source), so the session was gone and the object stranded — the
three-node takeover test (and, by the same mechanism, the replication of a head to *both* survivors) flaked
under the full suite's load, producing no drive attempts at all because the drive had no live session.

**Fix.** Each dispatched holder request now runs through `request_within` — a deadline-bounded
request/reply that **hands the endpoint back whatever the outcome** (reply, refusal, or timeout), bounded by
the collection loop's full progress-extended span (`CommitBudget::max_deadline_ns`). So a straggler that
never replies still returns its session, and the caller (`ship_records`/`drive_takeover`) retries the
timed-out commit or promotion **over the same warm session** instead of losing it. At `f = 1` — the daemon's
shape — there is one remote holder and no early quorum without it, so its session is always recovered; at
`f > 1` an early-quorum straggler cut off before its own deadline still loses its session (unavoidable
without waiting for it, and unchanged from before). This is the probe fix's keep-session discipline applied
to the record plane, and it is a correct robustness improvement — but it did **not** eliminate the takeover
flake (nor did a companion session-establishment fix: making `ship_records` drive the handshake on one
persistent socket, so the peer's pinned `accept` completes rather than a fresh-port re-dial being ignored).
The flake persisted at ~8%, which is what forced the real root out (Defect 4).

## Defect 4 — a head is shipped only until quorum, not to every candidate (the actual takeover-flake root)

The takeover test's real failure was its **setup** assertion, not the takeover: "both survivors hold A's
head before A dies." Under load ~8% of runs failed there, and the takeover itself would then be starved
(a survivor that never received the head cannot promise, so the promotion cannot reach quorum after the
death).

**Root cause.** The owner ships a head to its candidate holders with one **per-peer** ship task each
(`ship_records`, the per-peer-socket mesh). `unplaced_heads` gated on the object's **overall placement**:
once *any* ship task committed the head to `f + 1` distinct candidates (region-placed, durable), it inserted
`placed_heads[object]`, and **every other ship task then skipped the object**. So a head reached only its
first `f + 1` holders. At `f = 1`, three candidates {A, B, C}: A's ship-to-B commits (A + B = quorum) and
marks it placed, so A's ship-to-C — if it had not already shipped in the same period — skips the head, and
C never receives it. It was a race (both ship tasks usually read "unplaced" before either placed), which is
why it flaked rather than always failed. But the design is explicit — "records are sent to **all**
candidates; committed at `f + 1`" — because after a death the surviving candidates form the promotion quorum
and each must hold the head.

**Fix.** The gate is now **per holder**, not per object: `unplaced_heads` became
`heads_for_peer(state, local, peer_host)`, which returns a head for the ship task only if `peer_host` is a
candidate for it **and has not yet acknowledged it** (`placed_heads[object].acked` does not contain it). And
the placement is **merged, not overwritten** on each commit — the union of the per-peer ship tasks' acked
candidates is the true region placement — so a head is shipped to every candidate exactly until that
candidate holds it, and no further. With this the three-node full suite ran 30/30 under load (was ~8%
flaky). This was the actual scale-up cause; Defects 1–3's fixes are correct robustness that the fleet keeps.

A test-harness change accompanied it: the fleet tests now `std::thread::yield_now()` between poll checks
instead of `std::hint::spin_loop()`, so an off-shard test thread waiting on a daemon yields the core to that
daemon's shard threads rather than pinning it and starving the very progress it is waiting for. It reduces
the artificial starvation the busy-spin created (a test artifact — a real client blocks on a completion fd),
though it was not itself the flake's cause.

## Transport note (owed, flagged)

`Connection::handle_incoming` processes every packet's frames with no received-packet-number dedup, and
`forget_stream` lets a replayed frame re-open a forgotten stream — that is what lets a stale packet
re-complete a reused stream. The nonce fixes the observed failure at the application layer (the right layer
for probe liveness); a transport-layer received-packet-number dedup (RFC 9002 §5.3) is the deeper hardening,
owed and flagged to Ada. Likewise, a re-establishable session (the fixed-port mesh or an `accept` that
re-learns across a dialer's flights, both already on the books) would let a *genuinely* broken session be
rebuilt rather than only a transiently missed one recovered — the connection-management work owed with the
connection-ID demux. The keep-session fixes above make a transiently-missed session (a timeout) non-fatal;
a session whose peer truly died mid-establishment still needs that owed re-establishment.

## Probe-plane establishment robustness (persistent-socket retry) — and why the "formation flake" was a harness artifact

While validating the takeover under load, a three-node **formation** failure (`three_daemons_form_a_full_mesh`:
`[("a", false), ("b", false), ("c", true)]`, timed out at the 15 s deadline) showed up at ~2.5 % — but **only
when the fleet test binary was run as several concurrent OS processes** to manufacture load. It does **not**
reproduce in the real suite: 60/60 sequential runs pass, and 25/25 full-suite serialized runs
(`--test-threads=1`, one process) pass under heavy unrelated CPU load (8 spinners, then 24 oversubscribed on
an 18-core box). The genuine fleet is sound; the multi-process failure is a **test-harness artifact** of the
repro method, from two facts that only hold across separate processes:

1. *Ephemeral-port reuse across processes.* `four_free_ports`/`free_ports` bind `:0` sockets **all at once** so
   the OS hands back ports distinct **within one process**, then drop them for the daemons to rebind. Two
   concurrent processes can be handed the **same** released port, so process B's node dials an address that is
   process A's per-peer serve socket. `Endpoint::accept` pins the first source it hears, so that stray
   ClientHello pins A's accept to the wrong peer and A's real dialer can never complete that directed session —
   exactly the `a`/`b`-unmeshed, `c`-meshed shape observed.
2. *Host-id collision across processes.* `unique()` is a process-local `AtomicU64` starting at 0, so two
   processes mint **identical** host ids (the harness comment assumes in-process thread parallelism, which is
   how `cargo test` runs; the fleet tests also serialize on a process-global mutex, so only one runs at a time).

Neither can occur in real operation (distinct machine addresses; one address space) or in the real suite (one
process, fleet tests serialized, `unique()` genuinely monotonic). So this is **not** a fleet defect and needs
no fleet fix. Left as-is deliberately: making the harness safe across processes would need the daemon to accept
**pre-bound** sockets (holding the port through the rebind) plus a process salt in the host id — test-infra
scope beyond this bug, recorded here so it is not re-chased as a fleet flake.

The investigation did, however, motivate a real robustness improvement kept here: `probe_peer` (the SWIM probe
client) previously dialed with a **single** `establish()` attempt (`dial`) and **returned on failure** — so a
peer that took longer than one handshake budget to come up (a genuinely slow scale-up join, not the sub-ms
loopback case) would strand that probe task permanently, never probing or meshing the peer and never able to
retire it either. It now brings the session up with `client_for` + `establish_session` — one handshake attempt
per period **on the same socket**, retried until it completes — mirroring the record plane's `ship_records`
(the two now share `establish_session`). The detector ticks only when a probe is actually sent, so an
as-yet-unestablished session never ages the peer. `dial` is removed (fully replaced). This is scale-up
robustness, symmetric with the record plane; it is not the fix for the harness-artifact formation failure
above, which has no fleet-side cause.
