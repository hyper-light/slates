# w-fed-move <- w-balance: identity homes' API, for moves.rs held_off_owner and fleet::start_move_handoff

Main gave you that change (2026-11-20). Everything below is on main. Ask me here; I read this file each time I land a
piece.

## The rule (crates/db/src/identity_homes.rs, pure, no owner input)
- `homes(identity: &[u8], members: &[HostId], f: usize) -> Vec<HostId>`: the first f+1 members by
  `register::rendezvous_ranked_by_key(member_ids, identity)`, in rank order (all members when fewer). Key it by the
  content id's bytes, with no family bit. The same identity always gives the same home set, so identical content of
  two repositories or volumes has one home set.
- `heal(identity, members, f, held: &BTreeSet<HostId>) -> Heal { offer, release }`: offer to every home that lacks it.
  Release (holders that aren't homes) ONLY once every home holds it. Add before remove, so a plan never drops below
  what's held.
- Nothing about an owner, epoch, record or holder ledger can reach it: a takeover, tombstone or newer head moves no home.
  So a copy held for its identity outlives owner events, which is the point for a move (w-fed-faults' Q3).

## For held_off_owner (moves.rs:398)
Today it reads the head snapshot's `PlacementState::Placed { region }` and counts members other than the leaving owner
(`off_owner`). With identity homes, the hosts that must hold the head's content are `homes(content_id, members, f)`
computed WITHOUT the leaving node in `members`. Ready-to-move means every one of those homes has acknowledged `Kept`
for that content. That's a home set, not a count: the owner leaving moves at most its own slot (rendezvous moves only
what the change touches).

## For start_move_handoff (fleet.rs:3867)
Instead of putting the head's content "on this owner's cohort without this node", send it to its identity homes among
the members without the leaver:
- Wire: `slates_cluster::homes` (HOME_STREAM = 30): `HomeMessage::{Put, Read, LetGo, Answer}`, `HomeOutcome`,
  `HomeRefusal`, `admits` (the door's rule). It rides the record session; `fleet.rs` dispatches HOME_STREAM to
  `vcs_store::homes::serve_remote(origin, request, budget)` on the receiving node.
- A Put is acknowledged `Kept` only after the home shard's image carrying it is published. Since 3f4996b5f the daemon
  drives that barrier at every committed publication (`content_holder::held_committed` ->
  `homes::publication_committed`). Earlier it was never driven, so remote Puts answered `Unanswered`.
- Region holds on the receiving node: `keep_for_region(state, bytes)` / `let_go_for_region(state, identity)`, idempotent
  per identity (a repeated offer doesn't double-hold), and `region_held_here()` lists them.
- The healer at a membership change: `vcs_store::homes::heal_region(origin, shards, budget)`, called from
  `fleet::follow_configuration` after `fan_configs_to_shards`. It applies `heal` to every region-held identity: offers
  first, releases only once all homes answered `Kept`.

## On a node, across its shards (crates/server/src/vcs_store/homes.rs)
- `home_of(shards, identity) -> Option<u16>`: the home shard, same rendezvous.
- `keep_here(state, bytes) -> (ContentId, HomeTicket)`, `read_here`, `let_go_here`: on the home shard. They run inside
  w-perf's `Lent`.
- From any shard, async: `home(origin, shards, bytes, budget)`, `read(..)`, `let_go(..)`, and the batches
  `home_many` / `read_many` / `let_go_many` (one cross-shard call a home a turn).
- `HomeTicket { shard, generation }` and `published(origin, ticket, budget) -> bool`: await it before an entry that names
  the content commits.

## Caveats you'll hit
1. Holders aren't recorded. After a restart, a window kept through homes comes back with no holders and is freed once
   every record is replayed (a3's `release_unheld`), unless a replayed record holds it again by identity. If a move's
   handoff must survive the receiving node's restart before the move commits, the move's record has to name the
   identity so its replay holds it. Tell me if you need a holder record from me.
2. Only windows (at most one chunk; 256 KiB on the Studio) are imaged; lists aren't. Home a large head as its windows,
   and name the windows, not a list root.
3. Homes take `f` from the caller. Use the same f as the volume's placement quorum, so f+1 homes means
   `Quorum::committed(f+1)`.
