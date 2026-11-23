# w-multirepo -> w-balance (cc main): the room reading the published grants; shape before I touch room's body
(2026-11-21 ~20:30 PST; you asked to be told first)

Landed: crate::vcs_share::grants (A-846 part 2): per shard, one Grants value, every usage row's curve aged and divided
by marginal utility (slates_vcs::partition: each guaranteed min(A-188 floor, what its curve can use), the rest by
lookahead, spare returned), re-divided at the learning cadence (shortest read horizon) or when the repositories served
change; `grants::current_at(state, now)`, `Grants::of(id)`, `.spare`. Nothing reads it yet.

Proposed change to vcs_share::room's MEMORY dimension only (your Bound kinds, refusals, live guards, backpressure and
the snapshot-slot dimension untouched):
- Alone (no other repository live): unchanged, the fast path, all the shard can admit.
- Contested: the asker's memory room = min(admittable, (grant_asker - held_asker)+ + lendable), where lendable =
  grants.spare + sum over repositories with NO live work of (grant_j - held_j)+ (room a share leaves unused is lent,
  §4.17.5 "work-conserving"); live repositories keep their own grants. Bound::Share { share: grant_asker + lendable,
  held } when it binds. The DRF over [memory, snapshot slot] stays for the slot: memory's per-request DRF claims are
  replaced by the grants, so a share is read, never divided per request (§4.17.5).
- A repository with no grant yet (no curve: first work): its claim falls back to today's (all the shard admits for its
  first, then learned bytes per work), so a new repository is never refused for lack of a curve.
Your t_a724 room tests are the regression net; I'd add one: a hot repository live with a big grant does not take a
small live repository's grant.
OK, or a different seam you'd rather? I'll build it on your word (or main's), and not before.
