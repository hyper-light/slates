# w-shared -> main: making the owner's durable step small, two shapes (2026-11-21 ~03:55 PST)

Measured (32 MiB holder submit, debug, this Mac): the owner's step for `SubmitComputed` is one 2.3 to 6.9 s step:
append 1.2 s, and ~1.1 s more in the transaction's commit and the publish, all writing and digesting the chain entry,
which holds the increment's encoding byte for byte (`slates_merge::chain_entry`, format 5: "the increment's own
encoding ... since a merge record's inputs are those bytes"). In release at 320 MiB the same parts are ~300 + ~600 ms
(the bench's append/place). Decode is 0 (it lends), the verdict by position 5 ms.

**(A) By reference.** The chain entry names the increment by its inputs manifest identity, and the bytes live once in
the content store, held by the version's record until it folds into a checkpoint. The record plane already ships a
version's inputs by that identity, so holders change nothing. Costs: a durable format change (chain entry 6), and the
owner's recovery then needs the content store to hold the bytes across a restart. Its durability today is the
segment's, as the db's is, which I'd verify before building. Memory: one copy of each byte (MEMORY SIZING).

**(B) Sliced append, format unchanged.** The large entry is staged a slice a turn (copied into the log's segment and
fed to the digest a slice per turn, BLAKE3 streaming), and committed by a small final step, as A-757 part 4 takes a
large live message (its entries hashed a slice a turn before they are taken). No durable format change and no change
to recovery; the step bound holds. It keeps two copies (the decoded increment and the entry).

**Recommendation:** (B) first. It closes the bounded-work bug and the 32-320 MiB liveness with no durable-format
change, and I can start now. (A) after, as the MEMORY SIZING follow-up, once you or Ada rule on the chain entry
format and the content store's durability. Main's 3b, content sent ahead once by identity so a retry re-sends nothing,
goes with (A). With (B) alone, a retry after a real stall still re-sends.

Unless you say otherwise, I'm starting (B): first reading the db's write path (`Partition::mutate` / the segment
publish) to see where a staged entry fits, red test the 32 MiB holder submit at 10 of 10, and each slice under the
step quantum (`merge.submit.owner_step` and `daemon.beat.late` the proof).
