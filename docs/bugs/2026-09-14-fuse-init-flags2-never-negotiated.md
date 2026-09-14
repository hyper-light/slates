# 2026-09-14 — `FUSE_INIT` never negotiated a second-word capability (`flags2`)

**Ledger:** GAP-A9-3 ("independent ABI checks"); AC-3.10 / T-3.13; audit BUG-6's family.
**Design:** §4.6 "FUSE ABI vectors must be checked against the kernel headers independently of
the encoder"; §4.6 "Cache posture" (`EXPIRE_ONLY` is negotiated at `FUSE_INIT`).
**Baseline:** `4f5deae` (main's head on 2026-09-14); found while adding `FUSE_HAS_EXPIRE_ONLY`
to the negotiation for the invalidation delivery (`9a960bb`.. this change).

## Description

Two defects, both in `crates/bridge-fuse/src/init.rs`, each sufficient on its own to keep every
capability of the kernel's second flags word (`flags2`: `FUSE_SECURITY_CTX` .. `FUSE_REQUEST_TIMEOUT`,
including `FUSE_HAS_EXPIRE_ONLY`) from ever being negotiated:

1. **The request's `flags2` was read from the wrong offset.** `negotiate` skipped one 32-bit word
   it took for reserved padding before `flags2` and read the word after it. The header
   (`include/uapi/linux/fuse.h`, torvalds/linux master, 2026-09-14) lays `fuse_init_in` out as
   `major, minor, max_readahead, flags, flags2, unused[11]`: `flags2` follows `flags` directly at
   byte 16. The codec read `unused[0]` — always zero — so the kernel's high word was always seen
   as empty.
2. **The reply never echoed `FUSE_INIT_EXT`.** The kernel's `process_init_reply` reads the
   reply's `flags2` only when the reply's `flags` carry `FUSE_INIT_EXT`; `wanted()` did not include
   it, so even a correctly read high word would never have reached the kernel.

Observable: an independent header vector
(`crates/bridge-fuse/tests/abi.rs::the_init_reply_has_the_headers_layout`) offering every bit of
both words expected `FUSE_HAS_EXPIRE_ONLY` in the reply's `flags2` and read `0`
(`left: 0, right: 34359738368`).

## Root cause

The `fuse_init_in` layout was transcribed from memory with a padding word that does not exist;
the crate's own tests (`codec.rs`) built their INIT bodies from the same belief, so they agreed
with the codec. Nothing checked the layout against the header until the vectors of this sweep.

## Impact

No second-word capability could be negotiated: expire-only entry invalidation
(`FUSE_HAS_EXPIRE_ONLY`, needed to expire a live-source entry under a busy directory without
tearing it down) was unreachable, and any future `flags2` capability would have been too. First
word capabilities (writeback cache, readdirplus, parallel dirops, explicit data invalidation) were
unaffected.

## Exact edits

- `crates/bridge-fuse/src/init.rs`: read `flags2` directly after `flags` when `INIT_EXT` is set
  and a word remains; add `INIT_EXT` and `HAS_EXPIRE_ONLY` to `wanted()`.
- `crates/bridge-fuse/tests/abi.rs`: `a_kernel_offering_every_bit_negotiates_only_the_named_flags`
  asserts `INIT_EXT | HAS_EXPIRE_ONLY` negotiate; `the_init_reply_has_the_headers_layout` asserts
  the reply's low word echoes `INIT_EXT` and its `flags2` carries `HAS_EXPIRE_ONLY`.

## Siblings

- `fuse_init_out` (the reply) was laid out correctly (`flags2` at byte 32); no change.
- The FSKit and NFS bridges have no `INIT`; not affected.
