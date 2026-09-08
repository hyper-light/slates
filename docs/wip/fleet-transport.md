# §4.10a Fleet transport — the claims plane (design draft, to ratify)

> Status: **design draft, awaiting ratification** (2026-09-08). slates has no design section
> for the inter-node transport yet (Phase 8, owed); the project's law is "read the design
> section before writing a line," so this draft is the section. It adapts hecate's accepted
> claims-plane protocol (`../hecate/docs/specs/PROTOCOL.md`, ADR-0002, `WIRE_SECURITY.md`) to
> slates, reconciled to slates's own **D-15** (TLS 1.3, not Noise). The evidence and the
> rejected alternatives are in `research/fleet-transport.md`. **It amends D-15's substrate to
> QUIC/UDP** — a numbered decision, Ada's to ratify.
>
> Built so far (`crates/transport`, `crates/rt`): slice 1 (the wire codec), slice 3 (the `rt` UDP
> driver, real kqueue/epoll + the deterministic sim fabric), and slice 2 (the crypto): the
> AES-256-GCM seal (`seal.rs`: `Sealer`/`Opener`, counter nonces, replay/forgery/exhaustion refusals,
> a golden vector) **and** the HKDF-Expand-Label key schedule (`schedule.rs`: per-channel keys from a
> control secret, RFC 5869 + RFC 8446 §7.1, RFC known-answer + golden + schedule→seal→open tests).
> The schedule takes the control secret **injected**, and the seal takes the key **injected** — that
> injection is the ratification boundary: **enrollment** (the source of the secret and the
> acceptance-point key lookup, §4.13) and the **QUIC session plane** are not coded, and wait on your
> ratifying this section (the D-15→QUIC/UDP substrate amendment). Ada authorized proceeding on the
> crypto 2026-09-08 ("just … do it"); enrollment and QUIC remain the owed subsystems below.
>
> The whole **control plane is proven end to end over UDP** (`crates/transport/tests/plane.rs`): a
> node derives its keys, seals a datagram, and sends it over a `UdpSocket`; the peer receives the
> bytes and runs the acceptance order, recovering the authentic datagram — deterministically at N=1
> on the simulation UDP fabric, no OS network. So slice 1 (codec) + slice 2 (seal, schedule, accept)
> + slice 3 (UDP driver) are a working control plane; what is left is the session plane (QUIC) and
> the identity that populates the keyring (enrollment).
>
> The **session plane is now proven end to end over UDP too** (`crates/transport/tests/session.rs`,
> slice 4f): two `Endpoint`s complete the `rustls::quic` TLS 1.3 handshake over the UDP socket, then
> the client sends a stream in packets protected by the 1-RTT keys and the server reassembles it byte-
> for-byte — the whole session stack (handshake + packet protection + framing + stream reassembly)
> live at N=1 on the sim fabric. The layers above it (reliability, flow-credit, congestion, header
> protection, multi-stream) are built or owed as listed in slice 4; enrollment still populates neither
> plane's identity.

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

> Buildability confirmed (2026-09-08, probe then reverted): `rustls = { version = "0.23",
> default-features = false, features = ["ring", "std"] }` **builds in this sandbox** — `ring` 0.17
> ships prebuilt asm for `aarch64-apple-darwin`, so no `cmake`/C-toolchain step (the default
> `aws-lc-rs` provider needs `cmake`, which is absent and banned to install, so **use the `ring`
> provider**). So the handshake has no environment gate. When it is built: slates authenticates with
> the **enrolled identity as a raw public key** (RFC 7250 — rustls 0.23 supports raw keys), *not* CA
> PKI, and `ring` (pulled by rustls) also mints the test keys, so no `rcgen`. The handshake must be
> test-driven (a client↔server handshake completing and producing matching QUIC keys), so it lands
> as its own careful slice with rustls's `quic` + raw-key API to hand — not rushed blind.

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

1. **Wire codec** — **built** (`crates/transport/src/lib.rs`): prologue + envelope header + body,
   golden + hostile tests, on every host.
2. **Control-plane seal + key schedule** — **built** (`crates/transport/src/seal.rs`,
   `schedule.rs`): AES-256-GCM under a key the **HKDF-Expand-Label schedule** (RFC 5869 + RFC 8446
   §7.1, `hkdf`/`sha2`) derives per `(sender, key_epoch, direction)` from an injected control secret;
   the nonce-counter discipline, replay/forgery/exhaustion refusals, a seal golden vector, the RFC
   5869 known-answer vector, and a schedule→seal→open end-to-end test. **Owed:** the *source* of the
   control secret (enrollment, §4.13) and the acceptance-point key lookup (unknown sender ⇒ drop,
   zero crypto spent) that lives with it — the schedule takes the secret injected, exactly as the
   seal takes the key injected.
3. **`rt` UDP driver** — **built** (`crates/rt/src/udp.rs`, `driver.rs`, `sim.rs`): a UDP readiness
   source in kqueue and epoll, plus the deterministic sim fabric (the sim arm, so the whole plane is
   testable at N=1); io_uring/IOCP register-readable are typed-owed.
4. **`slates-quic`** — **built:** the frame codec (4a), ordered streams both sides (4b), the
   reliability core / ACK+loss recovery (4c), the flow-control credit law (4d), the
   `rustls::quic` TLS 1.3 handshake with pinned-identity auth (4e), the packet-number codec (4g,
   `packet_number.rs`; RFC 9000 §17.1 + App. A, oracle-tested), and the **live `Endpoint`** (4f,
   `endpoint.rs`) that drives the handshake to the 1-RTT keys over the `rt` UDP socket, protects each
   packet to full RFC 9001 shape — an RFC 9000 short header (§17.3) carrying a truncated packet number,
   payload AEAD with that header as associated data, and **header protection** (§5.4) masking the first
   byte and packet-number field — and carries a stream end to end. Proven by the N=1 live-session
   integration test (`tests/session.rs`: two endpoints handshake over the sim UDP fabric, the client
   sends a stream, the server reassembles it byte-for-byte) and unit tests that header protection
   genuinely masks the header (non-vacuity) and a tampered packet is refused. The **sans-io connection
   driver** (4h, `connection.rs`) composes the stream, reliability and (soon) flow layers into
   *reliable, ordered, exactly-once* delivery over a lossy, reordering path — `poll_transmit` /
   `handle_incoming` with no socket or clock — recovering a mid-stream drop by the reorder threshold and
   a lost tail by a probe (RFC 9002 §6.2), proven by a proptest that any loss pattern still delivers the
   exact stream, with a retransmit counter as the non-vacuity check. The **`Endpoint` now pumps that
   `Connection`** (4i): `send_stream`/`recv_stream` protect what it wants to send and feed it what
   arrives, so the live session delivers a stream *reliably* with acknowledgements flowing (proven over
   the lossless sim by `tests/session.rs`; the loss/probe paths are the oracle's, and driving the probe
   from a real timeout is owed with the runtime timer). TLS session-resumption tickets are disabled on
   the server (slates authenticates by enrolled identity, not TLS resumption; a post-handshake
   `NewSessionTicket` would otherwise reach the 1-RTT packet reader as unparseable CRYPTO bytes).
   **Flow control is enforced** (4j): the `Connection` runs the `flow.rs` credit law — the receiver
   advertises `MaxStreamData` a bounded window ahead of what it has read (piggybacked on every
   acknowledgement, so a lost credit frame re-advertises), and the sender frames only within it. The
   initial window is derived — `(REORDER_THRESHOLD + 1) × frame_cap`, the least in-flight budget that
   keeps reorder-based loss detection working (BDP autotuning owed). The oracle now drives a stream
   through a window far smaller than the object and asserts the **never-whole-object** invariant
   (`send_offset ≤ read_offset + window`) holds under loss.
   The connection **multiplexes many streams** (4k): several independent ordered byte streams share one
   connection (one handshake, one packet-number space, one reliability/ack machine), each with its own
   flow window, served round-robin so none starves; a received frame is demultiplexed to its stream's
   reassembler. Proven by an oracle that drives several fingerprinted streams through any loss pattern
   and reassembles each exactly, never confusing one for another.
   **Owed on the connection:** a real probe timeout, congestion control (its validation needs a real
   network), connection-level `MaxData`, multi-range ACKs, connection IDs, several frames per packet (an
   MTU budget), and loom on the state machine.
   The acceptance enforcement order over a received datagram is **built** (`accept.rs`, slice 2c); its
   fencing step rides membership. The **`Keyring`'s population is built** (`enrollment.rs`, slice 6):
   `Enrollment::from_membership` turns an admitted-membership record (the shared control secret + the
   enrolled member ids) into this node's control `Sealer` and a keyring of `Opener`s over its peers,
   proven end to end against the control plane (an enrolled peer's datagram opens; an un-enrolled sender
   is refused `UnknownSender`; a wrong-secret datagram fails the seal). The **distribution** of the
   secret and the list — configuration-group admission, human-authorized (§4.13) — is the owed half,
   designed in `docs/wip/enrollment.md` (a draft to ratify).
5. **Register/placement wiring** — the **request/reply RPC seam is built** (`endpoint.rs`
   `request`/`serve_once`, slice 5a): a client sends a request reliably as a stream and receives the
   peer's reply on the same stream id (the reply travels the other direction over one `Connection`),
   proven live end to end (`tests/session.rs`: a 250-byte request, a transformed reply, exact). This is
   the seam §4.8's "lookups route by id to the current owner" and the owner→holder record ship use.
   **Owed:** wiring `crates/db/src/register.rs` onto it — the owner ships a head/merge-record register
   to its `2f+1` candidate holders and commits at `f+1` acks (`Placement`/`Quorum`), holders fence by
   host epoch (`Fence`) — and the N=1 ≡ simulated-fleet differential (AC-2.5 extended) as the gate.
   Whether the holder's ack rides a session reply (built) or a control datagram is the to-ratify shape
   (recommendation: the session reply, so one `Connection` and one flow/loss machine carry both).

## 6. What this does not change

MCP stays the tool-plane edge (HTTP/1.1 loopback, built). `slates-wire` (payload codec) and the RIFL
completion records (D-15) are reused unchanged. The local ring IPC (§4.7, D-10) is untouched — it is
the agent↔daemon path, not the mesh.

## 7. Security (design draft, to ratify) — the pre-crypto artifact for slice 2

Crypto is the one place the project's "cite tiered evidence, design before code" rule binds hardest,
so this fixes the seal, the key schedule and the enforcement order before any AEAD lands. It adapts
hecate `WIRE_SECURITY.md` to slates's D-15 (TLS 1.3, not Noise). **Ratify before slice 2 codes it.**

- **Identity is the host's enrollment credential** (§4.13 principals, D-16 fencing). A node holds a
  long-lived enrollment key pair; the region's configuration group (D-14) is the authority that admits
  a node and stamps its **key epoch**. This is the identity both planes key from; no per-pod identity
  (slates has volumes and shards, not pods).
- **The session plane keys via TLS 1.3.** `slates-quic` runs the RFC 9001 TLS 1.3 handshake
  (`rustls::quic`) authenticated by the enrollment credential; record protection is TLS 1.3's own
  (AES-256-GCM or ChaCha20-Poly1305 as negotiated). No bespoke session crypto — this is the whole
  reason D-15 chose TLS 1.3 over Noise. A term/epoch advance (D-16) drops the session, typed.
- **The control plane seals each datagram with an AEAD** derived from the same identity. The key
  schedule: `HKDF-Expand-Label` (RFC 5869; the TLS 1.3 labelled form, RFC 8446 §7.1) from a control
  secret established at enrollment, one key **per (sender, key_epoch, direction)**. Cipher:
  **AES-256-GCM** (the RustCrypto `aes-gcm` crate — a vetted implementation, never hand-rolled). Nonce:
  **a 96-bit counter** = 64-bit per-(key,direction) message counter ‖ 32-bit channel id, **never
  random**; the counter is persisted-enough that reuse across a restart is refused, and any reuse is a
  **typed, counted refusal** (the no-panic law — no assertion path). The sealed region wraps exactly
  the plaintext envelope + body the codec (slice 1) already lays out; the prologue's `sealed_len`
  becomes `len(nonce) + len(ciphertext) + 16-byte tag`.
- **Enforcement order at every acceptance point** (host parsers both planes), adapted from hecate
  PROTOCOL.md §1.3: length caps → prologue parse → **key lookup (unknown sender ⇒ drop, zero crypto
  spent)** → AEAD verify+decrypt (one bounded pass) → envelope parse (flags must-be-zero, version) →
  **fencing check (epoch/term)** → replay window → decode. The HLC bounds the replay window's memory
  (liveness only); **safety rests on the nonce counter + AEAD + fencing**, never the clock. Garbage
  without a key dies at the tag, counted. Identical categorized counters on every path.
- **Slice 2 (crypto) breakdown:** (a) the key-schedule types + HKDF derivation (test vectors) —
  **built** (`schedule.rs`: `KeySchedule::from_control_secret`/`seal_key`/`sealer`/`opener`, RFC 5869
  known-answer + a golden + a schedule→seal→open test; control secret injected); (b) the AEAD
  seal/unseal over the codec's plaintext (round-trip, tamper ⇒ typed refusal, nonce-reuse ⇒ typed
  refusal, golden vectors) — **built** (`seal.rs`, key injected); (c) the enforcement-order parser —
  **built** (`accept.rs`: length cap → prologue → **key lookup that drops an unknown sender before
  any crypto** → AEAD verify/decrypt/replay, over an injected `Keyring` trait; a test proves an
  unknown sender with a corrupt body is `UnknownSender`, not `BadSeal`). The **fencing** step
  (epoch/term) is owed with membership; the `Keyring`'s *population* is owed with enrollment.
  Key **distribution/enrollment** (the region group admitting a node, minting the control secret) is
  its own slice tied to §4.13/§4.8 membership — the largest remaining piece, and the one most needing
  your review, since it is where identity and authority live. **What (b) deliberately does not do**
  without ratification: derive or persist a key, look one up by sender, or enforce fencing — it is the
  primitive, exactly as the codec is the wire shape without a socket.

## 8. The session plane (slice 4) — the owned QUIC dialect, TLS 1.3 not Noise

> Draft to ratify (2026-09-08): the framing decisions below are architecture-level (Ada's to
> ratify). The **frame codec** is built as the pure foundation (`crates/transport/src/session.rs`),
> as the control-datagram codec was; the ordered streams (4b), reliability/ACK/loss (4c), flow-credit
> law (4d), `rustls::quic` TLS 1.3 handshake (4e), the packet-number codec (4g, `packet_number.rs`),
> and the live `Endpoint` that wires the handshake, full RFC 9001 packet protection (short header +
> truncated packet number + payload AEAD + **header protection**), and a stream together over the UDP
> socket (4f, `endpoint.rs` + `tests/session.rs`) are built; congestion control, connection IDs, and
> multi-stream multiplexing remain.

slates's session plane carries every **reliable** class (version chains, merge records, content
transfer, cross-region). It is an owned RFC 9000/9002-shaped dialect — adapting hecate-quic's
ordered streams and flow-control law — reconciled to slates:

- **TLS 1.3, not Noise.** The handshake runs over `rustls::quic` (D-15; hecate uses Noise-IKpsk2,
  which slates dropped). Connection identity is the host's enrollment identity; a term/epoch advance
  kills the session with a typed reason (D-16 fencing).
- **No warden/pod frame classes.** hecate's four classes exist because a pod's payload is sealed past
  the host it never trusts; slates has no pods, so a host touches payloads directly and the dialect
  needs only the ordinary QUIC frames. (This is the biggest simplification from hecate.)
- **Ordered streams.** A stream per (session, subject); records carry an absolute `offset`;
  delivery is strictly ordered, no gaps (the ordered-log archetype). The green-chain subscription
  (§4.16) is a named subject.
- **Flow control is the ratified credit law, in the transport** (built, `flow.rs`, slice 4d).
  Dual-level (stream and connection), **absolute-offset** credits (idempotent under loss/reorder) a
  bounded window `window_ahead` beyond what the app has consumed; the **never-whole-object-in-credit**
  invariant is proven by `a_stream_flows_through_a_bounded_window` — a 200-byte stream flows through a
  40-byte window, the sender never racing more than a window ahead of the reader, every byte arriving.
  `window_ahead` is derived (`k × frame_cap`, BDP-autotuned — owed), not a constant.

**The frame set** (fixed-layout little-endian, slates's wire style, D-15 — not QUIC's varints), the
payload of a TLS-1.3-protected packet:

- `STREAM { stream_id, offset, fin, data }` — ordered stream data at an absolute offset.
- `ACK { largest, range }` — acknowledges packet numbers `[largest - range, largest]`.
- `MAX_DATA { max }` — the connection's absolute flow-control credit.
- `MAX_STREAM_DATA { stream_id, max }` — a stream's absolute flow-control credit.

**Built (slice 4a):** the frame codec — encode/decode of a packet payload's frame sequence, every
length bounds-checked, hostile-fuzzed, never a panic. **Built (slice 4b):** both sides of an ordered
stream (`stream.rs`). Receive — `StreamAssembler`: `Stream` frames at absolute offsets buffered under
a bounded window and drained contiguously, in order, once each (idempotent under reorder/duplicate/
overlap), with a proptest oracle that reassembles a split-and-shuffled-and-duplicated stream to the
original. Send — `StreamSender`: buffered bytes framed **only within the flow-control credit** the
peer grants, at most a frame cap per frame (the never-whole-object-in-credit invariant enforced on
send), and a `send_and_receive_round_trip` test proves the two sides compose into reliable ordered
delivery. An offer past the window is a typed refusal.
**Built (slice 4c):** the reliability core (`conn.rs`) — packet-number assignment, ACK generation
(receive side: the top contiguous run), and ACK processing with loss detection (send side: free the
acknowledged, declare a gap lost past the RFC 9002 reorder threshold, retransmit its frames), proven
by a `reliable_delivery_survives_loss` test that drops a packet and shows the whole stream still
arrives once each (the assembler dedups the retransmit). **Built (slice 4e):** the **TLS 1.3
handshake over `rustls::quic`** (`handshake.rs`) — a node authenticates with its enrolled identity as
a **pinned certificate** (no CA PKI), the client↔server handshake completing over the `quic`
`read_hs`/`write_hs` interface and negotiating TLS 1.3, with a test that a **wrong pin is rejected**
(authenticated, not permissive). `rustls` uses the `ring` provider (no cmake); its config `Arc` is
D-8's sanctioned exception. **Owed:** multi-range ACKs, timer-based tail-loss recovery, congestion
control, the credit accounting that *sets* the window from the peer's `MaxStreamData`, and the
`Connection` that wires streams+reliability+flow+handshake+record-protection onto the `rt` UDP driver
(the control plane's `accept`/seal are the datagram-plane analogue already wired). CRYPTO frames are
`rustls::quic`'s, not ours; the handshake keys drive the record protection (owed with the wiring).
