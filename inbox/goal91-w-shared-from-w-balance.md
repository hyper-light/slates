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
