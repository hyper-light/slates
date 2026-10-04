# Standard QUIC under slates' transport: the combined design (A-52)

> Status (2026-10-01): **plan, stage 0.** The vendor-and-conform stages (1, 2 and 4) run in the shared
> `hyper-quic` crate, not in slates (§5). Ada's directive: the fleet transport must be RFC 9000, 9001 and
> 9002 compliant without losing slates' measured performance work ("your goal is to combine the two"). Ada
> chose to vendor `quinn-proto` 0.11.18 and conform it to slates' rules ("vendor and conform"), not to grant
> exceptions and not to rewrite slates' own wire layer.

## 1. Why

The fleet transport (`crates/transport`, §4.10a, `docs/wip/fleet-transport.md`) speaks a QUIC dialect.
Mantle's review (its note 30) and Ada's audit name seven defects. Each departs from a standard or breaks a
property the design promises. They are reported, not yet reproduced here: each gets a red test in slates'
current stack before its fix is claimed (stage 5 keeps one regression per defect).

| # | Defect | Standard or rule | Effect |
|---|---|---|---|
| 1 | Probe-timeout backoff is capped. | RFC 9002 §6.2.1: the PTO doubles on each consecutive expiry, without limit. | A sender keeps probing a dead or saturated path at a steady rate: the retransmission storm the RFC rules out. |
| 2 | The first handshake retransmit fires at 1 ms. | RFC 9002 §6.2.2 and Appendix A.2: with no RTT sample the initial RTT is `kInitialRtt` = 333 ms, so through §6.2.1's PTO formula a handshake starts with a PTO of about one second. | On a thin link the early copies queue behind the first and slow the handshake. |
| 3 | Bytes in flight count payload only. | RFC 9002 §2 and Appendix B: bytes in flight are whole packets, headers and AEAD tag included. | The sender sends more than the controller believes: overload and unfairness to other flows. |
| 4 | The priority class is carried in the stream ID. | Audit §13.3; R4. | A peer chooses its own priority: a privilege escalation. |
| 5 | Whole exchanges are retained before admission. | Audit §11.8; §4.2 bounded admission. | An 8 KiB receive ceiling still accepted a 32 MiB request. |
| 6 | No connection migration or path validation. | RFC 9000 §8.2 and §9. | A laptop moving from Wi-Fi to cellular loses every connection. |
| 7 | Not interoperable. | RFC 9000/9001 wire image. | Standard tools (Wireshark, qlog) cannot decode it; no conformance evidence against another stack. |

## 2. The combined design

| Layer | Source | Notes |
|---|---|---|
| Wire: packet format, loss recovery, timers, connection IDs, migration, path validation, key update, stateless reset. | Standard QUIC (RFC 9000, 9001, 9002) through vendored `quinn-proto`, conformed to slates' rules. | Correct, interoperable and decodable by standard tools. Fixes defects 1, 2, 3, 6 and 7 at their root. |
| Congestion behaviour: Copa, the 1 ms pacing quantum with a two-datagram floor, RACK-style adaptive reordering, the path-MTU refinements. | slates (and focal's Copa port), as patches to the vendored `quinn-proto` through its congestion and pacing seams. | Every measured result is re-measured after the port; a regression does not land. |
| Application protocol: one connection per peer, a request and reply per exchange, priority classes with a credit reserve for the classes above, absolute credits, typed refusals. | slates' model (`endpoint.rs`, `streams.rs`, `flow.rs`), on `quinn-proto` streams. | Fixes defect 4: the class is set by message kind and the sender's role, never read from the peer's stream ID. Fixes defect 5: a request streams through an admitted reservation, never retained whole first. |
| Removed. | slates' own packet codec, loss recovery, PTO, handshake sequencing and bytes-in-flight accounting. | Replaced, not layered (banned item 7). |

## 3. Conformance of the vendored crate

The vendored crate must meet slates' rules before anything builds against it. Measured in 0.11.18, test
modules included: 182 `Arc` sites, 18 `Mutex`/`RwLock`, about 570 `unwrap`/`expect` and about 1,100
`panic!`/`assert!`/`unreachable!` sites, in about 30,700 lines.

- **No `Arc`, `Mutex` or `RwLock` (R2, banned item 1).** Shared configuration becomes owned values or
  `&'static` process singletons. The connection's crypto session is owned by its connection. The one
  permitted site is rustls's configuration API, which takes `Arc` by signature: the existing D-8
  exception 2, commented with its two owners.
- **No panics (banned item 6).** Every `unwrap`, `expect`, `panic!`, `assert!`, `unreachable!`, indexing
  and unchecked arithmetic in non-test code becomes a typed error, `.get()`, or `checked_*`/`saturating_*`.
  Protocol violations become the connection's own transport errors (RFC 9000 §20). The crate sits under
  the workspace lint wall like any other.
- **No magic numbers (R3).** RFC constants stay, named and cited (`kInitialRtt`, `kPacketThreshold`,
  `kGranularity`). Tunables slates derives (pacing quantum, windows) come from slates' derivations.
- **Runtime.** `quinn-proto` is sans-IO: no tokio and no foreign runtime (banned item 2). slates' runtime
  drives it through the UDP socket and timer it already owns.
- **Dependencies.** Kept to what the conformed crate still needs, each listed in the stage that adds it.
  rustls with the aws-lc-rs provider is already in the build (A-66, 2026-10-03).
- **Licence.** Upstream's MIT/Apache-2.0 licence files are kept with the vendored source, and the upstream
  version and commit are recorded.

## 4. Stages

Each stage lands with its tests, its GAPS row and its design status in the same commit.

1. **Vendor.** `quinn-proto` 0.11.18 in-tree as a workspace crate, licence kept, building on its own.
2. **Conform.** Remove every `Arc`, `Mutex`, `RwLock` and panic; pass the lint wall and `cargo xtask
   check`. Upstream's tests keep passing throughout, as the oracle that conformance changed no behaviour.
3. **Integrate.** slates' runtime drives the connection; slates' application layer (exchanges, priority
   classes set by kind and role, credits, streaming reservations) runs on its streams. The fleet suite
   passes on the new stack.
4. **Port slates' refinements.** Copa, pacing, RACK-style reordering and path-MTU as patches through
   `quinn-proto`'s congestion and pacing seams, each re-measured against its recorded number in
   `docs/wip/BENCHMARKS.md`.
5. **Prove it.**
   - Interop: the conformed stack completes handshakes and exchanges with unmodified upstream `quinn-proto`
     (a test-only dependency, never shipped).
   - Migration and path validation: a client rebinds its address mid-session and the session continues.
   - Defect regressions: one test per defect in §1 (PTO doubling past any cap, the first handshake PTO at
     the RFC value, bytes in flight counting whole packets, a peer unable to choose its class, a request
     past its reservation refused before it is retained).
   - qlog output decodable by standard tooling.

## 5. Where the work lives (settled 2026-10-01)

Ada: "we're building shared crates", located in `../hyper-raft` (github.com/hyper-light/hyper-raft). The
conformed `quinn-proto` is the shared crate `hyper-quic`, built there by the mantle session, with the shared
transport, SWIM and Raft crates beside it. slates does not vendor `quinn-proto` itself. It vendors a snapshot
of the shared crates with the source revision recorded, as mantle and focal do. The shared repository takes
the strictest union of the three projects' rules as its floor. Two changes go beyond §3 here:

- rustls is vendored and conformed too, so the D-8 exception at its configuration API is not needed.
- The crypto provider is aws-lc-rs, the move slates had already queued.

What slates still owns:
- **Stage 3, the integration.** slates' runtime drives the connection, and slates' application layer runs
  on its streams.
- **The slates-side half of defects 4 and 5.** The class is set by message kind and the sender's role, and
  a request streams through an admitted reservation.
- **Stage 5's acceptance tests**, run in slates' fleet suite.

The properties slates needs from `hyper-quic` were sent to its owner on 2026-10-01:
- the class is set by kind and role, never by the peer;
- refusal happens at the reservation, before any bytes are retained;
- congestion and pacing seams can take slates' refinements as patches;
- the crate is sans-IO;
- every refusal is typed and nothing panics.

## 6. Integrating the shared crates (A-67, 2026-10-03)

Ada, 2026-10-03: "you also need to integrate ../hyper-raft". slates takes the shared crates as a vendored snapshot
(`vendor/hyper-raft/`, revision in `SNAPSHOT`), never a path or git dependency. Each step is one gated commit with its
tests, its GAPS row and its design status; a crate replaces slates' implementation only where hyper-raft's benchmarks
show it at least as fast and allocating no more (hyper-raft CLAUDE.md §1a; its `docs/benchmarks.md`). A change slates
needs in a shared crate goes to the crate's owner (the mantle session) as a proposal.

| Step | What | Waits for | State |
|---|---|---|---|
| H-1 | Snapshot hyper-timing, hyper-swim, hyper-datagram at hyper-raft `687244f` | — | done |
| H-2 | Membership on hyper-swim, its probes on hyper-datagram's sealed plane | — | built: the core (H-2a), the control shard's task (H-2b, `crate::member_task`), and the transport's unused control-datagram codec, seal and key schedule removed (H-2c). Owed: re-vendor for hyper-raft's zero-granularity fix |
| H-3 | The Raft core (X-1), hyper-timing's election law by suspicion, hyper-liveness's node-pair stream, hyper-durable over an anchor-RAM `LogStore` | hyper-raft R-3 (API change) | waiting |
| H-4 | Stage 3: hyper-quic, hyper-tls and hyper-transport under slates' runtime and application layer | the mantle session's `quic-tls` merge | waiting |

**H-2, membership.** One membership task on the control shard owns hyper-swim's `Detector`, hyper-datagram's
`Plane` and the plane's UDP socket. It is the shape of hyper-swim's own five-process test (hyper-raft
`crates/hyper-swim/tests/cluster.rs`):
1. poll the detector and send the probe, its relay requests and its anti-entropy chunks;
2. wait for a datagram until the detector's wake, open it, and hand each message to the detector;
3. flush, at most one packed datagram per peer.

It replaces slates' per-peer probe tasks, each of which held a full detector of its own: O(N) detectors and views per
node, against one detector with one estimator per pair and a view bounded by placement.

| Today (per-peer probe task over a QUIC probe session) | On the plane |
|---|---|
| The probe socket, a QUIC demultiplexer | The same advertised port, now the plane's UDP socket (`deploy::Plane::Probe`); the port block, the manifests and the KIND chart keep their shape |
| A probe authenticated by its session's certificate (`rostered_anchor`) | A datagram authenticated by its epoch's key, expanded from the TLS exporter of a record session whose certificate was authenticated and rostered; the epoch is tied to that peer's anchor |
| Keys | One epoch per pair's canonical connection: the record session the lower member id dials. Its dialer announces the epoch (a per-peer counter that only rises) in one record-plane exchange, and the acceptor installs the keys before it answers, so neither end seals under keys the other cannot open. Counting connections independently would drift when a handshake completes on one side only |
| `learn_member` from the acknowledgement's announced id and boot nonce | Unchanged, from the same fields of hyper-swim's `Ping`, `Ack`, `IndirectAck` and `Sync`, checked against the epoch's anchor |
| The owner lease (`lease.answered`) from the acknowledgement's standing and configuration version, timed from the probe's send | Unchanged: the task records each probe's send time by nonce, bounded by the detector's outstanding probes |
| `sample_path` from the probe's round trip | The acknowledgement's receive time less the probe's send time |
| Per-peer detectors folded into `FleetNode` by `sync_peer` | The one detector's membership folded into `FleetNode`. `FleetNode` stays the authority, since retirement goes through configuration-group records (D-14). The detector is liveness evidence |
| The indirect stage across probe tasks (`ShardState::indirect` queues, AUD-15) | hyper-swim's `PingReq` and `IndirectAck` with its relay choice; the task keeps one relay entry per target, bounded by the membership |
| Rejoin by refutation (A-15) | hyper-swim's refutation, anti-entropy and `forget` past the dissemination window |
| `DetectorTiming`, `health_multiplier`, `observe_rtt` | Measured per-pair detectors (hyper-raft `docs/timing.md` §2.7) |

Removed with it:
- slates' `crates/cluster/src/{detector,gossip,coordinates,fixed}.rs` and the driver half of `swim.rs`
  (hyper-swim's codec replaces the rest);
- the per-peer probe and probe-serve tasks and `ShardState::indirect`;
- the transport's unused control-datagram codec, seal and schedule.

Acceptance:
- the in-process fleet suite and the 3-process CLI fleet tests pass on it;
- the SWIM regressions pass, or are replaced by hyper-swim's equivalents with the reason recorded;
- KIND formation and takeover pass;
- `crates/cluster/tests/member_plane.rs` (built with H-2a) runs three members on simulated time. It checks that
  acknowledgements carry the identity, standing and round trip, that a killed member is condemned (by a survivor's
  own probes within its stated bound, or by gossip) with no live member condemned, and that a message claiming
  another sender is refused and counted.
- A real-UDP test of the task on slates' runtime comes with H-2b.

Gotcha (found building H-2a): a detector samples no round trip until it has measured its own wake lateness (`G`) and
found it non-zero (hyper-swim `Detector::granularity`). A harness that polls exactly at the asked wake never judges
anyone. The task must poll from real timer wakes, and a simulation must model a timer's lateness.

**H-3's R1 constraint.** hyper-durable's log writes files through hyper-log. slates is RAM-only (R1), so its
`LogStore` is anchor RAM, as hyper-durable's own `RamStore` is in memory. Nothing in H-3 touches disk outside a
granted landing.
