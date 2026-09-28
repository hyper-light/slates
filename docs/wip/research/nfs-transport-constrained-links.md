# NFS over thin and unstable links: TCP, QUIC, and where the transport belongs

Status: research note and design proposal, **ratified by Ada 2026-09-27** with these instructions:
the placement (§8.1) stands; every §5 change is built, each contested choice decided by an end-to-end
bake-off under stress, and the loser removed (no fallback kept); RPC-over-QUIC (§6) is built now and
decided by the same kind of bake-off; the measurement lane (§9) is approved. The build ledger at the end
records each slice as it lands. Ada
asked whether slates' NFSv4.x should replace TCP with a QUIC-like UDP transport, because TCP holds
up poorly on low-bandwidth or unstable links, and asked for the design to centre on low latency over
limited bandwidth. This note records the evidence, answers the question, and proposes the design.
Nothing here changes code; the decisions it asks for are listed in §8.

Evidence tiers as elsewhere: [A] peer-reviewed paper, [B] standard or vendor documentation,
[C] deployed code or its design record, [D] blog or vendor benchmark (flagged), [M] measured here.

## 1. The short answer

1. **The premise is half right.** TCP does behave badly on thin and unstable links. But most of the
   damage people attribute to TCP comes from congestion control and queueing, which QUIC inherits
   unchanged unless it is also redesigned (§2). What QUIC genuinely fixes is head-of-line blocking
   between independent requests, ACK precision, handshake round trips, and survival of an address
   change (§2.2).
2. **In slates the kernel's NFS client never crosses a network.** It mounts the daemon on the same
   host over loopback (`127.0.0.1`; `xtask/src/conformance/slates.rs`). Remote data moves
   daemon-to-daemon over the fleet transport (§4.10a). That transport is already an owned QUIC dialect
   over UDP: TLS 1.3 through `rustls::quic`, RFC 9002 loss recovery and connection ids (§4.10 "Remote
   attach"). So the TCP hop is one with no loss and no bandwidth limit, and replacing it with QUIC
   would only add cost (§3).
3. **The work Ada is really asking for belongs in the fleet session plane.** That is the hop that
   meets thin and unstable links. §5 designs it for low latency on limited bandwidth.
4. **NFS itself over QUIC (RPC-over-QUIC) has no client to talk to today.** No operating system's NFS
   client speaks it, and the draft that defines it lists no implementations. §6 records how slates
   would add it when one exists; the v4 front end is already transport-agnostic, so the cost is small.

## 2. What goes wrong on a thin or unstable link, and which layer causes it

### 2.1 Causes QUIC does not remove by itself

- **Loss-based congestion control caps throughput under random loss.** The Mathis model bounds a
  loss-based flow at roughly `MSS / (RTT · √p)` [A: Mathis, Semke, Mahdavi, Ott, CCR 27(3) 1997].
  At 1% loss and 100 ms RTT that is about 1.2 Mbit/s, whatever the link's capacity. QUIC's standard
  congestion controller is NewReno (RFC 9002 §7) [B], so the same bound applies to QUIC. So does our
  session plane (`crates/transport/src/congestion.rs`, NewReno).
- **Bufferbloat.** On a link with a large buffer, loss-based control keeps the buffer full, so every
  packet waits behind the standing queue. On a link with a small buffer, it reads non-congestion loss
  as congestion and throttles [A: Cardwell et al., "BBR: Congestion-Based Congestion Control", ACM
  Queue 14(5), 2016]. That paper notes that more than half of the world's mobile subscriptions at the
  time ran over 8–114 kbit/s links, where this behaviour dominates. The same paper reports BBR on
  Google's B4 WAN at 2–25× CUBIC's throughput.
- **Serialization delay on a slow link.** A 1 MiB write occupies a 1 Mbit/s link for about 8.4 s.
  Any request queued behind it waits that long, loss or no loss. This is a scheduling problem at the
  sender, not a transport-protocol problem (§5.3).
- **CPU cost on fast links.** Over fast links, QUIC stacks lose to TCP: up to 45.2% lower data rate
  than TCP+TLS, because of receive-side overhead (no UDP GRO, user-space ACK processing) [A: Zhang et
  al., "QUIC is not Quick Enough over Fast Internet", WWW 2024]. In-kernel QUIC for Linux measured
  3.48 Gbit/s against kTLS's 10.8 Gbit/s for 64 KiB messages (MTU 1500), and 13.5 against plain TCP's
  39.2 Gbit/s (MTU 9000). The authors attribute the gap to no GSO and an extra copy [C: Xin Long,
  "net: introduce QUIC infrastructure" v14 cover letter, 2026-07-15; unmerged]. None of this matters
  on a thin link, but R8 says the same code runs on fast ones.

### 2.2 Causes QUIC does remove

- **Head-of-line blocking between independent requests.** An NFS client sends many RPCs over one
  connection: one per session slot. On TCP a single lost segment holds back every later byte,
  including replies to unrelated calls, until it is retransmitted. QUIC recovers loss per stream
  (RFC 9000 §2) [B]. The RPC-over-QUIC draft names this as its motivation: "spreading RPC traffic
  across multiple streams enables workloads to continue largely unperturbed" (§1.1) [B]. Linux's
  `nconnect` (up to 16 TCP connections per mount, since 5.3) [B: nfs(5)] spreads that risk but keeps
  it within each connection, and adds a handshake per connection.
- **ACK precision.** A TCP SACK option carries at most three blocks when timestamps are on (four
  without) [B: RFC 2018]. A QUIC ACK frame carries any number of ranges (RFC 9000 §19.3) [B], and QUIC
  never reuses a packet number, so there is no retransmission ambiguity in RTT samples. Kakhki et al.
  attribute part of QUIC's advantage to exactly this, and find it outperforms TCP when bandwidth
  fluctuates [A: "Taking a Long Look at QUIC", IMC 2017].
- **Handshake round trips.** TCP plus TLS 1.3 costs two round trips before the first request; QUIC
  costs one. 0-RTT is forbidden for RPC, because RPC procedures are not idempotent (draft §6,
  following RFC 9289) [B]. On a 300 ms satellite RTT the saving is 300 ms per new connection.
- **Surviving an address change.** A TCP connection is its 4-tuple. A Wi-Fi-to-cellular handoff or a
  NAT rebinding kills it, and NFSv4.1 must reconnect and replay through its session reply cache
  (RFC 8881 §2.9.2, §2.10.6) [B]. That is correct but costs a handshake plus the retry. A QUIC
  connection is named by connection ids and migrates after path validation (RFC 9000 §8.2, §9) [B].
- **Congestion control without privilege.** An unprivileged Linux process may choose only from
  `tcp_allowed_congestion_control`, which by default is `reno` plus the system default [B: Linux
  ip-sysctl documentation]. R10 forbids requiring privilege, so a TCP design cannot count on BBR. An
  owned UDP transport runs whatever controller slates decides, on every OS.

### 2.3 Where QUIC measured worse

Kakhki et al. [A] measured QUIC worse than TCP under packet reordering, because Chrome's QUIC then
counted reordered packets as lost. RFC 9002 §6.1 uses a time threshold (9/8 of the RTT) as well as a
packet threshold, which is the same fix TCP's RACK makes [B: RFC 8985; reordering window min_RTT/4].
They also measured QUIC worse on 2013–2014 phones (a CPU-bound stack) and found it unfair to TCP: it
took about twice its share of a 5 Mbit/s bottleneck, because its Cubic grew its window more
aggressively. That unfairness is a reason for care, not an advantage to count.

## 3. Why the kernel↔daemon hop stays TCP on loopback

- **It has no loss and no bandwidth limit.** Every cause in §2.1 and §2.2 is absent on loopback.
  After A-39 a stat through the kernel's client costs 25 µs over NFSv3 and 43 µs over NFSv4.2
  [M: `docs/wip/BENCHMARKS.md` "NFS transports", 2026-09-26].
- **QUIC only adds cost there.** The added AEAD per packet and user-space ACKs are exactly the costs
  in §2.1, with nothing to buy back.
- **There is no QUIC client to talk to.** The only NFS clients slates serves are the kernels' own:
  Linux (TCP; RDMA), macOS (TCP, UDP for v3), and on Windows WinFsp instead of NFS. Linux's in-kernel
  QUIC is unmerged (v14, July 2026) [C], and no kernel RPC-over-QUIC client exists (draft §5: "There
  are no known implementations") [B].
- **RFC 8881 §2.9.1 requires TCP support** for NFSv4.1: "an NFSv4.1 implementation MUST support
  operation over the TCP transport protocol", and "UDP by itself MUST NOT be used" [B]. A QUIC
  transport would be an addition, never a replacement.
- **This is not a mode switch (R8).** The laptop and the fleet run the same two hops: kernel↔daemon
  on loopback, daemon↔daemon over the fleet plane. A one-node laptop simply has no second hop in use.

## 4. Where the constrained link actually is in slates

| Path | Transport | Meets a thin or unstable link? |
|---|---|---|
| kernel NFS client ↔ local daemon | NFSv3/v4.x over loopback TCP | no |
| daemon ↔ daemon: attach, chunk faults, record replication, landing coordination, membership | the owned QUIC dialect over UDP (§4.10a) | **yes** |
| an SDK or MCP client ↔ local daemon | shared-memory rings (§4.7) | no |

So "NFS over a bad link" in slates means: a volume owned by node A, mounted by an agent on node B,
with B's daemon faulting chunks and forwarding writes to A over the fleet plane (§4.10 "Remote
attach", "Ownership follows the writer"). That plane has the multi-stream design and connection ids,
but today it runs loss-based NewReno with no pacing, a fixed window, one frame per packet, and no
priority between streams (`docs/wip/fleet-transport.md` §8). Those gaps are what a thin link exposes.

## 5. Design: the fleet session plane for low latency over limited bandwidth

What a remote operation costs is three terms: **round trips × RTT + bytes ÷ rate + queueing
delay**. Each subsection attacks one term. Every number is derived from the path's own measurements,
never a constant (R3).

### 5.1 Fewer round trips

- **Zero round trips for the common case, by design.** Ownership follows the writer, so a steady
  writer's owner is local. Remote attach serves the namespace from a fetched manifest, and only chunk
  reads fault across the link.
- **At most one round trip per remote operation.** A forwarded operation is one request stream and
  one reply, never a conversation. This is the NFSv4 COMPOUND idea applied to the fleet plane.
- **Prefetch sized by the path.** §4.10 already records the paths read after an attach. The prefetch
  depth should be the bandwidth-delay product: `depth_bytes = btl_bw × min_rtt`. That keeps one
  round trip's worth of chunks in flight, and never more, because more only adds queueing (§5.3).

### 5.2 Fewer bytes

- **Never send what the peer has.** Chunks move by identity (§4.10), so the requester names
  identities it lacks and the holder sends only those. A write forwarded to a remote owner ships the
  changed range and its identity, not the enclosing chunk.
- **Compress when it saves time, decided per path.** Compress a chunk when the CPU time spent is less
  than the transmit time saved: `compress_ns(bytes) < (bytes − compressed_bytes) ÷ btl_bw`. The
  compressor's rate and ratio come from a boot or first-use measurement on this machine and data
  (the `derived!` discipline). On a 1 Mbit/s link nearly every compressible chunk passes; on
  10 Gbit/s almost none do. It is one rule with no mode, and it lives in slates-archive's existing
  codec, not a new one.
- **Fewer ACK bytes on an asymmetric link.** RFC 9000 acknowledges every second ack-eliciting packet.
  On a link whose return direction is a small fraction of the forward one, ACKs compete with requests.
  The ACK-frequency extension (draft-ietf-quic-ack-frequency) lets the sender ask for fewer ACKs; the
  cap should come from measured return-path capacity. This is a proposal to verify, not a measured need.

### 5.3 No queueing delay: priority plus model-based congestion control

This is the heart of "low latency on limited bandwidth".

- **Priority classes on one connection.** Streams carry a class: `control` (membership, fences,
  registers) ahead of `metadata` (lookups, attributes, small forwarded operations) ahead of `bulk`
  (chunk transfers, archive shipping). The sender fills each packet from the highest class with data
  ready. A metadata request therefore waits for at most the packets already handed to the path,
  never for a bulk transfer's remaining bytes. A single TCP connection cannot do this, because its
  send buffer is FIFO. `TCP_NOTSENT_LOWAT` can approximate it at the sender, but not the receiver's
  head-of-line blocking under loss.
- **Model-based congestion control with pacing.** Replace NewReno with a controller that estimates
  bottleneck bandwidth and minimum RTT, paces at the estimated bandwidth, and caps what is in flight
  near the BDP: BBR's model [A: Cardwell et al. 2016], in its version that also reacts to loss,
  because BBRv1 takes about 40% of a link against up to 16 loss-based flows regardless of fair share
  [A: Ware et al., IMC 2019]. Keeping in-flight data near the BDP holds the bottleneck queue near
  empty, so the "packets already handed to the path" above is about one BDP. On a 1 Mbit/s,
  100 ms path that is about 12.5 KB, or about 100 ms of wait, against the seconds a buffer-filling
  controller adds. The controller's inputs are measured per path, which is what R3 asks.
- **Frames sized to the latency budget.** A bulk frame need be no larger than a packet, and a packet
  no larger than the path MTU (DPLPMTUD, RFC 8899, owed in §4.10a). On a thin link the preemption
  granularity is then one packet: at 1 Mbit/s a 1,200-byte packet is about 10 ms on the wire.

### 5.4 Unstable paths

- **Migration.** Connection ids are built (A-14). What is missing is path validation (RFC 9000 §8.2)
  and reacting to a validated new path: reset the congestion state and RTT estimate for it (RFC 9000
  §9.4), keep the streams. The RTT estimator and PTO already exist (`f6f384e`).
- **Loss recovery.** RFC 9002's time-threshold detection and probe timeouts are built. Forward error
  correction is **rejected**: Google disabled it and removed it from QUIC in early 2016 for poor
  performance [A: Kakhki et al. 2017, Table 1 note and footnote 4]. Reconsider only on a measured link
  class that loses bursts too long for a retransmission within the latency budget.
- **Idle links and NAT.** A NAT drops idle UDP mappings, so the fleet's existing probe traffic must
  run more often than the measured mapping lifetime. Otherwise a quiet link comes back as an address
  change.

### 5.5 Fast links under the same code (R8)

The same plane must not lose on a 10–100 Gbit/s link (§2.1). Batch the syscalls
(`sendmmsg`/`recvmmsg`, or io_uring multishot receives, which the rt driver already has), use UDP GSO
and GRO where the OS offers them (Linux `UDP_SEGMENT`, `UDP_GRO`), and coalesce frames into a packet,
which §4.10a already owes. These are the named causes of the WWW 2024 gap.

## 6. If NFS itself must cross a network: RPC-over-QUIC

This applies only if an NFS client without a local daemon mounts a remote slates node.
draft-cel-nfsv4-rpc-over-quicv1 (datatracker, current revision fetched 2026-09-27, not an IETF
standard) [B] specifies:

- ALPN `sunrpc`, TLS peer authentication as in RFC 9289, no STARTTLS, and no 0-RTT;
- RPC record marking kept on each bidirectional stream;
- replies on the stream that carried the call;
- NFSv4.1's "connection" meaning a stream, so BIND_CONN_TO_SESSION binds a stream;
- a server that may refuse new streams from a client with many open, which the client must tolerate.

slates' v4 front end already takes a call's bytes and length regardless of transport
(`compound::serve(backend, args, request_bytes)`, A-39). So the work would be a listener that
terminates QUIC with the session plane's own stack, maps streams to the calls, and applies the §5.3
scheduler. The per-stream flow-control credit should cover the negotiated `ca_maxrequestsize`, so no
call ever waits for credit mid-request.

**Recommendation: do not build it now.** No client exists (§3). Build it when a kernel client ships
(Linux's in-kernel QUIC plus a sunrpc QUIC transport), and meanwhile recommend running the daemon on
the client host: it is RAM-only and needs no privilege, which is what slates is designed for.

## 7. Rejected alternatives

- **NFS over plain UDP.** RFC 8881 §2.9.1 forbids it for v4.1 [B]. NFSv3 over UDP has no congestion
  control and fragments large transfers.
- **SCTP.** Allowed by RFC 8881 §2.9.1 and it has streams, but macOS and Windows ship no SCTP, and
  middleboxes commonly drop it.
- **`nconnect` or several TCP connections.** This spreads head-of-line blocking without removing it,
  multiplies handshakes, and cannot prioritize across connections.
- **TCP with BBR.** It fixes §2.1's queueing but needs privilege to select on default Linux
  configurations (R10), leaves head-of-line blocking and address changes unsolved, and is Linux-only.
- **QUIC for the loopback hop.** See §3: cost without benefit, and no client.

## 8. Decisions for Ada

1. **Ratify the placement.** The kernel↔daemon hop stays NFS over loopback TCP. Constrained-link
   behaviour is the fleet session plane's job (§4.10a).
2. **Ratify the session-plane changes** in §5, each to be built piecewise with a failing test first:
   - priority classes and the packet-filling scheduler;
   - a model-based, loss-aware controller with pacing, replacing NewReno (a §4.10a/D-15 amendment);
   - compress-when-it-saves-time, derived per path;
   - DPLPMTUD and frame coalescing;
   - path validation and migration;
   - NAT-lifetime-derived probing;
   - GSO/GRO and batched I/O.
3. **Defer RPC-over-QUIC (§6)** until an NFS client implements the draft, recorded as a tripwire in
   GAPS.
4. **Authorize the measurement lane (§9)**, which needs `tc netem` inside a privileged container: a
   change inside the container only, nothing on the host.

## 9. How the claims will be measured before anything lands

These are recorded commands in the BENCHMARKS.md format. Two daemons run in one privileged Linux
container, joined by a `veth` pair shaped by `tc netem`/`tbf`:

- rate 64 kbit/s, 1 Mbit/s, 10 Mbit/s and 100 Mbit/s;
- RTT 20, 100 and 300 ms;
- loss 0, 0.1, 1 and 5%;
- reordering 0 and 1%;
- a scripted address change mid-transfer.

Each scenario measures:

- **The head-of-line gate.** p50 and p99 latency of a small metadata operation issued while a 64 MiB
  chunk transfer runs, compared with the same operation on an idle link. The acceptance target:
  p99 within one BDP plus one RTT of idle.
- **Goodput.** Bulk goodput against the link rate, and against the Mathis bound for the loss rate.
- **Migration.** Time from an address change to the first byte delivered on the new path.
- **The baseline.** The same three measurements for NFSv4.2 over TCP from the kernel client straight
  to a remote daemon (the listener bound to the veth for the experiment only). That number answers
  the original question on our own hardware, and every rejection above either survives it or is
  withdrawn.

## Sources

- Mathis, Semke, Mahdavi, Ott, "The Macroscopic Behavior of the TCP Congestion Avoidance Algorithm",
  CCR 27(3), 1997 — https://www.cs.utexas.edu/~lam/395t/papers/Mathis1998.pdf
- Kakhki, Jero, Choffnes, Nita-Rotaru, Mislove, "Taking a Long Look at QUIC", IMC 2017 —
  https://mislove.org/publications/QUIC-IMC.pdf
- Zhang et al., "QUIC is not Quick Enough over Fast Internet", WWW 2024 —
  https://dl.acm.org/doi/10.1145/3589334.3645323
- Langley et al., "The QUIC Transport Protocol: Design and Internet-Scale Deployment", SIGCOMM 2017 —
  https://dl.acm.org/doi/10.1145/3098822.3098842
- Cardwell et al., "BBR: Congestion-Based Congestion Control", ACM Queue 14(5), 2016 —
  https://queue.acm.org/detail.cfm?id=3022184
- Ware, Mukerjee, Seshan, Sherry, "Modeling BBR's Interactions with Loss-Based Congestion Control",
  IMC 2019 — https://dl.acm.org/doi/10.1145/3355369.3355604
- Lever, draft-cel-nfsv4-rpc-over-quicv1 — https://datatracker.ietf.org/doc/draft-cel-nfsv4-rpc-over-quicv1/
- RFC 8881 §2.9 (NFSv4.1 transports), RFC 9000 (QUIC), RFC 9002 (QUIC loss recovery), RFC 9289
  (RPC-with-TLS), RFC 2018 (TCP SACK), RFC 8985 (RACK-TLP), RFC 8899 (DPLPMTUD)
- LWN, "QUIC for the kernel", 2025-07-22 — https://lwn.net/Articles/1029851/; v14 cover letter,
  2026-07-15 — https://ratatoskr.run/linux-cifs/2026/07/17265232/t
- Linux `nfs(5)` (`nconnect`) — https://www.man7.org/linux/man-pages/man5/nfs.5.html; Linux
  ip-sysctl (`tcp_allowed_congestion_control`) — https://docs.kernel.org/5.10/networking/ip-sysctl.html

## Build ledger

- **Slice 1 (2026-09-27): the network under test.** The simulated fabric (`crates/rt/src/sim.rs`) now
  models a bottleneck link serializing at its rate into a bounded drop-tail queue that several flows
  share (`SimLink`), Gilbert–Elliott random and burst loss (`SimLoss`), a path MTU, a bounded receive
  buffer (the mailbox had been unbounded), and a NAT whose mapping expires and rebinds (`SimNat`), with
  drop counters by cause (`SimFabricStats`). `SimDelay` became `SimPath` (replaced, not layered).
  Proven by `crates/rt/tests/sim_path.rs`: closed-form serialization and queue arithmetic, binomial loss
  bounds, burst run length, the MTU black hole, the buffer bound, NAT expiry and rebinding, and seed
  replay. The first run found the model's own bug: an answer to an expired NAT port was counted as a
  closed port's drop.
- **Slice 2a (2026-09-27): the clocked connection and the three candidate controllers.**
  - **Clock and time machinery.** The session-plane connection (`crates/transport/src/connection.rs`)
    takes the caller's clock at every entry point. It owns the RTT estimator, and declares loss by both
    RFC 9002 thresholds (the time threshold was missing). A single `next_timeout`/`on_timeout` pair drives
    its timers: the loss timer; the probe timeout with backoff (held under the larger of the PTO and the
    initial PTO, the endpoint's existing bound); and the pacer's release.
  - **Probes are copies (RFC 9002 §6.2.4).** A probe now sends a copy of the oldest packet, which stays in
    flight; the probe used to remove it and so hid tail losses from the controller
    (`docs/bugs/2026-09-27-session-plane-probes-hid-tail-losses.md`). Persistent congestion (§7.6) is
    detected.
  - **Rate sampling and pacing.** Every acknowledgement produces a delivery-rate sample
    (`crate::delivery`, draft-ietf-ccwg-bbr-06 §4.1.2). Ack-eliciting packets are paced
    (`crate::pacer`, a token bucket of one send quantum).
  - **Window auto-tuning.** The receive window auto-tunes, doubling when a window is read within two RTTs
    (Chromium's rule), up to a derived per-session ceiling: `DaemonConfig::fleet_session_receive_bytes` =
    a quarter of the control shard's reserve over every fleet session. It had been fixed at four frames.
  - **The candidates.** The bake-off controllers are all built to their specifications:
    `congestion::newreno` (RFC 9002 §7), `congestion::cubic` (RFC 9438 with HyStart++, RFC 9406), and
    `congestion::bbr` (draft-ietf-ccwg-bbr-06). All use integer arithmetic, so a simulated history
    replays exactly.
  - **Tests.** The connection's oracle runs every loss, reorder, probe and multiplexing test under all
    three controllers. The transport, cluster and rt suites and the fleet suite (50/50) pass. The fleet
    keeps NewReno until the bake-off decides.
- **Slice 2b (2026-09-28): concurrent, prioritized exchanges — the tail's head-of-line cause.**
  - **Why.** The first bake-off grid (45 scenarios, `837142a`) showed tail latency dominated by a single
    session running one exchange at a time: a small request waited behind a whole transfer. This is the
    §5.3 priority-class design, built.
  - **Streams.** `crates/transport/src/streams.rs` owns the stream-id space. The kind, the `Priority`
    class, the initiator and a per-initiator sequence are packed into each id. Each end opens its own
    streams without collision, and a closed stream is known closed forever with no tombstones.
    Concurrency is credited with `MaxStreams` (frame kind 7, RFC 9000 §19.11). A sender past the limit
    waits, then refuses typed (`StreamRefusal::Backlogged`).
  - **Connection.** It schedules fresh data by class. The scheduler is a bake-off selector:
    `RoundRobin`, `StrictPriority` and `Weighted` (deficit round-robin). It resets and stops streams
    (`ResetStream`, `StopSending`), and reports a leak census.
  - **Endpoint.** `begin`, `take_reply`, `abandon`, `drive`, `next_request`, `reply` and `settle` run many
    exchanges at once. `settle` waits only for what the peer needs (stream data and resets).
  - **Fleet classes.** Records, consensus and SWIM run as `Control`; forwarded verbs, discovery and
    catch-up fetches as `Metadata`; content as `Bulk`.
  - **Measured.** `tests/exchanges.rs` exercises one session through a 1 Mbit/s bottleneck with a one-BDP
    queue that a 250 kB transfer keeps full. Every control ping completes within RTT + one queue drain +
    four packet times (the single-exchange session made each wait about 2 s). Exchanges past the stream
    limit survive 5 % random loss, burst loss and reordering over 4 seeds with no protocol violation.
    Abandonment and peer death leave nothing behind, and every run is bounded by a virtual deadline.
  - **Bugs found and fixed test-first.** Data past the receiver's stream limit was dropped but
    acknowledged, a deadlock. An abandoned request's bytes stayed in the endpoint. `settle` waited on
    credit frames. Probe copies toward a silent peer grew by one tracked packet per PTO (136,106 after
    90,640 virtual seconds); now bounded at the originals plus two copies. Records:
    `docs/bugs/2026-09-28-*.md`.
  - **Measured and rejected.** A transport user timeout (RFC 5482) killed live pooled sessions: 136 false
    kills in `wan_election`.
  - **Still to run.** The scheduler bake-off (p99 of the control and metadata classes under bulk load),
    and the congestion grid re-run on this code. The first grid's partial run (`3a0d86e`) predates this
    slice, and its harness had no per-run virtual deadline: one Copa run at 1 Mbit/s, 300 ms, 1 % loss
    spun for 2.5 h. The harness bounds every run and records a stall before the re-run.
- **Slice 2c (2026-09-28): packets fit the floor; credit cannot strand a sender; bounded ack state.**
  - **How it was found.** The first scheduler grid (`4a3f6d7`) disqualified every scheduler, which pointed
    at a shared layer rather than scheduling. Two findings:
    - At 100 Mbit/s and 1 % loss, NewReno held about 10 packets in flight against a BDP of about 1,100.
      Control and metadata load alone exceeded the carried rate: congestion collapse, the congestion
      bake-off's question. So the controller is decided first, and the scheduler bake-off re-runs with
      the winner.
    - At 100 Mbit/s with **no** loss, the strict and weighted schedulers got 17 % of the link. The
      `SCHED_DIAG` trace showed spurious losses from truncated oversized datagrams: the packet budget had
      counted only stream data.
  - **Fixes.** Exact packing (`Frame::encoded_len`), the derived `MAX_PACKET_PAYLOAD` = 1,171 with typed
    refusals for a bad budget or an oversized datagram, `DataBlocked`/`StreamDataBlocked` with re-armed
    reports, bounded merged ack ranges, and every simulated path carrying the floor as its MTU. Record:
    `docs/bugs/2026-09-28-packets-grew-past-the-datagram-floor.md`.
- **Slice 2d (2026-09-28): the decisions — Copa and strict priority; the losers deleted.**
  - **The congestion grid (57 scenarios, three seeds).** It took three rounds, each fixing the
    transport or controller bug the previous round exposed:
    - Copa's growth guard also blocked decreases, freezing an overshot window
      (`docs/bugs/2026-09-28-copa-froze-an-overshot-window.md`).
    - An idle peer's late acknowledgements of stream credit inflated the RTT, so a lost reply cost 7 s
      (`docs/bugs/2026-09-28-idle-peer-acks-inflated-the-rtt.md`). Stream credit now rides every
      acknowledgement, and `StreamsBlocked` (kind 10) is the liveness path.
  - **Congestion control: Copa.** On the deciding round (`0def3b4`) it was the only law that never stalled
    and stayed RTT-fair. It had the best goodput, with a shortfall geomean of 1.068 against NewReno's 15.8,
    CUBIC's 14.0 and BBRv3's 2.56. Its ping p99 was within 3.05× of the best law in every scenario.
  - **Scheduler: strict priority.** Its control p99 was within 1.15× of the best everywhere. Round-robin
    reached 4.8×, and the weighted scheduler starved the control and metadata classes.
  - **Deleted, not kept as a fallback.** `congestion::newreno`, `cubic` and `bbr`; `ControllerKind`;
    Copa-Meta; the round-robin and weighted schedulers and `Scheduler`; and the delivery-rate sampler,
    which only BBR used (Copa reads each acknowledgement's RTT sample and acknowledged bytes).
  - **The harnesses remain as benchmarks of the chosen design.** `congestion_bakeoff` became
    `congestion_bench`, and `scheduler_bakeoff` became `class_latency_bench`; each exits non-zero on a
    stall, starvation or unfairness. Record: `docs/wip/BENCHMARKS.md` "Session-plane congestion control
    and scheduling bake-offs", with the deciding rounds' raw rows under `docs/wip/research/data/`.
- **Slice 3a (2026-09-28): transport parameters — the base of path MTU discovery.**
  - **Why first.** RFC 8899 as applied to QUIC (RFC 9000 §14.3) needs a sender to know the largest
    datagram its peer reads (`max_udp_payload_size`, RFC 9000 §18.2). Until now the handshake carried a
    fixed tag (`b"slates-quic-v1"`), and the code noted the codec was owed.
  - **What.** `crates/transport/src/params.rs` encodes a type-length-value list in slates's fixed-layout
    little-endian dialect (D-15), carried by `rustls::quic` in the authenticated handshake. It holds a
    required dialect version and the largest UDP payload the end reads. Unknown identifiers are skipped
    (RFC 9000 §7.4.2); a repeated one, a wrong length, a truncation, another dialect, or a limit below
    1,200 bytes is refused typed.
  - **Endpoint.** Every endpoint decodes its peer's parameters where it binds the connection id, the
    first thing any post-handshake use does, and refuses a bad peer with `EndpointError::PeerParameters`.
    SWIM judges that `Broken`.
  - **What each end declares.** Today it declares what it actually reads, its 2,048-byte receive buffer.
    Deriving that from the host is the next slice.
  - **Tests.**
    - A golden vector.
    - Hostile-input tests: missing, truncated, `u16::MAX` length, repeated, wrong length, no dialect,
      foreign dialect, below the floor, plus every prefix and every bit flip.
    - The handshake-level refusal of a dialect-2 peer, whose bytes cross unchanged.
    - A live session in which each end holds exactly what its peer declared.
  - **Next slices.**
    - Receive buffers and the declared limit derived from the host, and don't-fragment on real sockets
      (`rt` netsys, paired per platform).
    - The RFC 8899 search, black-hole detection and raise timer in the connection, driven over the
      simulated fabric's path MTU and black hole.
    - The controller, packet budget and fleet frame cap following the discovered size, with a goodput
      benchmark.
