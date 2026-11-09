# A holder's complete page that left a copy out read as holding nothing

**Found.** Review of A-251's takeover rule (raft-chaos, 2026-11-08), after raft's
`2026-11-07-a-holder-restarted-warm-lost-its-copies-routing.md`, whose holder "answered the successor's page empty and
complete, so the round counted its promise while its copy was never fenced". No run has shown a lost record.

**Root cause.** A successor counted a holder's complete paged answer as a promise of every object it did not list.
`takeover::list_page` lists a copy only when the holder's own routing and lineage (`object_owner`, `departed_owners`) say
it falls to the asker, and that routing is not persisted. So a holder's silence about an object said nothing about what
it held. A successor could reach an object's recovery quorum on such an empty promise and adopt an older record over a
committed newer one. Step (c) closed the same hole for a copy the holder could not promise
(`2026-11-08-a-holder-that-could-not-promise-a-copy-read-as-holding-nothing.md`).

**Impact.** A takeover could lose a committed record (NoLoss). `crates/db/tests/takeover_model.rs` shows the rule doing so:
over 2,000 generated histories, 216 committed records lost with holders leaving copies out of complete pages, none
without.

**The same hole in the successor's own copy.** It learns its own copies through its routing (`promise_local_copies`), and
counted its open lease gate as its promise. A successor whose routing lost its own newer copy could then adopt a member's
older record: with the nemesis leaving out the successor's own copies too, that rule lost 86 committed records over the
same histories. Found while designing step (d)'s process test.

**Edits (A-251 (b)).**
- `crates/db/src/takeover.rs`: `CountingRule::NamedAnswer`, and `answer_by_name`, the holder's answer from its own records.
- `crates/db/src/register.rs`: `HostPrepare::named` and `HostPromise::nothing`, appended to the wire.
- `crates/server/src/takeover.rs`: the successor asks each member whose pages are complete about what it did not list
  (`begin_round`, `unanswered_by_name`), folds the answers (`fold_answers`), and counts under the named rule
  (`ready_objects`). A holder answers through `answer_named`. The successor answers itself by name, from its own records,
  while its lease gate is open (`answer_own_copies_by_name`); the named rule no longer counts its gate alone.

**Tests.**
- `a_complete_page_promises_nothing_of_an_object_it_did_not_list` (a real shard): after a complete, empty page the object
  is not ready and the next round asks about it by name; after the answer it is ready.
- `a_holder_lists_a_copy_whose_lineage_it_lost_and_answers_by_name` (a real shard, the copy's lineage forgotten): its
  page lists the copy by its authority, and by name it reports the record and says it holds nothing of an absent object.
- `a_successor_whose_routing_lost_its_own_copy_still_adopts_it` (a real shard): the successor's own newer copy, its
  lineage forgotten, is adopted over a member's older record. Red with the successor not answering itself.
- `a_holder_answers_each_named_object_from_its_own_records` and `a_named_answer_keeps_to_its_budget_and_reports_a_higher_fence`
  in `crates/db/src/takeover.rs`.
- `a_named_question_and_its_answer_round_trip_and_a_page_keeps_its_bytes` and `a_hostile_named_list_is_refused`.
- The model, `crates/db/tests/takeover_model.rs`.

**The names (folded in at the supervisor's direction).** An object every holder leaves out was never learned, so nothing
asked about it by name. With the nemesis widened so any holder may omit any object, the named rule over today's pages left
664 committed objects unlearned over 1,000 histories. A holder now lists, and a successor learns of its own, every copy
whose authority's line, or the line of the owner an install moved that authority from, ends in this round with the asker as
successor (`slates_db::takeover::copy_falls_to`). Routing plays no part.
- **Measured and rejected:**
  - routing plus a chain path (204 lost);
  - "the chain or the asker", which learns the asker's own objects and would raise their fences;
  - the cohort-kept directory (one more commit per create).
- **Tests:**
  - `a_copy_moved_to_the_successor_is_listed_by_the_owner_it_moved_from` (a real shard): red without the moved-from half,
    and red with installs recording none.
  - `the_askers_own_object_is_never_listed_in_another_hosts_round` (a real shard): red with the literal rule.
  - `a_copy_falls_to_one_round_by_its_authority_alone`: red with the round unchecked; its first cut missed that.
  - `a_successor_holding_nothing_of_an_object_promises_so_by_name` (a real shard): red with no self-answer.
- `a_restored_copy_keeps_the_owner_it_moved_from_and_is_still_listed` (a real shard, a held image written and restored):
  red with the image not keeping the field, and with the reconcile recording no move.
- `an_acceptor_keeps_only_the_last_retired_owner_its_authority_moved_from`: red with a move from a live owner recorded.
- The held image keeps the field (image version 23).

The process test is step (d): `a_takeover_adopts_the_newest_head_a_holder_left_out_of_its_pages`.

**R3.** A question names the ids that fit one page after the request's fields (`named_per_question`, from `page_budget`). An
answer is bounded by `page_budget`.
