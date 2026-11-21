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

## Addendum ~04:05 PST, after reading the write path: (B) is a db change too, so I'm waiting for your ruling
`judge_submission` -> `record_accepted` encodes the chain entry (the increment's bytes whole) into the verb's transaction,
and `run_recorded`'s commit writes and checksums that one log record (`slates_db::record::checksum` shows in the
profile). Slicing it (B) means a transaction whose large record is written over several turns before its commit
point, with a torn record never replayed: a change to `crates/db`'s log and its crash safety, not only to the server.
It also overlaps A-767 (w-store's changeset log, the store history is moving to). So both shapes reach past my lane:
(A) changes the chain entry's format and leans on the content store's durability; (B) changes the db log's write path.
I'd rather you or Ada choose, or route it to w-store, than build either on my own reading. Until then: the lane's 3a is
landed (71e55b6c2), the 8 MiB liveness test holds, and at 32 MiB+ the owner's one long step is the measured blocker
(GAPS has it). Family three waits on kiln r14 (adacachy is not answering).

## main's ruling, 2026-11-21 00:52 PST: (A), by reference. Not (B), not even first.
- Why:
  - (B) builds a multi-turn staged record into crates/db's log, with its own torn-record crash safety, which (A) then
    deletes. That's layering (CLAUDE.md §2 item 7).
  - (B) keeps two copies of each byte (MEMORY SIZING).
  - (A) removes the owner's work instead of slicing it. The step writes an identity, and the content's BLAKE3 is
    computed once as it arrives.
  - (A) carries 3b, so a retry re-sends nothing.
- In order:
  1. Prove the content store's durability across kill -9 and restart in the same anchor-held segment as the db (a
     recovery test). If it's weaker, close that first, never by keeping the bytes in the log.
  2. Chain entry format 6 as a durable format change: amendment log, golden vectors, the recovery oracle. Do what
     §4.8 and the anchor handoff state about reading a predecessor's format-5 entries.
  3. Retention: the content is held until it folds into a checkpoint, charged as retention (A-714), with a typed
     refusal at the shard's room.
  4. One content store, the one A-767's design names. Record any overlap in GAPS for w-store, which is stopped
     pending Ada.
- Meanwhile: measure 3a (71e55b6c2) in release at 64-512 MiB, 10 runs each, all shown. If 320 and 512 pass 10 of 10,
  the liveness is closed. The owner's long step stays the bounded-work bug that (A) closes.
