# The handshake sent its certificates in plaintext (AUD-29-47)

## Description

- **The flattening.** `Endpoint::drain_handshake` concatenated every handshake byte TLS wrote, across
  encryption levels. It kept only the 1-RTT keys and dropped the Handshake keys.
- **What crossed in the clear.** `send_flight_at` sent the concatenation as plain fragments. The
  Handshake-level messages were readable by any on-path observer: EncryptedExtensions, both
  certificates, CertificateVerify and Finished.
- **The audit's reproduction.** Its probe (§7.4.2) found the server certificate's DER in the fragment
  payload.

## Root cause

- **Protection was never implemented.** `rustls::quic` hands the transport unencrypted handshake bytes
  and a key change at each level boundary. Protecting each level's bytes under that level's keys is the
  transport's job (RFC 9001 §4.1.3–4.1.4), and it was not done.

## Fix (the owned dialect, D-15)

- **Draining by level.** `drain_handshake` returns a `Flight { initial, handshake }`: bytes a `write_hs`
  call writes belong to the level it was writing at. It keeps the Handshake keys, and it refuses a byte
  written after the 1-RTT keys (tickets are off, so none is expected).
- **Sending.**
  - Initial-level bytes cross as the plain fragments.
  - Handshake-level bytes cross as sealed fragments: `[0x81][number: u32 LE][sealed body][tag]`. The body
    is the plain fragment's start, within, total and payload, sealed under the local Handshake packet key,
    with the tag and number as associated data (`crate::flight::{seal, open}`).
  - Each level has its own stream offsets. Every sealed fragment, retransmits included, takes a fresh
    number, and a number with no successor is refused (`NumbersExhausted` → `PacketNumbersExhausted`).
- **Receiving.**
  - One reassembler per level.
  - A sealed fragment opens under the remote Handshake key. One that arrives before the key waits,
    bounded at one flight's fragments, and is opened when the key arrives.
  - One that does not open is malformed: dropped, counted, never fed to TLS.

## Evidence

- **`crates/transport/tests/handshake_levels.rs`.** A recording relay sits on the simulated fabric between
  a mutually authenticating client and server.
  - `no_certificate_crosses_the_wire_in_plaintext`: red when Handshake-level bytes are filed under the
    Initial level (the old flattening), "a certificate crossed in plaintext"; green now.
  - `a_tampered_sealed_fragment_is_refused_and_its_retransmit_completes`.
  - `a_path_that_tampers_every_sealed_fragment_never_establishes` (typed `NotReady`).
  - `a_sealed_flight_ahead_of_its_hello_completes`.
- **`crate::flight` unit tests.** Sealed fragments open to the flight and carry no plaintext byte of it;
  tampered or foreign fragments do not open; numbers end without a repeat.
- **Suites.**
  - The transport and cluster suites pass. The fleet suite passes 59/59; its daemons handshake through
    this path.
  - The CLI flows with `SLATES_TEST_CLI=1` pass 12/13. The OCI container flow failed on Docker Desktop's
    own storage ("blob … input/output error" from containerd, whose VM disk is full), not on this change.
