# Fleet transport: the claims plane, QUIC, and why MCP is not it

> Status: research + a decision to ratify. Written 2026-09-07 after examining `../hecate`
> at Ada's direction ("we have a specific protocol we use over QUIC, right?"). The MCP
> loopback edge is built (HTTP/1.1, `crates/mcp`); this note is about the *fleet* transport
> (host↔host, cross-region), which is Phase 8 and owed. It extends D-15's substrate, so the
> decision below is Ada's to ratify before the (large) implementation.

## 1. The two surfaces are different protocols — do not conflate them

slates, like hecate, has two network surfaces with opposite requirements:

- **The tool-plane edge (agent ↔ its local daemon).** Per-agent, same-machine. This is
  **MCP**, and MCP *only*. hecate is explicit: ADR-0002 — "MCP remains the wire for the
  tool/skill plane only"; PROTOCOL.md — "TS/Python SDKs speak MCP/JSON at the tool plane
  and never touch the claims plane." slates matches: `crates/mcp` serves the tools over
  stdio or **loopback HTTP/1.1** (built 2026-09-07). QUIC is wrong here — loopback has no
  loss, RTT or NAT for QUIC to help, and MCP clients speak HTTP anyway. *(networking
  fundamentals; MCP-client wire from memory — confirm against target clients.)*

- **The claims/fleet plane (daemon ↔ daemon, cross-region).** This is where Meta scale
  lives — volume placement, the fenced merge-record and head registers replicated to
  holders (§4.8/§4.10, D-14/D-27), landing coordination, membership/takeover. This plane
  never speaks MCP.

## 2. hecate's claims-plane protocol (the exemplar)

hecate `docs/specs/PROTOCOL.md` (ACCEPTED 2026-08-18), ADR-0002, `WIRE_SECURITY.md`:
**two planes over UDP, no TCP in the mesh** (D-10(d): "QUIC + UDP over TCP … no fallback").

- **Control plane** — stateless, header-encrypted **bare-UDP datagrams** (AES-256-GCM,
  counter-based nonces; cleartext prologue is only the key-finding minimum). Carries
  consensus votes/terms/fencing, membership, gossip, health, liveness. Loss is absorbed by
  supersession or idempotent retry; the transport never retransmits (a resent stale
  heartbeat is anti-information).
- **Session plane** — **`hecate-quic`: an *owned* QUIC** ("RFC 9000/9002 dialect-as-exemplar",
  private version + private Initial salt, **Noise-IKpsk2** handshake in CRYPTO frames,
  connection identity = host identity, a term/epoch advance kills the session). Carries all
  reliable classes: delta/turn streams, consults, claims, directed commands, content
  transfer, cross-region. Frame classes for full-packet-protected control, warden-sealed
  claims (retransmit-preserved), bulk (payload-exempt under a name-verify guard), and an
  RFC 9221 `DATAGRAM` supersede class.

The load-bearing point: it is **owned**, **not off-the-shelf** — not gRPC, not `quinn`/
`quiche`, not MCP — so the claims-plane invariants (total order, resumable
watermark-replayed delta streams, delivery classes, dedup by delta identity) live in the
wire, not emulated above a general RPC. **So I was wrong earlier to suggest `quiche` for
slates's fleet** — the model is an owned dialect.

## 3. Where slates already diverged: crypto (D-15)

slates **D-15** decided the wire independently: "fixed-layout little-endian headers …
RIFL completion records for exactly-once; credit flow control with derived windows;
**TLS 1.3 between hosts**", and explicitly **"Lost: … Noise (TLS 1.3 via rustls is the
decision)."** So slates keeps hecate's *shape* (owned, fixed-layout, exactly-once,
credit-windowed) but takes **TLS 1.3, not Noise**. D-15 is **silent on QUIC vs TCP** as the
substrate — that is the open question.

## 4. Recommendation for slates's fleet transport (Phase 8) — to ratify

Adopt hecate's **two-plane structure and owned-QUIC session model**, reconciled to D-15:

1. **Owned QUIC dialect for the session plane**, but with **TLS 1.3 (RFC 9001), not Noise** —
   which is the natural fit: QUIC *is* "TLS 1.3 between hosts" over UDP (D-15's exact words),
   with built-in stream multiplexing (thousands of per-volume streams without head-of-line
   blocking) and per-stream/connection flow control that subsumes D-15's "credit flow
   control with derived windows." rustls has a QUIC API (`rustls::quic`), so the owned
   dialect drives rustls for the TLS 1.3 state machine and owns framing/loss/streams —
   consistent with D-15 and no banned async runtime (the QUIC core is sans-io on `rt`'s UDP
   driver; **not** `quinn`/`s2n-quic`, which pull tokio, #2).
2. **Bare-UDP control plane** for consensus/membership/health, as hecate — loss-tolerant,
   no retransmit.
3. **RIFL exactly-once + fixed-layout `slates-wire` bodies** ride the session plane (D-15
   unchanged).
4. **One code path (R8):** the transport's N=1 laptop-degenerate is the local/in-process
   path; the same code scales to QUIC-across-regions, asserted by the N=1 differential test.
5. **MCP stays the tool-plane edge** (HTTP/1.1 loopback), never the claims plane.

**This extends D-15's substrate to QUIC/UDP** (D-15 named TLS 1.3 but not the substrate).
That is a design amendment — Ada's to ratify — before the implementation, which is large
(an owned QUIC dialect is a subsystem, not a slice) and belongs to Phase 8. Recorded here
so the decision and its evidence precede the code.

## 5. Rejected for the fleet: off-the-shelf stacks

- `quinn`, `s2n-quic` — pull tokio (#2, banned).
- `quiche` — sans-io (runtime-OK) but off-the-shelf: it would emulate the claims-plane
  invariants above a general QUIC rather than owning them, against the ADR-0002 reasoning
  slates inherits. Kept on record as measured-by-reasoning, not chosen.
- TCP+TLS — loses QUIC's per-stream multiplexing (head-of-line blocking across many volume
  streams) and connection migration (node drain/takeover, D-14); D-10(d) in hecate rejects
  TCP in the mesh outright.
