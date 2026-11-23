# w-git -> main: does the upload stream before the whole pack is computed, and can its work spread? (r229 item 2)

I read the code at main a2165a087+ (w-git-p2). Nothing was measured: I need r229's profile split to confirm which
phase dominates.

## 1. What runs before the first pack byte

A fetch's request runs these, in order, all on the repository's owner shard:

1. **`git_derive::derive_for_read`.** It derives the sets of every version admitted with no walk (A-614 amended).
   It is **waited for whole**: the answer is made from those sets, and nothing bounds the wait by time.
   After a whole-history push into an empty repository (git/git's), every version was admitted unwalked.
2. **`git_encode::encode_for_fetch`.** It encodes each unencoded version's fragments (the delta search), waited for at
   most one liveness window (A-718 continued). Past the window the answer begins and `encode_behind` finishes the rest
   in a task.
3. **`Upload` stages `Objects` and `Gather`.** These build the wanted set whole, because the pack header counts the
   objects. git's `pack-objects` counts first too, but it shows "Counting objects" progress meanwhile. We send
   keepalives only.

Everything after the header streams, a pkt-line's room per step (`write_answer` / `write_within`, both edges):
- `Runs`: kept fragments copied;
- `Cover`: base packs and fragments;
- `Holders`;
- `Loose`: whatever no part covered, each object delta-searched and deflated a window a unit.

So the pack streams, but before its first byte comes (1), unbounded; then (2), up to one liveness window; then (3),
reads only.
**Likely suspect for r229's 80-100 s of single-shard CPU:** step (1) on a fresh whole-history push. Then (2) and the
loose stage's search, if the encoding fell behind (A-718 measured 72% of a clone's CPU in `deltas::Search::step`
before encoding existed).
**What I need from r229:** the sample's split across `git_derive`, `git_encode` / `Encoding::advance`, and
`Upload::advance` (the `Loose` stage), plus the counters `vcs_store.git_encode.*` and the upload's `loose_unsearched`.
That says which piece is first.

## 2. Can it spread across shards (§4.17 "Giant repositories": the owner keeps the register and ordered landing; "everything else spreads")?

Today none of it spreads:
- the derive, the encoding and the stream all run on the owner shard;
- git spreads its delta search over `pack.threads` (one per core by default, `find_deltas` per thread).

The work that can move is pure and per-version:
- **Encoding a version.** Its fragment depends on that version's objects, the bases a reference delta may name in
  the version before it (read only), and the repository's learned window and levels. Those last two are **learned as
  each unit runs and carried to the next version** (`encoding.rs`: "Each unit learns into the repository's own window
  and levels"). So versions are not independent today. Encoding them in parallel needs the rules taken as one snapshot
  when a batch of versions begins, each helper's learning merged back after. The bytes then equal a one-shard run
  under the same snapshot, deterministic but not today's bytes; the oracle compares against that. Each helper is lent
  the version's object bytes a window at a time (a cross-shard message, as the content homes already move windows) and
  sends back the fragment's bytes, which the owner keeps in version order. This is a rule change to §4.17.2's
  encoding, so it's yours to rule on.
- **The loose stage's entries.** Ahead of the stream's cursor, helpers could compute the next objects' entries (delta
  plus deflate), bounded by a pipeline depth derived from the link's learned rate (R3). The owner writes them in order
  and checks each count against the header, as it does now.
- **The derive of unwalked versions.** Per version along a chain, so it is sequential within a green but parallel across
  greens (git/git's 1,000+ tags are each a green).

What I'd build, red first (a 2-shard daemon, the CPU of the encoding spread over both shards, counted):
- **P1:** a version's encoding on a helper shard under a rules snapshot per batch, the owner keeping fragments in order.
  Oracle: the same fragments as one shard's under the same snapshot.
- **P2:** derives across greens in parallel.
- **P3:** loose entries pipelined ahead of the cursor.

Separately, and cheaper: give step (1) the same bounded wait as (2) (begin the answer past a liveness window; the rest
behind it), and send git's progress side-band during (1)-(3) as `pack-objects` does.

I start on whichever you order. Until then I'll do (b)(1), git_interop's per-step PATIENCE derived from the work, as
a separate piece.
