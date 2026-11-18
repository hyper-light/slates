# A restarted holder refused every later version of its greens (2026-11-18, §4.16 "Apply on holders", A-313)

Found by reading, while designing the takeover of a green over a base for goal g232 (100,000 clients against one
primary and two replicas editing the Linux kernel tree). The takeover's successor is one of the green's holders, so
what a holder keeps across a restart decides what a takeover can start from.

## Description

A holder recomputes every version of a green before it accepts the version's merge record (§4.16 "Apply on holders"):
it submits the record's increment to its replica of the green, an engine kept per green in `merge.replicas`, and
compares the head identity with the record's. The replica lived in memory alone.

A warm restart keeps a holder's member id, so it keeps everything the holder acknowledged:
- the merge records it accepted, in its acceptor (`holder_records`, rebuilt by `restore_held_registers`, §4.8
  "Recovery");
- the inputs it held, in its content hold (AUD-29-59).

Nothing rebuilt the replica. The restarted holder met the next version's record with no replica, and
`accept_merge_record` answered every version after 0 `merge.out_of_order`. Its content answer
(`merge_service::inputs_held`) still told the owner it held the record's inputs, so the owner shipped the record alone
on every pass of the record plane, and the holder never acknowledged it.

## Measured

The failing test, `a_holder_restarted_over_its_segment_recomputes_the_greens_next_version`
(`crates/server/tests/fleet.rs`): on a settled three-node fleet at `f = 1` whose nodes keep their state across a
restart, a green on node a takes version 1 on both holders; holder b is restarted over its own segment; holder c
withholds merge records, so a version can commit only through b; a second edit is submitted. The test asserts the
submit answered `Submitted { version: Some(2) }`, version 2 placed at the quorum within the placement deadline, b's
replica at version 2, `merge.replica.rebuilt` moved on b and `merge.record_withheld` on c. The runs, red and green,
follow.

## Root cause

The replica is derived state: everything it holds follows from the records the holder accepted and the inputs they
name, both of which a restart keeps. Version 0 had a path that builds it from the inputs (A-286's seed), but only for
version 0's own record; a holder whose records ran past version 0 had no path back to an engine at all.

## Fix

`accept_merge_record` (`crates/server/src/merge_service.rs`) asks `merge_origin::rebuild_replica` for a replica when it
has none. With no accepted record of the green there is nothing to rebuild, and version 0's first record is handled as
before. Otherwise the holder rebuilds the replica through the newest version it accepted (`ReplicaSeed::rebuild` in
`crates/server/src/merge_origin.rs`):
- version 0 seeded from its held inputs a share at a time, as A-286 seeds it, or the empty engine of a green with no
  origin;
- each later version's record read from the acceptor (`Acceptor::value_at`, one lookup, so nothing of a long chain is
  decoded up front), its increment gathered from the hold an extent a unit, decoded, submitted as exactly that version,
  and its head compared with the record's identity.

The build is sliced as the seed is: its first slice runs in the exchange that asked, so a short chain is acknowledged
there; a long one goes on in a task of the shard, owned by the green's entry in `seeding`, and every record of the
green is answered unacknowledged until it ends. A version missing from the records (`merge.replica.records_short`) or
inputs no longer held (`merge.inputs_unheld`, a green forgotten meanwhile) installs nothing; another identity refuses
the green on this holder for good (`merge.recompute_mismatch`), as a record's own recomputation does. Counted:
`merge.replica.rebuilt` once a rebuild, `merge.replica.rebuilt_versions` each version recomputed.

## Impact

With one primary and two replicas at `f = 1` a version commits at the owner and one holder. One replica's restart left
every version one holder's answer from stalling; a second restart, or the other replica's loss, stalled every submit to
the green, and a takeover had one fewer holder able to back it. Each pass also re-shipped every stalled record to the
restarted holder, a cost that never ended.

## Siblings (swept)

- **`merge.refused` is in memory too.** A green a holder refused for good after a mismatch is forgotten at its restart,
  so the holder rebuilds the replica and refuses the green again only when the mismatched version arrives. Same answer,
  later; recorded in GAPS.
- **A long chain is rebuilt from version 0.** The holder keeps no checkpoint of its replica, so a rebuild recomputes
  every version it accepted (300,000 at the 100,000-client rung's three rounds); the holder's growth per version is
  A-238's owed item. Recorded in GAPS.
- **The owner's own side was already masked** (A-246, A-252): a restarted owner rebuilds its green from its chain and
  ships the versions it still owes.
