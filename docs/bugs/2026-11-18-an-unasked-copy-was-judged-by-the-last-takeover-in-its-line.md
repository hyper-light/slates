# An unasked copy was judged by the last takeover in its line (2026-11-18, A-484)

## Description

A holder that no takeover round asked (it sits outside the retired owner's neighbourhoods, holding a copy an earlier
neighbourhood left there) let its copy go where a takeover showed the object recovered within tolerance (A-341). The
takeover it judged was the one its copy's routing had last moved from: `departed_owners`, overwritten at every move.
When the copy's line held two takeovers, that was the wrong one.

Found by reading, while building A-341's owed piece (the completion record), not seen in a run.

## Root cause

The tolerance argument holds only for the first takeover after the copy was written. Within tolerance every committed
record of the object is on a recovery quorum's worth of each settled cohort, so that takeover's round learned a record
at least as new as the copy and adopted it. A later takeover's cohorts are its own owner's, and that owner may never
have held the object.

The history that breaks it, at `f = 1`:

1. Member W writes object o to its cohort {W, A, X} and, jointly, to a neighbourhood it later leaves without
   settling. Holder H keeps a copy from that neighbourhood.
2. A dies, then W dies. W's takeover is past tolerance for o (one cohort host left). Its successor X asks the members
   of W's neighbourhoods. None of them holds o, so X never learns o and confirms its share without it. H's copy now
   routes to X, and A-341 keeps it, past tolerance.
3. X settles a neighbourhood that leaves H out, then dies. X's takeover is within tolerance for o: its cohort is
   whole, though none of it ever held o. H's copy routes on to X's successor, `departed_owners` now names X, and
   `within_tolerance(X, o)` is true.
4. H lets the copy go. It may have been o's last copy, a record placed at `f + 1` before the faults.

## Impact

A copy the design keeps (A-309: "past tolerance it may be the only one") was let go once a second takeover in its line
was within tolerance. That needs a past-tolerance takeover followed by a second death, so it is rare, but it loses the
last copy of an object nobody adopted.

## Fix (A-484)

- The holder judges the first takeover since the copy was written: its writer's. The acceptor keeps an in-memory
  `written_by`: the owner of the last record it accepted, kept while an install moves the authority to a successor.
  An acceptor rebuilt from an image after such a move knows no writer, and its copy is kept.
- `RegionalConfiguration::copy_stale(writer, object)` holds when two things are true of the writer's takeover:
  - its share of the object is done (`confirmed_for`);
  - it recovered the object within tolerance (`within_tolerance`).
- That takeover is judged on its kept retirement, or once it completed on its completion record, which now keeps the
  neighbourhoods it recovered through and the hosts that stayed members through it (regional layout 5).
- `within_tolerance` counts only the retirement's survivors that are still members, or the record's held hosts. A
  cohort host that retired first and was admitted again under its id no longer counts.
- Exact edits:
  - `crates/server/src/takeover.rs`: `unasked_copy_stale`, used in `stale_after_confirmation` and on a settled line.
  - `crates/db/src/register.rs`: the functions and fields above.
  - `crates/cluster/src/raft_wire.rs`: the record's encoding.

## Test

`a_copy_whose_writers_takeover_was_past_tolerance_is_kept_through_a_later_takeover` (`crates/server/src/daemon.rs`)
drives this history through the council and the holder's installs. Red before the fix (the copy let go at the second
install); green after (kept both times, `fleet.takeover.kept_unasked` 2, `fleet.takeover.unasked_covered` 0).

## Siblings swept

- The lineage model (`crates/db/tests/lineage_model.rs`) mirrored the same rule (`copy.departed`, overwritten at every
  move). It is changed with the server.
- Every other caller of `within_tolerance` judged a kept retirement through `departed_owners` alone; there was none
  besides `stale_after_confirmation`.
- The settled-line path (`Lineage::Settled`) kept every unasked copy, so it had no such hole. It now judges the
  writer's takeover the same way.
