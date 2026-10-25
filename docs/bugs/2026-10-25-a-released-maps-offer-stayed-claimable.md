# A released map's offer stayed claimable

- **Date:** 2026-10-25
- **Status:** fixed 2026-10-25, in the same change as this record (with A-173).
- **Subsystem:** the direct map's hand-off on macOS, `crates/server/src/direct_map.rs` (the hand-off, `release_map`)
  with the socket route's offers, `crates/server/src/shards_route.rs`, and the reap, `crates/server/src/reap.rs`
  (§4.6; A-172, A-173).
- **Severity:** a bound broken: daemon memory held outside the client's share and the fill budget, for up to an offer's
  life (two liveness budgets) per offer. No bytes of another file were reachable: each offered object is one file's
  copy, read alone.
- **Exposure:** macOS only, and only in the change that introduced the macOS hand-off (never committed): Linux sends a
  copy's descriptor before the reply, with no offer to outlive it.
- **Found by:** asking, while writing the hand-off, what bounds the offers table, then a failing test.

## Symptom

`crates/client/tests/client.rs`, `a_released_maps_unclaimed_offer_is_withdrawn`, on this Mac before the fix: a map
requested by hand on the ring, its offer left unclaimed, the map released, and then the offer claimed with its own
secret.

```
the released copy's offer was still claimable: Ok(OwnedFd { fd: 23 })
```

The claim handed over a live descriptor of a copy the daemon had already let go, no longer counted against the client's
share nor charged to the fill budget.

## Root cause

A copy is held on its client's slot and counted against the client's share from the moment it is made (A-172). The
offer on the socket route holds a second descriptor of the same object, and it was dropped only by its claim or by its
expiry. A release, and a reap, let go of the copy without touching its offer, so the object stayed alive in the offer
while the share and the budget forgot it. A client that mapped, never claimed, released and mapped again could keep one
live object per round for the offer's whole life: a structure bounded by time and rate, not by a derived bound (banned
item 8).

## Fix

One function lets go of a copy, `direct_map::let_go`: it withdraws the copy's unclaimed offer
(`shards_route::withdraw`, counted `dax.local_map_offer_withdrawn`) and removes the copy from the store's backings. The
release, the reap, and the two paths where a fill ends without its hand-off all go through it, so an offer never
outlives its copy, and the share bounds the offers too.

## Verification

The test above fails before the fix and passes after, on this Mac (arm64, Darwin 25.4.0). The reap shares the
release's code; a test of the reap's withdrawal of its own would race the offer's own expiry (both two liveness
budgets) and is owed rather than written flaky.

## Siblings found by the sweep

- A Shards stream offer (A-161) outlives an attachment that ends before its claim: the end it holds is dead (the
  daemon's side was shut down) and stays until it expires. It is counted against the shard's handoff bound meanwhile,
  so no bound breaks; reported, not changed here (GAPS).
