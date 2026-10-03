# rt: on a machine whose wake p99 is within a syscall's median, the daemon could not start (a one-slot wake ring)

**Date:** 2026-10-03. **Audit:** AUD-29-33 (a sibling of its fix). **Design:** §4.3, D-11 (derived tunables).
**Found by:** the macOS CI runner. Run 37148806956's `crates/client/tests/reap.rs` failed with
`Daemon::start … Runtime(Mem(BadCapacity { capacity: 1 }))`.

## Description

Each shard's wake ring is sized from the profile's derived `ring_entries`: Little's law at the overflow target,
`(wake.p99 / syscall.median).max(1).next_power_of_two()`. On a machine whose wake p99 is no longer than a
syscall's median, that is one slot. Since AUD-29-33, the multi-producer ring refuses one slot, because one-slot
sequences cannot tell a full slot from a free one. So the runtime's registration refused, and the daemon did not
start.

## Root cause

AUD-29-33 made the ring refuse capacities below `MIN_CAPACITY`. The derivation that feeds the ring was not
floored at that minimum. Every other runner measured a ratio of at least two, so nothing exercised it until this
runner.

## Impact

On such a machine no daemon starts at all. Until the derivation produced 1, it only affected that host.

## Exact edits

`crates/rt/src/runtime.rs` `wake_ring_entries`: the derived size, at least `slates_mem::mpsc::MIN_CAPACITY`.
`RuntimeConfig::from_profile` uses it.

## Proof

`a_derived_one_slot_wake_ring_is_raised_to_the_smallest_the_ring_admits` covers it. It fails with the floor
removed (the one-slot size stays 1). It passes with it, and the ring builds. Miri is clean on it.

## Sibling sweep

The single-producer pair rings (`SpscRing`) admit one slot, which is a power of two. Their sequences are a head
and a tail, not per-slot sequences, so the one-slot ambiguity does not arise there. The parking test rings use a
fixed capacity above the minimum.
