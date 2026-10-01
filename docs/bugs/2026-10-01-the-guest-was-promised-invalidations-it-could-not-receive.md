# The guest was promised invalidations it could not receive (AUD-29-79)

**Date:** 2026-10-01. **Audit:** AUD-29-79 (P1). **Design:** §4.6 (cache posture; virtio-fs).

## Description

The shared FUSE dispatch negotiated `FUSE_EXPLICIT_INVAL_DATA` at INIT for every transport, and
`VolumeBridge::cache_lifetime` gave the volume's own objects `Forever`, encoded as `u64::MAX` seconds of entry and
attribute validity. The native `/dev/fuse` channel earns that: it writes the seam's invalidations to the kernel before
each request. A virtio-fs guest has no such path (no notification queue is offered), yet it received the same flag
and the same lifetimes, and its capability reported explicit invalidation from the flag alone. A change made through
another attachment could stay invisible to the guest indefinitely.

## Root cause

The cache promise was a property of the dispatch, not of the transport that serves it.

## Exact edits

- `crates/bridge-core/src/authority.rs`: `CacheCoherence` (`Invalidated`, `Revalidated`); an attachment is admitted
  `Revalidated` and `Attachments::set_coherence` declares otherwise; `OpContext::coherence`.
- `crates/bridge-core/src/volume_bridge.rs`: a revalidated context's cache lifetime is `Bounded { ns: 0 }`.
- `crates/bridge-fuse/src/init.rs`: `negotiate(body, coherence)` — a revalidated transport asks no explicit
  invalidation and no expire-only entries, and asks `AUTO_INVAL_DATA` (`abi::flags::AUTO_INVAL_DATA`).
- `crates/bridge-fuse/src/bridge.rs`: INIT negotiates by the context's coherence.
- `crates/bridge-fuse/src/channel.rs`, `crates/server/src/fuse.rs`: the native channel negotiates and declares
  `Invalidated`; `crates/bridge-virtiofs/src/device.rs` records the guest's revalidated negotiation.

## Proof

`crates/bridge-virtiofs/tests/device.rs` `a_guest_without_invalidation_delivery_is_promised_no_cache`: the guest's
INIT answer carries `AUTO_INVAL_DATA` and not explicit invalidation; a created entry's, a GETATTR's and a LOOKUP's
lifetimes are zero. With the registry's default mutated to `Invalidated` the test fails (the flag is answered). The
native channel's suites on a Linux kernel mount still pass with their invalidations.

## Carried

A notification queue would let a guest cache again; a live-guest stale-read run comes with AUD-29-68.
