# Session keys were never updated, and their use never counted (AUD-29-48)

## Description

- **No key updates.** The 1-RTT keys from the handshake protected a session for its whole life.
  `KeyChange::OneRtt`'s `next` secrets were dropped, and the short header's key-phase bit was always zero.
- **No usage accounting.** Nothing counted packets sealed under a key or packets that failed to open.
  AES-GCM's confidentiality limit (2^23 packets per key, RFC 9001 §6.6) is far below the packet-number
  space, and invalid packets were discarded forever without counting toward the integrity limit.
- **The control plane too.** Its seal bounded its nonce counter only at `u64::MAX`.

## Root cause

- **Missing, not broken.** The key lifecycle of RFC 9001 §6 was never implemented.

## Fix

- **The schedule (`crate::keys::OneRtt`).**
  - It holds the current generation, the next one (derived ahead from `Secrets::next_packet_keys`) and
    one previous receive key.
  - It seals under the current generation, updating (flipping the key phase) when the generation has
    sealed its limit and the peer has acknowledged one of its packets. At the limit without that
    acknowledgement it refuses `ConfidentialityExhausted`.
  - It opens a current-phase packet under the current key. An other-phase packet numbered below the
    current generation's first opened packet opens under the previous key; any other other-phase packet
    is tried under the next generation, and adopting it is the update, for both directions.
  - Every failed open counts toward the integrity limit, and at the limit the session refuses
    `IntegrityExhausted`.
  - Header protection keeps the first generation's keys (RFC 9001 §6).
- **The endpoint.**
  - `protect_packet` seals through the schedule and writes the key-phase bit; `unprotect_packet` opens
    through it.
  - A failed open stays in the discard class, and the limits end the session `EndpointError::KeysExhausted`.
  - Acknowledgements confirm the current generation.
  - `Endpoint::cap_key_usage` caps the limits for an operator policy or a test.
  - The swim probe judges `KeysExhausted` broken.
- **The control-plane seal.** The `Sealer` refuses past 2^23 datagrams per key, and the `Opener` counts
  failed opens and refuses past 2^52.

## Evidence

- **Endpoint unit tests** (real handshake-derived keys).
  - `a_confirmed_generation_updates_at_its_limit_and_stragglers_still_open`.
  - `an_unconfirmed_generation_at_its_limit_seals_nothing_more`.
  - `forgeries_end_the_session_at_its_integrity_limit`.
- **`crates/transport/tests/key_update.rs`.** Eight packets per generation over forty live exchanges
  moves 13 generations each way, with every reply correct and zero forgeries.
- **Seal tests.** `a_key_at_its_confidentiality_limit_seals_nothing_more` and
  `a_key_at_its_integrity_limit_opens_nothing`.
- **Suites.** Transport and cluster pass, the fleet suite passes 59/59, and the three-daemon CLI flow
  passes.
