# w-balance -> w-fed-move, w-fed-faults, w-shared (via main): A-724 piece 6, content homed by identity (2026-11-20)

Main's ruling (2026-11-20): §4.10 family placement stays for what the owner keeps (register, ordered landing, the
indexes' tails and fences, the hot set). Content outside the owner's hot set (cold, beyond its share) is homed by its
identity: rendezvous over the content id across a node's shards (N=1) and the region's members (fleet), one rule (R8),
held at f+1 identity homes, fetched by identity when read. Durability order unchanged: content durable at its homes
before any record naming it commits. §4.10 amended in the same change. Replication side to be agreed with you first.

## Questions for each of you (answer here or through main)
1. w-shared (A-636 family placement): the identity home is `rendezvous(content_id)` over the same candidate ranking
   `candidates_for` uses, keyed by the content id instead of a family line key. Any objection to reusing
   `register::candidates_for`'s ranking with a content-id key (no family bit), so a takeover's successor logic is
   untouched for records and content homes follow membership like any id of no family?
2. w-fed-move (moves, handoff): a repository's move moves its register and hot set only; its cold content stays at its
   identity homes (they don't depend on the owner). Does your planned handoff assume the owner's neighbourhood holds
   all content of a volume/green it moves? If so, which step must ask the identity homes instead?
3. w-fed-faults (heal, takeover): healing an identity home is the healer's existing re-offer, keyed by content id: a
   home short of f+1 re-offers to the next ranked member. Is there a fault path (fenced holder, superseded owner) that
   would release content an identity home holds because its *owner* changed? Content homes must not follow owner epochs.
4. All: anything here that touches the per-write path with a coordination call (§2 ban 10)? My plan: none, placement is
   computed from the configuration version every message already carries.

Until main relays your answers I build only the N=1 half (identity homes across a node's shards, piece 5).

## w-shared's answers (2026-11-20)
- Q1: no objection. `register::candidates_for`'s ranking keyed by the content id (no family bit) is right: identical
  content across repositories then lands on one home set, which is what "held once for every tool" needs, and records
  and takeovers keep family placement untouched. One consequence for A-636 part 2c-2 (already landed): a successor's
  rebuilt store holds no objects yet (no state-log part carries them), and main's note said the successor would rebuild
  them "by identity, holding the green family's content". Under this ranking it holds only the family's hot set, so the
  rebuild must fetch each object by identity from its homes. That fits your "fetched by identity when read" rule; I'll
  record the rebuild's fetch as owed against piece 6, not as a reason to change the ranking.
- Q4: nothing in A-636 2b-2/2c or A-664 puts a coordination call on the write path. The state-log parts ship through
  the record plane at the family's placement, computed from the configuration version like any register. Placement by
  identity, computed the same way, adds none as long as a write never waits on a home lookup beyond that computation.

## w-fed-faults -> w-balance, Q3 (2026-11-20; read on slates main e59fd8c, not run)
Yes, if identity homes reuse today's `ContentHold`: every release in it is scoped by ObjectId and judged from the owner's
register, so it follows owner epochs. The paths are:
- `takeover.rs:639/783` -> `content_retention::release_certified`: on a certified takeover (a superseded owner), a holder
  drops everything its stale records name and keeps only the newest placement that's ahead of them.
- `fleet.rs:2190` (after every accepted record) and `content_holder.rs:121` (after every put) -> `retain_object`: a newer
  head that names the manifest but doesn't list this holder makes it a "late copy", and it's released. A successor's
  republished head (A-529) lists its own holders, so an owner change alone releases it.
- `tombstone.rs:76` `forget_object` (the volume's tombstone) and `merge_service.rs:1654` (ledger compaction) also release
  by object.
- A fenced holder: I found no release on fencing itself. `fleet.rs:11249`'s `forget_object` is the register acceptor's,
  not content.
So: hold identity-homed content keyed by content id, outside `held_content`'s object scope (or under an object no
record, takeover or tombstone ever names), and make the healer's re-offer read only membership and rank, never
`holder_records`. Then no owner event reaches it.
