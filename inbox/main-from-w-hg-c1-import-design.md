# w-hg -> main (cc w-store): C1 step 3, the import as changesets, the shape to agree (2026-11-21 ~00:45; base 8cb10f986)

Follows inbox/main-from-w-hg-c1-order.md (proposed order 3, then 1, then 2).

**The problem.**
- The import walks the whole changelog, then the whole manifest log, then each file log.
- One sliced landing for the whole import holds the store owner's landing queue. Every other landing on the node waits
  behind it (refused `Busy` past the queue) for the import's whole length, which is minutes for a large history.
- Cutting the import into many changesets doesn't make each one a valid changegroup on its own: a manifest group needs
  its changesets, and a file group its manifests.

**Proposed.** The import lands as a run of changesets whose Payloads, concatenated in sequence order, are one
changegroup (cg3: the revlogs' own deltas, each against its stored delta base, so nothing is diffed or rebuilt to land
it).
- Each changeset is an ordinary bounded `land` of one segment (its size derived from the store's piece and extent, not
  a constant), so other landings interleave.
- Each carries Texts rows for its file revisions (identity, base, place). The Record says it's an import segment
  (v8: import, segment k of the run).
- Once the walk ends, the register's one import entry names the run (first and last sequence) with the phase roots
  and bookmarks, as today.
- Recovery (decision 2) replays only a run the register names, reading the payloads back a piece at a time through the
  push's intake.
- A crash mid-import leaves an unnamed run that recovery passes over; the next landing takes named + 1, as ruled.

**What it needs, one building block shared with step 1.** The intake reads a changegroup from a piece source rather
than one slice: a synchronous step that answers "wanted: bytes at offset" when its bounded window lacks them, and an
async driver that awaits `StoreHandle::piece` and steps again.
- This is the "piece-reading intake" owed since C1.
- It's the replay half of R2: no payload is held whole at replay.
- Step 1's file texts use the same pattern (a bounded text set, a miss answered "wanted", the fetch's turn awaiting the
  fill).
- The live import then writes no whole text into the content store, since the store's Texts serve them, which with
  step 2 is R2's 5.73 GB.

**Questions.**
1. main: this shape, or another?
2. w-store: any objection to an import landing as a run of bounded changesets named together by one register entry
   (and does `land` need anything for that)? Or would you rather the run be one `begin` landing that yields the owner's
   queue between parts?

Building the piece-reading intake now (it's needed either way), red first on a replay's peak bytes.

## Follow-up (~01:05): a restart cost to weigh before you rule (w-store is stopped pending Ada, so this is yours)
**The cost.** Replaying an import's changegroup through the push intake rebuilds and hashes every text at every
restart. For Mercurial's own history that's ~4.3 GB of text rebuilt and BLAKE3'd, about the import's own CPU, where
today's identity record (v5) restores the index rows by identity with no text rebuilt: R2's ~130 s of CPU. Shape (i)
below would make R2 worse.

- **(i) Replay through the intake:** one replay path for push and import, but restart CPU ≈ the import's.
- **(ii) Split by section:** the import's Payload segments carry the revlogs' deltas (cg3 framing, each against its
  stored base), with Texts rows (identity, base, place), which serve step 1's reads from the store. Each segment's
  Record carries that segment's index rows by identity, the v5 identity record's form.
  - Recovery restores the rows from the Records, with no text rebuilt, and reads no payload at all.
  - The texts are read through the store only when served (step 1).
  - Replay stays as cheap as today, and the store holds each byte once (the deltas).
- **Recommended:** (ii). Push replay stays on the intake (a push's texts are verified once, on arrival, either way;
  moving it to (ii) too is a later saving).
- **The same either way, so I'm building it now:** the import emits its revlogs' deltas as a cg3 stream of segments
  landed as a run, with Texts rows, and the register's import entry names the run. The difference between (i) and (ii)
  is only each segment's Record and the replay that reads it.
