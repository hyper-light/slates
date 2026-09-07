# §4.10a Fleet transport — the claims plane (design draft, to ratify)

> Status: **design draft, awaiting ratification** (2026-09-07). slates has no design section
> for the inter-node transport yet (Phase 8, owed); the project's law is "read the design
> section before writing a line," so this draft is the section. It adapts hecate's accepted
> claims-plane protocol (`../hecate/docs/specs/PROTOCOL.md`, ADR-0002, `WIRE_SECURITY.md`) to
> slates, reconciled to slates's own **D-15** (TLS 1.3, not Noise). The evidence and the
> rejected alternatives are in `research/fleet-transport.md`. **It amends D-15's substrate to
> QUIC/UDP** — a numbered decision, Ada's to ratify — so no code lands from this until then.

## 1. Scope and the R8 spine

This is the **daemon↔daemon** plane: volume placement, the fenced head/merge-record registers
replicated to holders (§4.8/§4.10, D-14/D-27), landing coordination, membership/takeover. It is
**never MCP** — MCP is the agent-facing tool edge only (built, HTTP/1.1 loopback; `crates/mcp`).

**One code path (R8).** The transport degenerates at N=1 to a local, in-process delivery (a
loopback UDP socket, or a direct in-memory channel under the deterministic sim driver) and scales
unchanged to QUIC-across-regions; the N=1 differential test asserts identical observable semantics
(register commit, delta replay, refusal taxonomy) on one node and on the simulated fleet. **No mode
switch** (R8, D-10(d) verbatim: "no fallback. Period.").

## 2. Two planes over UDP (adapting hecate §1)

No TCP in the mesh. Every node speaks two planes on one UDP substrate:

- **Control plane — stateless header-encrypted datagrams.** Consensus (the configuration group of
  D-14: membership, takeover, neighbourhood, home changes — *not* per-write), liveness, gossip,
  health piggyback. Loss is absorbed by supersession or idempotent retry; the transport never
  retransmits (a resent stale heartbeat is anti-information). No connection state.
- **Session plane — the owned QUIC dialect (`slates-quic`).** Every reliable class: register
  writes/reads (head + merge-record ledger entries, §4.8), delta/base streaming, landing
  coordination, directed commands, cross-region. Streams multiplex per volume/register so one
  peer-pair's thousands of independent flows never head-of-line-block each other.

## 3. Crypto: TLS 1.3, not Noise (slates D-15)

The divergence from hecate. The session plane is an owned QUIC dialect whose handshake and record
protection are **TLS 1.3 via `rustls` (`rustls::quic`)**, RFC 9001 — which is exactly D-15's "TLS 1.3
between hosts," now over UDP. The control plane's datagrams are sealed with an AEAD (AES-256-GCM)
under keys derived from the same host-identity TLS credentials (a control-plane key schedule keyed
by the node's enrollment identity + a key epoch), counter-based nonces (never random; reuse is a
typed, counted refusal — the no-panic law). Connection identity = host/enrollment identity; a
term/epoch advance kills a session with a typed reason (fencing, D-16/§4.8). Cleartext on the wire
is only the key-finding minimum (the prologue below).

`rustls` is the one new external dependency, and it is the one D-15 already named. The QUIC core
(framing, loss recovery, streams, congestion) is **owned and sans-io** — driven by `rt`'s UDP
readiness driver — so it pulls **no async runtime** (#2; not `quinn`/`s2n-quic`, which do).

## 4. Wire layout (the part the first codec slice builds)

Fixed-layout little-endian headers; canonical `slates-wire` bodies with a schema hash (D-15). A
**control datagram** on the wire:

```
cleartext prologue (the key-finding minimum), fixed layout:
  ver:         u8     protocol version (floor 1)
  sender:      u64    the sending node's id (key lookup)
  key_epoch:   u32    which of the sender's key epochs sealed this
  sealed_len:  u32    the sealed region's byte length (bounds-checked before read)
sealed region (AEAD; a later slice):
  nonce:       12 B   = 64-bit per-(key,direction) counter ‖ 32-bit channel id
  ciphertext + 16-byte tag over the ENVELOPE:
     envelope header (fixed layout):
        kind:       u8    the message kind (a closed enum)
        class:      u8    the delivery class (§4.8 durability/ordering)
        flags:      u8    must-be-zero (a set bit is a typed refusal)
        epoch:      u64   the sender's region-membership epoch (fencing)
        hlc:        u64   hybrid-logical clock (liveness/replay window only)
        request_id: u64   pairs a reply with its request (RIFL, D-15)
     body: a canonical slates-wire value + its schema hash (D-15)
```

The **session plane** reuses the same envelope inside QUIC `STREAM`/`DATAGRAM` frames; the QUIC
packet/frame layout is the RFC 9000 dialect (private version + private Initial salt so it never
collides with public QUIC), specified in the next design slice.

The **first codec slice** builds the *cleartext prologue* + the *plaintext envelope header* +
`slates-wire` body — encode/decode over byte slices, every length checked against `sealed_len`/the
class cap before allocation, golden vectors and hostile-input tests (truncation, bad version,
must-be-zero flags set, a wild `sealed_len`), no crypto yet (the AEAD seal wraps this plaintext in
the crypto slice). This is pure and testable on every host, exactly like `bridge-fuse`'s ABI codec.

## 5. Phasing (each slice pure/testable before the network one)

1. **Wire codec** (this design → code): prologue + envelope header + body, golden + hostile tests.
2. **Control-plane seal**: the AEAD envelope (AES-256-GCM), nonce-counter discipline, key lookup
   (unknown sender = drop, zero crypto spent), replay window — determinism + reuse-refusal tests.
3. **`rt` UDP driver**: a UDP readiness source in each platform driver (kqueue/epoll/uring/iocp/sim),
   the sim arm first so the whole plane is deterministic-testable at N=1.
4. **`slates-quic`**: the owned dialect over `rustls::quic` — handshake, streams, loss recovery,
   congestion — with loom on the state machine and the N=1 differential harness.
5. **Register/placement wiring**: §4.8 head + merge-record registers and D-14 placement ride the
   session plane; the N=1 ≡ simulated-fleet differential (AC-2.5 extended) is the gate.

## 6. What this does not change

MCP stays the tool-plane edge (HTTP/1.1 loopback, built). `slates-wire` (payload codec) and the RIFL
completion records (D-15) are reused unchanged. The local ring IPC (§4.7, D-10) is untouched — it is
the agent↔daemon path, not the mesh.
