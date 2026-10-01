# A cut transfer lost all its progress, and every put waited for the slowest offer

**Date:** 2026-10-01. **Area:** `slates-cluster` (content plane), `slates-server` (holder path), `slates-vfs`
(shard image). **Audit:** AUD-29-55 (P2) and AUD-29-58 (P2), one change. **Design:** §4.9 "Transfer and
cancellation", §4.10 placement closure, §4.8 hedged placement, AC-7.7 / T-7.8.

## Description

- **No resumption (AUD-29-55).** A put was one whole archive in one stream body, verified only after its
  last byte. A transfer cut by a deadline, a cancelled round or a replaced session discarded every byte it
  had carried, and the next offer named every chunk again. Missing-set resumption existed only between whole
  successful holds.
- **A slowest-peer barrier (AUD-29-58).** The owner's round gathered every offer reply (or waited out its
  stall policy) before any put left. One slow offer delayed a fast holder that could have placed at once, and
  the recorded put latency started after that barrier, so the hedge's p95 excluded the delay.

## Root cause

- Puts were whole-archive request bodies with no intermediate verified state on the holder. The design's
  "verified ranges and resumable progress" (§4.9) had no realization.
- `put_content` ran the offer round to completion (`gather`), then the put round (`collect_bound`).

## Fix

- **Stages (`crates/cluster/src/content.rs`).**
  - An **offer** carries the manifest (header and tree, no chunks). The holder checks it against its
    retention rule and opens a stage for the object, or resumes the one a cut transfer left. It answers with
    the referenced chunks it holds neither for the object nor in the stage, or acknowledges at once when none
    are missing.
  - A **chunk** travels one per exchange, many in flight on the session up to the peer's stream credit. The
    holder checks the chunk is referenced and within the header's chunk cap, verifies it against its
    identity, and keeps it. The reply is progress (`Staged`). The chunk that completes the closure draws the
    bound acknowledgement.
  - A stage shares the hold's chunk store, reference-counted, so promotion copies nothing. It is charged
    like held content, one per object.
  - A stage is released by events, never by time:
    - its promotion;
    - a newer placement for the object;
    - the retention rule (records past it, a newer placement, the tombstone, the stale-copy reclaim).

    An older offer against a newer stage is refused `StaleStage`; a chunk for no stage is refused `Unstaged`.
    A stage found short at completion (a held chunk released meanwhile) answers with its progress.
  - Stages ride the shard's recovery image at each publish (image version 9). A stage is not an
    acknowledgement, so progress since the last publish carries no durability promise.
- **The owner's round.** Each holder progresses independently: its offer goes out at once, and the moment its
  missing set arrives its chunk transfer starts. Acknowledgements are collected as they arrive. Each transfer
  gets a full span from its own start, and the progress policy's clock restarts with it, so the round is
  bounded by one offer span plus one transfer span, the same bound the two separate rounds had. Latency is
  timed from the round's start, offer included.
- **The whole-archive `hold`** (recovery, tests) runs through the same stage, so there is one path.
- **The test fault** that refuses content placement now refuses offers and chunks, since a complete stage
  acknowledges at the offer.

## Tests

- **The ownership oracle, split in two over one model of the design's rule:**
  - `the_hold_owns_exactly_what_its_manifests_reference`: whole puts and forgets, with its original census.
  - `a_cut_transfer_keeps_its_verified_chunks_and_resumes_from_them`: also offers at three sequences, chunks
    (aimed and drawn, some corrupt) and whole-object forgets. Its 16-case census includes resumed, replaced
    and stale offers, duplicate, unstaged, unreferenced and corrupt chunks, and a stage short at completion.
  - After every step the hold and the model agree on the replies, the staged progress, the stored bytes
    (verified) and the charges. The image round trip carries the stages, and everything releases to zero.
  - Mutation check: a resume that forgets its staged chunks fails the oracle at once, with a shrunk history
    (`Missing({1, 2, 3})` against the model's `Missing({3})`).
- `a_cut_transfer_resumes_with_exactly_the_chunks_still_owed` (daemon, production holder path). For every cut
  after 0..=3 chunks:
  - every chunk before the cut is answered with progress;
  - the content is not held while cut;
  - the re-offer names exactly the chunks still owed;
  - the rest completes it;
  - a re-offer after completion (a lost acknowledgement) is acknowledged at once.
- `an_abandoned_transfer_returns_every_charge_when_replaced` (daemon). A transfer cut after one chunk and
  replaced by a newer placement leaves exactly the replacement's charge (bytes and index, equal to a
  same-shape control placement), and no stage.
- `a_fast_holder_places_while_a_slow_offer_is_outstanding` (simulated transport, AUD-29-58's acceptance). One
  acknowledgement still needed, two offers out, one holder stalled for four spans under a two-span hard
  budget: the fast holder's acknowledgement and the round's return both come inside the offer span. Under
  the barrier the fast put could only leave at the span's end. That is reasoning, not a run of the old code;
  the old message format no longer exists.
- `replies_delivered_during_the_final_poll_place_the_content` first failed on this change, because the first
  cut bounded a late transfer by what was left of the round's span. That is why each transfer now gets a full
  span.

## Same change, second commit: fetch by chunk (AUD-29-55's fetch half)

- **Root cause.** A takeover successor's `Fetch` returned the whole archive in one reply, so a cut fetch
  started over.
- **Fix.**
  - `Fetch` → `Have` now carries the manifest only, and `FetchChunk` → `Piece` serves one chunk. A holder
    serves a piece only for an object that holds the named manifest and references the chunk (AUD-29-45).
  - The successor (`fleet::fetch_into_hold`) stages the manifest in its own hold (`stage_fetched`). It fetches
    each chunk the stage lacks, many in flight, verifying and keeping each as it arrives (`stage_piece`), and
    completes the stage once whole (`complete_stage`).
  - A cut fetch keeps its verified chunks, so the next period asks only for the rest.
- **Tests.**
  - `a_cut_fetch_resumes_over_a_session_with_exactly_the_chunks_still_owed`, over a real session on the
    simulated fabric: 2 chunks wanted, the first fetch cut after one, exactly 1 wanted on resumption, and the
    archive rebuilt byte for byte.
  - `a_cut_fetch_resumes_with_exactly_the_chunks_still_owed`, at the hold level.
  - The scoping test now also refuses another object's chunk fetch.
  - The fleet takeover, successor and holder tests: 16/16.

## Siblings reported

- **Collector latency is quantized by its poll interval.** The owner's collector polls its reply channels on
  the budget's poll interval rather than being woken, so an acknowledgement is observed up to one poll late.
  The barrier test had to use a quarter-span poll to see the protocol rather than the poll.
- **A transfer whose spawn is refused loses its holder's session** (moved into the refused task). This is the
  same pattern as `dispatch_round`'s refusal path.
