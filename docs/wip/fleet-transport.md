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
4. **`slates-quic`** — owed: the owned dialect over `rustls::quic` — handshake, streams, loss
   recovery, congestion — with loom on the state machine and the N=1 differential harness.
   The acceptance enforcement order over a received datagram is **built** (`accept.rs`, slice 2c);
   its fencing step and the `Keyring`'s population ride membership/enrollment.
5. **Register/placement wiring** — owed: §4.8 head + merge-record registers and D-14 placement ride
   the session plane; the N=1 ≡ simulated-fleet differential (AC-2.5 extended) is the gate.

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
