# Investigation: the post-takeover reseal placement occasionally stalls under sustained load (task #32)

- **Date:** 2026-09-12
- **Area:** §4.8 record plane — the placement of a **reseal** (a fresh snapshot the takeover successor writes
  after adopting a dead owner's object), driven by `crates/server/src/fleet.rs`
  (`run_record_period`/`unplaced_heads`/`ship_head`/`put_seal_content`) and the config-version convergence
  between the successor and the surviving holder.
- **Severity:** test-reliability tail, **not** observed to affect a single CI run. Pre-existing (it appeared
  in the pre-#22 validation runs, so the ephemeral-id work did not introduce it). Status: **investigated,
  mechanism narrowed, not yet reproduced under instrumentation — fix blocked on a live reproduction.**

## Symptom

`a_takeover_successor_serves_the_dead_owners_content_over_nfs` occasionally fails its **final** assertion
(`resealed` — the successor takes a *further* snapshot after the takeover and it must place over the remaining
holder), only inside the **full 24/25-test suite run back-to-back** (never in isolation, never in a single
suite run). The timing is **bimodal**: instrumented, the reseal placement completes in **~220–350 ms** almost
always (and always in isolation, including 6× back-to-back of just this test), but on a rare loaded run it does
**not** place within the deadline at all (observed at both the old 20 s and the raised 60 s). Fast-or-never,
not a smooth slowdown.

## What was ruled out

- **A stale reseal-head generation.** `unplaced_heads` rebuilds the head `Record` with `generation:
  config.version` **every period** (`fleet.rs:1036`), so after any config refresh the next ship carries the
  current generation — the head is not pinned to a stale generation.
- **A record-link idle stalling the ship.** `establish_record_link` idles (stops dialing) **only** when the
  peer is fully *retired* from the neighbourhood, not on a transient suspicion; and #31 hardened the suite
  against false retirement of a live peer. The surviving holder is a live daemon, so its record session is
  kept and `put_seal_content`/`ship_head` retry each period against it.
- **A #31-class dead-voter wait.** The record/content commit uses `collect_acks`/`collect_promises` (already
  progress-aware), and the consensus fan-out uses the #31 progress-aware `broadcast`.

## Narrowed suspect (unconfirmed)

**Config-version convergence lag between the successor and the holder, post-takeover.** The takeover retires
the dead owner — a configuration change. The reseal head ships under the successor's `config.version`; the
holder refuses a record whose generation does not match its own installed version (`accept_held_record`,
`fleet.rs:939`) with `ConfigurationStale`, which flags `config_refresh_wanted` so the behind side fetches the
committed configuration and the head re-ships next period. This normally converges in a few periods (~300 ms).
The hypothesis is that under sustained back-to-back load the council commit of the owner's retirement and/or
the `CONFIG_FETCH_STREAM` refresh occasionally does not converge for the whole deadline, so the reseal head is
refused period after period — a genuine stall, not a slow op. Consistent with the bimodal shape (converged
fast, or not within the window) and with it appearing only under the full-suite accumulated load.

This is **not confirmed**: it did not reproduce under a 120 s-deadline instrumented run (all placements
~200–350 ms) nor across a fresh 3× full-suite run (25/25 each), so the exact stage that stalls (council
commit, config fetch, content put, or head commit) has not been observed. A speculative fix to this
consensus-adjacent path is not warranted (R4) until the mechanism is seen.

## How to reproduce + pin it (next session, when it recurs)

1. Reproduce: run the full fleet suite **back-to-back 3–5×** on a loaded machine (the trigger is accumulated
   suite load / thermal, not the NFS test alone). It is ~1-in-several; not every 3× run stalls.
2. Instrument (daemon-side, temporary) the reseal path per period for the resealing object: log
   `config.version` on the successor and the holder; whether `ship_head` saw `committed.stale_version` (the
   `ConfigurationStale` refusal) and the versions; whether `put_seal_content` found a holder session and its
   `placed.outcome`; and whether the council/config-fetch is advancing. A stall showing repeated
   `stale_version` with non-converging versions confirms the config-convergence hypothesis; a stall in the
   content put or head commit with converged versions points elsewhere.
3. Fix per the confirmed stage (e.g. ensure the post-takeover retirement commit + the config fetch converge
   promptly, or make the reseal await the config generation the takeover installed).

## Meanwhile

The suite's reseal deadline is a generous fixed 60 s (`PLACEMENT_DEADLINE`), which rides out the common
convergence latency; a single CI run is reliably green. The residual is the rare loaded-back-to-back stall
above, tracked here for a live reproduction.
